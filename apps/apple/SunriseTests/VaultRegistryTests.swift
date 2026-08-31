import Foundation
import Testing

@testable import Sunrise

/// The list of vaults, and the one thing it must never do: forget where the
/// vault that already exists is filed.
@MainActor
struct VaultRegistryTests {
    private func scratchDefaults() throws -> UserDefaults {
        try #require(UserDefaults(suiteName: "sunrise-tests-\(UUID().uuidString)"))
    }

    /// A Mac that has been running Sunrise since before there was a registry
    /// has a vault at the `default` id. If this list started empty, the
    /// switcher would offer to open a vault that is not the one on disk.
    @Test
    func aMacWithNoStoredListStillKnowsAboutTheVaultItAlreadyHas() throws {
        let registry = VaultRegistry(defaults: try scratchDefaults())

        #expect(registry.vaults.count == 1)
        #expect(registry.selectedID == VaultRegistry.firstVaultID)
        #expect(registry.selected.id == "default")
    }

    @Test
    func addingAVaultDoesNotOpenIt() throws {
        let registry = VaultRegistry(defaults: try scratchDefaults())
        let added = registry.add(name: "Work")

        #expect(registry.vaults.count == 2)
        #expect(added.name == "Work")
        #expect(added.id != VaultRegistry.firstVaultID)
        #expect(
            registry.selectedID == VaultRegistry.firstVaultID,
            "opening is the session's business — it has to close something first"
        )
    }

    /// The id is the Keychain account and the directory leaf, so it must
    /// survive a rename. A name-derived id would move a vault's data every
    /// time someone corrected a typo.
    @Test
    func renamingAVaultLeavesItsIdentityAlone() throws {
        let registry = VaultRegistry(defaults: try scratchDefaults())
        let added = registry.add(name: "Wrok")
        registry.rename(added.id, to: "Work")

        #expect(registry.vaults.last?.id == added.id)
        #expect(registry.vaults.last?.name == "Work")
    }

    @Test
    func anUnknownSelectionIsIgnoredRatherThanStored() throws {
        let registry = VaultRegistry(defaults: try scratchDefaults())
        registry.select("no-such-vault")

        #expect(registry.selectedID == VaultRegistry.firstVaultID)
    }

    @Test
    func theListAndTheSelectionSurviveARestart() throws {
        let defaults = try scratchDefaults()
        let first = VaultRegistry(defaults: defaults)
        let added = first.add(name: "Work")
        first.select(added.id)

        let second = VaultRegistry(defaults: defaults)
        #expect(second.vaults.map(\.id) == first.vaults.map(\.id))
        #expect(second.selectedID == added.id)
    }

    /// Forgetting is a list operation and nothing more. Neither of these
    /// refusals is tidiness: one would leave the app with no vault to open,
    /// and the other would drop the vault out from under a live core.
    @Test
    func forgettingRefusesTheOpenVaultAndTheLastOne() throws {
        let registry = VaultRegistry(defaults: try scratchDefaults())
        #expect(registry.forget(VaultRegistry.firstVaultID) == false, "it is the only one")

        let added = registry.add(name: "Work")
        registry.select(added.id)
        #expect(registry.forget(added.id) == false, "it is the one that is open")
        #expect(registry.forget(VaultRegistry.firstVaultID))
        #expect(registry.vaults.map(\.id) == [added.id])
    }

    /// An unnamed vault still needs something on the picker.
    @Test
    func aBlankNameBecomesSomethingTheSwitcherCanShow() throws {
        let registry = VaultRegistry(defaults: try scratchDefaults())
        let added = registry.add(name: "   ")

        #expect(!added.name.trimmed.isEmpty)
    }
}

/// Where each vault's bytes go, once there is more than one of them.
struct PerVaultLocationTests {
    /// The first vault does not move. Relocating it into the multi-vault
    /// layout would be a migration whose failure mode is an unopenable vault,
    /// for tidiness nobody can see.
    @Test
    func theFirstVaultKeepsThePathItAlreadyHad() throws {
        let standard = try VaultLocation.standard()
        let byID = try VaultLocation.forVault(VaultRegistry.firstVaultID)

        #expect(standard.directory == byID.directory)
        #expect(standard.directory.lastPathComponent == "vault")
    }

    @Test
    func everyOtherVaultGetsItsOwnDirectory() throws {
        let id = UUID().uuidString.lowercased()
        let location = try VaultLocation.forVault(id)

        #expect(location.directory.lastPathComponent == id)
        #expect(location.directory.deletingLastPathComponent().lastPathComponent == "vaults")
        #expect(location.directory != (try VaultLocation.standard()).directory)
    }

    /// An id is always a generated UUID, so one that is not is a bug. Throwing
    /// says so; sanitising would quietly write a vault somewhere else.
    @Test
    func anIdentifierThatCouldClimbOutOfItsDirectoryIsRefused() {
        for bad in ["../../etc", "a/b", "", "Has Spaces", "UPPER"] {
            #expect(throws: VaultLocationError.self) {
                try VaultLocation.forVault(bad)
            }
        }
        #expect(VaultLocation.isSafeIdentifier(UUID().uuidString.lowercased()))
    }
}
