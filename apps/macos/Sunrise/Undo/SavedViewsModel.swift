import Foundation

/// Named views, saved per device.
///
/// The file is `~/.config/sunrise/views.toml` — the same one `sunrise-cli`
/// reads — so a view saved here is recalled there. The parsing, the spec
/// format and the one-line summary are all `sunrise-client-core`'s; this model
/// holds the rows and turns one into somewhere to be.
///
/// Contexts are carried by **name**, which is the store's decision and a load-
/// bearing one: an `EntityRef` is a vault-local ULID, so a view saved on one
/// machine and read on a paired one would resolve to an empty set and look
/// like an empty vault. The cost is that a name can stop existing, which is
/// reported on recall rather than quietly filtering everything away.
@MainActor
@Observable
final class SavedViewsModel {
    private(set) var views: [SavedView] = []
    /// One line per view that could not be read. A typo costs that view and
    /// nothing else.
    private(set) var warnings: [String] = []
    private(set) var errorMessage: String?
    /// Why the last recall could not be honoured in full — a context name the
    /// vault no longer has. Not an error: the view still opens.
    private(set) var recallNote: String?

    private let store: SavedViews

    init(store: SavedViews = SavedViews.atDefaultPath()) {
        self.store = store
    }

    /// Where the file is, for the settings screen to show.
    var location: String? { store.location() }

    func load() async {
        let file = await Self.read(store)
        views = file.views
        warnings = file.warnings
    }

    /// Save `destination` under `name`, replacing any view of that name.
    ///
    /// Every destination the sidebar offers can be named by the store — see
    /// ``Destination/primary`` — so the only thing refused here is an empty
    /// name, which would produce a view nobody could pick out of the menu.
    func save(name: String, destination: Destination, query: String, contexts: [String]) async {
        let trimmed = name.trimmed
        guard !trimmed.isEmpty else { return }
        let view = SavedView(
            name: trimmed,
            view: destination.primary,
            query: destination.savesQuery ? query : "",
            contexts: contexts,
            // Written by the store; whatever is passed here is overwritten on
            // the next read, so an empty string is the honest placeholder.
            summary: ""
        )
        var next = views.filter { $0.name != trimmed }
        next.append(view)
        next.sort { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
        await write(next)
    }

    func delete(_ view: SavedView) async {
        await write(views.filter { $0.name != view.name })
    }

    /// Where a saved view points, and what it narrows to.
    ///
    /// Context names are resolved against the vault here rather than at save
    /// time. A name that no longer exists is reported and dropped from the
    /// filter — showing an empty list because of an invisible filter is the
    /// failure the name-based storage exists to avoid, and reporting it is the
    /// other half of that bargain.
    func recall(_ view: SavedView, contexts book: NameBook) -> Destination {
        let resolved = view.contexts.compactMap { name in
            book.contexts.first { $0.value.caseInsensitiveCompare(name) == .orderedSame }?.key
        }
        let missing = view.contexts.count - resolved.count
        recallNote = missing == 0
            ? nil
            : "\(missing) of this view's contexts no longer exist and were not applied."

        return destination(for: view.view, contexts: resolved, book: book)
    }

    /// One primary view, as somewhere to be.
    ///
    /// Split out from ``recall(_:contexts:)`` rather than inlined: the store
    /// now names ten views, and a function that both resolved context names
    /// and branched ten ways was over the complexity the linter allows.
    private func destination(
        for view: PrimaryView,
        contexts resolved: [EntityRef],
        book: NameBook
    ) -> Destination {
        switch view {
        case .today: .list(.today(contexts: resolved))
        case .inbox: .list(.inbox)
        case .stream: browse(contexts: resolved, book: book)
        case .search: .search
        case .calendar: .calendar
        case .focus: .focus
        case .routines: .routines
        case .review: .review
        case .morning: .morning
        case .evening: .evening
        }
    }

    /// A saved "browse" view carries context names and no stream id — a stream
    /// id is a vault-local ULID and would not survive the trip. So it recalls
    /// the first context it names, and Today when it names none.
    private func browse(contexts resolved: [EntityRef], book: NameBook) -> Destination {
        guard let first = resolved.first,
              let name = book.contexts[first] else { return .list(.todayAll) }
        return .list(.context(id: first, name: name))
    }

    func dismissRecallNote() { recallNote = nil }

    private func write(_ next: [SavedView]) async {
        do {
            try await Self.persist(store, next)
            await load()
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    // The store's methods are synchronous file IO. The file is a few hundred
    // bytes, but it is still a disk touch, so it runs off the main actor.
    nonisolated private static func read(_ store: SavedViews) async -> SavedViewFile {
        await _Concurrency.Task.detached { store.load() }.value
    }

    nonisolated private static func persist(
        _ store: SavedViews,
        _ views: [SavedView]
    ) async throws {
        try await _Concurrency.Task.detached { try store.save(viewsToWrite: views) }.value
    }
}

extension Destination {
    /// Which primary view this destination is, in the saved-view store's
    /// vocabulary.
    ///
    /// Total, and that is the point. `PrimaryView` mirrors
    /// `sunrise_client_core::views::View` variant for variant, deliberately,
    /// so that adding a view upstream fails the seam's build rather than
    /// producing an unrepresentable value — which is how `Morning` and
    /// `Evening` announced themselves. Now that the store names all ten, every
    /// destination the sidebar offers can be saved, and the menu item that used
    /// to grey out on the two briefs has nothing left to grey out for.
    var primary: PrimaryView {
        switch self {
        case let .list(kind):
            switch kind {
            case .today: .today
            case .inbox: .inbox
            case .stream, .context: .stream
            case .search: .search
            }
        case .search: .search
        case .calendar: .calendar
        case .focus: .focus
        case .routines: .routines
        case .review: .review
        case .morning: .morning
        case .evening: .evening
        }
    }

    /// Whether a saved view of this destination should carry search text.
    var savesQuery: Bool { primary == .search }
}
