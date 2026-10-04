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

    /// `refresh` crosses the seam into `SunriseCore::edit_gate()` over a real
    /// vault and replaces what the model held. The model starts locked, so the
    /// only way it ends open is the core's answer: a fresh vault requires no
    /// feature.
    @Test
    func refreshReplacesTheGateWithTheCoresAnswer() async throws {
        let vault = try await TestVault()
        let model = EditGateModel(gate: gate(locksAll: true))
        #expect(!model.allows(.task))

        await model.refresh(from: vault.bridge)

        #expect(!model.isReadOnly)
        for entity in EditedEntity.allCases {
            #expect(model.allows(entity))
        }
        await vault.bridge.shutdown()
    }

    /// `follow` reads the gate as it starts, keeps re-reading on the change
    /// feed, and returns once the vault closes rather than outliving it.
    @Test
    func followReadsTheGateAndEndsWhenTheVaultCloses() async throws {
        let vault = try await TestVault()
        let model = EditGateModel(gate: gate(lockedTags: ["task"]))
        let ended = Flag()

        let following = Task {
            await model.follow(vault.bridge)
            ended.raise()
        }
        defer { following.cancel() }

        try await until { !model.isReadOnly }
        #expect(model.allows(.task))

        // A write announces a change batch; the re-read it triggers must
        // leave the open answer in place.
        _ = try await vault.bridge.submit(.createTask(draft: draft("Still editable")))
        #expect(model.allows(.task))

        await vault.bridge.shutdown()
        try await until { ended.isRaised }
    }

    /// Poll until `condition` holds, or fail rather than hang.
    private func until(
        _ condition: @MainActor () -> Bool,
        timeout: Duration = .seconds(3)
    ) async throws {
        let deadline = ContinuousClock.now.advanced(by: timeout)
        while ContinuousClock.now < deadline {
            if condition() { return }
            try await Task.sleep(for: .milliseconds(10))
        }
        Issue.record("condition never became true within \(timeout)")
    }

    private func draft(_ title: String) -> TaskDraftIn {
        TaskDraftIn(
            title: title,
            body: nil,
            streamId: nil,
            contexts: [],
            priority: nil,
            energy: nil,
            estimatedDurationS: nil,
            scheduledAt: nil,
            dueAt: nil,
            schedulingConstraints: [],
            assignee: nil,
            reminderLeadS: nil
        )
    }
}

/// Set once from the task running `follow`, read from the test.
@MainActor
private final class Flag {
    private(set) var isRaised = false
    func raise() { isRaised = true }
}
