import SwiftUI

/// Everything the keyboard can put on screen, attached to the window once.
///
/// A modifier rather than five `.sheet`s written into `VaultView`'s body: they
/// are one feature, they all read the same selection, and a sheet that lived
/// inside the list would be a sheet the command palette could not open — the
/// palette is not inside the list.
struct KeyboardSurfaces: ViewModifier {
    @Bindable var sheets: RowSheets
    let palette: CommandPaletteModel
    @Bindable var preferences: KeyboardPreferences
    /// The list the row sheets write to. Today, a stream, or Search's results.
    let list: TaskListModel
    /// Whether the screen behind has rows, which decides what `?` prints.
    let hasList: Bool
    @Binding var showingCheatSheet: Bool
    @Binding var creatingStream: Bool
    let perform: (AppAction) -> Void
    let createStream: (String, StreamEdit) async -> Void

    func body(content: Content) -> some View {
        content
            .sheet(item: $sheets.editing) { task in
                TaskEditorView(
                    task: task,
                    bridge: list.bridge,
                    apply: { await list.apply($0, to: task) },
                    delete: { await list.delete(task) }
                )
            }
            .sheet(item: $sheets.scheduling) { batch in
                ScheduleSheet(count: batch.tasks.count) { date in
                    await list.schedule(batch.tasks, at: date)
                }
            }
            .sheet(item: $sheets.moving) { batch in
                MoveToStreamSheet(count: batch.tasks.count, streams: list.streamChoices) { stream in
                    await list.move(batch.tasks, toStream: stream)
                }
            }
            .sheet(isPresented: paletteBinding) {
                CommandPaletteView(model: palette) { action in
                    palette.dismiss()
                    perform(action)
                }
            }
            .sheet(isPresented: $showingCheatSheet) {
                CheatSheetView(preferences: preferences, hasList: hasList) {
                    showingCheatSheet = false
                }
            }
            .sheet(isPresented: $creatingStream) {
                StreamEditorView(stream: nil, commit: createStream)
            }
    }

    /// The palette owns whether it is up, because it also owns clearing the
    /// query on the way down — a palette that reopened still holding last
    /// week's search would run the wrong command on the first Return.
    private var paletteBinding: Binding<Bool> {
        Binding(
            get: { palette.isPresented },
            set: { if !$0 { palette.dismiss() } }
        )
    }
}
