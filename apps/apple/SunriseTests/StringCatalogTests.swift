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
}
