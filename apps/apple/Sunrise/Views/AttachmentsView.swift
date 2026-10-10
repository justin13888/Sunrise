#if os(macOS)
import Quartz
#else
import QuickLook
#endif
import SwiftUI
import UniformTypeIdentifiers

/// One task's attachments: the list, the inline preview, and the two ways in.
struct AttachmentsView: View {
    @Bindable var model: AttachmentsModel

    @State private var picking = false
    @State private var selection: EntityRef?
    /// The row whose Download is waiting on the cellular size confirmation.
    @State private var confirming: AttachmentRow?

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
        .onDisappear { model.closePreview() }
        .confirmationDialog(
            confirmTitle,
            isPresented: Binding(
                get: { confirming != nil },
                set: { if !$0 { confirming = nil } }
            ),
            presenting: confirming
        ) { row in
            Button("Download \(row.sizeText)") { model.download(row) }
        } message: { _ in
            Text("You are on a cellular network.")
        }
    }

    private var confirmTitle: String {
        guard let row = confirming else { return "" }
        return "Download \(row.item.filename) over cellular?"
    }

    private var header: some View {
        HStack {
            Text("Attachments").font(.headline)
            Spacer()
            if model.isBusy { ProgressView().controlSize(.small) }
            Button("Attach…", systemImage: "paperclip") { picking = true }
                .disabledUnlessEditable(.attachment)
                .accessibilityIdentifier("attach-file")
        }
    }

    private var list: some View {
        List(model.rows, selection: $selection) { row in
            // Read once per row, so the icon, the subtitle and the buttons
            // cannot disagree about what this attachment is doing.
            let transfer = model.transfer(row)
            HStack(spacing: 8) {
                icon(for: row, transfer)
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
                .disabledUnlessEditable(.attachment)
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
            Button("Open in…", systemImage: "arrow.up.forward.app") {
                Task {
                    if let url = await model.exportToTemporary(row) {
                        Platform.openExternal(url)
                    }
                }
            }
            .labelStyle(.iconOnly)
            .buttonStyle(.plain)
            .accessibilityIdentifier("open-attachment-in")
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
                if model.downloadNeedsConfirmation(row) {
                    confirming = row
                } else {
                    model.download(row)
                }
            }
            .labelStyle(.iconOnly)
            .buttonStyle(.plain)
            .accessibilityIdentifier("download-attachment")
        }
    }

    /// The preview (ADR-0053 §4). Only what a system framework renders: an
    /// image through ImageIO, inline, and everything else through QuickLook,
    /// out of process. A type QuickLook cannot preview says so and offers
    /// **Open in…**; no decoder is bundled for it.
    @ViewBuilder
    private var preview: some View {
        if let previewing = model.previewing,
           let row = model.rows.first(where: { $0.id == previewing.id }) {
            VStack(alignment: .trailing, spacing: 4) {
                Button("Close preview", systemImage: "xmark") { model.closePreview() }
                    .labelStyle(.iconOnly)
                    .buttonStyle(.plain)
                switch previewing {
                case let .image(_, data):
                    if let image = PlatformImage(data: data) {
                        Image(platformImage: image)
                            .resizable()
                            .scaledToFit()
                            .frame(maxHeight: 260)
                            .accessibilityLabel(row.item.filename)
                    }
                case let .file(_, url):
                    if QuickLookPreview.canPreview(url) {
                        QuickLookPreview(url: url)
                            .frame(minHeight: 260)
                            .accessibilityLabel(row.item.filename)
                    } else {
                        Text("\(row.item.filename) cannot be previewed here. Use Open in….")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
            }
        }
    }

    /// The row's leading image: its thumbnail when one has arrived, its
    /// type's symbol otherwise, and the transfer's symbol while the bytes are
    /// not here and there is no thumbnail to show instead.
    @ViewBuilder
    private func icon(for row: AttachmentRow, _ transfer: AttachmentTransfer) -> some View {
        if let data = model.thumbnails[row.id], let image = PlatformImage(data: data) {
            Image(platformImage: image)
                .resizable()
                .scaledToFill()
                .frame(width: 32, height: 32)
                .clipShape(.rect(cornerRadius: 4))
                .opacity(transfer == .here ? 1 : 0.7)
                .accessibilityHidden(true)
        } else {
            Image(systemName: symbol(for: row, transfer))
                .foregroundStyle(transfer == .here ? .primary : .secondary)
                .frame(width: 32, height: 32)
        }
    }

    private func symbol(for row: AttachmentRow, _ transfer: AttachmentTransfer) -> String {
        switch transfer {
        case .running: return "arrow.down.circle"
        case .interrupted: return "exclamationmark.icloud"
        case .absent: return "icloud.and.arrow.down"
        case .here:
            let type = UTType(mimeType: row.item.mimeType)
            if type?.conforms(to: .image) == true { return "photo" }
            if type?.conforms(to: .pdf) == true { return "doc.richtext" }
            return "doc"
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

/// A file, previewed by QuickLook: `QLPreviewView` on macOS,
/// `QLPreviewController` on iOS. QuickLook renders PDFs and dozens of other
/// types out of process, and the OS patches it, which is why the pane bundles
/// no decoder of its own.
struct QuickLookPreview {
    let url: URL

    /// Whether QuickLook can preview `url`. macOS has no such question to
    /// ask: `QLPreviewView` draws a generic icon for a type it cannot render,
    /// which reads as a preview of nothing, so there it answers for the types
    /// the system declares previewable content for.
    @MainActor
    static func canPreview(_ url: URL) -> Bool {
        #if os(macOS)
        guard let type = UTType(filenameExtension: url.pathExtension) else { return false }
        return [UTType.pdf, .text, .image, .movie, .audio, .presentation, .spreadsheet,
                .rtf, .html, .compositeContent, .threeDContent]
            .contains { type.conforms(to: $0) }
        #else
        return QLPreviewController.canPreview(url as NSURL)
        #endif
    }
}

#if os(macOS)
extension QuickLookPreview: NSViewRepresentable {
    func makeNSView(context: Context) -> QLPreviewView {
        let view = QLPreviewView(frame: .zero, style: .normal) ?? QLPreviewView()
        view.autostarts = true
        return view
    }

    func updateNSView(_ view: QLPreviewView, context: Context) {
        if (view.previewItem as? URL) != url {
            view.previewItem = url as NSURL
        }
    }

    static func dismantleNSView(_ view: QLPreviewView, coordinator: ()) {
        view.close()
    }
}
#else
extension QuickLookPreview: UIViewControllerRepresentable {
    func makeCoordinator() -> Source { Source(url: url) }

    func makeUIViewController(context: Context) -> QLPreviewController {
        let controller = QLPreviewController()
        controller.dataSource = context.coordinator
        return controller
    }

    func updateUIViewController(_ controller: QLPreviewController, context: Context) {
        if context.coordinator.url != url {
            context.coordinator.url = url
            controller.reloadData()
        }
    }

    /// The one item the controller shows.
    @MainActor
    final class Source: NSObject, QLPreviewControllerDataSource {
        var url: URL

        init(url: URL) { self.url = url }

        func numberOfPreviewItems(in controller: QLPreviewController) -> Int { 1 }

        func previewController(
            _ controller: QLPreviewController,
            previewItemAt index: Int
        ) -> QLPreviewItem {
            url as NSURL
        }
    }
}
#endif
