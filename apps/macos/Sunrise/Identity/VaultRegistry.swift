import Foundation

/// One vault this Mac knows about.
///
/// The `id` is doing three jobs at once, and that is deliberate: it is the
/// Keychain account under `KeychainVaultRootStore`, the directory leaf under
/// `Application Support/Sunrise/vaults/`, and the registry's key. One
/// identifier means a vault cannot end up with its key filed under one name
/// and its data under another — which is the failure that looks exactly like a
/// lost vault.
struct VaultDescriptor: Codable, Equatable, Sendable, Identifiable {
    /// Stable, generated, and never derived from the name — renaming a vault
    /// must not move its data or orphan its key.
    let id: String
    /// What the user calls it. Display only.
    var name: String
}

/// The vaults on this Mac, and which one is open.
///
/// `UserDefaults`, not the vault: this is the list you have to be able to read
/// *before* any vault is open, including on the screen that exists because no
/// vault would open. Nothing here is secret — the names, not the keys — and
/// the keys stay in the Keychain under `id`.
///
/// The first vault keeps the id `default`, which is what
/// `KeychainVaultRootStore(vaultName:)` has always defaulted to and what
/// `VaultLocation.standard()` has always pointed at. An existing single-vault
/// install therefore appears here as one already-registered vault rather than
/// as a vault the app has forgotten.
@MainActor
@Observable
final class VaultRegistry {
    /// The id the app used before it could hold more than one vault.
    ///
    /// `nonisolated` because `VaultLocation` — which is not main-actor bound,
    /// and must not be: it is read on the way to opening a vault — has to name
    /// the same constant.
    nonisolated static let firstVaultID = "default"

    private(set) var vaults: [VaultDescriptor]
    private(set) var selectedID: String

    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        let stored = defaults.data(forKey: Key.vaults).flatMap {
            try? JSONDecoder().decode([VaultDescriptor].self, from: $0)
        }
        // An empty or unreadable list is not an empty Mac: the pre-registry
        // vault is at the `default` id whether or not anything ever wrote this
        // key, so the fallback has to name it rather than start from nothing.
        let known = (stored?.isEmpty == false) ? (stored ?? []) : [Self.firstVault]
        let wanted = defaults.string(forKey: Key.selected) ?? Self.firstVaultID
        vaults = known
        selectedID = known.contains { $0.id == wanted }
            ? wanted
            : (known.first?.id ?? Self.firstVaultID)
    }

    /// The vault the app should open. Never `nil`: the list cannot be emptied.
    var selected: VaultDescriptor {
        vaults.first { $0.id == selectedID } ?? Self.firstVault
    }

    /// Register a vault and return it. Does **not** select it — opening is the
    /// session's business, because opening means closing something first.
    @discardableResult
    func add(name: String) -> VaultDescriptor {
        let trimmed = name.trimmed
        let descriptor = VaultDescriptor(
            id: UUID().uuidString.lowercased(),
            name: trimmed.isEmpty ? Self.defaultName(index: vaults.count + 1) : trimmed
        )
        vaults.append(descriptor)
        persist()
        return descriptor
    }

    /// Record which vault is open. Ignores an id this registry does not know,
    /// so a stale selection cannot point the app at a directory with no entry.
    func select(_ id: String) {
        guard vaults.contains(where: { $0.id == id }) else { return }
        selectedID = id
        persist()
    }

    func rename(_ id: String, to name: String) {
        let trimmed = name.trimmed
        guard !trimmed.isEmpty, let index = vaults.firstIndex(where: { $0.id == id }) else { return }
        vaults[index].name = trimmed
        persist()
    }

    /// Stop listing a vault.
    ///
    /// Deliberately not a delete: the directory stays, the Keychain item
    /// stays, and re-registering the same id would open it again. This screen
    /// is reachable by someone who has just mistyped something, and the
    /// version of this button that removed the data would be the one that made
    /// this app's central promise a lie.
    ///
    /// Refuses the open vault and refuses the last one, because neither leaves
    /// the app anywhere to go.
    @discardableResult
    func forget(_ id: String) -> Bool {
        guard id != selectedID, vaults.count > 1,
              let index = vaults.firstIndex(where: { $0.id == id }) else { return false }
        vaults.remove(at: index)
        persist()
        return true
    }

    private static var firstVault: VaultDescriptor {
        VaultDescriptor(id: firstVaultID, name: "My vault")
    }

    private static func defaultName(index: Int) -> String { "Vault \(index)" }

    private func persist() {
        defaults.set(try? JSONEncoder().encode(vaults), forKey: Key.vaults)
        defaults.set(selectedID, forKey: Key.selected)
    }

    private enum Key {
        static let vaults = "vaults.known"
        static let selected = "vaults.selected"
    }
}
