import Foundation
import Network

/// Settings → Storage: what this device's attachment cache holds, its limit,
/// **Clear cache**, and whether originals fetch unasked on cellular
/// (ADR-0053 §5–§6).
///
/// The cache itself is the core's. This model only reads its usage and writes
/// the two device-local preferences that steer it, `attachments.cache_limit_bytes`
/// and `attachments.auto_fetch_on_cellular`, through the same command every
/// other preference goes through.
@MainActor
@Observable
final class AttachmentCacheModel {
    /// Sealed bytes the cache holds.
    private(set) var usedBytes: UInt64 = 0
    /// What Clear cache would free.
    private(set) var evictableBytes: UInt64 = 0
    /// The limit eviction keeps the cache under.
    private(set) var limitBytes: UInt64 = 0
    /// Whether originals under the auto-fetch threshold fetch on cellular.
    private(set) var autoFetchOnCellular = false
    private(set) var errorMessage: String?

    /// The limits Settings offers, inside the preference's 100 MB to 50 GB.
    static let limitChoices: [UInt64] = [
        200_000_000, 500_000_000, 1_000_000_000, 2_000_000_000,
        5_000_000_000, 10_000_000_000, 50_000_000_000
    ]

    static let limitKey = "attachments.cache_limit_bytes"
    static let cellularKey = "attachments.auto_fetch_on_cellular"

    private let bridge: CoreBridge

    init(bridge: CoreBridge) {
        self.bridge = bridge
        // ADR-0053 §5: plaintext handed to a previewer is deleted at the next
        // launch. A vault opening is that launch.
        PreviewFiles.sweep()
    }

    /// The choices to show, with the current limit among them even when it
    /// was set to something else on this device before.
    var choices: [UInt64] {
        Self.limitChoices.contains(limitBytes) || limitBytes == 0
            ? Self.limitChoices
            : (Self.limitChoices + [limitBytes]).sorted()
    }

    static func format(_ bytes: UInt64) -> String {
        ByteCountFormatStyle(style: .file).format(Int64(clamping: bytes))
    }

    func refresh() async {
        do {
            let usage = try await bridge.attachmentCacheUsage()
            usedBytes = usage.usedBytes
            evictableBytes = usage.evictableBytes
            limitBytes = usage.limitBytes
            if case let .preferences(preferences) = try await bridge.query(.preferences),
               let cellular = preferences.first(where: { $0.key == Self.cellularKey }),
               case let .bool(value)? = cellular.value {
                autoFetchOnCellular = value
            }
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func setLimit(_ bytes: UInt64) async {
        await write(Self.limitKey, .uint(value: bytes))
        // A lowered limit takes effect now rather than at the next fetch.
        _ = try? await bridge.enforceAttachmentCache()
        await refresh()
    }

    func setAutoFetchOnCellular(_ on: Bool) async {
        await write(Self.cellularKey, .bool(value: on))
        await refresh()
    }

    func clear() async {
        do {
            _ = try await bridge.clearAttachmentCache()
        } catch {
            errorMessage = error.localizedDescription
        }
        await refresh()
    }

    private func write(_ key: String, _ value: PreferenceValue) async {
        do {
            _ = try await bridge.submit(.setPreference(key: key, value: value, target: .device))
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}

/// The class of network this device is on, as the core wants it reported:
/// constrained (Low Data Mode), expensive (cellular, a personal hotspot), or
/// neither.
///
/// One per process, because the OS has one path: every open vault's bridge is
/// told on each change.
@MainActor
@Observable
final class NetworkClassMonitor {
    static let shared = NetworkClassMonitor()

    private(set) var current: NetworkClass = .unmetered
    /// Weak, so a vault that closed is not kept alive to be told.
    private var bridges: [WeakBridge] = []
    private let monitor = NWPathMonitor()

    private init() {
        monitor.pathUpdateHandler = { path in
            let network = Self.classify(path)
            Task { @MainActor in NetworkClassMonitor.shared.update(network) }
        }
        monitor.start(queue: DispatchQueue(label: "sunrise.network-class"))
    }

    nonisolated static func classify(_ path: NWPath) -> NetworkClass {
        if path.isConstrained { return .constrained }
        if path.isExpensive { return .cellular }
        return .unmetered
    }

    /// Tell `bridge` the current class now and on every change.
    ///
    /// Called by `CoreBridge` as it makes each bridge, before anything can
    /// run the fetch drain, and not by a view: an iOS background launch
    /// (BGAppRefresh, a silent push) syncs with no scene on screen, and a
    /// bridge never told would drain as `Unmetered` on cellular or Low Data
    /// Mode (ADR-0053 §6). Awaited, so the class is set when this returns.
    func report(to bridge: CoreBridge) async {
        bridges.removeAll { $0.bridge == nil }
        bridges.append(WeakBridge(bridge: bridge))
        // The monitor's path, not `current`: the first path update reaches
        // `current` through a hop to the main actor, which a launch can beat.
        let network = Self.classify(monitor.currentPath)
        await bridge.setNetworkClass(network)
    }

    private func update(_ network: NetworkClass) {
        current = network
        for case let bridge? in bridges.map(\.bridge) {
            Task { await bridge.setNetworkClass(network) }
        }
    }

    private struct WeakBridge {
        weak var bridge: CoreBridge?
    }
}

/// Where decrypted plaintext handed to QuickLook or "Open in…" lives
/// (ADR-0053 §5): one directory per launch, a file deleted when its preview
/// closes, and everything from earlier launches deleted at the next one. None
/// of it counts as cache.
enum PreviewFiles {
    static var root: URL {
        FileManager.default.temporaryDirectory.appending(path: "sunrise-previews")
    }

    /// This launch's directory.
    static let launch: URL = root.appending(path: UUID().uuidString)

    /// Delete every earlier launch's files.
    static func sweep() {
        let fm = FileManager.default
        guard let entries = try? fm.contentsOfDirectory(at: root, includingPropertiesForKeys: nil) else {
            return
        }
        for entry in entries where entry.lastPathComponent != launch.lastPathComponent {
            try? fm.removeItem(at: entry)
        }
    }

    /// Write `bytes` under this launch's directory as `filename`, in a folder
    /// of its own so two attachments with one name do not collide.
    static func write(_ bytes: Data, id: EntityRef, filename: String) throws -> URL {
        let url = launch.appending(path: id).appending(path: filename)
        try FileManager.default.createDirectory(
            at: url.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        try bytes.write(to: url, options: .atomic)
        return url
    }

    /// Delete one preview's file and its folder.
    static func remove(_ url: URL) {
        try? FileManager.default.removeItem(at: url.deletingLastPathComponent())
    }
}
