//! A `DeviceRow` lowers into the `DeviceListRow` a client renders, flag for
//! flag, driven through `From<&DeviceRow>` rather than through a vault.
//!
//! The other half of the device list, that the row is read from a vault and
//! carries an id `RevokeDevice` takes, is in `seam.rs`
//! (`every_device_row_carries_the_readmission_signal`).

use sunrise_core::DeviceRow;
use sunrise_core_bindings::dto::DeviceListRow;

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
