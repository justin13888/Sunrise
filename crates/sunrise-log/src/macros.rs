//! Public macros: `event!`, `error_event!`.
//!
//! Per `docs/10-cross-cutting/logging.md` §6, the type system enforces that
//! `Plain<T>` cannot be passed in any record field. The macros therefore
//! accept values exclusively through the [`crate::ctx::CtxValue`] enum,
//! which has no `Plain<T>` variant.

/// Emit one log record.
///
/// Usage:
///
/// ```ignore
/// use sunrise_log::{Level, Ctx, CtxKey, CtxValue};
/// sunrise_log::event!(
///     level = Level::Info,
///     ev = "sync.session.opened",
///     msg = "session opened",
///     ctx = Ctx::new()
///         .with(CtxKey::StreamH, CtxValue::Str("abc123"))
///         .with(CtxKey::Epoch, CtxValue::U64(4)),
/// );
/// ```
#[macro_export]
macro_rules! event {
    (level = $lv:expr, ev = $ev:expr, msg = $msg:expr $(, ctx = $ctx:expr)? $(,)?) => {{
        let ev: &'static str = $ev;
        // Compile-time validate the event name shape.
        const _: $crate::EventName = $crate::EventName::const_new($ev);
        #[allow(unused_mut, unused_assignments)]
        let mut ctx_owned = $crate::Ctx::new();
        $(ctx_owned = $ctx;)?
        $crate::init::emit_event(
            $lv,
            ev,
            None,
            ::core::module_path!(),
            $msg,
            &ctx_owned,
            None,
            false,
        );
    }};
}

/// Emit one log record at warn/error level with an attached `err` envelope.
#[macro_export]
macro_rules! error_event {
    (level = $lv:expr, ev = $ev:expr, msg = $msg:expr, err = $err:expr $(, ctx = $ctx:expr)? $(,)?) => {{
        let ev: &'static str = $ev;
        const _: $crate::EventName = $crate::EventName::const_new($ev);
        #[allow(unused_mut, unused_assignments)]
        let mut ctx_owned = $crate::Ctx::new();
        $(ctx_owned = $ctx;)?
        let err: &$crate::ErrField = &$err;
        $crate::init::emit_event(
            $lv,
            ev,
            None,
            ::core::module_path!(),
            $msg,
            &ctx_owned,
            Some(err),
            false,
        );
    }};
}
