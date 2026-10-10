import SwiftUI
import UIKit

/// iOS's print path: the shared `PrintDocument`, rendered to the shared pages,
/// handed to `UIPrintInteractionController`.
///
/// The Mac's twin is `macOS/PrintJob.swift`. What is on the paper — which
/// screens print, how a list is sectioned, where a page breaks — is decided in
/// `Sunrise/Print/` for both, so the two cannot print different documents for
/// the same list. The system print sheet also offers saving the PDF to Files,
/// which is why iOS has no separate Export as PDF.
@MainActor
enum PrintController {
    /// Show the system print sheet for `document`. `false` means nothing
    /// could be rendered, or this device cannot print at all.
    @discardableResult
    static func present(_ document: PrintDocument) -> Bool {
        guard UIPrintInteractionController.isPrintingAvailable,
              let data = PrintPage.pdfData(for: document) else { return false }
        let info = UIPrintInfo(dictionary: nil)
        info.jobName = document.title
        info.outputType = .general
        let controller = UIPrintInteractionController.shared
        controller.printInfo = info
        controller.printingItem = data
        controller.present(animated: true)
        return true
    }
}

/// What ⌘P and the toolbar's Print button do with a document.
///
/// The same rule the Mac's `PrintCommand` holds, under the same name so the
/// shared tests ask both platforms the same question: nothing to print — no
/// paper shape for this screen, or an empty list — is refused with the
/// platform's refusal feedback, never sent to the printer as a blank sheet,
/// which costs paper and is only discovered at the printer.
@MainActor
enum PrintCommand {
    @discardableResult
    static func run(_ action: AppAction, document: PrintDocument?) -> Bool {
        guard action == .printView, let document, !document.isEmpty,
              PrintController.present(document) else {
            Platform.refusalFeedback()
            return false
        }
        return true
    }
}

/// The Print button on a task list's toolbar.
///
/// In the secondary placement — the overflow on a phone, the bar on an iPad —
/// because printing is occasional and Capture is not. Enabled whatever the
/// list holds, for the reason ⌘P is on the Mac: a button that greyed out as
/// tasks came and went would be worse than one that refuses an empty list.
struct PrintToolbarItem: ToolbarContent {
    let document: () -> PrintDocument?

    var body: some ToolbarContent {
        ToolbarItem(placement: .secondaryAction) {
            Button(AppAction.printView.title, systemImage: "printer") {
                PrintCommand.run(.printView, document: document())
            }
            .accessibilityIdentifier("print")
        }
    }
}
