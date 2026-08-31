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
            HStack(spacing: 8) {
                Image(systemName: symbol(for: row))
                    .foregroundStyle(row.isLocal ? .primary : .secondary)
                VStack(alignment: .leading, spacing: 1) {
                    Text(row.item.filename)
                    Text(subtitle(for: row))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                if row.isLocal {
                    Button("Open", systemImage: "arrow.up.forward.app") {
                        Task {
                            if let url = await model.exportToTemporary(row) {
                                Platform.openExternal(url)
                            }
                        }
                    }
                    .labelStyle(.iconOnly)
                    .buttonStyle(.plain)
                }
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

    private func symbol(for row: AttachmentRow) -> String {
        guard row.isLocal else { return "icloud.and.arrow.down" }
        switch row.previewKind {
        case .image: return "photo"
        case .pdf: return "doc.richtext"
        case .none: return "doc"
        }
    }

    private func subtitle(for row: AttachmentRow) -> String {
        row.isLocal
            ? "\(row.sizeText) · \(row.item.mimeType)"
            : "\(row.sizeText) · not on this device"
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
