import PDFKit
import SwiftUI
import UniformTypeIdentifiers

/// One task's attachments: the list, the inline preview, and the two ways in.
struct AttachmentsView: View {
    @Bindable var model: AttachmentsModel

    @State private var picking = false
    @State private var selection: EntityRef?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            header
            if let error = model.errorMessage {
                NoteBanner(text: error) { model.dismissError() }
            }
            if model.rows.isEmpty {
                ContentUnavailableView(
                    "No attachments",
                    systemImage: "paperclip",
                    description: Text("Drop a file here, or use Attach.")
                )
                .frame(maxWidth: .infinity, minHeight: 120)
            } else {
                list
            }
            preview
        }
        .padding(12)
        .frame(minWidth: 420, minHeight: 320)
        .dropDestination(for: URL.self) { urls, _ in
            for url in urls {
                Task { await model.attach(contentsOf: url) }
            }
            return !urls.isEmpty
        }
        .fileImporter(isPresented: $picking, allowedContentTypes: [.item]) { result in
            guard case let .success(url) = result else { return }
            Task { await model.attach(contentsOf: url) }
        }
        .task { await model.refresh() }
        .task { await model.follow() }
    }

    private var header: some View {
        HStack {
            Text("Attachments").font(.headline)
            Spacer()
            if model.isBusy { ProgressView().controlSize(.small) }
            Button("Attach…", systemImage: "paperclip") { picking = true }
                .accessibilityIdentifier("attach-file")
        }
    }

    private var list: some View {
        List(model.rows, selection: $selection) { row in
            // Read once per row, so the icon, the subtitle and the buttons
            // cannot disagree about what this attachment is doing.
            let transfer = model.transfer(row)
            HStack(spacing: 8) {
                Image(systemName: symbol(for: row, transfer))
                    .foregroundStyle(transfer == .here ? .primary : .secondary)
                VStack(alignment: .leading, spacing: 1) {
                    Text(row.item.filename)
                    Text(subtitle(for: row, transfer))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                transferControls(for: row, transfer)
                Button("Remove", systemImage: "trash") {
                    Task { await model.detach(row) }
                }
                .labelStyle(.iconOnly)
                .buttonStyle(.plain)
            }
            .tag(row.id)
            .contentShape(.rect)
            .onTapGesture { Task { await model.preview(row) } }
        }
        .frame(minHeight: 120)
    }

    /// What the row offers to do about its bytes.
    ///
    /// `docs/02-domain/attachments.md` §Lazy fetch names three of these four
    /// and the fourth is the ordinary case. An attachment on this device opens;
    /// one over the 10 MiB auto-fetch threshold "shows an inline placeholder
    /// with file name, size, and a 'Download' button" — the name and size are
    /// the two lines to the left, which is what makes this the button rather
    /// than a whole placeholder view; and "the 'Cancel' button during transfer
    /// aborts".
    ///
    /// Under the threshold none of this is drawn for long: the sync driver
    /// fetches those unasked, so the row is `.absent` only until the next drain
    /// and then opens.
    @ViewBuilder
    private func transferControls(
        for row: AttachmentRow,
        _ transfer: AttachmentTransfer
    ) -> some View {
        switch transfer {
        case .here:
            Button("Open", systemImage: "arrow.up.forward.app") {
                Task {
                    if let url = await model.exportToTemporary(row) {
                        Platform.openExternal(url)
                    }
                }
            }
            .labelStyle(.iconOnly)
            .buttonStyle(.plain)
        case .running:
            ProgressView().controlSize(.small)
            Button("Cancel", systemImage: "xmark.circle") {
                model.cancelDownload(row)
            }
            .labelStyle(.iconOnly)
            .buttonStyle(.plain)
            .accessibilityIdentifier("cancel-attachment-download")
        case .interrupted, .absent:
            Button(
                transfer == .interrupted ? "Download again" : "Download",
                systemImage: "arrow.down.circle"
            ) {
                model.download(row)
            }
            .labelStyle(.iconOnly)
            .buttonStyle(.plain)
            .accessibilityIdentifier("download-attachment")
        }
    }

    /// The inline preview. Images and PDFs only — everything else opens in the
    /// app that owns it, because guessing at a renderer for an arbitrary type
    /// is how a text editor ends up showing a `.zip` as mojibake.
    @ViewBuilder
    private var preview: some View {
        if let previewing = model.previewing,
           let row = model.rows.first(where: { $0.id == previewing.id }) {
            switch row.previewKind {
            case .image:
                if let image = PlatformImage(data: previewing.data) {
                    Image(platformImage: image)
                        .resizable()
                        .scaledToFit()
                        .frame(maxHeight: 260)
                        .accessibilityLabel(row.item.filename)
                }
            case .pdf:
                PdfPreview(data: previewing.data)
                    .frame(minHeight: 260)
            case .none:
                Text("\(row.item.filename) opens in another app.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private func symbol(for row: AttachmentRow, _ transfer: AttachmentTransfer) -> String {
        switch transfer {
        case .running: return "arrow.down.circle"
        case .interrupted: return "exclamationmark.icloud"
        case .absent: return "icloud.and.arrow.down"
        case .here:
            switch row.previewKind {
            case .image: return "photo"
            case .pdf: return "doc.richtext"
            case .none: return "doc"
            }
        }
    }

    /// The second line of the placeholder: the size, always, and then what the
    /// row is waiting for.
    ///
    /// "Interrupted" is worth its own wording rather than folding into "not on
    /// this device". It is the one state where pressing the button again is
    /// the whole remedy, and a user who cannot tell it from an ordinary
    /// un-downloaded row has no reason to press anything.
    private func subtitle(for row: AttachmentRow, _ transfer: AttachmentTransfer) -> String {
        switch transfer {
        case .here: return "\(row.sizeText) · \(row.item.mimeType)"
        case .running: return "\(row.sizeText) · downloading…"
        case .interrupted: return "\(row.sizeText) · download interrupted"
        case .absent: return "\(row.sizeText) · not on this device"
        }
    }
}

/// A PDF, drawn by the system's own viewer.
///
/// `PDFView` itself is the same class on both platforms — PDFKit ships on iOS
/// too — so only the representable wrapper differs, and it differs in nothing
/// but the two method names. Configuring the view is therefore written once,
/// below, and each conformance forwards to it.
private struct PdfPreview {
    let data: Data

    // `@MainActor` because `PDFView` is, and because the representable methods
    // these stand in for carry that isolation themselves — factoring them out
    // is what dropped it.
    @MainActor
    fileprivate func makeView() -> PDFView {
        let view = PDFView()
        view.autoScales = true
        return view
    }

    @MainActor
    fileprivate func update(_ view: PDFView) {
        view.document = PDFDocument(data: data)
    }
}

#if os(macOS)
extension PdfPreview: NSViewRepresentable {
    func makeNSView(context: Context) -> PDFView { makeView() }
    func updateNSView(_ view: PDFView, context: Context) { update(view) }
}
#else
extension PdfPreview: UIViewRepresentable {
    func makeUIView(context: Context) -> PDFView { makeView() }
    func updateUIView(_ view: PDFView, context: Context) { update(view) }
}
#endif
