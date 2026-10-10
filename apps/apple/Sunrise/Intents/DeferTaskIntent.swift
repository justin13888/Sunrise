import AppIntents
import Foundation

/// How far to push a task, for the Shortcuts picker.
///
/// The three spans a reminder's snooze buttons already offer, mirrored rather
/// than reused for the reason ``FocusLength`` mirrors `SessionLength`: the
/// generated enum belongs to the seam, and conforming it to `AppEnum` would let
/// a rename there break saved shortcuts.
enum DeferSpan: String, CaseIterable, AppEnum {
    case oneHour
    case tomorrow
    case nextWeek

    static let typeDisplayRepresentation = TypeDisplayRepresentation(name: "Defer Until")

    static let caseDisplayRepresentations: [DeferSpan: DisplayRepresentation] = [
        .oneHour: "In an hour",
        .tomorrow: "Tomorrow",
        .nextWeek: "Next week"
    ]

    var snoozeSpan: SnoozeSpan {
        switch self {
        case .oneHour: .oneHour
        case .tomorrow: .tomorrow
        case .nextWeek: .nextWeek
        }
    }

    /// The words a dialog ends with.
    var phrase: String {
        switch self {
        case .oneHour: "for an hour"
        case .tomorrow: "to tomorrow"
        case .nextWeek: "to next week"
        }
    }
}

/// Push a task out.
///
/// `mobile-ios.md` §Shortcuts and App Intents lists defer beside capture and
/// mark-done, and it is the third thing a person does to a list without opening
/// it.
///
/// **It defers the way the rest of the app defers today.** The write is
/// `Command::DeferTask`, the same one the row's Defer menu and a reminder's
/// snooze buttons send, so the deferral count goes up exactly as it would from
/// either. The target instant is the seam's `snooze_target_ms`, the civil
/// computation the snooze buttons use, so "tomorrow" is a date and not
/// 86,400,000 ms. When the plan-time semantics of ADR-0047 land (#334), this
/// intent changes with the command, not on its own.
struct DeferTaskIntent: AppIntent {
    static let title: LocalizedStringResource = "Defer Task"

    static let description = IntentDescription(
        "Pushes a Sunrise task out by an hour, to tomorrow, or to next week.",
        categoryName: "Tasks",
        searchKeywords: ["defer", "postpone", "snooze", "later", "tomorrow"]
    )

    /// Runs without pulling Sunrise forward; see ``CaptureTaskIntent``.
    static let supportedModes: IntentModes = .background

    static var parameterSummary: some ParameterSummary {
        Summary("Defer \(\.$task) \(\.$span)")
    }

    @Parameter(title: "Task", requestValueDialog: "Which task do you want to defer?")
    var task: TaskEntity

    @Parameter(title: "Until", default: .tomorrow)
    var span: DeferSpan

    init() {}

    init(task: TaskEntity, span: DeferSpan) {
        self.task = task
        self.span = span
    }

    func perform() async throws -> some IntentResult & ReturnsValue<TaskEntity> & ProvidesDialog {
        let target = task.id
        let until = span
        let deferred = try await IntentVault.withVault { bridge in
            try await Self.deferTask(target, span: until, in: bridge)
        }
        return .result(value: deferred.task, dialog: IntentDialog("\(deferred.message)"))
    }

    /// What one deferral produced.
    struct Deferred: Sendable, Equatable {
        let task: TaskEntity
        /// Epoch ms the task now starts at.
        let toMs: UInt64
        let message: String
    }

    /// The whole of the work, with the vault passed in, so a test can run it
    /// against a scratch vault — see ``CaptureTaskIntent/capture(_:into:)``.
    static func deferTask(
        _ id: EntityRef,
        span: DeferSpan,
        in bridge: CoreBridge,
        timeZone: TimeZone = .current
    ) async throws -> Deferred {
        do {
            // Read first, so a task deleted since it was picked is reported
            // rather than written to.
            let item = try await TaskLookup.read(id, in: bridge)
            guard item.state != .done, item.state != .cancelled else {
                throw IntentError.taskAlreadyFinished(item.title)
            }
            let now = await bridge.nowMs()
            let to = target(fromMs: now, span: span, timeZone: timeZone)
            _ = try await bridge.submit(.deferTask(id: id, toMs: to))
            return Deferred(
                task: TaskEntity(item),
                toMs: to,
                message: "Deferred “\(item.title)” \(span.phrase)."
            )
        } catch {
            throw IntentError.wrapping(error)
        }
    }

    /// When the task lands: the seam's civil computation, clamped the way
    /// `ReminderScheduler` clamps a snooze.
    static func target(fromMs now: UInt64, span: DeferSpan, timeZone: TimeZone) -> UInt64 {
        let target = snoozeTargetMs(fromMs: now, span: span.snoozeSpan, tz: timeZone.identifier)
        return target <= 0 ? now : UInt64(target)
    }
}
