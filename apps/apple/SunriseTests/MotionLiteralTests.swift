import Foundation
import Testing

/// ADR-0072 decision 1: a view animates only through `Motion`, never with a
/// literal duration, curve or spring.
///
/// Read from source, the way `NoLiteralChordTests` reads the keymap: the claim
/// is about what the code says. A literal is what ignores Reduce Motion, so
/// every site has to name `Motion`, whose accessors return `nil` under it.
struct MotionLiteralTests {
    /// `apps/apple`, found relative to this file.
    private static let root = URL(filePath: #filePath)
        .deletingLastPathComponent()
        .deletingLastPathComponent()

    /// The app's own sources, and the extensions built beside it.
    private static let folders = ["Sunrise", "macOS", "iOS", "Widgets", "Share"]

    /// The calls that start an animation.
    private static let calls = ["withAnimation", ".animation("]

    @Test
    func everyAnimationIsReadFromTheMotionTokens() throws {
        let sites = try Self.sites()
        // The anchor: a scan that found nothing would be green over nothing.
        #expect(sites.count >= 4, "found only \(sites.count) animation sites")
        #expect(sites.contains { $0.file == "DropHighlight.swift" })
        #expect(sites.contains { $0.file == "CommandPaletteView.swift" })
        for site in sites where !Self.isTokenised(site.argument) {
            Issue.record(
                """
                \(site.file):\(site.line) animates with `\(site.argument)`.

                A view never writes a literal duration, curve or spring \
                (ADR-0072 decision 1). Use `Motion.fast`, `Motion.standard` or \
                `Motion.slow` from `Sunrise/Design/Tokens.swift`, which return \
                `nil` under Reduce Motion.
                """
            )
        }
    }

    /// The scan's own rule, pinned on the shapes it has to tell apart.
    @Test
    func theRuleRejectsLiteralsAndAcceptsTokens() {
        #expect(Self.isTokenised("Motion.fast(reduceMotion: reduceMotion), value: x)"))
        #expect(Self.isTokenised("(Motion.standard(reduceMotion: reduceMotion)) {"))
        #expect(Self.isTokenised("nil, value: x)"))
        #expect(!Self.isTokenised("(.linear(duration: 0.1)) {"))
        #expect(!Self.isTokenised(".spring(), value: x)"))
        #expect(!Self.isTokenised(".default, value: x)"))
        // A bare `withAnimation { }` is `.default`, a curve nobody chose.
        #expect(!Self.isTokenised("{"))
        #expect(!Self.isTokenised(""))
    }

    /// Whether what follows the call names a `Motion` accessor, or `nil`.
    private static func isTokenised(_ argument: String) -> Bool {
        var rest = Substring(argument)
        if rest.hasPrefix("(") { rest = rest.dropFirst() }
        rest = rest.drop { $0 == " " }
        return rest.hasPrefix("Motion.") || rest.hasPrefix("nil")
    }

    private struct Site {
        let file: String
        let line: Int
        let argument: String
    }

    /// Every animation call in the app's sources, comments stripped so a
    /// sentence about an animation is not mistaken for one. An argument that
    /// starts on the next line is read from there.
    private static func sites() throws -> [Site] {
        var found: [Site] = []
        for folder in folders {
            let directory = root.appending(path: folder)
            guard FileManager.default.fileExists(atPath: directory.path(percentEncoded: false)) else {
                continue
            }
            let paths = try FileManager.default
                .subpathsOfDirectory(atPath: directory.path(percentEncoded: false))
            for path in paths where path.hasSuffix(".swift") {
                let file = directory.appending(path: path)
                let lines = try String(contentsOf: file, encoding: .utf8)
                    .components(separatedBy: .newlines)
                    .map { $0.components(separatedBy: "//").first ?? $0 }
                for (index, code) in lines.enumerated() {
                    for call in calls {
                        guard let range = code.range(of: call) else { continue }
                        var argument = code[range.upperBound...].trimmingCharacters(in: .whitespaces)
                        if argument.isEmpty || argument == "(", index + 1 < lines.count {
                            argument += lines[index + 1].trimmingCharacters(in: .whitespaces)
                        }
                        found.append(Site(file: file.lastPathComponent, line: index + 1, argument: argument))
                    }
                }
            }
        }
        return found
    }
}
