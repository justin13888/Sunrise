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

    func body(content: Content) -> some View {
        content
            .background(
                RoundedRectangle(cornerRadius: 5)
                    .fill(Color.accentColor.opacity(isActive ? 0.08 : 0))
                    .stroke(
                        Color.accentColor.opacity(isActive ? 1 : 0),
                        lineWidth: DropHighlight.borderWidth
                    )
            )
            .animation(.easeOut(duration: 0.1), value: isActive)
    }

    /// The spec's own number.
    static let borderWidth: CGFloat = 2
    /// The spec's own number: what a dragged item is drawn at while in flight.
    static let ghostOpacity: Double = 0.65
}

extension View {
    /// Show that this view would accept the drag currently in flight.
    func dropHighlight(isActive: Bool) -> some View {
        modifier(DropHighlight(isActive: isActive))
    }
}
