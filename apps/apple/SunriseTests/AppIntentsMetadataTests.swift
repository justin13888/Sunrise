import Foundation
import Testing

@testable import Sunrise

/// The App Intents *metadata bundle*, which is the difference between an
/// intent that exists and an intent the system will ever run.
///
/// `appintentsmetadataprocessor` writes `Metadata.appintents` into the app
/// while it builds. Nothing in Swift references it, so nothing in Swift would
/// notice it going missing — a project change dropping the AppIntents link
/// would leave every intent above compiling, passing, and invisible to
/// Spotlight, Shortcuts and Siri.
///
/// These tests are hosted by the app (`TEST_HOST`), so `Bundle.main` *is*
/// `Sunrise.app`.
struct AppIntentsMetadataTests {
    @Test
    func theBuiltAppCarriesItsIntentMetadata() throws {
        let bundle = Bundle.main
        try #require(
            bundle.bundleURL.pathExtension == "app",
            "expected to be hosted by Sunrise.app; got \(bundle.bundleURL.path)"
        )
        // The two platforms lay a bundle out differently: macOS nests
        // resources under `Contents/Resources/`, and an iOS bundle is flat.
        // The assertion — that `appintentsmetadataprocessor` actually ran and
        // left its output where the system will look — is the same on both.
        #if os(macOS)
        let metadata = bundle.bundleURL
            .appending(path: "Contents/Resources/Metadata.appintents")
        #else
        let metadata = bundle.bundleURL.appending(path: "Metadata.appintents")
        #endif
        #expect(
            FileManager.default.fileExists(atPath: metadata.path(percentEncoded: false)),
            """
            No Metadata.appintents in the built app: the intents will not be \
            offered by Spotlight, Shortcuts or Siri. Check that project.yml \
            still links AppIntents.framework on the Sunrise target.
            """
        )
    }
}
