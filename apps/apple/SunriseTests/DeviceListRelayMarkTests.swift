import Foundation
import Testing

@testable import Sunrise

/// The device row's relay mark (ADR-0056 §5, #1332).
///
/// The drain records the relay's refusal of a reinstatement after the
/// withdrawal's report was shown, so the row is the one place a user sees it,
/// and with it that pairing the device again is the remedy.
@MainActor
struct DeviceListRelayMarkTests {
    /// A refused reinstatement marks the row with the remedy, a queued one
    /// without it, and a clear row carries no relay mark.
    @Test
    func aDeviceRowSaysWhereTheRelaysReinstatementStands() {
        #expect(
            Self.row(relay: .refused).relayMark(device: "Mac")
                == L10n.Devices.markRelayRefused(device: "Mac")
        )
        #expect(
            Self.row(relay: .pending).relayMark(device: "Mac")
                == L10n.Devices.markRelayQueued(device: "Mac")
        )
        #expect(Self.row(relay: .clear).relayMark(device: "Mac") == nil)
        #expect(
            L10n.Devices.markRelayRefused(device: "Mac").lowercased().contains("pair it again"),
            "a refused reinstatement names the remedy"
        )
        #expect(
            !L10n.Devices.markRelayQueued(device: "Mac").lowercased().contains("pair it again"),
            "a reinstatement not yet refused is no reason to pair again"
        )
    }

    /// What the read answers reaches the row, and a failed read is `clear`,
    /// so the list never marks a request or a refusal it did not see.
    @Test
    func aFailedReadMarksNothing() async {
        struct ReadFailed: Error {}
        #expect(await DeviceListModel.relayOnRow { .refused } == .refused)
        #expect(await DeviceListModel.relayOnRow { throw ReadFailed() } == .clear)
    }

    /// The refresh reads each row's state over the real seam: a fresh vault's
    /// own device has nothing to reinstate.
    @Test
    func theRefreshReadsEachRowsRelayState() async throws {
        let vault = try await TestVault()
        let model = DeviceListModel(bridge: vault.bridge)
        await model.refresh()

        #expect(model.errorMessage == nil)
        let me = try #require(model.rows.first)
        #expect(me.relay == .clear)
        #expect(me.relayMark(device: "Mac") == nil)
        await vault.bridge.shutdown()
    }

    private static func row(relay: RelayReinstatementKind) -> DeviceListModel.DeviceRow {
        DeviceListModel.DeviceRow(
            id: "dev_00000000000000000000000002",
            deviceID: String(repeating: "0", count: 31) + "2",
            nickname: "Old laptop",
            platform: "macos",
            revoked: false,
            readBounded: false,
            current: true,
            admittedAfterRevocation: false,
            revokedByAuthority: false,
            withdrawable: false,
            isThisDevice: false,
            relay: relay
        )
    }
}
