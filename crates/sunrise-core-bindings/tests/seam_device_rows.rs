//! A `DeviceRow` lowers into the `DeviceListRow` a client renders, flag for
//! flag, driven through `From<&DeviceRow>` rather than through a vault.
//!
//! The other half of the device list, that the row is read from a vault and
//! carries an id `RevokeDevice` takes, is in `seam.rs`
//! (`every_device_row_carries_the_readmission_signal`).
//!
//! The report a withdrawal of a revocation returns, which a device list acts
//! on, lowers here too.

use sunrise_core::{CommandResult, DeviceRow, Withdrawal, WithdrawalOutcome};
use sunrise_core_bindings::dto::{CommandOutcome, DeviceListRow, WithdrawalOutcomeKind};
use sunrise_id::{EntityKind, EntityRef};

/// **`read_bounded: true, revoked: false` crosses the seam as itself.**
///
/// The unwind is the one state migration 0028 makes expressible and the one a
/// user is most likely to be misled by: the account no longer calls the device
/// revoked, and it receives no key this vault mints. A lowering that dropped
/// `read_bounded`, or folded it into `revoked`, would put that device back on
/// the list as an ordinary member — the outcome the field was added to prevent
/// — and no existing seam assertion would notice, because every other case
/// carries it as `false`.
///
/// All four flags are set to a combination no default produces, so a mapping
/// that returned a constant fails here rather than in an app. **Crossed wires
/// need the second row and cannot be caught by this one**, and the reason is
/// counting rather than effort: four booleans admit six pairs, and any
/// assignment of four booleans repeats at least one value, so some pair is
/// always equal and a lowering that swapped that pair would satisfy every
/// assertion here. This row separates four of the six —
/// `(revoked, read_bounded)`, `(revoked, current)`, `(read_bounded,
/// admitted_after_revocation)` and `(current, admitted_after_revocation)` —
/// and leaves `(revoked, admitted_after_revocation)`, both `false`, and
/// `(read_bounded, current)`, both `true`.
///
/// `a_plainly_revoked_device_row_crosses_the_seam_as_itself` carries the row
/// that separates those two, and the property the **pair** establishes is the
/// one the docstring used to claim alone: no swap of two of `DeviceListRow`'s
/// four flags leaves both tests' assertions satisfied.
#[test]
fn an_unwound_device_row_crosses_the_seam_as_itself() {
    let row = DeviceRow {
        device_id: [0xc1; 16],
        nickname: "Old laptop".into(),
        platform: "macos".into(),
        revoked: false,
        read_bounded: true,
        current: true,
        admitted_after_revocation: false,
        revoked_by_authority: false,
        withdrawable: false,
    };

    let lowered = DeviceListRow::from(&row);

    assert!(
        lowered.read_bounded,
        "the bound is the durable half of revocation and must reach the client"
    );
    assert!(
        !lowered.revoked,
        "and it must not be folded into `revoked`, which is the register and \
         has stopped naming this device"
    );
    assert!(lowered.current);
    assert!(!lowered.admitted_after_revocation);
    assert_eq!(lowered.device_id, "c1".repeat(16));
    assert_eq!(lowered.nickname, "Old laptop");
    assert_eq!(lowered.platform, "macos");
}

/// **The plainly revoked row, which separates the two pairs the unwind cannot.**
///
/// `an_unwound_device_row_crosses_the_seam_as_itself` leaves `(revoked,
/// admitted_after_revocation)` and `(read_bounded, current)` equal, so a
/// lowering that swapped either pair passes it. This row is `revoked: true,
/// read_bounded: true, current: false, admitted_after_revocation: false`,
/// which separates both, and together the two tests admit no swap of any two
/// of the four flags.
///
/// Every flag here is the state the engine actually produces for an ordinary
/// revocation rather than a combination chosen to make the count work.
/// `revoked: true, read_bounded: true` is the invariant `DeviceRow::read_bounded`'s
/// own doc states — "`read_bounded: false, revoked: true` cannot happen: the
/// fold takes the bound over the register it is about to write" — so the
/// revoked set is always a subset of the bounded one. `current: false` is what
/// `sunrise-core`'s `engine::query` device-list statement yields from
/// `d.identity_id = ?1` once the revocation has rotated the account identity
/// away from the device. And
/// `admitted_after_revocation: false` is the ordinary member's value: the
/// column records how the row was *first seen*, which for a device revoked
/// later has nothing to do with the revocation.
#[test]
fn a_plainly_revoked_device_row_crosses_the_seam_as_itself() {
    let row = DeviceRow {
        device_id: [0xa2; 16],
        nickname: "Retired phone".into(),
        platform: "ios".into(),
        revoked: true,
        read_bounded: true,
        current: false,
        admitted_after_revocation: false,
        revoked_by_authority: false,
        withdrawable: false,
    };

    let lowered = DeviceListRow::from(&row);

    assert!(
        lowered.revoked,
        "the register names this device and the client must be able to say so"
    );
    assert!(
        lowered.read_bounded,
        "and the bound must not be folded into `revoked`: they are two tables \
         since migration 0028 and this row is the case where they agree"
    );
    assert!(
        !lowered.current,
        "a revocation rotates the identity away from the device, so `current` \
         must not be crossed with `read_bounded`"
    );
    assert!(
        !lowered.admitted_after_revocation,
        "how the row was first seen is independent of its revocation, so this \
         must not be crossed with `revoked`"
    );
    assert_eq!(lowered.device_id, "a2".repeat(16));
    assert_eq!(lowered.nickname, "Retired phone");
    assert_eq!(lowered.platform, "ios");
}

/// **A device revoked by the account identity crosses the seam as one.**
///
/// The row is `a_plainly_revoked_device_row_crosses_the_seam_as_itself`'s
/// with one flag changed, so the pair pins `revoked_by_authority` to the core's
/// value rather than to a constant: a lowering that returned `false` fails
/// here, and one that returned `true` fails there. It also cannot be crossed
/// with any of the other four, because each of them holds the same value in
/// both rows while this one does not.
///
/// The distinction is ADR-0058 §7's whole ask of the device list: a
/// revocation nothing can unwind, beside one a later op can.
#[test]
fn a_device_revoked_by_authority_crosses_the_seam_as_one() {
    let row = DeviceRow {
        device_id: [0xa3; 16],
        nickname: "Stolen laptop".into(),
        platform: "macos".into(),
        revoked: true,
        read_bounded: true,
        current: false,
        admitted_after_revocation: false,
        revoked_by_authority: true,
        withdrawable: false,
    };

    let lowered = DeviceListRow::from(&row);

    assert!(
        lowered.revoked_by_authority,
        "a client that received `false` here would describe a revocation \
         nothing can unwind as one a later op can"
    );
    assert!(lowered.revoked);
    assert!(lowered.read_bounded);
    assert!(!lowered.current);
    assert!(!lowered.admitted_after_revocation);
    assert_eq!(lowered.device_id, "a3".repeat(16));
    assert!(!lowered.withdrawable);
}

/// **A row this device may withdraw its revocation of crosses the seam as
/// one** (ADR-0056 §6).
///
/// The row is `a_plainly_revoked_device_row_crosses_the_seam_as_itself`'s
/// with `withdrawable` set, so the pair pins the flag to the core's value: a
/// lowering that returned a constant fails one of the two.
#[test]
fn a_withdrawable_device_row_crosses_the_seam_as_one() {
    let row = DeviceRow {
        device_id: [0xa4; 16],
        nickname: "Retired phone".into(),
        platform: "ios".into(),
        revoked: true,
        read_bounded: true,
        current: false,
        admitted_after_revocation: false,
        revoked_by_authority: false,
        withdrawable: true,
    };

    let lowered = DeviceListRow::from(&row);

    assert!(
        lowered.withdrawable,
        "the client offers \"Withdraw revocation\" only where this is true"
    );
    assert!(lowered.revoked);
    assert!(!lowered.revoked_by_authority);
}

/// **What a withdrawal did crosses the seam whole**, and every other
/// command's outcome carries none (ADR-0056 §6). The three flags hold three
/// different values across the two reports, so a lowering that crossed any
/// two of them, or mapped one to a constant, fails here.
#[test]
fn a_withdrawal_report_crosses_the_seam_whole() {
    let base = || {
        CommandResult::new(
            EntityRef::new(EntityKind::Device, [0x11; 16]),
            None,
            [0x22; 16],
            7,
        )
    };
    assert!(CommandOutcome::from(&base()).withdrawal.is_none());

    let back = CommandOutcome::from(&base().with_withdrawal(Withdrawal {
        outcome: WithdrawalOutcome::Reinstated,
        read_bound_released: true,
        keys_resent: 4,
        relay_intent_cancelled: false,
    }))
    .withdrawal
    .expect("a withdrawal reports");
    assert_eq!(back.outcome, WithdrawalOutcomeKind::Reinstated);
    assert!(back.read_bound_released && !back.relay_intent_cancelled);
    assert_eq!(back.keys_resent, 4);

    let claimed = CommandOutcome::from(&base().with_withdrawal(Withdrawal {
        outcome: WithdrawalOutcome::StillClaimed,
        read_bound_released: false,
        keys_resent: 0,
        relay_intent_cancelled: true,
    }))
    .withdrawal
    .expect("a withdrawal reports");
    assert_eq!(claimed.outcome, WithdrawalOutcomeKind::StillClaimed);
    assert!(!claimed.read_bound_released && claimed.relay_intent_cancelled);

    for (core, seam) in [
        (
            WithdrawalOutcome::NotAuthor,
            WithdrawalOutcomeKind::NotAuthor,
        ),
        (
            WithdrawalOutcome::RepairRequired,
            WithdrawalOutcomeKind::RepairRequired,
        ),
    ] {
        assert_eq!(WithdrawalOutcomeKind::from(core), seam);
    }
}
