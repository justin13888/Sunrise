import Foundation
import SwiftUI
import Testing

@testable import Sunrise

/// The note editor, against the real codec on the other side of the seam.
///
/// No vault: the grammar is a pure function of the bytes, so these are the
/// tests that can be fast *and* honest at once. What they are really about is
/// the interlock — the editor may render anything, and may write back only
/// what it can put back exactly.
enum NoteFixture {
    /// An unmarked run of text.
    static func plain(_ text: String) -> [NoteInline] {
        [.text(text: text, marks: [])]
    }

    /// Blocks as the bytes the core would store them as.
    static func body(_ blocks: [NoteBlock]) -> NoteBody {
        encodeNoteBody(blocks: blocks)
    }

    /// The edit a model would submit, so a test can assert on both what it
    /// writes and what it refuses to write.
    @MainActor
    static func saved(_ model: NoteEditorModel) -> TaskEdit {
        var edit = TaskEdit()
        model.apply(to: &edit)
        return edit
    }
}

@MainActor
struct NoteEditorModelTests {
    private func plain(_ text: String) -> [NoteInline] { NoteFixture.plain(text) }
    private func body(_ blocks: [NoteBlock]) -> NoteBody { NoteFixture.body(blocks) }
    private func saved(_ model: NoteEditorModel) -> TaskEdit { NoteFixture.saved(model) }

    // MARK: - Loading

    @Test
    func anEmptyNoteOpensWithSomewhereToType() {
        let model = NoteEditorModel(body: nil)
        #expect(model.blocks.count == 1)
        #expect(model.blocks[0].kind == .paragraph)
        #expect(!model.isReadOnly)
        #expect(model.isEmpty)
    }

    @Test
    func openingANoteAndSavingItUnchangedWritesNothing() {
        // The body must not be rewritten just because someone opened the
        // sheet to change a deadline. A NoteBody is one LWW unit, so a
        // gratuitous write is a real write — it beats a concurrent edit from
        // another device.
        let model = NoteEditorModel(body: body([.paragraph(inline: plain("hello"))]))
        #expect(!model.isDirty)
        let edit = saved(model)
        #expect(edit.setBody == nil)
        #expect(edit.clearBody == false)
    }

    @Test
    func everyBlockKindSurvivesBeingLoadedAndSavedAgain() {
        let original: [NoteBlock] = [
            .heading(level: .one, inline: plain("Trip")),
            .heading(level: .two, inline: plain("Packing")),
            .heading(level: .three, inline: plain("Carry-on")),
            .paragraph(inline: plain("Leave on Tuesday.")),
            .list(ordered: false, items: [NoteListItem(inline: plain("socks"), children: [])]),
            .list(ordered: true, items: [NoteListItem(inline: plain("first"), children: [])]),
            .checklist(items: [NoteChecklistItem(checked: true, inline: plain("passport"))]),
            .code(language: "sh", content: "echo hi"),
            .quote(inline: plain("Pack light.")),
            .divider
        ]
        let model = NoteEditorModel(body: body(original))
        #expect(!model.isReadOnly, "every one of these is a kind the editor draws")
        #expect(model.noteBlocks == original)
    }

    @Test
    func everyMarkSurvivesBeingLoadedAndSavedAgain() {
        let marked: [NoteBlock] = [
            .paragraph(inline: [
                .text(text: "bold", marks: [.bold]),
                .text(text: "italic", marks: [.italic]),
                .text(text: "under", marks: [.underline]),
                .text(text: "struck", marks: [.strike]),
                .text(text: "code", marks: [.code]),
                .text(text: "all", marks: [.bold, .italic, .underline, .strike, .code])
            ])
        ]
        let model = NoteEditorModel(body: body(marked))
        #expect(!model.isReadOnly)
        #expect(model.noteBlocks == marked)
    }

    @Test
    func aNestedListKeepsItsShape() {
        let nested: [NoteBlock] = [
            .list(ordered: false, items: [
                NoteListItem(inline: plain("outer"), children: [
                    .list(ordered: false, items: [
                        NoteListItem(inline: plain("inner"), children: [])
                    ])
                ])
            ])
        ]
        let model = NoteEditorModel(body: body(nested))
        #expect(!model.isReadOnly)
        #expect(model.blocks[0].rows.map(\.depth) == [0, 1], "nesting shows as indentation")
        #expect(model.noteBlocks == nested)
    }

    // MARK: - The interlock

    @Test
    func aPlainTextBodyIsShownButNotEdited() {
        // What `sunrise` CLI writes. It renders — an editor that showed
        // nothing here would look broken over a note that still has words in
        // it — but the bytes are not the grammar, so they are not overwritten.
        let model = NoteEditorModel(body: Data("remember the kangaroo".utf8))
        #expect(model.lock == .unreadable)
        #expect(model.blocks.count == 1)
        #expect(String(model.blocks[0].text.characters) == "remember the kangaroo")
        let edit = saved(model)
        #expect(edit.setBody == nil && edit.clearBody == false, "read-only means read-only")
    }

    @Test
    func aBodyFromANewerBuildIsShownButNotEdited() {
        // A block kind this build has no name for. The core reports it as
        // lossy; the editor turns that into "you may read this".
        var bytes = body([.paragraph(inline: plain("kept"))])
        bytes.append(contentsOf: [0xFF])
        let model = NoteEditorModel(body: bytes)
        #expect(model.isReadOnly)
        #expect(saved(model).setBody == nil)
    }

    @Test
    func anExistingLinkSurvivesAnEditToTheTextAroundIt() {
        // A link rides on the standard link attribute, so it renders, stays
        // clickable, and comes back out as a link. Authoring a *new* one is
        // what this editor has no control for.
        let linked: [NoteBlock] = [
            .paragraph(inline: [.link(href: "https://example.com", label: "booking")])
        ]
        let model = NoteEditorModel(body: body(linked))
        #expect(model.lock == nil)
        #expect(String(model.blocks[0].text.characters) == "booking")
        #expect(model.noteBlocks == linked, "the link is not flattened into plain text")
    }

    @Test
    func aLinkWhoseHrefIsNotAUrlIsShownButNotEdited() {
        // The href is stored as text, and text is a wider set than URLs. One
        // that will not survive `URL` is one this editor must not rewrite —
        // and the round-trip check is what notices, without having to know
        // the rules `URL` parses by.
        let model = NoteEditorModel(body: body([
            .paragraph(inline: [.link(href: "not a url at all", label: "booking")])
        ]))
        #expect(model.lock == .unsupported)
        #expect(String(model.blocks[0].text.characters) == "booking", "it still reads")
        #expect(saved(model).setBody == nil)
    }

    @Test
    func aNoteWithAnEntityReferenceIsShownButNotEdited() {
        let model = NoteEditorModel(body: body([
            .paragraph(inline: [.ref(target: "tsk_01ARZ3NDEKTSV4RRFFQ69G5FAV")])
        ]))
        #expect(model.lock == .unsupported)
        #expect(saved(model).setBody == nil)
    }

    @Test
    func aBlockNestedUnderAListItemIsShownButNotEdited() {
        // The grammar allows any block under a list item; this editor offers
        // indent and outdent, which cannot say "a code block goes here".
        let model = NoteEditorModel(body: body([
            .list(ordered: false, items: [
                NoteListItem(inline: plain("outer"), children: [
                    .code(language: nil, content: "echo hi")
                ])
            ])
        ]))
        #expect(model.lock == .unsupported)
        #expect(saved(model).setBody == nil)
    }

    @Test
    func aLockedNoteRefusesEveryStructuralEdit() {
        let model = NoteEditorModel(body: Data("plain".utf8))
        let before = model.blocks
        model.setKind(.heading1, for: model.blocks[0].id)
        model.insertBlock(.quote, after: model.blocks[0].id)
        model.removeBlock(model.blocks[0].id)
        model.moveBlock(model.blocks[0].id, by: 1)
        #expect(model.blocks == before, "nothing a locked note offers may change it")
    }
}

/// The edits themselves: what each control does to the document, and what the
/// grammar's own rules refuse to let it do.
@MainActor
struct NoteEditorEditingTests {
    private func plain(_ text: String) -> [NoteInline] { NoteFixture.plain(text) }
    private func body(_ blocks: [NoteBlock]) -> NoteBody { NoteFixture.body(blocks) }
    private func saved(_ model: NoteEditorModel) -> TaskEdit { NoteFixture.saved(model) }

    // MARK: - Editing

    @Test
    func typingIntoAnEmptyNoteWritesABody() throws {
        let model = NoteEditorModel(body: nil)
        model.blocks[0].text = AttributedString("Buy milk")
        #expect(model.isDirty)

        let edit = saved(model)
        let written = try #require(edit.setBody)
        #expect(decodeNoteBody(body: written).blocks == [.paragraph(inline: plain("Buy milk"))])
    }

    @Test
    func emptyingANoteClearsTheFieldRatherThanStoringNothing() {
        let model = NoteEditorModel(body: body([.paragraph(inline: plain("gone"))]))
        model.blocks[0].text = AttributedString()
        let edit = saved(model)
        #expect(edit.clearBody, "an emptied body is cleared, not written as zero blocks")
        #expect(edit.setBody == nil)
    }

    @Test
    func changingAParagraphToAHeadingKeepsTheWords() {
        let model = NoteEditorModel(body: body([.paragraph(inline: plain("Packing"))]))
        model.setKind(.heading2, for: model.blocks[0].id)
        #expect(model.noteBlocks == [.heading(level: .two, inline: plain("Packing"))])
    }

    @Test
    func changingAListToAParagraphJoinsItsRowsRatherThanDroppingThem() {
        let model = NoteEditorModel(body: body([
            .list(ordered: false, items: [
                NoteListItem(inline: plain("socks"), children: []),
                NoteListItem(inline: plain("shoes"), children: [])
            ])
        ]))
        model.setKind(.paragraph, for: model.blocks[0].id)
        #expect(model.noteBlocks == [.paragraph(inline: plain("socks shoes"))])
    }

    @Test
    func changingAListToAChecklistFlattensItBecauseAChecklistCannotNest() {
        let model = NoteEditorModel(body: body([
            .list(ordered: false, items: [
                NoteListItem(inline: plain("outer"), children: [
                    .list(ordered: false, items: [NoteListItem(inline: plain("inner"), children: [])])
                ])
            ])
        ]))
        model.setKind(.checklist, for: model.blocks[0].id)
        #expect(model.blocks[0].rows.allSatisfy { $0.depth == 0 })
        #expect(model.noteBlocks == [.checklist(items: [
            NoteChecklistItem(checked: false, inline: plain("outer")),
            NoteChecklistItem(checked: false, inline: plain("inner"))
        ])])
    }

    @Test
    func deletingTheLastBlockLeavesSomewhereToType() {
        let model = NoteEditorModel(body: body([.paragraph(inline: plain("only"))]))
        model.removeBlock(model.blocks[0].id)
        #expect(model.blocks.count == 1)
        #expect(model.isEmpty)
    }

    @Test
    func aBlockMovesUpAndDownAndStopsAtTheEnds() {
        let model = NoteEditorModel(body: body([
            .paragraph(inline: plain("one")),
            .paragraph(inline: plain("two"))
        ]))
        let second = model.blocks[1].id
        model.moveBlock(second, by: -1)
        #expect(model.noteBlocks.first == .paragraph(inline: plain("two")))
        model.moveBlock(second, by: -1)
        #expect(model.noteBlocks.first == .paragraph(inline: plain("two")), "no wrapping past the top")
    }

    @Test
    func checkingABoxIsTheOnlyThingItChanges() {
        let model = NoteEditorModel(body: body([
            .checklist(items: [NoteChecklistItem(checked: false, inline: plain("passport"))])
        ]))
        let block = model.blocks[0]
        model.toggleChecked(block.rows[0].id, in: block.id)
        #expect(model.noteBlocks == [.checklist(items: [
            NoteChecklistItem(checked: true, inline: plain("passport"))
        ])])
    }

    // MARK: - Nesting rules

    @Test
    func theFirstRowCannotBeIndentedBecauseItHasNoParent() {
        let model = NoteEditorModel(body: body([
            .list(ordered: false, items: [NoteListItem(inline: plain("only"), children: [])])
        ]))
        let block = model.blocks[0]
        model.indentRow(block.rows[0].id, in: block.id)
        #expect(model.blocks[0].rows[0].depth == 0)
    }

    @Test
    func aRowCannotBeIndentedMoreThanOneLevelBelowTheRowAboveIt() {
        // The grammar nests by containment, so a row two levels below its
        // predecessor has no parent to hang from — it is not merely ugly, it
        // is unwritable.
        let model = NoteEditorModel(body: body([
            .list(ordered: false, items: [
                NoteListItem(inline: plain("one"), children: []),
                NoteListItem(inline: plain("two"), children: [])
            ])
        ]))
        let block = model.blocks[0]
        model.indentRow(block.rows[1].id, in: block.id)
        #expect(model.blocks[0].rows[1].depth == 1)
        model.indentRow(block.rows[1].id, in: block.id)
        #expect(model.blocks[0].rows[1].depth == 1, "refused a second time")
    }

    @Test
    func outdentingARowPullsItsOrphanedChildrenBackWithIt() {
        let model = NoteEditorModel(body: body([
            .list(ordered: false, items: [
                NoteListItem(inline: plain("one"), children: [
                    .list(ordered: false, items: [
                        NoteListItem(inline: plain("two"), children: [
                            .list(ordered: false, items: [
                                NoteListItem(inline: plain("three"), children: [])
                            ])
                        ])
                    ])
                ])
            ])
        ]))
        #expect(model.blocks[0].rows.map(\.depth) == [0, 1, 2])
        let block = model.blocks[0]
        model.outdentRow(block.rows[1].id, in: block.id)
        #expect(
            model.blocks[0].rows.map(\.depth) == [0, 0, 1],
            "the grandchild follows its parent rather than being orphaned"
        )
    }

    @Test
    func indentedRowsRoundTripThroughTheGrammar() throws {
        let model = NoteEditorModel(body: nil)
        model.setKind(.bulleted, for: model.blocks[0].id)
        let block = model.blocks[0].id
        model.blocks[0].rows[0].text = AttributedString("outer")
        model.addRow(to: block)
        model.blocks[0].rows[1].text = AttributedString("inner")
        model.indentRow(model.blocks[0].rows[1].id, in: block)

        let written = try #require(saved(model).setBody)
        #expect(decodeNoteBody(body: written).blocks == [
            .list(ordered: false, items: [
                NoteListItem(inline: plain("outer"), children: [
                    .list(ordered: false, items: [
                        NoteListItem(inline: plain("inner"), children: [])
                    ])
                ])
            ])
        ])
    }

    // MARK: - Marks

    @Test
    func togglingAMarkOverASelectionMarksExactlyThatRun() throws {
        let model = NoteEditorModel(body: body([.paragraph(inline: plain("one two"))]))
        let text = model.blocks[0].text
        model.blocks[0].selection = AttributedTextSelection(range: try #require(text.range(of: "one")))
        model.focusedBlock = model.blocks[0].id

        model.toggleMark(.bold)
        #expect(model.noteBlocks == [.paragraph(inline: [
            .text(text: "one", marks: [.bold]),
            .text(text: " two", marks: [])
        ])])

        model.toggleMark(.bold)
        #expect(model.noteBlocks == [.paragraph(inline: plain("one two"))], "and off again")
    }

    @Test
    func aMarkIsReportedActiveOnlyWhenTheWholeSelectionCarriesIt() throws {
        let model = NoteEditorModel(body: body([.paragraph(inline: [
            .text(text: "one", marks: [.bold]),
            .text(text: " two", marks: [])
        ])]))
        model.focusedBlock = model.blocks[0].id
        let text = model.blocks[0].text

        model.blocks[0].selection = AttributedTextSelection(range: try #require(text.range(of: "one")))
        #expect(model.isActive(.bold))

        model.blocks[0].selection = AttributedTextSelection(
            range: text.startIndex..<text.endIndex
        )
        #expect(!model.isActive(.bold), "half a bold selection is not a bold selection")
    }

    @Test
    func aCollapsedSelectionMarksNothingRatherThanTheWholeBlock() {
        let model = NoteEditorModel(body: body([.paragraph(inline: plain("untouched"))]))
        model.focusedBlock = model.blocks[0].id
        model.toggleMark(.bold)
        #expect(model.noteBlocks == [.paragraph(inline: plain("untouched"))])
    }

    // MARK: - Export

    @Test
    func theMarkdownIsTheCoresAndIsNotWordedTwice() {
        let model = NoteEditorModel(body: body([
            .heading(level: .two, inline: plain("Packing")),
            .checklist(items: [NoteChecklistItem(checked: true, inline: plain("passport"))])
        ]))
        #expect(model.markdown == noteBodyMarkdown(blocks: model.noteBlocks))
        #expect(model.markdown.contains("## Packing"))
        #expect(model.markdown.contains("- [x] passport"))
    }

    @Test
    func aShortNoteSaysNothingAboutItsLength() {
        let model = NoteEditorModel(body: body([.paragraph(inline: plain("short"))]))
        #expect(model.lengthWarning == nil)
    }

    @Test
    func aNoteOverTheSoftLimitSaysSo() {
        let long = String(repeating: "a", count: Int(noteBodyLimits().softBytes) + 1)
        let model = NoteEditorModel(body: body([.paragraph(inline: plain(long))]))
        #expect(model.lengthWarning != nil)
    }
}
