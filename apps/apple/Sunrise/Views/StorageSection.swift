import SwiftUI

/// Settings → Storage (ADR-0053 §5–§6): what the attachment cache holds, its
/// limit, **Clear cache**, and the cellular toggle.
struct StorageSection: View {
    let model: AttachmentCacheModel

    @State private var confirmingClear = false

    var body: some View {
        Section("Storage") {
            LabeledContent("Attachment cache") {
                Text(
                    "\(AttachmentCacheModel.format(model.usedBytes)) of "
                        + AttachmentCacheModel.format(model.limitBytes)
                )
                .monospacedDigit()
            }
            .accessibilityIdentifier("attachment-cache-usage")
            Picker("Limit", selection: limit) {
                ForEach(model.choices, id: \.self) { bytes in
                    Text(AttachmentCacheModel.format(bytes)).tag(bytes)
                }
            }
            Toggle("Download attachments on cellular", isOn: cellular)
                .accessibilityIdentifier("auto-fetch-on-cellular")
            Text(
                "Thumbnails always download. Files you open always download. "
                    + "This decides whether smaller files download on their own over cellular."
            )
            .font(.caption)
            .foregroundStyle(.secondary)
            Button("Clear cache", role: .destructive) { confirmingClear = true }
                .disabled(model.evictableBytes == 0)
                .accessibilityIdentifier("clear-attachment-cache")
            if let error = model.errorMessage {
                Text(error).font(.caption).foregroundStyle(.red)
            }
        }
        .task { await model.refresh() }
        .confirmationDialog(
            "Clear \(AttachmentCacheModel.format(model.evictableBytes)) of attachments?",
            isPresented: $confirmingClear
        ) {
            Button("Clear cache", role: .destructive) {
                Task { await model.clear() }
            }
        } message: {
            Text(
                "They download again when you open them. Thumbnails and files "
                    + "not yet uploaded from this device stay."
            )
        }
    }

    private var limit: Binding<UInt64> {
        Binding(
            get: { model.limitBytes },
            set: { bytes in Task { await model.setLimit(bytes) } }
        )
    }

    private var cellular: Binding<Bool> {
        Binding(
            get: { model.autoFetchOnCellular },
            set: { on in Task { await model.setAutoFetchOnCellular(on) } }
        )
    }
}
