import AppKit
import Foundation

/// Choosing an `.ics` to read, and somewhere to write one — the macOS half.
///
/// A modal `NSOpenPanel` / `NSSavePanel`, which is what a Mac's File menu
/// does: the menu item is a scene command that can fire with every window
/// closed, so it needs a picker that does not need a window to present from.
/// iOS cannot do this — a document picker is presented *by* a view — so its
/// half is a `.fileImporter` on the screen that asked, not a function that
/// returns a URL.
extension IcalFiles {
    /// Ask for a file to import. `nil` when the user cancelled.
    @MainActor
    static func pickDocument() -> URL? {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = documentTypes
        panel.allowsMultipleSelection = false
        panel.canChooseDirectories = false
        panel.message = "Choose a calendar file to import as time blocks."
        panel.prompt = "Import"
        return panel.runModal() == .OK ? panel.url : nil
    }

    /// Ask where to write an export. `nil` when the user cancelled.
    @MainActor
    static func pickDestination(named suggestion: String) -> URL? {
        let panel = NSSavePanel()
        panel.allowedContentTypes = documentTypes
        panel.nameFieldStringValue = suggestion
        panel.message = "Choose where to write the calendar."
        panel.prompt = "Export"
        return panel.runModal() == .OK ? panel.url : nil
    }
}
