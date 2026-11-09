import Foundation

/// How the capture widget's ``OpenCaptureIntent`` reaches the capture field.
///
/// Split out of ``AppSurfaces``, which sits at SwiftLint's `file_length`
/// limit, so that `AppSurfaces` carries only the two calls into it: one from
/// `init`, one at the end of `attach(bridge:)`.
extension AppSurfaces {
    /// Make these surfaces the ones ``QuickCaptureRequest`` opens the field on.
    /// Called from `init`.
    func installCaptureRequestPresenter() {
        QuickCaptureRequest.presenter = { [weak self] in self?.presentRequestedCapture() ?? false }
    }

    /// Answer a capture widget tapped before a vault was open (a cold launch).
    /// Called at the end of `attach(bridge:)`, once the field exists.
    func answerPendingCaptureRequest() {
        if QuickCaptureRequest.takePending() { openQuickCapture() }
    }

    /// Open quick capture for ``QuickCaptureRequest``, or return `false` with
    /// no refusal feedback: the request is kept, and `attach` answers it.
    func presentRequestedCapture() -> Bool {
        guard capture != nil else { return false }
        openQuickCapture()
        return true
    }
}
