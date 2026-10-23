//! `sunrise://` links and shared captures, lowered onto UniFFI:
//! [`sunrise_client_core::links`] and [`sunrise_client_core::share`]. Free
//! functions, like [`crate::vocab`]: none touches a vault.
//!
//! Two mirrors carry a `Core` prefix — [`CoreDeepLink`] and
//! [`CoreTaskLinkAction`] — because the Apple app already declares a
//! `DeepLink` and a `TaskLinkAction` of its own, which these replace in a later
//! pass; the generated Swift must not shadow them first.

use sunrise_client_core::links::{DeepLink, LinkScreen, TaskLinkAction};
use sunrise_client_core::share::{self, ImageRefusal};
use sunrise_domain::note_body;
use sunrise_domain::SnoozeSpan;
use sunrise_id::EntityRef;

use crate::notes::NoteBlock;

/// See [`sunrise_client_core::links::LinkScreen`].
#[uniffi::remote(Enum)]
pub enum LinkScreen {
    Morning,
    Evening,
    Today,
    Calendar,
    Focus,
}

/// See [`sunrise_client_core::links::TaskLinkAction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CoreTaskLinkAction {
    /// Show the task.
    Open,
    /// Complete it.
    Complete,
    /// Snooze it.
    Snooze {
        /// For how long.
        span: SnoozeSpan,
    },
}

/// See [`sunrise_client_core::links::DeepLink`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum CoreDeepLink {
    /// `sunrise://morning`.
    MorningSummary,
    /// `sunrise://evening`.
    EndOfDayPlan,
    /// `sunrise://entity/<id>`.
    Entity {
        /// The entity.
        id: EntityRef,
    },
    /// `sunrise://capture?text=…&stream=…`, the stream folded in as `#name`.
    Capture {
        /// The capture line.
        text: String,
    },
    /// `sunrise://task/<id>?action=…`.
    Task {
        /// The task.
        id: EntityRef,
        /// What to do.
        action: CoreTaskLinkAction,
    },
    /// `sunrise://focus/<id>`.
    Focus {
        /// The task.
        id: EntityRef,
    },
}

impl From<DeepLink> for CoreDeepLink {
    fn from(link: DeepLink) -> Self {
        match link {
            DeepLink::MorningSummary => Self::MorningSummary,
            DeepLink::EndOfDayPlan => Self::EndOfDayPlan,
            DeepLink::Entity(id) => Self::Entity { id },
            DeepLink::Capture(text) => Self::Capture { text },
            DeepLink::Task(id, action) => Self::Task {
                id,
                action: match action {
                    TaskLinkAction::Open => CoreTaskLinkAction::Open,
                    TaskLinkAction::Complete => CoreTaskLinkAction::Complete,
                    TaskLinkAction::Snooze(span) => CoreTaskLinkAction::Snooze { span },
                },
            },
            DeepLink::Focus(id) => Self::Focus { id },
        }
    }
}

impl From<CoreDeepLink> for DeepLink {
    fn from(link: CoreDeepLink) -> Self {
        match link {
            CoreDeepLink::MorningSummary => Self::MorningSummary,
            CoreDeepLink::EndOfDayPlan => Self::EndOfDayPlan,
            CoreDeepLink::Entity { id } => Self::Entity(id),
            CoreDeepLink::Capture { text } => Self::Capture(text),
            CoreDeepLink::Task { id, action } => Self::Task(
                id,
                match action {
                    CoreTaskLinkAction::Open => TaskLinkAction::Open,
                    CoreTaskLinkAction::Complete => TaskLinkAction::Complete,
                    CoreTaskLinkAction::Snooze { span } => TaskLinkAction::Snooze(span),
                },
            ),
            CoreDeepLink::Focus { id } => Self::Focus(id),
        }
    }
}

/// Parse a `sunrise://` link, or `None` for a shape no client acts on.
#[uniffi::export]
#[must_use]
pub fn parse_deep_link(url: String) -> Option<CoreDeepLink> {
    DeepLink::parse(&url).map(CoreDeepLink::from)
}

/// The link's URL, for a notification's payload.
#[uniffi::export]
#[must_use]
pub fn deep_link_url(link: CoreDeepLink) -> String {
    DeepLink::from(link).to_url()
}

/// The screen the link lands on, or `None` for an operation that opens none.
#[uniffi::export]
#[must_use]
pub fn deep_link_screen(link: CoreDeepLink) -> Option<LinkScreen> {
    DeepLink::from(link).screen()
}

/// The entity the link asks to be shown on that screen.
#[uniffi::export]
#[must_use]
pub fn deep_link_reveal(link: CoreDeepLink) -> Option<EntityRef> {
    DeepLink::from(link).reveal()
}

/// See [`sunrise_client_core::share::ImageRefusal`].
#[uniffi::remote(Enum)]
pub enum ImageRefusal {
    Empty,
    TooLarge,
}

/// See [`sunrise_client_core::share::ShareDraft`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ShareDraft {
    /// The task's title.
    pub title: String,
    /// Its note; empty for none.
    pub body: Vec<NoteBlock>,
}

/// The Inbox task a share becomes. Not run through the capture parser: a
/// shared paragraph is somebody else's prose.
#[uniffi::export]
#[must_use]
pub fn share_draft(
    text: Option<String>,
    url: Option<String>,
    image_count: u32,
    refused: Vec<String>,
) -> ShareDraft {
    let draft = share::share_draft(
        text.as_deref(),
        url.as_deref(),
        usize::try_from(image_count).unwrap_or(usize::MAX),
        &refused,
    );
    ShareDraft {
        title: draft.title,
        body: draft
            .body
            .into_iter()
            .map(|b: note_body::NoteBlock| NoteBlock::from(b))
            .collect(),
    }
}

/// The refusal an image of `size` bytes meets every time, or `None`.
#[uniffi::export]
#[must_use]
pub fn share_image_refusal(size: u64) -> Option<ImageRefusal> {
    share::image_refusal(size)
}

/// The note sentence for a refused image.
#[uniffi::export]
#[must_use]
pub fn share_refusal_notice(name: String, refusal: ImageRefusal) -> String {
    share::refusal_notice(&name, refusal)
}

/// The name an image is attached under, fitted to the core's bound.
#[uniffi::export]
#[must_use]
pub fn share_attachment_name(name: String) -> String {
    share::attachment_name(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exports_lower_the_client_layer_unchanged() {
        let link = parse_deep_link(
            "sunrise://task/tsk_01ARZ3NDEKTSV4RRFFQ69G5FAV?action=snooze_1h".into(),
        )
        .expect("a task link");
        assert_eq!(deep_link_screen(link.clone()), None);
        assert_eq!(deep_link_reveal(link.clone()), None);
        assert_eq!(parse_deep_link(deep_link_url(link.clone())), Some(link));
        let draft = share_draft(Some("Title\nMore".into()), None, 0, vec![]);
        assert_eq!(draft.title, "Title");
        assert_eq!(draft.body.len(), 1);
        assert_eq!(share_image_refusal(0), Some(ImageRefusal::Empty));
        assert_eq!(share_attachment_name("  a.png ".into()), "a.png");
    }
}
