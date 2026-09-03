import SwiftUI

/// The drop-target treatment `docs/07-clients/interaction-patterns.md`
/// §Drag-and-drop UX tokens specifies: a 2 px solid accent border over an 8 %
/// accent tint.
///
/// One modifier rather than the numbers written at each of the drop sites,
/// because a target that looks different in the sidebar from how it looks on
/// the calendar teaches the user that some drops are a different kind of drop.
struct DropHighlight: ViewModifier {
    let isActive: Bool

    /// `nil` under Reduce Motion, which is how the highlight stops animating
    /// without a branch here. See `Sunrise/Design/Tokens.swift`.
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    func body(content: Content) -> some View {
        content
            .background(
                RoundedRectangle(cornerRadius: SunriseTokens.Radius.sm)
                    .fill(Color.accentColor.opacity(isActive ? DropHighlight.fillOpacity : 0))
                    .stroke(
                        Color.accentColor.opacity(isActive ? 1 : 0),
                        lineWidth: DropHighlight.borderWidth
                    )
            )
            .animation(Motion.fast(reduceMotion: reduceMotion), value: isActive)
    }

    // The corner radius above **changed**: it was a literal 5 and is now
    // `Radius.sm`, which is 4. No spec named 5 — it was the one number in this
    // file with no doc behind it — so nothing is violated by moving it onto the
    // scale, but it is a one-point visual change rather than a pure
    // substitution, and worth knowing about at a glance.
    //
    // The three numbers below stay literals, and deliberately. They are
    // `interaction-patterns.md` §Drag-and-drop UX tokens' own values — a border
    // width and two opacities — and `shared-ui-system.md`'s token set has no
    // scale for either. Inventing one would put values in
    // `packages/sunrise-ui-tokens/tokens/` that no design doc specifies, which
    // is the failure mode #29 is about, pointed the other way.

    /// The spec's own number.
    static let borderWidth: CGFloat = 2
    /// The spec's own number: the accent tint behind an active drop target.
    static let fillOpacity: Double = 0.08
    /// The spec's own number: what a dragged item is drawn at while in flight.
    static let ghostOpacity: Double = 0.65
}

extension View {
    /// Show that this view would accept the drag currently in flight.
    func dropHighlight(isActive: Bool) -> some View {
        modifier(DropHighlight(isActive: isActive))
    }
}
