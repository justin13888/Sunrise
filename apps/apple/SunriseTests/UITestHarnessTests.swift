#if DEBUG
import Testing

@testable import Sunrise

/// The guard that keeps ``UITestHarness/failCaptureFlag`` away from a real
/// vault: the flag refuses captures only in a launch that also names a scratch
/// vault, so a stray argument on a real launch cannot throw captures away.
struct UITestHarnessTests {
    private let scratch = [UITestHarness.flag, "/tmp/sunrise-ui-test-scratch"]

    @Test func theFailCaptureFlagWithoutAScratchVaultRefusesNothing() throws {
        let arguments = ["Sunrise", UITestHarness.failCaptureFlag]
        #expect(!UITestHarness.failsQuickCapture(arguments: arguments))
        try UITestHarness.refuseQuickCaptureIfAsked(arguments: arguments)
    }

    @Test func aScratchVaultFlagWithNoPathIsNoScratchVault() throws {
        let arguments = ["Sunrise", UITestHarness.failCaptureFlag, UITestHarness.flag]
        #expect(!UITestHarness.failsQuickCapture(arguments: arguments))
        try UITestHarness.refuseQuickCaptureIfAsked(arguments: arguments)
    }

    @Test func aScratchVaultWithoutTheFlagRefusesNothing() throws {
        let arguments = ["Sunrise"] + scratch
        #expect(!UITestHarness.failsQuickCapture(arguments: arguments))
        try UITestHarness.refuseQuickCaptureIfAsked(arguments: arguments)
    }

    @Test func theFlagWithAScratchVaultRefusesEveryCapture() {
        let arguments = ["Sunrise"] + scratch + [UITestHarness.failCaptureFlag]
        #expect(UITestHarness.failsQuickCapture(arguments: arguments))
        #expect(throws: RefusedByUITestHarness.self) {
            try UITestHarness.refuseQuickCaptureIfAsked(arguments: arguments)
        }
    }
}
#endif
