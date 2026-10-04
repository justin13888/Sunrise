import Foundation
import Testing

@testable import Sunrise

/// The read-only state a vault this build cannot fully write puts the app in
/// (ADR-0045 §8). The rule itself is the core's (`edit_gate_allows`); these
/// tests pin that the app asks it with the tags the core uses, and that a
/// gate that locks nothing still shows the banner.
@MainActor
struct EditGateModelTests {
    private func gate(
        readOnly: Bool = true,
        locksAll: Bool = false,
        lockedTags: [String] = [],
        missing: [MissingFeatureItem] = []
    ) -> EditGate {
        EditGate(readOnly: readOnly, locksAll: locksAll, lockedTags: lockedTags, missing: missing)
    }

    /// The falsifier for the whole type: a tag spelled differently here from
    /// the entity registry would leave that kind's actions enabled in a vault
    /// that locks it, and the core would refuse every one of them.
    @Test
    func everyEditedEntityIsARegistryTag() {
        let tags = Set(EditedEntity.allCases.map(\.rawValue))
        #expect(tags == [
            "task", "stream", "context", "routine", "block", "attachment",
            "focus_session", "review_snapshot",
        ])
    }

    @Test
    func anOpenVaultAllowsEverythingAndShowsNoBanner() {
        let model = EditGateModel()
        #expect(!model.isReadOnly)
        for entity in EditedEntity.allCases {
            #expect(model.allows(entity))
        }
    }

    @Test
    func anEntityLockDisablesThatKindOnly() {
        let model = EditGateModel(gate: gate(lockedTags: ["task"]))
        #expect(model.isReadOnly)
        #expect(!model.allows(.task))
        #expect(model.allows(.stream))
        #expect(model.allows(.focusSession))
    }

    @Test
    func aStructuralLockDisablesEveryKind() {
        let model = EditGateModel(gate: gate(locksAll: true))
        for entity in EditedEntity.allCases {
            #expect(!model.allows(entity), "\(entity) must be locked")
        }
    }

    /// A feature of an entity this build does not have locks nothing here,
    /// and the user is still told to update: they are missing data this build
    /// cannot show.
    @Test
    func aLockThatCoversNothingHereStillShowsTheBanner() {
        let model = EditGateModel(gate: gate(missing: [
            MissingFeatureItem(feature: "place.entity", lock: .nothing(entity: "place")),
        ]))
        #expect(model.isReadOnly)
        #expect(model.allows(.task))
    }
}
