//! From an appended batch to a provider call.
//!
//! [`Presence`] says who is online, [`Planner`] decides coalescing and the
//! per-device cap as a value over an explicit clock, and [`Dispatcher`] owns
//! the queue, the worker that drains it, and the delivery tasks.

use super::PushTokenRegistration;
use super::{ApnsProvider, PushError, PushIntent, PushKind, PushProvider, PushSetupError};
use crate::config::PushConfig;
use crate::metrics::{Metrics, LATENCY_BUCKETS};
use crate::state::{Clock, ServerState};
use crate::store::Store;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Presence
// ---------------------------------------------------------------------------

/// Which devices have an event stream open right now.
///
/// A device with one receives ops on it, so waking it would be noise. Held by
/// the stream's own task through [`Present`], so it ends exactly when the
/// stream does, whatever ended it.
#[derive(Debug, Clone, Default)]
pub struct Presence {
    held: Arc<Mutex<HashMap<String, u32>>>,
}

/// One open stream's claim that its device is online. Released on drop.
#[derive(Debug)]
pub struct Present {
    held: Arc<Mutex<HashMap<String, u32>>>,
    device_id: String,
}

impl Presence {
    /// Mark `device_id` online until the returned guard drops.
    #[must_use]
    pub fn hold(&self, device_id: &str) -> Present {
        *self.held.lock().entry(device_id.to_owned()).or_insert(0) += 1;
        Present {
            held: Arc::clone(&self.held),
            device_id: device_id.to_owned(),
        }
    }

    /// Whether `device_id` has an event stream open.
    #[must_use]
    pub fn is_online(&self, device_id: &str) -> bool {
        self.held.lock().contains_key(device_id)
    }
}

impl Drop for Present {
    fn drop(&mut self) {
        let mut held = self.held.lock();
        if let Some(n) = held.get_mut(&self.device_id) {
            *n -= 1;
            if *n == 0 {
                held.remove(&self.device_id);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Coalescing and the per-device cap
// ---------------------------------------------------------------------------

/// The bounds a dispatcher runs under.
#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    /// The coalescing window per `(device, stream, kind)`.
    pub window_ms: u64,
    /// Pushes one device may be sent in any sixty seconds.
    pub per_device_per_min: usize,
    /// Wakes that may wait for the worker before new ones are dropped.
    pub queue: usize,
    /// Deliveries in flight at once.
    pub in_flight: usize,
    /// Attempts per push, the first included.
    pub attempts: u32,
    /// Wait before the second attempt; doubled before each one after.
    pub backoff: Duration,
    /// How long one attempt may take.
    pub send_timeout: Duration,
    /// How often closed windows are checked for a trailing push.
    pub tick: Duration,
}

impl Default for Tuning {
    /// `push-notifications.md` §Coalescing: a 30 s window and ten pushes a
    /// minute per device.
    fn default() -> Self {
        Self {
            window_ms: 30_000,
            per_device_per_min: 10,
            queue: 1024,
            in_flight: 16,
            attempts: 3,
            backoff: Duration::from_secs(1),
            send_timeout: Duration::from_secs(10),
            tick: Duration::from_secs(1),
        }
    }
}

type WindowKey = (String, [u8; 16], PushKind);

#[derive(Debug, Clone, Copy)]
struct Window {
    opened_ms: u64,
    /// An op arrived after this window's push went out.
    trailing: bool,
}

/// What [`Planner::offer`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    /// No push for this key inside the window: send one now.
    Send,
    /// One already went; this op rides the trailing push at the window's end.
    Coalesced,
}

/// Coalescing and the per-device cap, as values over an explicit clock.
///
/// The first op for a `(device, stream, kind)` sends at once and opens a
/// window. Ops inside the window send nothing; if any arrived, one trailing
/// push goes when the window closes, because the device may have synced and
/// slept before they landed. Separately, a device is sent at most
/// [`Tuning::per_device_per_min`] pushes in any sixty seconds.
#[derive(Debug)]
pub struct Planner {
    windows: HashMap<WindowKey, Window>,
    sent: HashMap<String, VecDeque<u64>>,
    window_ms: u64,
    cap: usize,
}

impl Planner {
    /// An empty planner.
    #[must_use]
    pub fn new(tuning: &Tuning) -> Self {
        Self {
            windows: HashMap::new(),
            sent: HashMap::new(),
            window_ms: tuning.window_ms,
            cap: tuning.per_device_per_min,
        }
    }

    /// An op for `(device_id, stream_id, kind)` arrived at `now_ms`.
    ///
    /// Opens nothing: a window is opened by the push itself, through
    /// [`Planner::open`], so an op whose push is skipped or refused leaves the
    /// next op free to send at once.
    pub fn offer(
        &mut self,
        device_id: &str,
        stream_id: [u8; 16],
        kind: PushKind,
        now_ms: u64,
    ) -> Offer {
        let key = (device_id.to_owned(), stream_id, kind);
        match self.windows.get_mut(&key) {
            Some(w) if now_ms < w.opened_ms.saturating_add(self.window_ms) => {
                w.trailing = true;
                Offer::Coalesced
            }
            _ => Offer::Send,
        }
    }

    /// A push for `(device_id, stream_id, kind)` went out at `now_ms`: open
    /// its window.
    pub fn open(&mut self, device_id: &str, stream_id: [u8; 16], kind: PushKind, now_ms: u64) {
        self.windows.insert(
            (device_id.to_owned(), stream_id, kind),
            Window {
                opened_ms: now_ms,
                trailing: false,
            },
        );
    }

    /// Charge one push to `device_id`'s minute, or refuse it at the cap.
    pub fn admit(&mut self, device_id: &str, now_ms: u64) -> bool {
        let sent = self.sent.entry(device_id.to_owned()).or_default();
        while sent
            .front()
            .is_some_and(|t| now_ms.saturating_sub(*t) >= 60_000)
        {
            sent.pop_front();
        }
        if sent.len() >= self.cap {
            return false;
        }
        sent.push_back(now_ms);
        true
    }

    /// Close every window that has run its length, and return the keys owed
    /// a trailing push, in key order. None of them is reopened here: the
    /// caller opens a window for each trailing push it actually sends.
    pub fn due(&mut self, now_ms: u64) -> Vec<(String, [u8; 16], PushKind)> {
        let window_ms = self.window_ms;
        let mut owed = Vec::new();
        self.windows.retain(|key, w| {
            if now_ms < w.opened_ms.saturating_add(window_ms) {
                return true;
            }
            if w.trailing {
                owed.push(key.clone());
            }
            false
        });
        owed.sort();
        self.sent.retain(|_, sent| {
            sent.back()
                .is_some_and(|t| now_ms.saturating_sub(*t) < 60_000)
        });
        owed
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// One fresh batch, as the dispatcher sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wake {
    /// The account whose devices may need waking.
    pub account_id: String,
    /// The stream the batch went to.
    pub stream_id: [u8; 16],
    /// The device that published it, which already has it.
    pub origin: Option<String>,
    /// Why.
    pub kind: PushKind,
}

/// The push half of [`ServerState`]: presence, the queue, and the provider.
///
/// Cheap to clone. Without a configured provider it is disabled: presence is
/// still tracked, and [`Dispatcher::notify`] returns at once.
#[derive(Debug, Clone, Default)]
pub struct Dispatcher {
    presence: Presence,
    inner: Option<Arc<Inner>>,
}

#[derive(Debug)]
struct Inner {
    provider: Arc<dyn PushProvider>,
    tuning: Tuning,
    tx: mpsc::Sender<Wake>,
    /// Taken by the first [`Dispatcher::notify`], which starts the worker.
    /// Started lazily because a [`ServerState`] is built outside any runtime
    /// in places, and only a handler is certain to run inside one.
    rx: Mutex<Option<mpsc::Receiver<Wake>>>,
}

/// Build the dispatcher `[push]` describes. No provider configured is a
/// disabled dispatcher, not an error.
pub fn from_config(cfg: &PushConfig, clock: Arc<dyn Clock>) -> Result<Dispatcher, PushSetupError> {
    match &cfg.apns {
        Some(apns) => Ok(Dispatcher::new(Arc::new(ApnsProvider::from_config(
            apns, clock,
        )?))),
        None => Ok(Dispatcher::disabled()),
    }
}

impl Dispatcher {
    /// A dispatcher that sends nothing.
    #[must_use]
    pub fn disabled() -> Self {
        Self::default()
    }

    /// A dispatcher over `provider` with the documented bounds.
    #[must_use]
    pub fn new(provider: Arc<dyn PushProvider>) -> Self {
        Self::with_tuning(provider, Tuning::default())
    }

    /// A dispatcher over `provider` with `tuning`.
    #[must_use]
    pub fn with_tuning(provider: Arc<dyn PushProvider>, tuning: Tuning) -> Self {
        let (tx, rx) = mpsc::channel(tuning.queue.max(1));
        Self {
            presence: Presence::default(),
            inner: Some(Arc::new(Inner {
                provider,
                tuning,
                tx,
                rx: Mutex::new(Some(rx)),
            })),
        }
    }

    /// The `provider` label of the configured provider, if one is.
    #[must_use]
    pub fn provider(&self) -> Option<&'static str> {
        self.inner
            .as_ref()
            .map(|i| i.provider.platform().metric_label())
    }

    /// Mark `device_id` online for as long as the guard lives. `None` for a
    /// session with no device, which no push could reach anyway.
    #[must_use]
    pub fn hold(&self, device_id: Option<&str>) -> Option<Present> {
        device_id.map(|d| self.presence.hold(d))
    }

    /// Queue a wake for the worker. Never blocks and never fails: a full
    /// queue drops the wake and counts it as `result="dropped"`.
    pub fn notify(&self, state: &ServerState, wake: Wake) {
        let Some(inner) = &self.inner else {
            return;
        };
        self.start(state, inner);
        if inner.tx.try_send(wake).is_err() {
            state.metrics.incr_with(
                "sunrise_push_dispatch_total",
                &[
                    ("provider", inner.provider.platform().metric_label()),
                    ("result", "dropped"),
                ],
            );
        }
    }

    /// Spawn the worker, once, on the runtime the caller is running on.
    fn start(&self, state: &ServerState, inner: &Arc<Inner>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let Some(rx) = inner.rx.lock().take() else {
            return;
        };
        runtime.spawn(self.worker(state, inner).run(rx));
    }

    /// The worker that drains this dispatcher's queue, over `state`'s store,
    /// clock and registry.
    fn worker(&self, state: &ServerState, inner: &Arc<Inner>) -> Worker {
        Worker {
            inner: Arc::clone(inner),
            presence: self.presence.clone(),
            store: Arc::clone(&state.store),
            clock: Arc::clone(&state.clock),
            metrics: state.metrics.clone(),
            planner: Planner::new(&inner.tuning),
        }
    }

    /// A worker a test drives by hand, one wake at a time, against its own
    /// clock. `None` when disabled.
    #[cfg(test)]
    pub(super) fn test_worker(&self, state: &ServerState) -> Option<Worker> {
        self.inner.as_ref().map(|inner| self.worker(state, inner))
    }

    /// The presence this dispatcher reads.
    #[cfg(test)]
    pub(super) const fn presence(&self) -> &Presence {
        &self.presence
    }
}

/// The single consumer of the wake queue.
#[derive(Debug)]
pub(super) struct Worker {
    inner: Arc<Inner>,
    presence: Presence,
    store: Arc<Store>,
    clock: Arc<dyn Clock>,
    metrics: Metrics,
    planner: Planner,
}

impl Worker {
    async fn run(mut self, mut rx: mpsc::Receiver<Wake>) {
        let tuning = self.inner.tuning;
        // `interval` panics on a zero period, and `Tuning` is public.
        let mut tick = tokio::time::interval(tuning.tick.max(Duration::from_millis(1)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let slots = Arc::new(tokio::sync::Semaphore::new(tuning.in_flight.max(1)));
        loop {
            let intents = tokio::select! {
                wake = rx.recv() => match wake {
                    Some(wake) => self.plan_wake(&wake),
                    None => return,
                },
                _ = tick.tick() => self.plan_due(),
            };
            for intent in intents {
                let Ok(slot) = Arc::clone(&slots).acquire_owned().await else {
                    return;
                };
                let delivery = Delivery::new(
                    Arc::clone(&self.inner.provider),
                    Arc::clone(&self.store),
                    self.metrics.clone(),
                    tuning,
                );
                tokio::spawn(async move {
                    delivery.deliver(&intent).await;
                    drop(slot);
                });
            }
        }
    }

    /// The pushes one wake earns.
    pub(super) fn plan_wake(&mut self, wake: &Wake) -> Vec<PushIntent> {
        let now_ms = self.clock.now_ms();
        let platform = self.inner.provider.platform();
        let targets = match self
            .store
            .push_targets(&wake.account_id, platform.store_tag())
        {
            Ok(targets) => targets,
            Err(e) => {
                tracing::error!(
                    ev = "srv.push.lookup_failed",
                    provider = platform.metric_label(),
                    account_h = %crate::logging::account_h(&wake.account_id),
                    cause = %e,
                    "could not read the push targets; this wake is lost"
                );
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for (device_id, token) in targets {
            if wake.origin.as_deref() == Some(device_id.as_str())
                || self.presence.is_online(&device_id)
            {
                continue;
            }
            if self
                .planner
                .offer(&device_id, wake.stream_id, wake.kind, now_ms)
                == Offer::Coalesced
            {
                continue;
            }
            if let Some(intent) = self.admit(device_id, token, wake.stream_id, wake.kind, now_ms) {
                out.push(intent);
            }
        }
        out
    }

    /// The trailing pushes owed by windows that just closed.
    pub(super) fn plan_due(&mut self) -> Vec<PushIntent> {
        let now_ms = self.clock.now_ms();
        let platform = self.inner.provider.platform();
        let mut out = Vec::new();
        for (device_id, stream_id, kind) in self.planner.due(now_ms) {
            if self.presence.is_online(&device_id) {
                continue;
            }
            // Read again: the device may have been revoked, or re-registered,
            // since the window opened.
            let Ok(Some(token)) = self.store.push_target(&device_id, platform.store_tag()) else {
                continue;
            };
            if let Some(intent) = self.admit(device_id, token, stream_id, kind, now_ms) {
                out.push(intent);
            }
        }
        out
    }

    /// Charge a push to the device's cap and, if it is under it, open the
    /// push's window and return the intent. A push refused at the cap opens no
    /// window.
    fn admit(
        &mut self,
        device_id: String,
        token: String,
        stream_id: [u8; 16],
        kind: PushKind,
        now_ms: u64,
    ) -> Option<PushIntent> {
        let platform = self.inner.provider.platform();
        if !self.planner.admit(&device_id, now_ms) {
            self.metrics.incr_with(
                "sunrise_push_dispatch_total",
                &[
                    ("provider", platform.metric_label()),
                    ("result", "rate_limited"),
                ],
            );
            return None;
        }
        self.planner.open(&device_id, stream_id, kind, now_ms);
        Some(PushIntent {
            registration: PushTokenRegistration {
                device_id,
                platform,
                token,
            },
            kind,
        })
    }
}

/// One push's delivery, retries included.
#[derive(Debug)]
pub(super) struct Delivery {
    provider: Arc<dyn PushProvider>,
    store: Arc<Store>,
    metrics: Metrics,
    tuning: Tuning,
}

/// The correlation handle for a device id: [`crate::logging::id_h`] over its
/// 16 bytes.
fn device_h(device_id: &str) -> String {
    sunrise_id::crockford::decode_str(device_id).map_or_else(
        |_| crate::logging::account_h(device_id),
        |raw| crate::logging::id_h(&raw),
    )
}

impl Delivery {
    pub(super) const fn new(
        provider: Arc<dyn PushProvider>,
        store: Arc<Store>,
        metrics: Metrics,
        tuning: Tuning,
    ) -> Self {
        Self {
            provider,
            store,
            metrics,
            tuning,
        }
    }

    pub(super) async fn deliver(&self, intent: &PushIntent) {
        let provider = self.provider.platform().metric_label();
        let attempts = self.tuning.attempts.max(1);
        let mut backoff = self.tuning.backoff;
        for attempt in 1..=attempts {
            let started = tokio::time::Instant::now();
            let outcome =
                match tokio::time::timeout(self.tuning.send_timeout, self.provider.send(intent))
                    .await
                {
                    Ok(outcome) => outcome,
                    Err(_) => Err(PushError::Timeout),
                };
            self.metrics.observe(
                "sunrise_push_dispatch_duration_seconds",
                &[("provider", provider)],
                LATENCY_BUCKETS,
                started.elapsed().as_secs_f64(),
            );
            let error = match outcome {
                Ok(()) => {
                    self.count(provider, "ok");
                    return;
                }
                Err(e) if e.retryable() && attempt < attempts => {
                    tokio::time::sleep(backoff).await;
                    backoff = backoff.saturating_mul(2);
                    continue;
                }
                Err(e) => e,
            };
            self.count(provider, error.result());
            self.give_up(intent, &error, attempt);
            return;
        }
    }

    fn count(&self, provider: &'static str, result: &'static str) {
        self.metrics.incr_with(
            "sunrise_push_dispatch_total",
            &[("provider", provider), ("result", result)],
        );
    }

    fn give_up(&self, intent: &PushIntent, error: &PushError, attempt: u32) {
        let registration = &intent.registration;
        let provider = registration.platform.metric_label();
        if let PushError::Unregistered(reason) = error {
            let deleted = self.store.delete_push_token(
                &registration.device_id,
                registration.platform.store_tag(),
                &registration.token,
            );
            tracing::info!(
                ev = "srv.push.token_unregistered",
                provider,
                device_h = %device_h(&registration.device_id),
                reason = %reason,
                result = if matches!(deleted, Ok(true)) { "deleted" } else { "kept" },
                "the provider will not deliver to this token again"
            );
            return;
        }
        tracing::warn!(
            ev = "srv.push.delivery_failed",
            provider,
            device_h = %device_h(&registration.device_id),
            result = error.result(),
            retryable = error.retryable(),
            attempt = u64::from(attempt),
            cause = %error,
            "a wake-up push was not delivered"
        );
    }
}
