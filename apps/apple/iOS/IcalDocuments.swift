import SwiftUI
import UniformTypeIdentifiers

/// One rendered `.ics`, ready for a document picker to place.
///
/// The iOS half of the split `Sunrise/Ical/AppSurfaces+Ical.swift` describes:
/// reading and writing port unchanged, and *choosing the file* does not. macOS
/// runs an `NSSavePanel` from a menu item (`macOS/IcalPanels.swift`); iOS has
/// no menu and no modal panel, so an export is a document the system's own
/// exporter writes wherever the user says.
///
/// A `FileDocument` rather than a temporary file behind a `ShareLink`: the
/// exporter is the surface that offers **Files**, iCloud Drive and every other
/// document provider, which is what "export a calendar" means here. Sharing it
/// into a message is a share sheet away from there and needs nothing from this
/// app.
struct IcalDocument: FileDocument {
    /// What the exporter offers to write. `IcalFiles.documentTypes` is the
    /// same list the Mac's panels use, so both platforms agree about what an
    /// `.ics` is — see the note there on why it is built by extension rather
    /// than from `com.apple.ical.ics`.
    static let readableContentTypes: [UTType] = IcalFiles.documentTypes

    /// The type the exporter is told to write, which must be one of the above.
    static var contentType: UTType { readableContentTypes[0] }

    let text: String
    let filename: String

    init(text: String, filename: String) {
        self.text = text
        self.filename = filename
    }

    init(configuration: ReadConfiguration) throws {
        // Imports come through `fileImporter`, which hands back a URL that
        // `IcalFiles.read` opens itself — it has to, because an `.ics` that is
        // not UTF-8 is retried as Latin-1 and a `FileDocument` initialiser has
        // already lost the bytes by the time it could try.
        throw CocoaError(.fileReadUnsupportedScheme)
    }

    func fileWrapper(configuration: WriteConfiguration) throws -> FileWrapper {
        FileWrapper(regularFileWithContents: Data(text.utf8))
    }
}
