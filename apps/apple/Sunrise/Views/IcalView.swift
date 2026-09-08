import SwiftUI

/// What an import did, and — the reason this sheet exists — everything it
/// could not carry.
///
/// `docs/09-integrations/icalendar.md` §Edge cases requires the notices shown.
/// The CLI writes them to stderr; a GUI that swallowed them would be losing
/// the user's data more quietly than the CLI does, which is the wrong
/// direction for a client to differ in.
struct IcalReportView: View {
    let summary: IcalImportSummary
    let dismiss: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    ForEach(summary.notices) { group in
                        noticeGroup(group)
                    }
                    if !summary.blocks.isEmpty { blockList }
                }
                .padding(16)
            }
            Divider()
            HStack {
                Spacer()
                Button("Done", action: dismiss)
                    .keyboardShortcut(.defaultAction)
            }
            .padding(12)
        }
        .macSheetFrame(width: 520, height: 460)
        .accessibilityIdentifier("ical-report")
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(summary.headline)
                .font(.headline)
            Label(
                summary.noticeSummary,
                systemImage: summary.hasNotices ? "exclamationmark.triangle" : "checkmark.circle"
            )
            .font(.callout)
            .foregroundStyle(summary.hasNotices ? .orange : .secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(16)
    }

    private func noticeGroup(_ group: IcalNoticeGroup) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(group.title)
                .font(.subheadline.weight(.semibold))
                .foregroundStyle(group.isLoss ? .orange : .primary)
            ForEach(Array(group.lines.enumerated()), id: \.offset) { _, line in
                Text(line)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }

    /// What did land, so the sheet answers "and what came across?" too. The
    /// new/updated split is idempotence made visible: importing the same file
    /// twice shows the second run as all-updated and no duplicates.
    private var blockList: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Time blocks written")
                .font(.subheadline.weight(.semibold))
            ForEach(summary.blocks) { block in
                HStack(spacing: 8) {
                    Text(block.isNew ? "new" : "updated")
                        .font(.caption.monospaced())
                        .foregroundStyle(.secondary)
                        .frame(width: 56, alignment: .leading)
                    Text(block.title)
                        .font(.callout)
                    Spacer(minLength: 0)
                }
            }
        }
    }
}

/// The import report and the failure alert, hung on the window.
///
/// A modifier rather than two blocks written inline, because the same pair
/// belongs to the File menu's two items and the menu lives in a different
/// scene from the window that has to show their result.
struct IcalSurfaces: ViewModifier {
    let model: IcalModel?

    func body(content: Content) -> some View {
        content
            .sheet(isPresented: showingSummary) {
                if let summary = model?.summary {
                    IcalReportView(summary: summary) { model?.dismissSummary() }
                }
            }
            .alert(
                "The calendar could not be read",
                isPresented: showingError,
                presenting: model?.errorMessage
            ) { _ in
                Button("OK") { model?.dismissError() }
            } message: { message in
                Text(message)
            }
    }

    private var showingSummary: Binding<Bool> {
        Binding(
            get: { model?.summary != nil },
            set: { if !$0 { model?.dismissSummary() } }
        )
    }

    private var showingError: Binding<Bool> {
        Binding(
            get: { model?.errorMessage != nil },
            set: { if !$0 { model?.dismissError() } }
        )
    }
}
