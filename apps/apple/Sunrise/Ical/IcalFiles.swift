import Foundation
import UniformTypeIdentifiers

/// Reading and writing `.ics` documents on disk.
///
/// Split out of the menu items so the file half is testable without a picker:
/// ``read(_:)`` and ``write(_:to:)`` are the parts that can actually fail, and
/// neither an `NSOpenPanel` nor a `UIDocumentPicker` is something a test can
/// drive.
///
/// Everything here is portable. *Choosing* the file is not — macOS runs a
/// modal panel and iOS presents a document picker as a sheet — so each
/// platform adds its own picking API in an extension beside its own UI.
enum IcalFiles {
    /// The types a picker offers. `.ics` by extension rather than by a named
    /// `UTType` constant: the system's declaration for iCalendar is
    /// `com.apple.ical.ics`, which is Calendar.app's and not guaranteed to be
    /// registered on a device that has never opened one.
    static var documentTypes: [UTType] {
        [UTType(filenameExtension: "ics"), UTType(mimeType: "text/calendar")]
            .compactMap { $0 } + [.text]
    }

    /// Read a picked file as text.
    ///
    /// UTF-8 first, which is what RFC 5545 requires and what every calendar
    /// client this decade writes. A file that is not UTF-8 is retried as
    /// Latin-1 rather than refused: an `.ics` exported by an old Outlook is
    /// still an `.ics`, and the parser will report anything it cannot read as
    /// a notice.
    static func read(_ url: URL) throws -> String {
        if let text = try? String(contentsOf: url, encoding: .utf8) { return text }
        return try String(contentsOf: url, encoding: .isoLatin1)
    }

    static func write(_ text: String, to url: URL) throws {
        try Data(text.utf8).write(to: url, options: .atomic)
    }
}
