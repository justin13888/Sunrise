import Foundation

/// The iCalendar surface, from wherever it is asked for.
///
/// Split out of ``AppSurfaces`` because it is a self-contained leg — read a
/// document in, render one out — and because its two halves divide on a line
/// worth seeing in one place: reading and writing port unchanged, and
/// *choosing the file* does not. macOS runs a modal panel from a menu item
/// that exists with every window closed; iOS presents a document picker from a
/// view that must already be on screen. Both hand a URL to the same two
/// methods below.
extension AppSurfaces {

    /// Read an `.ics` the user has already chosen.
    ///
    /// The reading is the app's; the parsing is the core's. A read that fails
    /// lands on the same `errorMessage` a refused parse does, because from
    /// where the user is standing "that file could not be read" is one outcome
    /// however far down it failed.
    ///
    /// Takes a URL rather than picking one, because picking is the half that
    /// does not port: macOS runs a modal panel from a menu item that exists
    /// with every window closed, and iOS presents a document picker from a
    /// view. Both hand the chosen URL here.
    func importIcal(from url: URL) async {
        guard let ical else {
            Platform.refusalFeedback()
            return
        }
        do {
            await ical.importDocument(text: try IcalFiles.read(url))
        } catch {
            ical.summary = nil
            ical.errorMessage = error.localizedDescription
        }
    }

    /// Render one window of the calendar and write it to `url`.
    func exportIcal(_ window: ExportWindow, to url: URL) async {
        guard let ical else {
            Platform.refusalFeedback()
            return
        }
        guard let text = await ical.exportDocument(window: window) else { return }
        do {
            try IcalFiles.write(text, to: url)
        } catch {
            ical.errorMessage = error.localizedDescription
        }
    }

    #if os(macOS)
    /// **File → Import Calendar…**: pick an `.ics`, then read it in.
    func importIcal() async {
        guard ical != nil else {
            Platform.refusalFeedback()
            return
        }
        guard let url = IcalFiles.pickDocument() else { return }
        await importIcal(from: url)
    }

    /// **File → Export Calendar ▸ …**: pick a destination, then render into it.
    ///
    /// Destination first, then render, which is the order every macOS save
    /// takes — and it means a cancelled panel costs nothing.
    func exportIcal(_ window: ExportWindow) async {
        guard ical != nil else {
            Platform.refusalFeedback()
            return
        }
        guard let url = IcalFiles.pickDestination(named: window.suggestedFilename) else { return }
        await exportIcal(window, to: url)
    }
    #endif
}
