import XCTest

/// File → Export Everything…, driven from the menu bar through the real save
/// panel to a folder on disk (#350, `docs/04-storage/export-format.md`).
///
/// The panel is the system's own, driven the way a user drives it: ⌘⇧G to go
/// to a folder, a name typed into the name field, Return for the default
/// button. There is no harness seam that skips it, because the claim is that
/// the menu item reaches the core *through* the panel, and a seam would test
/// everything except the part people click.
@MainActor
final class ExportEverythingUITests: SunriseUITestCase {
    /// Where the export goes. Its own temporary directory rather than the
    /// scratch vault's, so the export is never written inside the vault it
    /// reads.
    private var parent: URL?

    override func tearDown() async throws {
        if let parent { try? FileManager.default.removeItem(at: parent) }
        try await super.tearDown()
    }

    func testExportEverythingWritesTheVaultToTheChosenFolder() throws {
        createVault()
        capture("Renew passport")

        let parent = FileManager.default.temporaryDirectory
            .appending(path: "sunrise-export-uitest-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: true)
        self.parent = parent
        let name = "Everything"

        activate(app.menuBars.menuBarItems["File"], named: "the File menu")
        activate(app.menuItems["Export Everything…"], named: "File → Export Everything…")

        let nameField = app.textFields["saveAsNameTextField"]
        XCTAssertTrue(nameField.appears(within: 10), "the save panel opens with a name field")

        // Go to the parent folder, then name the new one inside it.
        app.typeKey("g", modifierFlags: [.command, .shift])
        app.typeText(parent.path(percentEncoded: false) + "\n")
        activate(nameField, named: "the save panel's name field")
        nameField.typeKey("a", modifierFlags: .command)
        nameField.typeText(name + "\n")

        // The sheet's Done button is drawn only once the export is over.
        let done = app.buttons["export-everything.done"]
        XCTAssertTrue(done.appears(within: 60), "the export sheet reaches its outcome")
        XCTAssertFalse(
            app.staticTexts["export-everything.error"].exists,
            "the export finished rather than failing"
        )
        XCTAssertTrue(
            app.staticTexts["export-everything.summary"].firstMatch.exists,
            "the sheet says what was written"
        )

        // `manifest.json` is written last, so its presence is the export
        // having finished (`export-format.md` §Taking one).
        let folder = parent.appending(path: name)
        let manifest = folder.appending(path: "manifest.json")
        XCTAssertTrue(FileManager.default.fileExists(atPath: manifest.path(percentEncoded: false)))
        let tasks = try String(
            contentsOf: folder.appending(path: "entities/task.jsonl"),
            encoding: .utf8
        )
        XCTAssertTrue(tasks.contains("Renew passport"), "the captured task is in the export")

        activate(done, named: "the export sheet's Done button")
        XCTAssertTrue(
            waitUntil(timeout: 10) { !done.exists },
            "Done puts the sheet away"
        )
    }
}
