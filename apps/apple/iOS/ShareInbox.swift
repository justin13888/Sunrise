import Foundation

/// Files what the share extension left behind, whenever a vault is open and
/// the app comes forward.
///
/// The filing itself is ``SharedCapture``'s; this decides *when*, and makes
/// sure two triggers never file one capture twice. Both triggers are cheap
/// when nothing is waiting: one directory listing.
@MainActor
final class ShareInbox {
    static let shared = ShareInbox(store: PendingCaptureStore.appGroup())

    private let store: PendingCaptureStore?
    private var running: _Concurrency.Task<Void, Never>?

    init(store: PendingCaptureStore?) {
        self.store = store
    }

    /// File everything pending into `bridge`.
    ///
    /// Single-flight: a vault opening and the scene becoming active usually
    /// happen together, and a second pass started while the first is filing
    /// would read a capture the first has not yet removed. A call that arrives
    /// mid-pass waits for that pass instead.
    func file(into bridge: CoreBridge) async {
        guard let store else { return }
        if let running {
            await running.value
            return
        }
        // The report's failures are all ones a later pass can clear, so the
        // next trigger is their retry. What would fail forever is not in it:
        // `SharedCapture.file` drops it and says so in the task's note.
        let pass = _Concurrency.Task { _ = await SharedCapture.fileAll(from: store, into: bridge) }
        running = pass
        await pass.value
        running = nil
    }
}
