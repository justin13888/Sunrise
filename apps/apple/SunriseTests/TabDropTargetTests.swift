import Foundation
import Testing

/// Whether anything in the iOS shell lets a tab accept a dragged task.
///
/// `docs/07-clients/parity-matrix.md` grades *Task → Calendar block* **No** on
/// iOS, and one leg of that argument is a claim about the shell: no `Tab`
/// carries a `dropDestination`, and nothing configures spring-loading — so a
/// drag lifted from a task row has nothing to land on while the grid is behind
/// another tab.
///
/// That claim used to be checked by `SunriseiOSUITests/DragAcrossTabsUITests`,
/// which lifted a row on the simulator, dragged it onto the Calendar tab and
/// held it there for two seconds. This replaces it, and the reason is
/// [#215](https://github.com/justin13888/Sunrise/issues/215): that test asserted
/// a **negative** through a timing-dependent gesture. It failed the `ios-app`
/// job on a pull request that changed five markdown files and nothing else, then
/// passed a re-run of the same commit. A negative assertion that can fail
/// spuriously can pass spuriously too — if the long-press never lifted the row,
/// no drag ever reached the tab bar, and "the tab did not change" is then
/// satisfied by nothing having happened at all. Twenty-six minutes of CI bought
/// a published **No** that rested on the absence of an event the test could not
/// show had occurred.
///
/// What that cell actually claims is a property of the view tree, and the view
/// tree is on disk. A tab that took a drop would say so in SwiftUI — a
/// `dropDestination` or an `onDrop` inside the `TabView` — and spring-loading is
/// a `springLoadingBehavior` modifier or a `UISpringLoadedInteraction`. None of
/// them is written anywhere in the shell, this reads the sources and says so,
/// and it costs milliseconds and cannot flake.
///
/// Two things it deliberately does not claim. It is not evidence about what
/// UIKit does with a tab bar nobody configured — the matrix now says only what
/// this proves, which is that the app builds no such path. And it says nothing
/// about the **two-handed** gesture, which stays unmeasured for the same reason
/// it always did: XCUITest has no API for two independent simultaneous touches.
struct TabDropTargetTests {
    /// The iOS shell's own sources, found relative to this file.
    ///
    /// `#filePath` is the checkout the tests were *built* from, which is the
    /// checkout they are run from in CI and on a laptop alike. A simulator is
    /// not a device: the test process is a host process and reads host paths.
    private static let shell = URL(filePath: #filePath)
        .deletingLastPathComponent()
        .deletingLastPathComponent()
        .appending(path: "iOS")

    /// Every way SwiftUI or UIKit has of making something take a drag.
    ///
    /// Scoped to `iOS/` rather than the whole app, and that is the point rather
    /// than a shortcut: four views under `Sunrise/` *are* drop targets on
    /// purpose — the calendar grid, a task list, a stream row, a context row —
    /// and the cell is not about them. `iOS/` is the shell, and the shell is
    /// what would have to grow a drop target for a tab to take one.
    private static let dropConfiguration = [
        "dropDestination",
        "onDrop",
        "springLoadingBehavior",
        "isSpringLoaded",
        "UISpringLoadedInteraction",
        "dropProposal"
    ]

    @Test
    func noTabInTheIosShellTakesADropOrSpringsLoaded() throws {
        let sources = try Self.shellSources()
        for source in sources {
            for line in try Self.codeLines(of: source) {
                for token in Self.dropConfiguration where line.text.contains(token) {
                    Issue.record(
                        """
                        \(source.lastPathComponent):\(line.number) configures `\(token)`.

                        The iOS shell now builds a path a dragged task could \
                        follow across the tab bar, which is exactly what \
                        docs/07-clients/parity-matrix.md's *Task → Calendar \
                        block* **No** says it does not. Change the cell with \
                        the code, or take the configuration back out.
                        """
                    )
                }
            }
        }
    }

    /// The anchor, without which the test above is green over nothing.
    ///
    /// A source-level check has one failure mode a behavioural one does not: it
    /// passes when the file it was reading moves, gets renamed, or stops
    /// declaring the thing the claim is about. So this asserts the shape the
    /// other test assumes — that `VaultTabs.swift` is where it was, and still
    /// declares a `TabView` with the five tabs ``AppTab`` names.
    @Test
    func theShellStillDeclaresTheTabsThisIsAbout() throws {
        let tabs = Self.shell.appending(path: "VaultTabs.swift")
        let lines = try Self.codeLines(of: tabs)

        #expect(
            lines.contains { $0.text.contains("TabView(selection:") },
            "iOS/VaultTabs.swift no longer declares the TabView this suite reads"
        )
        let declarations = lines.filter {
            $0.text.trimmingCharacters(in: .whitespaces).hasPrefix("Tab(")
        }
        #expect(
            declarations.count == 5,
            """
            iOS/VaultTabs.swift declares \(declarations.count) tabs, not the \
            five AppTab names. The shell has been reshaped; check that the \
            drop-configuration test above still reads what it means to.
            """
        )
    }

    /// Every hand-written Swift file in the shell.
    private static func shellSources() throws -> [URL] {
        let found = try FileManager.default
            .contentsOfDirectory(at: shell, includingPropertiesForKeys: nil)
            .filter { $0.pathExtension == "swift" }
            .sorted { $0.lastPathComponent < $1.lastPathComponent }
        // Not `#expect`: with no sources there is nothing for the caller to
        // scan and a green result would mean "the gate could not run", which is
        // how a source-level check rots into a no-op.
        try #require(
            found.contains { $0.lastPathComponent == "VaultTabs.swift" },
            "no iOS/VaultTabs.swift under \(shell.path(percentEncoded: false))"
        )
        return found
    }

    /// A file's lines, numbered, with comments taken out.
    ///
    /// Comments are dropped for the reason `.github/scripts/grep-gate.sh` drops
    /// them: a comment in the shell that explains *why* there is no drop
    /// destination on a tab would otherwise be the thing that fails the test
    /// asserting there is none. Cutting at the first `//` also cuts a
    /// `sunrise://` inside a string literal short, which costs nothing here —
    /// no banned token follows one.
    private static func codeLines(of file: URL) throws -> [CodeLine] {
        let body = try String(contentsOf: file, encoding: .utf8)
        return body.components(separatedBy: .newlines).enumerated().compactMap { index, raw in
            let code: String
            if let marker = raw.range(of: "//") {
                code = String(raw[raw.startIndex..<marker.lowerBound])
            } else {
                code = raw
            }
            guard !code.trimmingCharacters(in: .whitespaces).isEmpty else { return nil }
            return CodeLine(number: index + 1, text: code)
        }
    }

    /// One line of code, with the number a reader would need to find it.
    private struct CodeLine {
        let number: Int
        let text: String
    }
}
