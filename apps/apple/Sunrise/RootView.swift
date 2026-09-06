import SwiftUI

/// What the window shows, decided by the session phase.
///
/// The four cases are kept apart on screen because they are apart in reality:
/// a first run has nothing to lose, a locked vault has everything to lose, and
/// a failure is neither.
struct RootView: View {
    let session: SessionModel
    let surfaces: AppSurfaces

    var body: some View {
        Group {
            switch session.phase {
            case .starting:
                // Transient by construction, and it has to stay that way: the
                // `.task` below fires once for the life of this window, so
                // nothing here would ever call `start()` a second time. Every
                // route into `.starting` — `start`, `switchTo`,
                // `adoptPairing` — drives itself out again on the same call,
                // and `lock()` lands on `.locked(.lockedByUser)` rather than
                // here for exactly this reason.
                ProgressView("Opening your vault…")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            case .firstRun:
                // The session goes down with it: "Pair with that device"
                // adopts a vault root, which is a thing only the session can
                // do.
                OnboardingView(create: session.createVault, session: session)
            case let .locked(reason):
                LockedView(reason: reason, retry: session.start, session: session)
            case .unlocked:
                if let bridge = session.bridge {
                    VaultShell(bridge: bridge, session: session, surfaces: surfaces)
                        // The menu bar item, the capture panel and the
                        // reminder schedule need the same open vault this
                        // window is using, and this is the first moment there
                        // is one.
                        //
                        // Keyed on the bridge's identity: switching vaults
                        // opens a different `Core`, and the three surfaces
                        // above have to be rebuilt against it. Without the id
                        // this would not re-run if SwiftUI ever reused the
                        // view — and the surfaces would go on writing to a
                        // core that has been shut down.
                        .task(id: ObjectIdentifier(bridge)) {
                            surfaces.attach(bridge: bridge)
                            await surfaces.reminders?.start()
                            // Recurrence, from here on, without anyone opening
                            // a screen. Keyed on the same bridge identity as
                            // the rest: the timer belongs to the `Core`, so a
                            // vault switch has to start a new one rather than
                            // inherit the old one's.
                            await surfaces.startRoutineTimer()
                        }
                }
            case let .failed(message):
                ContentUnavailableView(
                    "Sunrise could not start",
                    systemImage: "exclamationmark.triangle",
                    description: Text(message)
                )
            }
        }
        .task { await session.start() }
        // Every `sunrise://` link the OS hands this process arrives here.
        // Attached to the window's root rather than to a scene that may not
        // exist: a link that arrives while Sunrise is closed opens this window
        // to deliver it, which is exactly what a tapped reminder should do.
        //
        // Anything the parser refuses is dropped in silence, per
        // `docs/07-clients/interaction-patterns.md`.
        .onOpenURL { url in
            guard let link = DeepLink(url: url) else { return }
            surfaces.open(link)
        }
    }
}

/// A dismissible line of explanation. Not an error: what it reports has
/// already happened.
struct NoteBanner: View {
    let text: String
    let dismiss: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "info.circle")
            Text(text).font(.callout)
            Spacer()
            Button("Dismiss", systemImage: "xmark", action: dismiss)
                .labelStyle(.iconOnly)
                .buttonStyle(.plain)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(.quaternary.opacity(0.5))
    }
}
