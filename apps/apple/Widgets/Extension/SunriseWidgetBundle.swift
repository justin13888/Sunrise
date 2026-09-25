import SwiftUI
import WidgetKit

/// The widget extension's entry point: every widget Sunrise offers.
///
/// One bundle for both platforms. `project.yml` builds it twice — once into
/// the Mac app for the Notification Centre and the desktop, once into the iOS
/// app for the Home and Lock Screens — and the families each widget offers are
/// the only thing that differ, which ``NextUpWidget`` decides.
@main
struct SunriseWidgetBundle: WidgetBundle {
    var body: some Widget {
        NextUpWidget()
    }
}
