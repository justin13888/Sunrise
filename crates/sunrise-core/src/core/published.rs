//! The change events [`Core::submit`](super::Core::submit) publishes for a
//! command it applied.

use crate::commands::{Command, CommandResult};
use crate::events::DomainEvent;

/// What `cmd`, which returned `res`, changed, in the order subscribers see
/// it: the result's own entity first.
///
/// A write whose effect the command does not determine names its events on
/// the result ([`CommandResult::events`]), and those are taken as they are.
/// Otherwise they are read off the command.
pub(super) fn published(cmd: &Command, res: &CommandResult) -> Vec<DomainEvent> {
    if !res.events.is_empty() {
        return res.events.clone();
    }
    let first = match cmd {
        Command::CreateTask(_)
        | Command::CreateStream(_)
        | Command::CreateContext(_)
        | Command::CreateRoutine(_)
        | Command::CreateBlock(_)
        | Command::CreateSavedView(_)
        | Command::MergeBlocks { .. }
        | Command::AttachFile(_)
        // A focus session is created, never mutated: `StartFocus` mints a
        // new `fcs_` entity, and `EndFocus` appends a separate record to
        // the same id (so it reads as an update of the session view).
        | Command::StartFocus(_) => DomainEvent::Created(res.entity),
        Command::DeleteTask(_)
        | Command::DeleteStream { .. }
        | Command::DeleteContext(_)
        | Command::DeleteRoutine(_)
        | Command::DeleteBlock {
            scope: sunrise_domain::EditScope::All,
            ..
        }
        | Command::DeleteSavedView(_)
        | Command::DetachFile(_) => DomainEvent::Deleted(res.entity),
        _ => DomainEvent::Updated(res.entity),
    };
    // A command that writes a group touches more than its result names.
    let also: Vec<DomainEvent> = match cmd {
        Command::MergeBlocks { a, b } => {
            vec![DomainEvent::Deleted(*a), DomainEvent::Deleted(*b)]
        }
        Command::BulkTasks(items) => items
            .iter()
            .skip(1)
            .map(|i| DomainEvent::Updated(i.id))
            .collect(),
        Command::MoveTasks { ids, .. } | Command::Triage { ids, .. } => ids
            .iter()
            .skip(1)
            .map(|id| DomainEvent::Updated(*id))
            .collect(),
        _ => Vec::new(),
    };
    std::iter::once(first).chain(also).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_domain::EditScope;
    use sunrise_id::{EntityKind, EntityRef};

    fn block(n: u8) -> EntityRef {
        let mut bytes = [0u8; 16];
        bytes[15] = n;
        EntityRef::new(EntityKind::Block, bytes)
    }

    fn delete(scope: EditScope) -> Command {
        Command::DeleteBlock {
            id: block(1),
            scope,
        }
    }

    /// The events a write names are published as they are, in place of what
    /// its command reads as; a write that names none publishes the command's.
    #[test]
    fn a_result_that_names_its_events_overrides_the_command() {
        let this_and_future = || EditScope::ThisAndFuture {
            key: "2026-03-18T09:00".into(),
        };
        let ended = CommandResult::new(block(1), None, [0; 16], 1);
        assert_eq!(
            published(&delete(this_and_future()), &ended),
            vec![DomainEvent::Updated(block(1))]
        );
        let tombstoned = ended
            .clone()
            .with_events(vec![DomainEvent::Deleted(block(1))]);
        assert_eq!(
            published(&delete(this_and_future()), &tombstoned),
            vec![DomainEvent::Deleted(block(1))]
        );
        assert_eq!(
            published(&delete(EditScope::All), &ended),
            vec![DomainEvent::Deleted(block(1))]
        );

        let split = CommandResult::new(block(2), None, [0; 16], 2).with_events(vec![
            DomainEvent::Created(block(2)),
            DomainEvent::Updated(block(1)),
        ]);
        let edit = Command::UpdateBlock {
            id: block(1),
            scope: this_and_future(),
            patch: sunrise_domain::BlockPatch::default(),
        };
        assert_eq!(
            published(&edit, &split),
            vec![
                DomainEvent::Created(block(2)),
                DomainEvent::Updated(block(1))
            ]
        );
    }

    /// A merge still names its union and both blocks it tombstoned.
    #[test]
    fn a_merge_publishes_the_union_and_both_tombstones() {
        let res = CommandResult::new(block(3), None, [0; 16], 3);
        assert_eq!(
            published(
                &Command::MergeBlocks {
                    a: block(1),
                    b: block(2)
                },
                &res
            ),
            vec![
                DomainEvent::Created(block(3)),
                DomainEvent::Deleted(block(1)),
                DomainEvent::Deleted(block(2)),
            ]
        );
    }
}
