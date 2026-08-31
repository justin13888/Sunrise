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

    init(bridge: CoreBridge) {
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
