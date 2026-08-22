//! Where a mouse click lands.
//!
//! `docs/07-clients/tui.md` makes mouse support optional and insists the TUI
//! "works without it". That is the shape of this module: nothing here is
//! required to operate the client, and a terminal with no mouse — or a user
//! who leaves capture off, which is the default — loses nothing.
//!
//! # Why a shared layout function
//!
//! Hit-testing means knowing where the renderer put things, and the obvious
//! way to do that is to write the arithmetic twice: once to draw and once to
//! test. Two copies of a layout drift the first time a row is added, and the
//! failure is silent — clicks land one row off and the user blames the mouse.
//!
//! So [`layout`] is the single definition of where the tab bar, the body and
//! the task list sit, and `render_chrome` draws from it. A click is resolved
//! against the same rectangles the pixels came from.

use crate::view::{StreamPane, View, ViewState};
use ratatui::layout::Rect;

/// The regions of a frame a click can land in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    /// The one-row view tab bar.
    pub tabs: Rect,
    /// Everything between the tab bar and the status line.
    pub body: Rect,
    /// The status line.
    pub status: Rect,
}

/// Split `area` the way `render_chrome` does.
///
/// `preview` is whether the capture-preview row is showing, which is the only
/// thing that changes the split.
#[must_use]
pub fn layout(area: Rect, preview: bool) -> Frame {
    let preview_rows = u16::from(preview);
    let tabs = Rect {
        height: 1.min(area.height),
        ..area
    };
    let status_h = 1.min(area.height.saturating_sub(1));
    let body_h = area
        .height
        .saturating_sub(1)
        .saturating_sub(preview_rows)
        .saturating_sub(status_h);
    Frame {
        tabs,
        body: Rect {
            y: area.y.saturating_add(1),
            height: body_h,
            ..area
        },
        status: Rect {
            y: area.y + area.height.saturating_sub(status_h),
            height: status_h,
            ..area
        },
    }
}

/// What the user clicked on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// A tab in the view bar.
    Tab(View),
    /// Row `n` (counting from the top of the visible list) of the task list.
    TaskRow(usize),
    /// Row `n` of the Browse sidebar's stream list.
    StreamRow(usize),
    /// Row `n` of the Browse sidebar's context list.
    ContextRow(usize),
}

/// The labelled tabs, in bar order. Shared with the renderer so the clickable
/// columns are exactly the drawn ones.
pub const TABS: &[(&str, View, char)] = &[
    ("Today", View::Today, '1'),
    ("Inbox", View::Inbox, '2'),
    ("Stream", View::Stream, '3'),
    ("Search", View::Search, '4'),
    ("Focus", View::Focus, '5'),
    ("Routines", View::Routines, '6'),
    ("Review", View::Review, '7'),
];

/// The view whose tab covers column `x`, if any.
///
/// The widths come from the same format string the renderer uses (` {key}:
/// {label} `), so a renamed tab moves both together.
#[must_use]
pub fn tab_at(x: u16, origin: u16) -> Option<View> {
    let mut cursor = origin;
    for (label, view, key) in TABS {
        // " {key}:{label} " — two spaces, the key, the colon.
        let width = u16::try_from(label.chars().count() + 4).unwrap_or(u16::MAX);
        if x >= cursor && x < cursor.saturating_add(width) {
            let _ = key;
            return Some(*view);
        }
        cursor = cursor.saturating_add(width);
    }
    None
}

/// Resolve a click at `(x, y)` in `area`.
///
/// Returns `None` for anything that is not a target — the borders, the status
/// line, an overlay, or a view whose body is not a list. A click that resolves
/// to nothing must do nothing: guessing at the nearest row is how a mouse
/// completes a task the user never pointed at.
#[must_use]
pub fn hit(area: Rect, state: &ViewState, x: u16, y: u16) -> Option<Hit> {
    // Overlays swallow clicks: whatever is under them is not what the user can
    // see, and acting on it would be acting blind.
    if state.show_help
        || state.picker.is_some()
        || state.devices.is_some()
        || state.activity.is_some()
        || state.focus.stats.is_some()
    {
        return None;
    }
    let frame = layout(area, state.capture_preview.is_some());
    if y == frame.tabs.y {
        return tab_at(x, frame.tabs.x).map(Hit::Tab);
    }
    if y < frame.body.y || y >= frame.body.y.saturating_add(frame.body.height) {
        return None;
    }
    if state.triage {
        return None;
    }
    match state.view {
        // The Today view spends two rows on its header before the list block.
        View::Today => row_in_list(frame.body.y.saturating_add(2), frame.body, y).map(Hit::TaskRow),
        View::Inbox => row_in_list(frame.body.y, frame.body, y).map(Hit::TaskRow),
        // The search box is three rows tall above the results block.
        View::Search => {
            row_in_list(frame.body.y.saturating_add(3), frame.body, y).map(Hit::TaskRow)
        }
        View::Stream => hit_browse(frame.body, x, y),
        _ => None,
    }
}

/// A click inside the Browse view's three panes.
fn hit_browse(body: Rect, x: u16, y: u16) -> Option<Hit> {
    // 30 / 70, then 55 / 45 down the sidebar — the same split `render_stream`
    // asks Ratatui for.
    let side_w = body.width * 30 / 100;
    let split = body.height * 55 / 100;
    if x >= body.x.saturating_add(side_w) {
        return row_in_list(body.y, body, y).map(Hit::TaskRow);
    }
    if y < body.y.saturating_add(split) {
        let pane = Rect {
            height: split,
            ..body
        };
        return row_in_list(body.y, pane, y).map(Hit::StreamRow);
    }
    let pane = Rect {
        y: body.y.saturating_add(split),
        height: body.height.saturating_sub(split),
        ..body
    };
    row_in_list(pane.y, pane, y).map(Hit::ContextRow)
}

/// Row index inside a bordered list block whose top border is at `top`.
fn row_in_list(top: u16, pane: Rect, y: u16) -> Option<usize> {
    let first = top.saturating_add(1);
    let last = pane.y.saturating_add(pane.height).saturating_sub(1);
    if y < first || y >= last {
        return None;
    }
    Some(usize::from(y - first))
}

/// Which sidebar pane a [`Hit`] belongs to, for the runtime to focus.
#[must_use]
pub const fn pane_of(hit: Hit) -> Option<StreamPane> {
    match hit {
        Hit::StreamRow(_) => Some(StreamPane::Streams),
        Hit::ContextRow(_) => Some(StreamPane::Contexts),
        Hit::TaskRow(_) => Some(StreamPane::Tasks),
        Hit::Tab(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 24,
        }
    }

    #[test]
    fn the_layout_matches_the_chrome_it_describes() {
        let f = layout(area(), false);
        assert_eq!(f.tabs.height, 1);
        assert_eq!(f.body.y, 1);
        assert_eq!(f.status.y, 23);
        assert_eq!(f.body.height, 22);
        // The capture preview claims one row from the body, not the status.
        let f = layout(area(), true);
        assert_eq!(f.body.height, 21);
        assert_eq!(f.status.y, 23);
    }

    #[test]
    fn every_tab_is_clickable_across_its_whole_label() {
        // Walk the bar column by column: each tab must own a contiguous run,
        // and the runs must appear in bar order.
        let mut seen: Vec<View> = Vec::new();
        for x in 0..80u16 {
            if let Some(v) = tab_at(x, 0) {
                if seen.last() != Some(&v) {
                    seen.push(v);
                }
            }
        }
        let expected: Vec<View> = TABS.iter().map(|(_, v, _)| *v).collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn a_click_past_the_last_tab_hits_nothing() {
        assert_eq!(tab_at(200, 0), None);
    }

    #[test]
    fn clicks_resolve_to_rows_in_a_plain_list() {
        let mut state = ViewState::default();
        state.view = View::Inbox;
        // Body starts at y=1; its border is y=1, so the first row is y=2.
        assert_eq!(hit(area(), &state, 5, 1), None, "the border is not a row");
        assert_eq!(hit(area(), &state, 5, 2), Some(Hit::TaskRow(0)));
        assert_eq!(hit(area(), &state, 5, 5), Some(Hit::TaskRow(3)));
        assert_eq!(hit(area(), &state, 5, 22), None, "the bottom border");
        assert_eq!(hit(area(), &state, 5, 23), None, "the status line");
    }

    #[test]
    fn the_today_header_is_not_part_of_the_list() {
        let mut state = ViewState::default();
        state.view = View::Today;
        for y in 1..=3 {
            assert_eq!(hit(area(), &state, 5, y), None, "row {y}");
        }
        assert_eq!(hit(area(), &state, 5, 4), Some(Hit::TaskRow(0)));
    }

    #[test]
    fn the_browse_panes_each_own_their_side_of_the_frame() {
        let mut state = ViewState::default();
        state.view = View::Stream;
        // Sidebar is the left 30 columns; the stream list is its top 55%.
        assert_eq!(hit(area(), &state, 5, 2), Some(Hit::StreamRow(0)));
        assert_eq!(hit(area(), &state, 5, 14), Some(Hit::ContextRow(0)));
        assert_eq!(hit(area(), &state, 60, 2), Some(Hit::TaskRow(0)));
    }

    #[test]
    fn an_overlay_swallows_the_click() {
        // Acting on what is *under* an overlay is acting blind.
        let mut state = ViewState::default();
        state.view = View::Inbox;
        state.show_help = true;
        assert_eq!(hit(area(), &state, 5, 3), None);
    }

    #[test]
    fn a_tab_click_lands_on_its_view() {
        let state = ViewState::default();
        assert_eq!(hit(area(), &state, 2, 0), Some(Hit::Tab(View::Today)));
        assert_eq!(hit(area(), &state, 12, 0), Some(Hit::Tab(View::Inbox)));
    }
}
