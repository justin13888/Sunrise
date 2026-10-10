import Foundation

/// How sync health reads on screen.
///
/// The whole reason this is a type with tests rather than a `switch` inside a
/// view: `SyncState.Degraded` means the relay has reported a range of ops it
/// can no longer produce, so this device is **known** to be missing data that
/// re-subscribing cannot recover. It is the only state that is not about the
/// connection — the socket is healthy and ops are flowing — and a UI that
/// renders it as "Synced" re-creates the silent data loss issue #19 was filed
/// for. Everything here exists to make that mapping impossible to write by
/// accident.
struct SyncPresentation: Equatable {
    /// The badge's word.
    let label: String
    /// The sentence under it, when there is one worth saying.
    let detail: String?
    /// SF Symbol.
    let symbol: String
    let tone: Tone
    /// Whether this device is known to be missing data it cannot recover.
    ///
    /// True for `Degraded` and nothing else. Being offline is not this: an
    /// offline vault is complete as far as it knows and catches up on
    /// reconnect. Degraded has already been told it will not.
    let isKnownIncomplete: Bool

    enum Tone: Equatable {
        /// Everything through.
        case ok
        /// Work in progress; nothing wrong.
        case working
        /// Nothing wrong, nothing happening.
        case idle
        /// Needs attention.
        case alert
    }

    /// No snapshot yet — the first query has not answered.
    static var unknown: SyncPresentation {
        SyncPresentation(
            label: L10n.Sync.checking,
            detail: nil,
            symbol: "icloud",
            tone: .idle,
            isKnownIncomplete: false
        )
    }

    init(label: String, detail: String?, symbol: String, tone: Tone, isKnownIncomplete: Bool) {
        self.label = label
        self.detail = detail
        self.symbol = symbol
        self.tone = tone
        self.isKnownIncomplete = isKnownIncomplete
    }

    init(_ snapshot: SyncSnapshot) {
        let pending = snapshot.outboxPending
        switch snapshot.state {
        case .live:
            self.init(
                label: pending == 0 ? L10n.Sync.synced : L10n.Sync.sending,
                detail: pending == 0 ? nil : Self.pendingPhrase(pending),
                symbol: pending == 0 ? "checkmark.icloud" : "arrow.up.circle",
                tone: pending == 0 ? .ok : .working,
                isKnownIncomplete: false
            )
        case .catchingUp:
            self.init(
                label: L10n.Sync.catchingUp,
                detail: L10n.Sync.catchingUpDetail,
                symbol: "arrow.trianglehead.2.clockwise.rotate.90.icloud",
                tone: .working,
                isKnownIncomplete: false
            )
        case .disconnected:
            self.init(
                label: L10n.Sync.offline,
                detail: pending == 0
                    ? L10n.Sync.offlineDetail
                    : Self.pendingPhrase(pending),
                symbol: "icloud.slash",
                tone: .idle,
                isKnownIncomplete: false
            )
        case .degraded:
            // Deliberately not a variant of "Synced", and deliberately an
            // alert while connected. The relay dropped ops this device never
            // received and cannot serve them again; only another device that
            // still holds them can close the gap.
            self.init(
                label: L10n.Sync.changesMissing,
                detail: L10n.Sync.changesMissingDetail,
                symbol: "exclamationmark.icloud",
                tone: .alert,
                isKnownIncomplete: true
            )
        case .stopped:
            // Not "Offline": offline retries on its own and this does not.
            // The relay closed the session for a reason reconnecting cannot
            // fix — this device was revoked, or the relay could not read its
            // storage — so the driver waits for the user. Not known
            // incomplete either: nothing was lost, it just is not moving.
            self.init(
                label: L10n.Sync.stopped,
                detail: L10n.Sync.stoppedDetail,
                symbol: "xmark.icloud",
                tone: .alert,
                isKnownIncomplete: false
            )
        }
    }

    private static func pendingPhrase(_ count: UInt32) -> String {
        L10n.Sync.pending(count: Int(count))
    }
}
