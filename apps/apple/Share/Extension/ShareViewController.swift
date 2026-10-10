import SwiftUI
import UIKit

/// The share sheet's Sunrise entry.
///
/// It saves what it was handed for the app to file (``ShareReceiver``), says
/// so, and gets out of the way. There is no editor: what is shared lands in the
/// Inbox as written, and the Inbox is where it gets triaged — the same place
/// every other capture from outside the app goes.
///
/// Named in `project.yml` as the extension's principal class.
final class ShareViewController: UIViewController {
    private let status = ShareStatus()

    override func viewDidLoad() {
        super.viewDidLoad()
        let host = UIHostingController(rootView: ShareStatusView(status: status) { [weak self] in
            self?.finish()
        })
        addChild(host)
        host.view.frame = view.bounds
        host.view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        view.addSubview(host.view)
        host.didMove(toParent: self)

        let items = extensionContext?.inputItems.compactMap { $0 as? NSExtensionItem } ?? []
        Task { await save(items) }
    }

    private func save(_ items: [NSExtensionItem]) async {
        guard let store = PendingCaptureStore.appGroup() else {
            status.phase = .failed(ShareReceiver.Failure.noAppGroup.localizedDescription)
            return
        }
        do {
            try await ShareReceiver.save(items, to: store)
            status.phase = .saved
        } catch {
            status.phase = .failed(error.localizedDescription)
        }
    }

    private func finish() {
        if case .failed = status.phase {
            extensionContext?.cancelRequest(withError: CocoaError(.userCancelled))
        } else {
            extensionContext?.completeRequest(returningItems: nil)
        }
    }
}

/// Where the save stands, for the sheet to draw.
@MainActor
@Observable
final class ShareStatus {
    enum Phase: Equatable {
        case saving
        case saved
        case failed(String)
    }

    var phase: Phase = .saving
}

struct ShareStatusView: View {
    let status: ShareStatus
    let done: () -> Void

    var body: some View {
        VStack(spacing: 16) {
            switch status.phase {
            case .saving:
                ProgressView("Adding to Sunrise…")
            case .saved:
                Label("Added to your Inbox", systemImage: "tray.and.arrow.down")
                    .font(.headline)
                Text("Sunrise files it the next time you open the app.")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            case let .failed(message):
                Label("Not added", systemImage: "exclamationmark.triangle")
                    .font(.headline)
                Text(message)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            }
            Button("Done", action: done)
                .buttonStyle(.borderedProminent)
                .disabled(status.phase == .saving)
                .accessibilityIdentifier("share.done")
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(.regularMaterial)
    }
}
