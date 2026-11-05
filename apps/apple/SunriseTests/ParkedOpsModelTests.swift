import Foundation
import Testing

@testable import Sunrise

/// The parked-change count, against a real vault over the real seam.
///
/// A fresh vault has parked nothing, so the real-core assertion is the empty
/// answer; the counts themselves are pinned in Rust, where a test can park an
/// op (`sunrise-core`'s `an_op_an_older_build_parked_is_materialized_…` and the
/// bindings' `parked_op_counts_cross_the_seam_by_reason`).
@MainActor
struct ParkedOpsModelTests {
    @Test
    func aFreshVaultHasNothingParked() async throws {
        let vault = try await TestVault()
        let model = ParkedOpsModel()
        #expect(model.rows == nil, "nothing is claimed before the first answer")
        await model.refresh(from: vault.bridge)

        #expect(model.errorMessage == nil)
        #expect(model.rows == [])
        await vault.bridge.shutdown()
    }

    @Test
    func knownReasonsAreWordedAndUnknownOnesKeepTheirName() {
        let rows = [
            ParkedReasonCount(reason: "unknown_kind", count: 3),
            ParkedReasonCount(reason: "a_reason_from_a_newer_build", count: 1)
        ].map(ParkedOpsModel.row)

        #expect(rows.map(\.label) == ["From a newer version of Sunrise", "a_reason_from_a_newer_build"])
        #expect(rows.map(\.count) == [3, 1])
        #expect(ParkedOpsModel.caption(for: rows) != ParkedOpsModel.caption(for: []))
    }

    /// The reason #504's receive path parks a fingerprint-mismatched op under
    /// (`ParkReason::SchemaFpMismatch`) is worded, not shown raw, and the
    /// caption does not promise that any update retries it: only a build
    /// with a different `DOC_SCHEMA_V` does.
    @Test
    func aSchemaFingerprintMismatchIsWordedAndTheCaptionPromisesNoRetryOnEveryUpdate() {
        let rows = [ParkedReasonCount(reason: "schema_fp_mismatch", count: 2)].map(ParkedOpsModel.row)

        #expect(rows.map(\.label) == ["Read differently by this version of Sunrise"])
        #expect(rows.map(\.count) == [2])
        let caption = ParkedOpsModel.caption(for: rows)
        #expect(!caption.contains("each update"))
        #expect(caption.contains("changes the format this device reads"))
    }

    /// The reason the receive path parks an op nested past
    /// `sunrise_cbor::MAX_DOC_DEPTH` under (`ParkReason::TooDeep`) is worded,
    /// not shown raw.
    @Test
    func anOpNestedTooDeepIsWorded() {
        let rows = [ParkedReasonCount(reason: "too_deep", count: 1)].map(ParkedOpsModel.row)

        #expect(rows.map(\.label) == ["Nested deeper than this version of Sunrise reads"])
        #expect(rows.map(\.count) == [1])
    }
}
