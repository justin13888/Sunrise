import SwiftUI

/// A settings row's caption, and why the core did not take the value above
/// it, when it did not.
///
/// The refusal is a row of its own beneath the hint rather than a
/// replacement for it, so the hint still says what the field is for while
/// the warning says the value shown is not the one in use.
struct SettingsCaption: View {
    var hint: String?
    var refusal: String?

    var body: some View {
        if hint != nil || refusal != nil {
            VStack(alignment: .leading, spacing: 4) {
                if let hint {
                    Text(hint)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                if let refusal {
                    Label(refusal, systemImage: "exclamationmark.triangle")
                        .font(.caption)
                        .foregroundStyle(.orange)
                }
            }
        }
    }
}
