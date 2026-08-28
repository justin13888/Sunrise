import Foundation

/// The routines list, and everything that can be done to one.
///
/// `Query::Routines` returns whole routines rather than list projections, so
/// the cadence prose and the next occurrence are asked for per row — both from
/// the seam, never worded or computed here.
@MainActor
@Observable
final class RoutineModel {
    private(set) var routines: [RoutineItem] = []
    private(set) var nowMs: UInt64 = 0
    private(set) var names = NameBook()
    private(set) var errorMessage: String?
    private(set) var undoNote: String?
    /// Archived routines are hidden by default; archiving is not deletion.
    var showsArchived = false

    private let bridge: CoreBridge

    init(bridge: CoreBridge) {
        self.bridge = bridge
    }

    var visible: [RoutineItem] {
        showsArchived ? routines : routines.filter { !$0.archived }
    }

    func refresh() async {
        nowMs = await bridge.nowMs()
        names = await NameBook.load(from: bridge)
        do {
            if case let .routines(rows) = try await bridge.query(.routines) {
                routines = rows
            }
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func follow() async {
        for await batch in await bridge.changes() {
            guard !batch.isClosed else { return }
            await refresh()
        }
    }

    /// The cadence, in the domain's words — the inverse of the phrase the user
    /// typed to create it.
    func cadence(of routine: RoutineItem) -> String {
        recurrenceSummary(rule: routine.rrule)
    }

    /// When this routine next fires, as a relative day.
    ///
    /// Both halves come from the seam: the occurrence from
    /// `next_occurrence_ms`, which knows the per-frequency horizon and honours
    /// skips and pauses, and the wording from `relative_day`.
    func nextLabel(for routine: RoutineItem) -> DayLabel? {
        guard let at = nextOccurrenceMs(routine: routine, nowMs: nowMs) else { return nil }
        return DayLabel(relativeDay(
            at: .instant(at: at),
            nowMs: nowMs,
            tz: TimeZone.current.identifier
        ))
    }

    // MARK: - Writing

    func create(_ draft: RoutineDraftIn) async {
        await run(.createRoutine(draft: draft), label: "new routine “\(draft.template.title)”")
    }

    func update(_ routine: RoutineItem, _ edit: RoutineEdit) async {
        await run(
            .updateRoutine(id: routine.id, edit: edit),
            label: "edit “\(routine.template.title)”"
        )
    }

    func setPaused(_ routine: RoutineItem, _ paused: Bool) async {
        var edit = RoutineEdit()
        edit.paused = paused
        await run(
            .updateRoutine(id: routine.id, edit: edit),
            label: paused
                ? "pause “\(routine.template.title)”"
                : "resume “\(routine.template.title)”"
        )
    }

    func setArchived(_ routine: RoutineItem, _ archived: Bool) async {
        var edit = RoutineEdit()
        edit.archived = archived
        await run(
            .updateRoutine(id: routine.id, edit: edit),
            label: archived
                ? "archive “\(routine.template.title)”"
                : "unarchive “\(routine.template.title)”"
        )
    }

    func delete(_ routine: RoutineItem) async {
        await run(
            .deleteRoutine(id: routine.id),
            label: "delete “\(routine.template.title)”"
        )
    }

    /// Skip the next occurrence.
    ///
    /// The key is `YYYY-MM-DDTHH:MM` **in the routine's own timezone**, not
    /// the device's: the key is what makes a skip survive a tzdb change, and
    /// resolving it against the wrong zone would skip the wrong day.
    func skipNext(_ routine: RoutineItem) async {
        guard let at = nextOccurrenceMs(routine: routine, nowMs: nowMs) else { return }
        guard let key = Self.occurrenceKey(atMs: at, timeZone: routine.timezone) else { return }
        await run(
            .skipRoutineOccurrence(id: routine.id, occurrenceKey: key),
            label: "skip \(key) of “\(routine.template.title)”"
        )
    }

    /// Materialize every live routine against the core's clock — the same
    /// thing the periodic timer does, on demand.
    func materializeNow() async {
        let now = await bridge.nowMs()
        await run(.materializeRoutines(nowMs: now), label: "materialize routines")
    }

    func dismissUndoNote() { undoNote = nil }

    /// `YYYY-MM-DDTHH:MM` in `timeZone`.
    static func occurrenceKey(atMs: Int64, timeZone: String) -> String? {
        guard let zone = TimeZone(identifier: timeZone) else { return nil }
        let formatter = DateFormatter()
        formatter.calendar = Calendar(identifier: .gregorian)
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.timeZone = zone
        formatter.dateFormat = "yyyy-MM-dd'T'HH:mm"
        return formatter.string(from: Date(timeIntervalSince1970: Double(atMs) / 1000))
    }

    private func run(_ command: CoreCommand, label: String) async {
        do {
            let outcome = try await bridge.submitUndoable(command, label: label)
            undoNote = outcome.notUndoable.map(undoRefusalExplanation(refusal:))
            await refresh()
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}

/// A recurrence phrase being typed, and what the shared parser made of it.
///
/// The parser is `sunrise_domain::parse_recurrence`, reached through the seam.
/// Nothing here reads the words: this type only holds what was typed, the rule
/// it produced, and the message when it produced none.
@MainActor
@Observable
final class RecurrenceField {
    var text: String {
        didSet { reparse() }
    }

    private(set) var rule: Recurrence?
    /// Why the phrase could not be read, in the domain's words. Present only
    /// for a non-empty phrase: an empty field is not yet an error.
    private(set) var problem: String?

    init(text: String = "every day") {
        self.text = text
        reparse()
    }

    /// The rule described back, which is how the field says what it
    /// understood. `every mon, wed` reads back as `every week on Mo, We`.
    var summary: String? {
        rule.map { recurrenceSummary(rule: $0) }
    }

    var isValid: Bool { rule != nil }

    private func reparse() {
        guard !text.trimmed.isEmpty else {
            rule = nil
            problem = nil
            return
        }
        do {
            rule = try parseRecurrence(text: text)
            problem = nil
        } catch let error as BindingError {
            rule = nil
            // The seam carries the phrase and the domain's own reason. Neither
            // is reworded here — "every blue moon" is refused by the same
            // sentence in the app and in `sunrise-cli`.
            problem = error.localizedDescription
        } catch {
            rule = nil
            problem = error.localizedDescription
        }
    }
}
