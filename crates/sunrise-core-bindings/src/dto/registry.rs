//! Registry coverage: every synced record names its mirror.

use sunrise_domain::{
    Attachment, Block, Context, FocusEnd, FocusStart, Interruption, Preferences, ReviewSnapshot,
    Routine, Stream, Task, TaskTemplate,
};

use super::{
    AttachmentItem, BlockItem, ContextItem, InterruptionRow, PreferencesItem, RoutineItem,
    SessionEnd, SessionStart, Snapshot, StreamItem, TaskItem, Template,
};

/// A domain record's foreign mirror.
///
/// The exhaustive destructuring in each mirror's `From` impl catches a new
/// *field*; this catches a new *record*. Every record of every entity the
/// registry ([`sunrise_id::for_each_entity!`]) syncs must name its mirror
/// here, and the mirror must convert from it, or this crate does not build.
/// The mirror itself stays hand-written for the reasons the [`crate::dto`]
/// module docs give.
pub(crate) trait Mirrored {
    /// The `UniFFI` record that mirrors this domain record.
    type Dto: for<'a> From<&'a Self>;
}

impl Mirrored for Task {
    type Dto = TaskItem;
}
impl Mirrored for Stream {
    type Dto = StreamItem;
}
impl Mirrored for Context {
    type Dto = ContextItem;
}
impl Mirrored for Routine {
    type Dto = RoutineItem;
}
impl Mirrored for TaskTemplate {
    type Dto = Template;
}
impl Mirrored for Block {
    type Dto = BlockItem;
}
impl Mirrored for Attachment {
    type Dto = AttachmentItem;
}
impl Mirrored for FocusStart {
    type Dto = SessionStart;
}
impl Mirrored for FocusEnd {
    type Dto = SessionEnd;
}
impl Mirrored for Interruption {
    type Dto = InterruptionRow;
}
impl Mirrored for ReviewSnapshot {
    type Dto = Snapshot;
}
impl Mirrored for Preferences {
    type Dto = PreferencesItem;
}

// The saved view's mirror lives in `saved_views.rs`.
#[allow(unused_imports)]
use sunrise_domain::SavedView;

/// Requires [`Mirrored`] of every record of every entity that syncs.
/// `Unsynced` entities reach no foreign caller; `Control` ones have no record.
macro_rules! require_mirrors {
    (@merge Unsynced $($record:ident)*) => {};
    (@merge $merge:ident $($record:ident)*) => {
        $( mirrored::<$record>(); )*
    };
    (
        $(
            $(#[$kind_meta:meta])*
            $kind:ident {
                prefix: $prefix:literal,
                tag: $tag:literal,
                merge: $merge:ident,
                owner: $owner:ident $(($owner_field:literal))?,
                features: [$($feature:literal),* $(,)?],
                ops: [
                    $(
                        $(#[$op_meta:meta])*
                        $op:ident($payload:ty) = $inner_kind:literal, $class:ident, $target:ident;
                    )*
                ],
                records: [
                    $(
                        $record:ident @ $storage:tt {
                            $(
                                $field:ident $(as $wire:literal)?: $field_ty:ty => $crdt:ident $(($conflict:ident))? $(= $default:literal)?;
                            )*
                            $(..$unknown:ident)?
                        }
                    )*
                ],
            }
        )*
    ) => {
        #[allow(dead_code)]
        fn every_synced_record_is_mirrored() {
            fn mirrored<T: Mirrored>() {}
            $( require_mirrors!(@merge $merge $($record)*); )*
        }
    };
}

sunrise_id::for_each_entity!(require_mirrors);
