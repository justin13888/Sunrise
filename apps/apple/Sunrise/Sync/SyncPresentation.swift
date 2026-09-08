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
    static let unknown = SyncPresentation(
        label: "Checking…",
        detail: nil,
        symbol: "icloud",
        tone: .idle,
        isKnownIncomplete: false
    )

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
                label: pending == 0 ? "Synced" : "Sending",
                detail: pending == 0 ? nil : Self.pendingPhrase(pending),
                symbol: pending == 0 ? "checkmark.icloud" : "arrow.up.circle",
                tone: pending == 0 ? .ok : .working,
                isKnownIncomplete: false
            )
        case .catchingUp:
            self.init(
                label: "Catching up",
                detail: "Replaying changes from your other devices.",
                symbol: "arrow.trianglehead.2.clockwise.rotate.90.icloud",
                tone: .working,
                isKnownIncomplete: false
            )
        case .disconnected:
            self.init(
                label: "Offline",
                detail: pending == 0
                    ? "Your tasks are on this device and up to date here."
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
                label: "Changes missing",
                detail: """
                    The relay can no longer supply some changes this device never \
                    received. Another device may hold edits that will not \
                    arrive over this connection.
                    """,
                symbol: "exclamationmark.icloud",
                tone: .alert,
                isKnownIncomplete: true
            )
        }
    }

    private static func pendingPhrase(_ count: UInt32) -> String {
        count == 1 ? "1 change waiting to send" : "\(count) changes waiting to send"
    }
}
