import AVFoundation
import CoreGraphics
import SwiftUI
import Vision
#if os(iOS)
import VisionKit
#endif

/// Reads a pairing code off the other device's screen.
///
/// # Two cameras, one answer
///
/// - **iOS** uses VisionKit's `DataScannerViewController`, which owns the
///   preview, the focus and the highlight, and hands back a barcode's text.
/// - **macOS** has no `DataScannerViewController`, so it runs an
///   `AVCaptureSession` over the system's preferred camera — built in,
///   external, or an iPhone over Continuity Camera — and asks Vision for a QR
///   code in every few frames.
///
/// Either way the result is the string the symbol encodes, handed to the
/// pairing model exactly as read: the seam decodes it, so a client never
/// parses a pairing QR and cannot disagree with the encoder about one.
///
/// # Permission
///
/// Asked for only when the user chooses Scan, never on opening the sheet: a
/// pairing that is pasted needs no camera, and a prompt nobody asked for is
/// the one most often refused. A refusal is not a dead end — the scanner
/// shows why and offers the paste field in one tap.
enum QRDecoder {
    /// The first non-empty QR payload Vision finds through `handler`.
    static func firstPayload(_ handler: VNImageRequestHandler) -> String? {
        let request = VNDetectBarcodesRequest()
        request.symbologies = [.qr]
        do {
            try handler.perform([request])
        } catch {
            return nil
        }
        return request.results?.lazy
            .compactMap(\.payloadStringValue)
            .first { !$0.isEmpty }
    }

    static func firstPayload(in image: CGImage) -> String? {
        firstPayload(VNImageRequestHandler(cgImage: image, options: [:]))
    }

    static func firstPayload(in buffer: CVPixelBuffer) -> String? {
        firstPayload(VNImageRequestHandler(cvPixelBuffer: buffer, options: [:]))
    }
}

/// Where a scan reads from.
enum QRScanSource {
    /// The device's camera, behind the system's permission prompt.
    case camera
    /// A fixed picture, decoded by the same ``QRDecoder`` a camera frame is.
    /// The stub capture source tests and the UI-test harness inject a code
    /// through.
    case still(CGImage)
    /// A camera the user has refused: the harness's stand-in for the system's
    /// answer, which a UI test cannot give.
    case refused

    /// What this launch scans with: the camera, unless a UI test said
    /// otherwise.
    @MainActor
    static var current: QRScanSource {
        #if DEBUG
        if let injected = UITestHarness.scanSource() { return injected }
        #endif
        return .camera
    }
}

/// The camera permission, asked for at the moment it is needed.
enum CameraPermission {
    /// Whether the camera may be used, prompting only if the system has never
    /// asked. `.denied` and `.restricted` are answers, not prompts: the system
    /// will not show the dialog twice, so the scanner says where to change it.
    static func request() async -> Bool {
        switch AVCaptureDevice.authorizationStatus(for: .video) {
        case .authorized: true
        case .notDetermined: await AVCaptureDevice.requestAccess(for: .video)
        default: false
        }
    }
}

/// The scan sheet.
struct QRScannerView: View {
    let source: QRScanSource
    /// A payload was read. The caller closes the sheet and submits it.
    let found: (String) -> Void
    /// The user wants the paste field instead. Closes the sheet.
    let pasteInstead: () -> Void

    private enum Status: Equatable {
        case starting
        case scanning
        case refused
        case noCamera
    }

    @State private var status: Status = .starting

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Scan the pairing code")
                .font(.title3.weight(.semibold))
            viewfinder
                .frame(maxWidth: .infinity, minHeight: 240)
            HStack {
                Spacer()
                Button("Paste the code instead", systemImage: "doc.on.clipboard") {
                    pasteInstead()
                }
                .accessibilityIdentifier("pairing.scan.pasteInstead")
            }
            .controlSize(.large)
        }
        .padding(24)
        .macSheetFrame(width: 520, height: 440)
        .task { await start() }
    }

    @ViewBuilder
    private var viewfinder: some View {
        switch status {
        case .starting:
            ProgressView()
        case .scanning:
            scanningView
                .clipShape(RoundedRectangle(cornerRadius: 10))
                .accessibilityIdentifier("pairing.scan.viewfinder")
        case .refused:
            explanation(
                symbol: "video.slash",
                title: "Sunrise cannot use the camera",
                detail: """
                    Camera access is off for Sunrise. Paste the text shown under \
                    the code instead, or allow the camera in System Settings › \
                    Privacy & Security › Camera and choose Scan again.
                    """
            )
            .accessibilityIdentifier("pairing.scan.refused")
        case .noCamera:
            explanation(
                symbol: "camera.metering.unknown",
                title: "There is no camera here that can scan",
                detail: "Paste the text shown under the code instead."
            )
            .accessibilityIdentifier("pairing.scan.noCamera")
        }
    }

    @ViewBuilder
    private var scanningView: some View {
        switch source {
        case let .still(image):
            Image(decorative: image, scale: 1)
                .interpolation(.none)
        case .camera:
            #if os(iOS)
            DataScannerView(found: found)
            #else
            CameraScannerView(found: found)
            #endif
        case .refused:
            EmptyView()
        }
    }

    private func explanation(symbol: String, title: String, detail: String) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Image(systemName: symbol)
                .font(.system(size: 36))
                .foregroundStyle(.orange)
            Text(title).font(.headline)
            Text(detail)
                .font(.callout)
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func start() async {
        switch source {
        case .refused:
            status = .refused
        case let .still(image):
            status = .scanning
            if let payload = QRDecoder.firstPayload(in: image) {
                found(payload)
            } else {
                status = .noCamera
            }
        case .camera:
            guard await CameraPermission.request() else {
                status = .refused
                return
            }
            #if os(iOS)
            guard DataScannerViewController.isSupported, DataScannerViewController.isAvailable else {
                status = .noCamera
                return
            }
            #else
            guard CameraQRCapture.camera() != nil else {
                status = .noCamera
                return
            }
            #endif
            status = .scanning
        }
    }
}

#if os(iOS)
/// VisionKit's scanner, reporting the first QR code it recognises.
private struct DataScannerView: UIViewControllerRepresentable {
    let found: (String) -> Void

    func makeUIViewController(context: Context) -> DataScannerViewController {
        let scanner = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr])],
            qualityLevel: .balanced,
            recognizesMultipleItems: false,
            isHighFrameRateTrackingEnabled: false,
            isHighlightingEnabled: true
        )
        scanner.delegate = context.coordinator
        return scanner
    }

    func updateUIViewController(_ scanner: DataScannerViewController, context: Context) {
        context.coordinator.found = found
        if !scanner.isScanning, !context.coordinator.delivered {
            try? scanner.startScanning()
        }
    }

    static func dismantleUIViewController(_ scanner: DataScannerViewController, coordinator: Coordinator) {
        scanner.stopScanning()
    }

    func makeCoordinator() -> Coordinator { Coordinator(found: found) }

    @MainActor
    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        var found: (String) -> Void
        /// One payload per sheet: a code held in view is recognised again on
        /// every frame, and the pairing must be started once.
        var delivered = false

        init(found: @escaping (String) -> Void) {
            self.found = found
        }

        func dataScanner(
            _ dataScanner: DataScannerViewController,
            didAdd addedItems: [RecognizedItem],
            allItems: [RecognizedItem]
        ) {
            guard !delivered else { return }
            for case let .barcode(code) in addedItems {
                guard let payload = code.payloadStringValue, !payload.isEmpty else { continue }
                delivered = true
                dataScanner.stopScanning()
                found(payload)
                return
            }
        }
    }
}
#else
/// A Mac's camera, read by Vision.
private struct CameraScannerView: NSViewRepresentable {
    let found: (String) -> Void

    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        view.wantsLayer = true
        let preview = AVCaptureVideoPreviewLayer(session: context.coordinator.capture.session)
        preview.videoGravity = .resizeAspectFill
        view.layer = preview
        context.coordinator.capture.start()
        return view
    }

    func updateNSView(_ view: NSView, context: Context) {}

    static func dismantleNSView(_ view: NSView, coordinator: Coordinator) {
        coordinator.capture.stop()
    }

    func makeCoordinator() -> Coordinator {
        Coordinator(found: found)
    }

    @MainActor
    final class Coordinator {
        let capture: CameraQRCapture

        init(found: @escaping (String) -> Void) {
            capture = CameraQRCapture { payload in found(payload) }
        }
    }
}

/// An `AVCaptureSession` whose frames go to ``QRDecoder``.
///
/// `@unchecked Sendable` because AVFoundation calls the delegate on the queue
/// this class owns, and every mutable field is behind `lock`.
final class CameraQRCapture: NSObject, AVCaptureVideoDataOutputSampleBufferDelegate,
    @unchecked Sendable {
    let session = AVCaptureSession()
    private let queue = DispatchQueue(label: "dev.sunrise.pairing.qr-capture")
    private let lock = NSLock()
    private var delivered = false
    private var frames = 0
    private let found: @MainActor (String) -> Void

    /// Every few frames rather than every one: a code on a screen does not
    /// move, and Vision on each of thirty frames a second is heat for nothing.
    private static let stride = 6

    init(found: @escaping @MainActor (String) -> Void) {
        self.found = found
    }

    /// The camera the system prefers — which is the one the user picked in
    /// any app, Continuity Camera included — or the first one there is.
    static func camera() -> AVCaptureDevice? {
        if let preferred = AVCaptureDevice.systemPreferredCamera { return preferred }
        return AVCaptureDevice.DiscoverySession(
            deviceTypes: [.builtInWideAngleCamera, .external, .continuityCamera],
            mediaType: .video,
            position: .unspecified
        ).devices.first
    }

    func start() {
        queue.async { [self] in
            guard let device = Self.camera(),
                  let input = try? AVCaptureDeviceInput(device: device) else { return }
            session.beginConfiguration()
            if session.canAddInput(input) { session.addInput(input) }
            let output = AVCaptureVideoDataOutput()
            output.alwaysDiscardsLateVideoFrames = true
            output.setSampleBufferDelegate(self, queue: queue)
            if session.canAddOutput(output) { session.addOutput(output) }
            session.commitConfiguration()
            session.startRunning()
        }
    }

    func stop() {
        queue.async { [self] in
            if session.isRunning { session.stopRunning() }
        }
    }

    func captureOutput(
        _ output: AVCaptureOutput,
        didOutput sampleBuffer: CMSampleBuffer,
        from connection: AVCaptureConnection
    ) {
        guard shouldRead(), let buffer = CMSampleBufferGetImageBuffer(sampleBuffer),
              let payload = QRDecoder.firstPayload(in: buffer), claim() else { return }
        session.stopRunning()
        let found = found
        Task { @MainActor in found(payload) }
    }

    private func shouldRead() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        frames += 1
        return !delivered && frames % Self.stride == 0
    }

    /// True for the first payload only.
    private func claim() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard !delivered else { return false }
        delivered = true
        return true
    }
}
#endif

#if DEBUG
extension UITestHarness {
    /// The launch argument that puts a real pairing code in front of the
    /// scanner, as a still image (#464).
    ///
    /// A simulator has no camera, so the only way a UI test proves a scan
    /// reaches the pairing model is to give the scanner something to read. The
    /// code is minted the way a device being added mints one, drawn by
    /// ``QRCode`` and decoded by ``QRDecoder`` — the same symbol and the same
    /// reader a camera frame would go through.
    static let scanFixtureFlag = "-sunrise-ui-test-scan-fixture"

    /// The launch argument that answers the camera prompt with "Don't Allow".
    static let cameraRefusedFlag = "-sunrise-ui-test-camera-refused"

    /// The source a UI-test launch scans with, or `nil` for the camera.
    ///
    /// `nil` without a scratch vault, whatever else was passed, as every other
    /// harness switch is.
    static func scanSource(
        arguments: [String] = ProcessInfo.processInfo.arguments
    ) -> QRScanSource? {
        guard scratchVault(arguments: arguments) != nil else { return nil }
        if arguments.contains(cameraRefusedFlag) { return .refused }
        guard arguments.contains(scanFixtureFlag),
              let pairing = try? DevicePairing.offer(
                  relayUrl: "https://relay.example",
                  accountEmail: "ui-test@example.com"
              ),
              let payload = pairing.qrPayload(),
              let image = QRCode.cgImage(for: payload) else { return nil }
        return .still(image)
    }
}
#endif
