import SwiftUI
import UniformTypeIdentifiers

// The long-lived modifiers ``VaultTabs`` hangs on its tab view.
//
// They live in a file of their own rather than beside the shell for one
// reason: `body` cannot be type-checked with any of them written inline, which
// is why they are modifiers at all, and with all four of them in it the shell
// was past the length this project lints for. Nested in ``VaultTabs`` rather
// than left at file scope, because names this general — `Routing`, `Lifecycle`
// — belong to the shell that uses them and not to a module the Mac's sources
// share.
extension VaultTabs {
    /// Every route into the app, delivered to the shell.
    ///
    /// A `sunrise://` link, a tapped reminder, an App Intent, the Control Center
    /// control and the widget all set `pendingDestination` or `pendingCommand` on
    /// the shared ``AppSurfaces``; the macOS window takes the same values into its
    /// sidebar selection. Split into a modifier because `body` could not be
    /// type-checked with it inline.
    struct Routing: ViewModifier {
        let surfaces: AppSurfaces
        let show: (Destination) -> Void
        let reveal: (EntityRef) -> Void
        let perform: (AppAction) -> Void

        func body(content: Content) -> some View {
            content
                .onChange(of: surfaces.pendingDestination) { _, destination in
                    guard let destination else { return }
                    show(destination)
                    surfaces.destinationTaken()
                }
                .onChange(of: surfaces.pendingReveal) { _, entity in
                    guard let entity else { return }
                    surfaces.revealTaken()
                    reveal(entity)
                }
                .onChange(of: surfaces.pendingCommand) { _, command in
                    guard let command else { return }
                    surfaces.commandTaken()
                    perform(command)
                }
                .onChange(of: surfaces.notifications.policy) {
                    Task { await surfaces.reminders?.reconcile() }
                }
        }
    }

    /// The long-lived work an open vault starts: sync, the reminder schedule, the
    /// undo feed and the saved-view list. The same set the macOS window starts,
    /// and for the same reasons.
    struct Lifecycle: ViewModifier {
        let bridge: CoreBridge
        let models: VaultModels
        let surfaces: AppSurfaces
        @Binding var deviceID: String
        let startSync: () async -> Void

        func body(content: Content) -> some View {
            content
                .task {
                    deviceID = await bridge.deviceId()
                    models.account.restore()
                    await startSync()
                }
                .task { await models.sync.poll(from: bridge) }
                .task { await surfaces.reminders?.follow() }
                .task { await models.undo.follow() }
                .task { await models.savedViews.load() }
                .onChange(of: models.settings.relayURL) { Task { await startSync() } }
                .onChange(of: models.account.accessToken) { _, token in
                    Task { await bridge.setSyncCredential(token) }
                }
        }
    }

    /// Saved views and iCalendar.
    ///
    /// Both are shared, both compile into this product, and until this modifier
    /// neither had an iOS caller: `SavedViewsMenu` was instantiated only at
    /// `macOS/VaultWindow.swift:89` and `IcalSurfaces` applied only at `:141`, so
    /// a saved view made on the Mac synced to the phone and was unreachable there.
    /// `docs/07-clients/parity-matrix.md` graded both **unmet** for that reason.
    ///
    /// A modifier rather than four things inline for the same reason ``Routing``
    /// is one: `body` stops type-checking.
    struct LibrarySurfaces: ViewModifier {
        let surfaces: AppSurfaces
        @Binding var savingView: Destination?
        @Binding var newViewName: String
        @Binding var importingIcal: Bool
        @Binding var exportingIcal: IcalDocument?
        let save: (String, Destination) async -> Void

        func body(content: Content) -> some View {
            content
                .modifier(IcalSurfaces(model: surfaces.ical))
                .sheet(item: $savingView) { destination in
                    SaveViewSheet(name: $newViewName, summary: destination.title) {
                        await save(newViewName, destination)
                        newViewName = ""
                    }
                    // Top, not centre, for the reason ``QuickCaptureView``
                    // gives: a `VStack` handed more height than it needs
                    // centres itself in the detent.
                    .frame(maxHeight: .infinity, alignment: .top)
                    // A name and two buttons, so the sheet asks for the height a
                    // name and two buttons need. The Mac's counterpart is a 360
                    // point panel; the phone's equivalent of that number is a
                    // detent, not a width.
                    .presentationDetents([.height(300), .medium])
                }
                .fileImporter(
                    isPresented: $importingIcal,
                    allowedContentTypes: IcalFiles.documentTypes
                ) { result in
                    guard case let .success(url) = result else { return }
                    Task {
                        // A picked document is security-scoped on iOS and the
                        // scope has to be open across the read. `IcalFiles.read`
                        // opens the file itself, and it runs before the first
                        // suspension inside `importIcal(from:)`, but the scope is
                        // held for the whole call rather than resting on that.
                        let scoped = url.startAccessingSecurityScopedResource()
                        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
                        await surfaces.importIcal(from: url)
                    }
                }
                .fileExporter(
                    isPresented: Binding(
                        get: { exportingIcal != nil },
                        set: { if !$0 { exportingIcal = nil } }
                    ),
                    document: exportingIcal,
                    contentType: IcalDocument.contentType,
                    defaultFilename: exportingIcal?.filename
                ) { _ in exportingIcal = nil }
        }
    }

    /// The capture sheet.
    ///
    /// iOS's answer to the Mac's borderless panel: `AppSurfaces.openQuickCapture`
    /// sets a flag here rather than presenting a window, because a sheet can only
    /// be presented by a view that is already on screen. `.presentationDetents`
    /// keeps it to the height a single field needs — a full-screen sheet for one
    /// line of text is the thing that makes quick capture stop feeling quick.
    struct CaptureSheet: ViewModifier {
        let surfaces: AppSurfaces

        func body(content: Content) -> some View {
            content.sheet(
                isPresented: Binding(
                    get: { surfaces.isCapturing },
                    set: { if !$0 { surfaces.captureDismissed() } }
                )
            ) {
                if let capture = surfaces.capture {
                    QuickCaptureView(
                        model: capture,
                        commit: { try await surfaces.commitCapture($0) },
                        dismiss: { surfaces.captureDismissed() }
                    )
                    // `.medium` beside the fixed height, not instead of it. The
                    // small detent is the point — a full-screen sheet for one line
                    // of text is what makes quick capture stop feeling quick — but
                    // the content grows: one label per token the parser could not
                    // place, and enough of those would push Add, the sheet's only
                    // commit control, past a height nothing can scroll.
                    .presentationDetents([.height(280), .medium])
                    .presentationDragIndicator(.visible)
                }
            }
        }
    }
}
