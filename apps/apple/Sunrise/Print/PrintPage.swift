import CoreGraphics
import Foundation
import SwiftUI

/// One sheet of paper, drawn.
///
/// Deliberately plain: a printed list is read on paper with no accent colour,
/// no hover state and no dark mode, and everything this draws has to survive
/// being a grey rectangle. The document it draws has already decided *what* is
/// on the page — see `PrintDocument` — so this view makes no decisions at all
/// and nothing here needs a test.
///
/// Shared by both apps: the Mac hands the PDF to `NSPrintOperation`
/// (`macOS/PrintJob.swift`) and iOS to `UIPrintInteractionController`
/// (`iOS/PrintController.swift`), and the pages are the same pages.
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
        .padding(PrintPage.margin)
        .frame(width: PrintPage.size.width, height: PrintPage.size.height, alignment: .topLeading)
        .background(.white)
        .environment(\.colorScheme, .light)
    }
}

/// Rendering a `PrintDocument` to PDF.
///
/// `ImageRenderer` plus a `CGContext` PDF consumer, one render per page. The
/// pages come from ``PrintDocument/paginated(rowsPerPage:)`` already split, so
/// each render is a view constrained to exactly one sheet and there is no
/// context arithmetic here to get wrong.
@MainActor
enum PrintPage {
    /// US Letter at 72 dpi, which is what `NSPrintInfo` defaults to and what a
    /// PDF point is.
    static let size = CGSize(width: 612, height: 792)
    static let margin: CGFloat = 44

    /// Render every page into one PDF document.
    ///
    /// `nil` when Core Graphics refuses the consumer, which in practice means
    /// out of memory — reported rather than crashed, because a print is not
    /// worth taking the app down for.
    static func pdfData(for document: PrintDocument) -> Data? {
        let pages = document.paginated()
        let data = NSMutableData()
        var box = CGRect(origin: .zero, size: size)
        guard let consumer = CGDataConsumer(data: data),
              let context = CGContext(consumer: consumer, mediaBox: &box, nil) else { return nil }

        for page in pages {
            let renderer = ImageRenderer(content: PrintPageView(document: page))
            renderer.proposedSize = ProposedViewSize(size)
            renderer.render { _, draw in
                context.beginPDFPage(nil)
                draw(context)
                context.endPDFPage()
            }
        }
        context.closePDF()
        return data as Data
    }
}
