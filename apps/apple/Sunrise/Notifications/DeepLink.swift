import Foundation

/// A `sunrise://` link, parsed and validated.
///
/// `docs/07-clients/interaction-patterns.md` §URL scheme defines the shapes and
/// says the last word on them: *"All deep links are validated; unknown shapes
/// are ignored."* So this is a failable parser over a closed set rather than a
/// dictionary of whatever a URL happened to carry — a link the app cannot act
/// on returns `nil` here and is dropped, instead of arriving half-understood at
/// a router that then has to guess.
///
/// Two of the cases are the views issue #9 asks a notification to open into.
/// They are not in the doc's list because they did not exist when it was
/// written; the rest are its list, minus the three noted on ``init(url:)``.
enum DeepLink: Equatable, Sendable {
    /// `sunrise://morning` — the morning summary.
    case morningSummary
    /// `sunrise://evening` — the end-of-day plan.
    case endOfDayPlan
    /// `sunrise://entity/<EntityRef>` — show where that entity lives.
    case entity(EntityRef)
    /// `sunrise://capture?text=…&stream=…` — quick capture, pre-filled.
    case capture(text: String)
    /// `sunrise://task/<id>?action=…` — act on one task.
    case task(EntityRef, TaskLinkAction)

    /// The scheme, lowercase. Registered in `apps/apple/project.yml`.
    static let scheme = "sunrise"

    /// Parse a URL, or refuse it.
    ///
    /// Three documented shapes are deliberately **not** parsed:
    /// `sunrise://focus/<TaskId>`, `sunrise://share/<token>` and the
    /// "open entity in detail" reading of `sunrise://entity/…`. The macOS app
    /// has no route to any of them yet — focus starts from a chosen task
    /// inside the Focus screen, share invites are not implemented, and there
    /// is no standalone entity detail window — and a parser case whose router
    /// arm does nothing is a link that silently fails rather than one that is
    /// honestly ignored.
    init?(url: URL) {
        guard url.scheme?.lowercased() == Self.scheme,
              let parts = URLComponents(url: url, resolvingAgainstBaseURL: false),
              // `sunrise://morning` puts "morning" in `host`, not in `path`.
              let route = parts.host?.lowercased() else { return nil }
        // These routes take at most one path segment.
        let segments = parts.path
            .split(separator: "/", omittingEmptySubsequences: true)
            .map(String.init)
        guard let link = Self.parse(
            route: route,
            segments: segments,
            query: parts.queryItems ?? []
        ) else { return nil }
        self = link
    }

    /// The route table itself. A `nil` here is the "unknown shapes are
    /// ignored" rule, in one place.
    private static func parse(
        route: String,
        segments: [String],
        query: [URLQueryItem]
    ) -> DeepLink? {
        switch (route, segments.count) {
        case ("morning", 0):
            return .morningSummary
        case ("evening", 0):
            return .endOfDayPlan
        case ("capture", 0):
            let text = captureLine(query)
            return text.isEmpty ? nil : .capture(text: text)
        case ("entity", 1):
            return entityRef(segments[0]).map { .entity($0) }
        case ("task", 1):
            guard let id = entityRef(segments[0], kind: "tsk_"),
                  let action = TaskLinkAction(query: query) else { return nil }
            return .task(id, action)
        default:
            return nil
        }
    }

    /// The link itself, for a notification's payload.
    var url: URL? {
        var parts = URLComponents()
        parts.scheme = Self.scheme
        switch self {
        case .morningSummary:
            parts.host = "morning"
        case .endOfDayPlan:
            parts.host = "evening"
        case let .entity(id):
            parts.host = "entity"
            parts.path = "/\(id)"
        case let .capture(text):
            parts.host = "capture"
            parts.queryItems = [URLQueryItem(name: "text", value: text)]
        case let .task(id, action):
            parts.host = "task"
            parts.path = "/\(id)"
            parts.queryItems = [URLQueryItem(name: "action", value: action.identifier)]
        }
        return parts.url
    }

    /// Where in the app this link lands, when it lands on a screen at all.
    ///
    /// `nil` for the links that are an *operation*: completing or snoozing a
    /// task from a notification button must not drag a window forward, which
    /// is what `docs/07-clients/interaction-patterns.md` means by "translates
    /// to a CRDT op without opening UI when possible".
    var destination: Destination? {
        switch self {
        case .morningSummary: .morning
        case .endOfDayPlan: .evening
        case .capture: nil
        case let .entity(id): Self.home(of: id)
        case let .task(_, action): action == .open ? .list(.todayAll) : nil
        }
    }

    /// The screen an entity lives on.
    ///
    /// Coarser than the doc's "open entity in detail", and said plainly rather
    /// than pretended: a block belongs to the calendar grid and a task to
    /// Today, and those are the two kinds a reminder can be about.
    private static func home(of id: EntityRef) -> Destination {
        id.hasPrefix("blk_") ? .calendar : .list(.todayAll)
    }

    /// `text` and `stream`, recombined into one capture line.
    ///
    /// `#stream` is the shared parser's own annotation, so folding the
    /// `stream` parameter into the text hands the resolution to the same code
    /// a typed line goes through — rather than teaching this file a second way
    /// to name a stream.
    private static func captureLine(_ query: [URLQueryItem]) -> String {
        let text = value(of: "text", in: query)?.trimmed ?? ""
        guard let stream = value(of: "stream", in: query)?.trimmed, !stream.isEmpty else {
            return text
        }
        // A stream name with a space in it cannot be written as `#name`, and
        // inventing a quoting rule the parser does not have would produce a
        // line that reads back wrong. The name is dropped, the text is not.
        guard !stream.contains(" ") else { return text }
        return text.isEmpty ? "#\(stream)" : "\(text) #\(stream)"
    }

    private static func value(of name: String, in query: [URLQueryItem]) -> String? {
        query.first { $0.name == name }?.value
    }

    /// An `EntityRef` shaped like one: `tsk_` + 26 Crockford characters.
    ///
    /// The core validates it properly — `EntityRef::parse_any` throws
    /// `BindingError.BadId` — so this is not the safety check. It is the
    /// *shape* check that keeps a malformed link out of a command instead of
    /// turning into an error banner the user cannot act on.
    private static func entityRef(_ raw: String, kind: String? = nil) -> EntityRef? {
        guard raw.count == 30 else { return nil }
        let prefix = String(raw.prefix(4))
        guard Self.kinds.contains(prefix) else { return nil }
        if let kind, prefix != kind { return nil }
        let body = raw.dropFirst(4)
        guard body.allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber) }) else { return nil }
        return raw
    }

    /// Every prefix `sunrise-id`'s `EntityKind` defines.
    private static let kinds: Set<String> = [
        "tsk_", "str_", "ctx_", "rtn_", "blk_",
        "not_", "att_", "prs_", "dev_", "idn_", "fcs_", "rvw_"
    ]
}

/// What a `sunrise://task/<id>` link asks for.
///
/// The identifiers are the ones
/// `docs/07-clients/interaction-patterns.md` §Action wiring names — `complete`
/// and `snooze_1h` — so a link built by any client is understood by this one.
enum TaskLinkAction: Equatable, Sendable {
    /// Show the task. The default when no `action` is given, and what tapping
    /// a notification body does.
    case open
    case complete
    case snooze(SnoozeSpan)

    init?(query: [URLQueryItem]) {
        guard let raw = query.first(where: { $0.name == "action" })?.value else {
            self = .open
            return
        }
        switch raw.lowercased() {
        case "open": self = .open
        case "complete": self = .complete
        case "snooze_1h": self = .snooze(.oneHour)
        case "snooze_tomorrow": self = .snooze(.tomorrow)
        case "snooze_next_week": self = .snooze(.nextWeek)
        default: return nil
        }
    }

    var identifier: String {
        switch self {
        case .open: "open"
        case .complete: "complete"
        case let .snooze(span): span.linkAction
        }
    }
}

extension SnoozeSpan {
    /// The `action=` value that means this span.
    var linkAction: String {
        switch self {
        case .oneHour: "snooze_1h"
        case .tomorrow: "snooze_tomorrow"
        case .nextWeek: "snooze_next_week"
        }
    }
}
