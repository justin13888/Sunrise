import Foundation

/// Every view model an open vault needs, built once against one bridge.
///
/// Extracted so the two shells are only a *layout* apart. `VaultWindow` on
/// macOS and `VaultTabs` on iOS present the same eleven models in a sidebar
/// and in a tab bar respectively; without this they would each declare the
/// same eleven `@State` properties and the same eleven constructions, and the
/// day a twelfth model arrived one of them would quietly not get it.
///
/// A class rather than a struct of `@State`s: the models are reference types
/// with their own observation, the shells hold this in a single `@State`, and
/// tearing it down and rebuilding is what a vault switch already does through
/// ``SessionModel``.
@MainActor
@Observable
final class VaultModels {
    let list: TaskListModel
    let capture: CaptureModel
    let browse: BrowseModel
    let search: SearchModel
    let calendar: CalendarModel
    let focus: FocusModel
    let routines: RoutineModel
    let review: ReviewModel
    let morning: MorningSummaryModel
    let evening: EndOfDayPlanModel
    let undo: UndoModel

    /// Per-device and vault-independent, so they are built here too rather
    /// than by each shell: settings, the signed-in account, the sync banner
    /// and the saved-view list are the same objects whichever shell is drawing
    /// them.
    let settings = AppSettings()
    let account = AccountModel()
    let sync = SyncStatusModel()
    let savedViews = SavedViewsModel()

    private let bridge: CoreBridge

    init(bridge: CoreBridge) {
        self.bridge = bridge
        list = TaskListModel(bridge: bridge)
        capture = CaptureModel(bridge: bridge)
        browse = BrowseModel(bridge: bridge)
        search = SearchModel(bridge: bridge)
        calendar = CalendarModel(bridge: bridge)
        focus = FocusModel(bridge: bridge)
        routines = RoutineModel(bridge: bridge)
        review = ReviewModel(bridge: bridge)
        morning = MorningSummaryModel(bridge: bridge)
        evening = EndOfDayPlanModel(bridge: bridge)
        undo = UndoModel(bridge: bridge)
    }
}

/// What a link that named an entity turned out to be.
///
/// Two cases because two exist: `EntityRef` covers twelve kinds, and the only
/// ones a `sunrise://entity/<id>` link is written for — by "Copy permalink"
/// (`docs/02-domain/identifiers.md`) and by a block reminder — are a Task and
/// a Block. Anything else resolves to nothing rather than to a screen picked
/// on its behalf.
enum EntityReveal: Equatable {
    case task(TaskItem)
    case block(BlockGridRow)
}

extension VaultModels {
    /// Find the entity a link named, and put the screen it lives on in front
    /// of it.
    ///
    /// The second half is the part that was missing. ``DeepLink/destination``
    /// says which screen, and for a block that is the calendar — but the
    /// calendar opens on today, so revealing the block means moving the grid
    /// onto its day first. That is why this returns after a write to
    /// ``CalendarModel``: by the time the caller has the row, the screen
    /// behind it is already showing the right day.
    ///
    /// Split on the id's prefix rather than on the query's answer, which is
    /// the split ``DeepLink`` already makes when it decides which screen an
    /// entity lives on — one question, answered the same way in both places.
    func reveal(_ id: EntityRef) async -> EntityReveal? {
        if id.hasPrefix("blk_") {
            return await calendar.reveal(id).map(EntityReveal.block)
        }
        guard case let .task(item)? = try? await bridge.query(.entityById(id: id)) else {
            return nil
        }
        return .task(item)
    }
}
