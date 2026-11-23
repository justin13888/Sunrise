import Foundation
import Testing

@testable import Sunrise

/// The feature gate's refusal of a withdrawal (ADR-0056 §7), as the device
/// list words it.
///
/// Synthetic, because the refusal needs a second device without the feature.
/// The message is the one the seam writes for
/// `BindingError::FeatureUnsupportedByDevices`, pinned on the Rust side by
/// `the_feature_gate_refusal_crosses_with_its_device_ids` in
/// `seam_device_rows.rs`; `featureRefusal` here is the generated binding of
/// the function that test reads it back with.
@MainActor
struct DeviceListWithdrawalRefusalTests {
    /// **The feature refusal.** A vault with a device that would drop the op
    /// refuses the withdrawal, with no override; the user is told how many
    /// devices to update and which, rather than the core's English. A device
    /// the list holds is named; one it does not is shown by its id. Anything
    /// else falls back to the error's own description, never to a withdrawal.
    @Test
    func aWithdrawalTheFeatureGateRefusesNamesTheDevicesToUpdate() {
        let phone = String(repeating: "ab", count: 16)
        let unknown = String(repeating: "01", count: 16)
        let refusal = BindingError.FeatureUnsupportedByDevices(
            message: "enabling `core.revoke_withdraw` needs 2 other device(s) to update first: "
                + "\(phone), \(unknown)"
        )
        let rows = [Self.row(deviceID: phone, nickname: "Phone")]
        #expect(
            DeviceListModel.withdrawalRefusal(refusal, rows: rows)
                == [
                    L10n.Devices.withdrawAwaitsUpdate(count: 2),
                    "Phone (\(phone.prefix(16)))",
                    String(unknown.prefix(16))
                ].joined(separator: "\n")
        )

        let other = BindingError.Core(
            message: "a device cannot withdraw a revocation of itself: it never made one"
        )
        #expect(DeviceListModel.withdrawalRefusal(other, rows: []) == other.localizedDescription)

        let notAWithdrawal = BindingError.FeatureUnsupportedByDevices(
            message: "enabling `core.field_ops` needs 1 other device(s) to update first: \(phone)"
        )
        #expect(
            DeviceListModel.withdrawalRefusal(notAWithdrawal, rows: [])
                == notAWithdrawal.localizedDescription,
            "only the withdrawal features' gate is a withdrawal refusal"
        )
    }

    /// **The authority feature's refusal.** Withdrawing a removal the account
    /// identity made is gated by its own feature,
    /// `core.revoke_withdraw_authority`; its refusal names what to update just
    /// as the device feature's does, not the core's English.
    @Test
    func anIdentityWithdrawalTheFeatureGateRefusesNamesTheDevicesToUpdate() {
        let ids = ["0a", "0b", "0c"].map { String(repeating: $0, count: 16) }
        let refusal = BindingError.FeatureUnsupportedByDevices(
            message: "enabling `core.revoke_withdraw_authority` needs 3 other device(s) to update first: "
                + ids.joined(separator: ", ")
        )
        #expect(
            DeviceListModel.withdrawalRefusal(refusal, rows: [])
                == ([L10n.Devices.withdrawAwaitsUpdate(count: 3)] + ids.map { String($0.prefix(16)) })
                .joined(separator: "\n")
        )
    }

    private static func row(deviceID: String, nickname: String) -> DeviceListModel.DeviceRow {
        DeviceListModel.DeviceRow(
            id: "dev_00000000000000000000000002",
            deviceID: deviceID,
            nickname: nickname,
            platform: "ios",
            revoked: false,
            readBounded: false,
            current: true,
            admittedAfterRevocation: false,
            revokedByAuthority: false,
            withdrawable: false,
            isThisDevice: false
        )
    }
}
