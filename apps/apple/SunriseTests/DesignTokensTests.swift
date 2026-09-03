import Foundation
import SwiftUI
import Testing

@testable import Sunrise

/// The adapter between the generated token set and SwiftUI.
///
/// What these pin is not "the tokens have these values" — the drift gate in
/// `packages/sunrise-ui-tokens/test/` does that, against the TOML. It is that
/// the app *reaches* them: that every `StreamColor` resolves to a generated
/// tint rather than a system colour, that the two themes actually differ, and
/// that Reduce Motion produces no animation at all rather than a fast one.
struct DesignTokensTests {
    /// Every variant, so a ninth `StreamColor` fails here rather than drawing
    /// whatever the compiler picks.
    ///
    /// `StreamColor.all` is hand-written in `BrowseSidebar.swift` and tracked
    /// nothing until this test; a bare `count == 8` would have moved with it.
    /// It is pinned against the generated name list instead, which the TOML
    /// owns and `invariants.test.ts` pins to the Rust enum in turn.
    @Test
    func everyStreamColourResolvesToItsGeneratedTint() {
        #expect(StreamColor.all.count == SunriseTokens.Stream.names.count)
        for stream in StreamColor.all {
            #expect(SunriseTokens.Stream.names.contains(String(describing: stream)))
            #expect(stream.tint(for: .light) != stream.tint(for: .dark))
        }
    }

    @Test
    func streamTintsComeFromTheGeneratedTable() {
        #expect(StreamColor.slate.tint(for: .light) == Color(SunriseTokens.Stream.Light.slate))
        #expect(StreamColor.pink.tint(for: .dark) == Color(SunriseTokens.Stream.Dark.pink))
    }

    /// `StreamTint` is a `ShapeStyle` so that `.foregroundStyle(stream.tint)`
    /// follows the colour scheme with no `@Environment` at the call site.
    @Test
    func theStreamStyleResolvesAgainstTheEnvironment() {
        var light = EnvironmentValues()
        light.colorScheme = .light
        var dark = EnvironmentValues()
        dark.colorScheme = .dark

        let tint = StreamColor.emerald.tint
        #expect(tint.resolve(in: light) == Color(SunriseTokens.Stream.Light.emerald))
        #expect(tint.resolve(in: dark) == Color(SunriseTokens.Stream.Dark.emerald))
    }

    @Test
    func thePaletteHasATrueDarkTheme() {
        let light = SunrisePalette.resolved(for: .light)
        let dark = SunrisePalette.resolved(for: .dark)
        #expect(light != dark)
        #expect(light.bg == Color(SunriseTokens.Surface.Light.bg))
        #expect(dark.bg == Color(SunriseTokens.Surface.Dark.bg))
    }

    /// The whole point of returning `Animation?`: Reduce Motion is `nil`, not a
    /// shorter duration.
    @Test
    func reduceMotionRemovesTheAnimationEntirely() {
        #expect(Motion.fast(reduceMotion: true) == nil)
        #expect(Motion.standard(reduceMotion: true) == nil)
        #expect(Motion.slow(reduceMotion: true) == nil)

        #expect(Motion.fast(reduceMotion: false) != nil)
        #expect(Motion.standard(reduceMotion: false) != nil)
        #expect(Motion.slow(reduceMotion: false) != nil)
    }

    /// The generated durations are seconds, not milliseconds — a token read as
    /// the wrong unit is a 120-second animation nobody would ship.
    @Test
    func motionDurationsAreSeconds() {
        #expect(SunriseTokens.Motion.fast.duration == 0.12)
        #expect(SunriseTokens.Motion.med.duration == 0.22)
        #expect(SunriseTokens.Motion.reducedDuration == 0)
    }

    /// The spacing scale the pipeline settled on, which is not the one
    /// `packages/sunrise-ui` used to carry: `md` was 16 there.
    @Test
    func theSpacingScaleIsTheSixStepOne() {
        #expect(SunriseTokens.Space.xs == 4)
        #expect(SunriseTokens.Space.sm == 8)
        #expect(SunriseTokens.Space.md == 12)
        #expect(SunriseTokens.Space.lg == 16)
        #expect(SunriseTokens.Space.xl == 24)
        #expect(SunriseTokens.Space.xxl == 32)
    }
}
