import Foundation
import Testing

@testable import Sunrise

/// The adapter between the generated string catalog and the app.
///
/// The drift gate in `packages/sunrise-i18n/test/` pins what the generated
/// files say. What these pin is that the app *reaches* them: that the String
/// Catalog is compiled into the bundle (an accessor that returned its own key
/// would mean it is not), that a plural substitution selects its arm through
/// Foundation, and that an argument is substituted verbatim — a `%` in a
/// device's nickname included.
struct StringCatalogTests {
    @Test
    func accessorsResolveFromTheCompiledCatalog() {
        #expect(L10n.Devices.title == "Devices")
        #expect(L10n.Common.productName == "Sunrise")
    }

    @Test
    func pluralsSelectTheirArm() {
        #expect(L10n.Devices.unwound(count: 1).contains("one other device, because"))
        #expect(L10n.Devices.unwound(count: 3).contains("3 other devices, because"))
        #expect(L10n.Devices.removedSomeUnrotated(count: 1).contains("every Stream key but one."))
        #expect(L10n.Devices.removedSomeUnrotated(count: 2).contains("every Stream key but 2."))
    }

    @Test
    func argumentsAreSubstitutedVerbatim() {
        #expect(L10n.Devices.removeConfirmTitle(name: "100% Mac") == "Remove 100% Mac?")
        #expect(
            L10n.Devices.gatedReason(device: "Mac", name: "Phone")
                .hasPrefix("This Mac has itself been removed")
        )
    }

    @Test
    func anEmptyNicknameGetsTheUnnamedConfirmTitle() {
        #expect(DeviceListSection.removeConfirmTitle(nickname: "") == "Remove this device?")
        #expect(DeviceListSection.removeConfirmTitle(nickname: nil) == "Remove this device?")
        #expect(DeviceListSection.removeConfirmTitle(nickname: "Mac") == "Remove Mac?")
    }

    @Test
    func anEmptyNicknameGetsTheUnnamedWithdrawTitle() {
        let unnamed = "Withdraw your removal of this device?"
        #expect(DeviceListSection.withdrawConfirmTitle(nickname: "") == unnamed)
        #expect(DeviceListSection.withdrawConfirmTitle(nickname: nil) == unnamed)
        #expect(DeviceListSection.withdrawConfirmTitle(nickname: "Mac") == "Withdraw your removal of Mac?")
    }

    /// ADR-0056 §6's three warnings, each a paragraph, in its order: keys,
    /// other removals, relay.
    @Test
    func theWithdrawMessageGivesTheThreeWarningsInOrder() {
        let paragraphs = DeviceListSection.withdrawConfirmMessage(device: "Mac")
            .components(separatedBy: "\n\n")
        #expect(paragraphs == [
            L10n.Devices.withdrawConfirmKeys(device: "Mac"),
            L10n.Devices.withdrawConfirmOthers,
            L10n.Devices.withdrawConfirmRelay
        ])
        #expect(paragraphs.first?.contains("every key this Mac holds") == true)
        #expect(paragraphs.last?.contains("relay") == true)
    }
}
