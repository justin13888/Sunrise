import AppKit
import SwiftUI

// File → Export Everything…: the macOS front of the full decrypted export
// (`docs/04-storage/export-format.md`). The writer is the core's —
// `SunriseCore.export_all`, the same one `sunrise export all` runs — so the
// app and the CLI write the same bytes, and everything here is choosing where
// and saying how it went.

extension CoreBridge {
    /// Write the whole vault, decrypted, into `directory`, fetching any
    /// attachment whose bytes are not on this device first.
    ///
    /// A path rather than bytes, unlike the iCal export: an export is a
    /// directory of many files, attachments included, and cannot cross the
    /// seam as one buffer. The app is not sandboxed (ADR-0031), so the folder
    /// the save panel returned needs no security scope held around the call.
    func exportAll(to directory: URL) async throws -> FullExportSummary {
        try await core.exportAll(dir: directory.path(percentEncoded: false), fetch: true)
    }
}

/// Where an Export Everything stands, for the sheet that shows it.
///
/// Owned by the app rather than by a window, for the same reason
/// ``AppSurfaces/ical`` is: the menu item is a scene command that can fire
/// with every window closed, and the export it starts has to outlive whichever
/// window happened to be in front.
@MainActor
@Observable
final class ExportEverythingModel {
    enum Phase {
        /// The core is writing. No progress figure: the seam returns once,
        /// at the end (`docs/04-storage/export-format.md` §Not yet).
        case running(folder: URL)
        case finished(folder: URL, summary: FullExportSummary)
        case failed(message: String)
    }

    /// `nil` when there is nothing to show, which is also when the sheet is
    /// down.
    private(set) var phase: Phase?

    var isRunning: Bool {
        if case .running = phase { return true }
        return false
    }

    /// **File → Export Everything…**: pick a new folder, then write into it.
    ///
    /// Destination first, as every macOS save does, so a cancelled panel
    /// costs nothing. A second request while one is running is refused rather
    /// than queued: the menu item is disabled then, and two exports racing
    /// for the same vault read would only be slower.
    func start(from bridge: CoreBridge?) async {
        guard let bridge, !isRunning else {
            Platform.refusalFeedback()
            return
        }
        guard let folder = ExportEverythingPanel.pickDestination() else { return }
        phase = .running(folder: folder)
        do {
            let summary = try await bridge.exportAll(to: folder)
            phase = .finished(folder: folder, summary: summary)
        } catch {
            phase = .failed(message: error.localizedDescription)
        }
    }

    /// Put the sheet away. Refused while the export runs: the call cannot be
    /// cancelled across the seam, and a sheet that went away would leave a
    /// write going on with nothing on screen to say so.
    func dismiss() {
        guard !isRunning else { return }
        phase = nil
    }
}

/// The save panel. Not an open panel on an existing folder: the export
/// refuses a directory that already holds anything, so asking for a *new*
/// folder's name is asking for the one thing that is sure to work.
enum ExportEverythingPanel {
    /// `nil` when the user cancelled.
    @MainActor
    static func pickDestination(now: Date = .now) -> URL? {
        let panel = NSSavePanel()
        panel.canCreateDirectories = true
        panel.nameFieldStringValue = suggestedName(now: now)
        panel.message = L10n.Mac.exportEverythingMessage
        panel.prompt = L10n.Mac.exportEverythingPrompt
        return panel.runModal() == .OK ? panel.url : nil
    }

    /// "Sunrise Export 2026-11-07", dated in the zone the user is in.
    static func suggestedName(now: Date, zone: TimeZone = .current) -> String {
        let date = now.formatted(Date.ISO8601FormatStyle(timeZone: zone).year().month().day())
        return L10n.Mac.exportEverythingFolder(date: date)
    }
}

/// The sheet over the main window while an export runs, and its outcome after.
struct ExportEverythingSheet: View {
    let model: ExportEverythingModel

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            switch model.phase {
            case .running:
                HStack(spacing: 12) {
                    ProgressView()
                        .controlSize(.small)
                        .accessibilityIdentifier("export-everything.progress")
                    Text(L10n.Mac.exportEverythingRunning).font(.headline)
                }
                Text(L10n.Mac.exportEverythingRunningDetail)
                    .foregroundStyle(.secondary)
            case let .finished(folder, summary):
                Text(L10n.Mac.exportEverythingFinished).font(.headline)
                ForEach(Self.lines(for: summary, folder: folder), id: \.self) { line in
                    Text(line)
                }
                .accessibilityIdentifier("export-everything.summary")
            case let .failed(message):
                Text(L10n.Mac.exportEverythingFailed).font(.headline)
                Text(message)
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .accessibilityIdentifier("export-everything.error")
            case nil:
                EmptyView()
            }
            if !model.isRunning {
                buttons
            }
        }
        .padding(20)
        .frame(width: 420, alignment: .leading)
        .interactiveDismissDisabled(model.isRunning)
    }

    private var buttons: some View {
        HStack {
            Spacer()
            if case let .finished(folder, _) = model.phase {
                Button(L10n.Mac.exportEverythingReveal) {
                    NSWorkspace.shared.activateFileViewerSelecting([folder])
                }
            }
            Button(L10n.Mac.exportEverythingDone) { model.dismiss() }
                .keyboardShortcut(.defaultAction)
                .accessibilityIdentifier("export-everything.done")
        }
    }

    /// What the finished sheet says: the files, then what was left out and
    /// why, so "export again while online" is said only when it would help.
    static func lines(for summary: FullExportSummary, folder: URL) -> [String] {
        var lines = [
            L10n.Mac.exportEverythingFiles(count: Int(summary.files), folder: folder.lastPathComponent),
            L10n.Mac.exportEverythingAttachments(count: Int(summary.attachmentsMissing))
        ]
        if summary.attachmentsFetchFailed > 0 {
            lines.append(L10n.Mac.exportEverythingFetchFailed(count: Int(summary.attachmentsFetchFailed)))
        }
        return lines
    }
}
