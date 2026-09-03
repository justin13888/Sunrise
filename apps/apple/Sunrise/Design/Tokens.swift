import SwiftUI

/// The SwiftUI adapter over the generated token set.
///
/// `packages/sunrise-ui-tokens/generated/tokens.swift` is compiled into both
/// app targets and knows nothing about SwiftUI — it is numbers and sRGB
/// triples. Everything that turns one of those into something a view can draw
/// lives here, and only here, so that "what colour is a stream" has exactly one
/// answer on this platform.
///
/// Two environment values do the resolving: `\.colorScheme` picks light or dark,
/// and `\.accessibilityReduceMotion` decides whether there is an animation at
/// all. Neither is a build flag, so neither can be baked into the generated
/// file.

extension Color {
    /// A generated sRGB triple as a SwiftUI colour.
    init(_ token: SunriseTokens.RGB) {
        self.init(.sRGB, red: token.red, green: token.green, blue: token.blue, opacity: 1)
    }
}

/// The semantic surface palette, resolved for one colour scheme.
///
/// A view reads it as
/// `SunrisePalette.resolved(for: colorScheme)` with
/// `@Environment(\.colorScheme) private var colorScheme`.
struct SunrisePalette: Equatable {
    let bg: Color
    let fg: Color
    let muted: Color
    let accent: Color
    let accentText: Color
    let border: Color
    let danger: Color
    let warning: Color
    let success: Color
    let info: Color

    /// The palette for `scheme`. The only place light and dark are chosen.
    static func resolved(for scheme: ColorScheme) -> SunrisePalette {
        scheme == .dark
            ? SunrisePalette(
                bg: Color(SunriseTokens.Surface.Dark.bg),
                fg: Color(SunriseTokens.Surface.Dark.fg),
                muted: Color(SunriseTokens.Surface.Dark.muted),
                accent: Color(SunriseTokens.Surface.Dark.accent),
                accentText: Color(SunriseTokens.Surface.Dark.accentText),
                border: Color(SunriseTokens.Surface.Dark.border),
                danger: Color(SunriseTokens.Surface.Dark.danger),
                warning: Color(SunriseTokens.Surface.Dark.warning),
                success: Color(SunriseTokens.Surface.Dark.success),
                info: Color(SunriseTokens.Surface.Dark.info)
            )
            : SunrisePalette(
                bg: Color(SunriseTokens.Surface.Light.bg),
                fg: Color(SunriseTokens.Surface.Light.fg),
                muted: Color(SunriseTokens.Surface.Light.muted),
                accent: Color(SunriseTokens.Surface.Light.accent),
                accentText: Color(SunriseTokens.Surface.Light.accentText),
                border: Color(SunriseTokens.Surface.Light.border),
                danger: Color(SunriseTokens.Surface.Light.danger),
                warning: Color(SunriseTokens.Surface.Light.warning),
                success: Color(SunriseTokens.Surface.Light.success),
                info: Color(SunriseTokens.Surface.Light.info)
            )
    }
}

extension StreamColor {
    /// The swatch for a stream's colour, in `scheme`.
    ///
    /// The *names* are the domain's — `slate`, `rose`, `emerald` — and this only
    /// decides what each one looks like. It used to decide it with system
    /// colours (`.gray`, `.pink`, …) plus one raw `Color(red:green:blue:)`,
    /// which meant the Mac and the web app drew different palettes under the
    /// same eight names.
    func tint(for scheme: ColorScheme) -> Color {
        scheme == .dark ? Color(darkToken) : Color(lightToken)
    }

    private var lightToken: SunriseTokens.RGB {
        switch self {
        case .slate: SunriseTokens.Stream.Light.slate
        case .rose: SunriseTokens.Stream.Light.rose
        case .amber: SunriseTokens.Stream.Light.amber
        case .emerald: SunriseTokens.Stream.Light.emerald
        case .sky: SunriseTokens.Stream.Light.sky
        case .indigo: SunriseTokens.Stream.Light.indigo
        case .violet: SunriseTokens.Stream.Light.violet
        case .pink: SunriseTokens.Stream.Light.pink
        }
    }

    private var darkToken: SunriseTokens.RGB {
        switch self {
        case .slate: SunriseTokens.Stream.Dark.slate
        case .rose: SunriseTokens.Stream.Dark.rose
        case .amber: SunriseTokens.Stream.Dark.amber
        case .emerald: SunriseTokens.Stream.Dark.emerald
        case .sky: SunriseTokens.Stream.Dark.sky
        case .indigo: SunriseTokens.Stream.Dark.indigo
        case .violet: SunriseTokens.Stream.Dark.violet
        case .pink: SunriseTokens.Stream.Dark.pink
        }
    }

    /// The swatch as a style, so a call site needs no `@Environment` of its own.
    ///
    /// `ShapeStyle.resolve(in:)` is handed the `EnvironmentValues` the view is
    /// drawing in, which is how `.foregroundStyle(stream.tint)` follows the
    /// colour scheme without every call site plumbing one through.
    var tint: StreamTint { StreamTint(light: Color(lightToken), dark: Color(darkToken)) }
}

/// A stream swatch that resolves against the environment it is drawn in.
///
/// It holds the two resolved colours rather than the `StreamColor` itself:
/// `ShapeStyle` refines `Sendable`, and the UniFFI-generated enum is `public`,
/// so it gets no implicit `Sendable` conformance. `Color` has one.
struct StreamTint: ShapeStyle {
    let light: Color
    let dark: Color

    func resolve(in environment: EnvironmentValues) -> Color {
        environment.colorScheme == .dark ? dark : light
    }
}

/// Animations built from the generated motion tokens.
///
/// Every accessor returns an *optional*: `nil` is how SwiftUI is told not to
/// animate, so honouring `\.accessibilityReduceMotion` is one `nil` rather than
/// a branch at every call site. `docs/10-cross-cutting/accessibility.md` asks
/// for Reduce Motion support; this is the hook it hangs on.
enum Motion {
    /// 120 ms — a state change the user caused and is looking at.
    static func fast(reduceMotion: Bool) -> Animation? {
        animation(SunriseTokens.Motion.fast, reduceMotion: reduceMotion)
    }

    /// 220 ms — the default for anything that moves rather than just changes.
    static func standard(reduceMotion: Bool) -> Animation? {
        animation(SunriseTokens.Motion.med, reduceMotion: reduceMotion)
    }

    /// 360 ms — a whole surface arriving or leaving.
    static func slow(reduceMotion: Bool) -> Animation? {
        animation(SunriseTokens.Motion.slow, reduceMotion: reduceMotion)
    }

    private static func animation(
        _ token: SunriseTokens.MotionToken,
        reduceMotion: Bool
    ) -> Animation? {
        guard !reduceMotion else { return nil }
        return .timingCurve(
            token.easing.x1,
            token.easing.y1,
            token.easing.x2,
            token.easing.y2,
            duration: token.duration
        )
    }
}
