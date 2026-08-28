import SwiftUI

/// One task, with the facets it actually carries.
///
/// Every string here comes from the seam. The row decides layout and nothing
/// else — which is why "overdue", "tomorrow" and "1h30" read identically in
/// this app and in `sunrise-cli`.
struct TaskRowView: View {
    let facets: TaskFacets
    let complete: () async -> Void
    let edit: () -> Void

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Button {
                Task { await complete() }
            } label: {
                Image(systemName: facets.isDone ? "checkmark.circle.fill" : "circle")
                    .foregroundStyle(facets.isDone ? .green : .secondary)
            }
            .buttonStyle(.plain)
            .accessibilityLabel(facets.isDone ? "Completed" : "Complete “\(facets.title)”")
            .disabled(facets.isDone)

            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    if let priority = facets.priority {
                        Text("!\(priority)")
                            .font(.caption.weight(.bold))
                            .foregroundStyle(priority <= 2 ? .orange : .secondary)
                            .accessibilityLabel("Priority \(priority)")
                    }
                    Text(facets.title)
                        .strikethrough(facets.isDone)
                        .foregroundStyle(facets.isDone ? .secondary : .primary)
                    if facets.isBlocked {
                        Image(systemName: "pause.circle")
                            .foregroundStyle(.secondary)
                            .help("Waiting on another task")
                    }
                }
                facetLine
            }

            Spacer(minLength: 0)
        }
        .contentShape(.rect)
        .onTapGesture(count: 2, perform: edit)
        .padding(.vertical, 3)
    }

    @ViewBuilder
    private var facetLine: some View {
        let chips = chipTexts
        if !chips.isEmpty {
            HStack(spacing: 8) {
                ForEach(Array(chips.enumerated()), id: \.offset) { _, chip in
                    Text(chip.text)
                        .font(.caption)
                        .foregroundStyle(chip.isLate ? .orange : .secondary)
                }
            }
        }
    }

    private struct Chip {
        let text: String
        var isLate = false
    }

    private var chipTexts: [Chip] {
        var chips: [Chip] = []
        if let stream = facets.streamName { chips.append(Chip(text: "#\(stream)")) }
        chips.append(contentsOf: facets.contextNames.map { Chip(text: "@\($0)") })
        if let due = facets.due {
            chips.append(Chip(text: "due \(due.text)", isLate: due.isPast))
        }
        if let scheduled = facets.scheduled, facets.due == nil {
            chips.append(Chip(text: scheduled.text, isLate: scheduled.isPast))
        }
        if let estimate = facets.estimate { chips.append(Chip(text: estimate)) }
        if let energy = facets.energy { chips.append(Chip(text: energy)) }
        if let constraints = facets.constraints { chips.append(Chip(text: constraints)) }
        if facets.deferrals > 1 {
            chips.append(Chip(text: "deferred \(facets.deferrals)×"))
        }
        return chips
    }
}
