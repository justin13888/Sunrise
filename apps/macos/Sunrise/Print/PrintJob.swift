import AppKit
import PDFKit
import SwiftUI

/// One sheet of paper, drawn.
///
/// Deliberately plain: a printed list is read on paper with no accent colour,
/// no hover state and no dark mode, and everything this draws has to survive
/// being a grey rectangle. The document it draws has already decided *what* is
/// on the page — see `PrintDocument` — so this view makes no decisions at all
/// and nothing here needs a test.
struct PrintPageView: View {
    let document: PrintDocument

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 2) {
                Text(document.title)
                    .font(.system(size: 20, weight: .semibold))
                Text(document.subtitle)
                    .font(.system(size: 10))
                    .foregroundStyle(.secondary)
            }
            Divider()
            ForEach(document.sections) { section in
                VStack(alignment: .leading, spacing: 4) {
                    Text(section.heading)
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(.secondary)
                    ForEach(section.rows) { row in
                        HStack(alignment: .firstTextBaseline, spacing: 8) {
                            if !row.leading.isEmpty {
                                Text(row.leading)
                                    .font(.system(size: 11).monospacedDigit())
                                    .frame(width: 34, alignment: .leading)
                            }
                            Text(row.title).font(.system(size: 12))
                            Spacer(minLength: 8)
                            if !row.detail.isEmpty {
                                Text(row.detail)
                                    .font(.system(size: 10))
                                    .foregroundStyle(.secondary)
                            }
                        }
                    }
                }
            }
            if document.sections.isEmpty {
                Text("Nothing to print.")
                    .font(.system(size: 12))
                    .foregroundStyle(.secondary)
            }
            Spacer(minLength: 0)
        }
        .padding(PrintJob.margin)
        .frame(width: PrintJob.pageSize.width, height: PrintJob.pageSize.height, alignment: .topLeading)
        .background(.white)
        .environment(\.colorScheme, .light)
    }
}

/// Rendering a `PrintDocument` to PDF, and handing it to the print panel.
///
/// `ImageRenderer` plus a `CGContext` PDF consumer, one render per page. The
/// pages come from ``PrintDocument/paginated(rowsPerPage:)`` already split, so
/// each render is a view constrained to exactly one sheet and there is no
/// context arithmetic here to get wrong.
@MainActor
enum PrintJob {
    /// US Letter at 72 dpi, which is what `NSPrintInfo` defaults to and what a
    /// PDF point is.
    static let pageSize = CGSize(width: 612, height: 792)
    static let margin: CGFloat = 44

    /// Render every page into one PDF document.
    ///
    /// `nil` when Core Graphics refuses the consumer, which in practice means
    /// out of memory — reported rather than crashed, because a print is not
    /// worth taking the app down for.
    static func pdfData(for document: PrintDocument) -> Data? {
        let pages = document.paginated()
        let data = NSMutableData()
        var box = CGRect(origin: .zero, size: pageSize)
        guard let consumer = CGDataConsumer(data: data),
              let context = CGContext(consumer: consumer, mediaBox: &box, nil) else { return nil }

        for page in pages {
            let renderer = ImageRenderer(content: PrintPageView(document: page))
            renderer.proposedSize = ProposedViewSize(pageSize)
            renderer.render { _, draw in
                context.beginPDFPage(nil)
                draw(context)
                context.endPDFPage()
            }
        }
        context.closePDF()
        return data as Data
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
