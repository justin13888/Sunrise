import AppKit
import PDFKit
import SwiftUI

/// Keeps the File menu's two print items in step with the screen on show.
///
/// A modifier rather than an `onChange` written into the window, for the same
/// reason `IcalSurfaces` is one: the File menu is a *scene* command that
/// outlives every window, so it cannot read the window's `@State` selection —
/// the window has to hand the answer over, and that hand-off is a thing worth
/// naming once rather than four lines inside a two-hundred-line `body`.
///
/// `initial: true` because the first screen is shown without the selection
/// ever changing. Without it the menu would be correct only after the first
/// navigation, which is to say wrong exactly at launch.
struct PrintMenuSync: ViewModifier {
    let surfaces: AppSurfaces
    let destination: Destination?
    let reviewTab: ReviewTab

    func body(content: Content) -> some View {
        content.onChange(of: refusal, initial: true) { _, reason in
            surfaces.printRefusalChanged(to: reason)
        }
    }

    private var refusal: String? {
        PrintDocument.refusal(for: destination, reviewTab: reviewTab)
    }
}

/// Handing a `PrintDocument` to the Mac's print panel and save panel.
///
/// The pages themselves — `PrintPageView` and the PDF they render to — are
/// `Sunrise/Print/PrintPage.swift`, shared with iOS so that both platforms
/// print the same sheets.
@MainActor
enum PrintJob {
    static func pdfData(for document: PrintDocument) -> Data? {
        PrintPage.pdfData(for: document)
    }

    /// Show the system print panel for `document`.
    ///
    /// Routed through `PDFKit` rather than an `NSView` of our own: the document
    /// is already paginated into PDF pages, and `PDFDocument.printOperation`
    /// is what knows how to hand a fixed set of pages to a printer without
    /// re-laying them out. `false` means nothing could be rendered.
    @discardableResult
    static func present(_ document: PrintDocument, info: NSPrintInfo = .shared) -> Bool {
        guard let data = pdfData(for: document),
              let pdf = PDFDocument(data: data),
              let operation = pdf.printOperation(
                  for: info,
                  scalingMode: .pageScaleNone,
                  autoRotate: false
              ) else { return false }
        operation.jobTitle = document.title
        operation.showsPrintPanel = true
        operation.showsProgressPanel = true
        operation.run()
        return true
    }

    /// Ask where to write a PDF, and write it. `false` when the user cancelled
    /// or nothing could be rendered.
    @discardableResult
    static func exportPDF(_ document: PrintDocument) -> Bool {
        guard let data = pdfData(for: document) else { return false }
        let panel = NSSavePanel()
        panel.allowedContentTypes = [.pdf]
        panel.nameFieldStringValue = document.suggestedFilename
        panel.message = "Choose where to write the PDF."
        panel.prompt = "Export"
        guard panel.runModal() == .OK, let url = panel.url else { return false }
        do {
            try data.write(to: url, options: .atomic)
            return true
        } catch {
            NSSound.beep()
            return false
        }
    }
}

/// What ⌘P and "Export as PDF…" do with a document.
///
/// A type of its own rather than two methods on the window, so that "an empty
/// list beeps instead of printing a blank sheet" is a rule with one home and
/// one test, and the window is left holding only the question of *which*
/// document is on screen.
@MainActor
enum PrintCommand {
    /// Run `action` against `document`. `false` when there was nothing to
    /// print — no paper shape for this screen, or an empty list — in which
    /// case the caller has already been told by the beep.
    ///
    /// A blank sheet is the failure worth avoiding here: it costs paper and
    /// the user only discovers it at the printer.
    @discardableResult
    static func run(_ action: AppAction, document: PrintDocument?) -> Bool {
        guard let document, !document.isEmpty else {
            NSSound.beep()
            return false
        }
        switch action {
        case .printView: return PrintJob.present(document)
        case .exportPDF: return PrintJob.exportPDF(document)
        default: return false
        }
    }
}
