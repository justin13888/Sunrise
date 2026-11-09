import Foundation
import Testing

@testable import Sunrise

/// The route from the Lock Screen capture widget's intent to the capture field.
///
/// Every test here is synchronous on the main actor, so no other test can run
/// between its steps and touch the one shared request. Each leaves the
/// request clear and the presenter as it found it.
@MainActor
struct QuickCaptureRequestTests {
    /// A request the app can serve is served at once and leaves nothing
    /// waiting.
    @Test
    func aServedRequestIsNotKept() {
        let saved = QuickCaptureRequest.presenter
        defer { QuickCaptureRequest.presenter = saved }
        var presented = 0
        QuickCaptureRequest.presenter = {
            presented += 1
            return true
        }

        QuickCaptureRequest.post()

        #expect(presented == 1)
        #expect(!QuickCaptureRequest.takePending())
    }

    /// With no field to show yet — or no app listening at all — the request
    /// waits, and is taken exactly once.
    @Test
    func anUnservedRequestWaitsToBeTakenOnce() {
        let saved = QuickCaptureRequest.presenter
        defer { QuickCaptureRequest.presenter = saved }

        QuickCaptureRequest.presenter = { false }
        QuickCaptureRequest.post()
        #expect(QuickCaptureRequest.takePending())
        #expect(!QuickCaptureRequest.takePending())

        QuickCaptureRequest.presenter = nil
        QuickCaptureRequest.post()
        #expect(QuickCaptureRequest.takePending())
    }

    /// Surfaces with no vault cannot show the field, so they keep the request
    /// rather than refusing it.
    @Test
    func surfacesWithNoVaultKeepTheRequest() {
        let saved = QuickCaptureRequest.presenter
        defer { QuickCaptureRequest.presenter = saved }
        let surfaces = AppSurfaces()

        QuickCaptureRequest.post()

        #expect(!surfaces.presentRequestedCapture())
        #expect(QuickCaptureRequest.takePending())
    }

    #if os(iOS)
    /// A widget tapped before the vault opened opens the capture sheet as
    /// soon as it does — the cold launch from the Lock Screen.
    @Test
    func attachingAVaultAnswersAWaitingRequest() async throws {
        let vault = try await TestVault()
        let saved = QuickCaptureRequest.presenter
        defer { QuickCaptureRequest.presenter = saved }
        let surfaces = AppSurfaces()
        // No suspension point between these two lines, so no other test's
        // surfaces can take the request first.
        QuickCaptureRequest.post()
        surfaces.attach(bridge: vault.bridge)

        #expect(surfaces.isCapturing)
        #expect(!QuickCaptureRequest.takePending())
        surfaces.detach()
        await vault.bridge.shutdown()
    }
    #endif
}
