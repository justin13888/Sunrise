import Foundation

/// The words on every leg of the pairing script.
///
/// Split from ``PairingModel`` because they are a different kind of thing: what
/// the model *is* — a state machine over a seam — barely changes, and this is
/// prose that gets rewritten whenever somebody watches a user get stuck. A file
/// each keeps a copy edit out of the diff that changes the protocol.
///
/// The two methods are internal rather than private only because Swift's
/// `private` is file-scoped and their one caller is now in a different file.
extension PairingModel {
    /// Text this device has produced that the other one needs.
    struct HandOff: Equatable {
        let leg: Leg
        let title: String
        let instruction: String
        let text: String
        /// Only the QR payload is drawn as a code. Every other message is
        /// bigger than a screen-readable symbol and is copied, not scanned.
        let drawsCode: Bool
    }

    /// Text the other device is showing that this one needs.
    struct Prompt: Equatable {
        let leg: Leg
        let title: String
        let instruction: String
    }

    /// The code, on the device being added, when the relay carries the rest.
    ///
    /// The text under it is still there: pasting it is the fallback for a Mac
    /// with no camera, or a user who would rather not grant one.
    func relayCodeHandOff(_ payload: String) -> HandOff {
        HandOff(
            leg: .code,
            title: L10n.Pairing.Legs.relayCodeTitle,
            instruction: L10n.Pairing.Legs.relayCodeInstruction,
            text: payload,
            drawsCode: true
        )
    }

    var doneSummary: String {
        intent == .addThisMac
            ? L10n.Pairing.doneJoined(device: Platform.deviceName)
            : L10n.Pairing.doneSponsored
    }

    /// What `.working` says while the last messages cross.
    var finishingLabel: String {
        intent == .addThisMac
            ? L10n.Pairing.finishingJoining(device: Platform.deviceName)
            : L10n.Pairing.finishingSponsoring
    }

    static var reachingRelay: String { L10n.Pairing.reachingRelay }
    static var connecting: String { L10n.Pairing.connecting }

    // MARK: - Why this pairing is copy and paste

    static var noRelayConfigured: String {
        L10n.Pairing.Notice.noRelay(device: Platform.deviceName)
    }

    static var notSignedIn: String {
        L10n.Pairing.Notice.notSignedIn(device: Platform.deviceName)
    }

    /// Why the device with the vault could not join the pairing a code names,
    /// and the way out.
    ///
    /// A code drawn for copy and paste reads exactly like a relay code, so
    /// scanning one over the relay reaches a pairing that never existed, and
    /// the seam's own words for that ("start again from a new code") would
    /// send the user round the same rescan.
    static func relayCodeFailed(_ error: any Error) -> String {
        // The seam's own message, then the way out as its own paragraph.
        "\(error.localizedDescription)\n\n\(L10n.Pairing.Notice.relayCodeFailed)"
    }

    static func relayDidNotAnswer(_ relayURL: String) -> String {
        L10n.Pairing.Notice.relayDidNotAnswer(url: relayURL)
    }

    static func relayDidNotAnswerHere(_ relayURL: String) -> String {
        L10n.Pairing.Notice.relayDidNotAnswerHere(url: relayURL, device: Platform.deviceName)
    }

    static func relayRefused(_ error: any Error) -> String {
        L10n.Pairing.Notice.relayRefused(error: error.localizedDescription)
    }

    func handOff(for leg: Leg, text: String) -> HandOff {
        switch leg {
        case .code:
            HandOff(
                leg: leg,
                title: L10n.Pairing.Legs.codeTitle,
                instruction: L10n.Pairing.Legs.codeInstruction,
                text: text,
                drawsCode: true
            )
        case .first, .second, .third:
            HandOff(
                leg: leg,
                title: L10n.Pairing.Legs.messageTitle,
                instruction: L10n.Pairing.Legs.messageInstruction,
                text: text,
                drawsCode: false
            )
        case .compare:
            HandOff(leg: leg, title: "", instruction: "", text: text, drawsCode: false)
        case .offer:
            HandOff(
                leg: leg,
                title: L10n.Pairing.Legs.offerTitle,
                instruction: L10n.Pairing.Legs.offerInstruction,
                text: text,
                drawsCode: false
            )
        case .request:
            HandOff(
                leg: leg,
                title: L10n.Pairing.Legs.requestTitle,
                instruction: L10n.Pairing.Legs.requestInstruction(device: Platform.deviceName),
                text: text,
                drawsCode: false
            )
        case .grant:
            HandOff(
                leg: leg,
                title: L10n.Pairing.Legs.grantTitle,
                instruction: L10n.Pairing.Legs.grantInstruction,
                text: text,
                drawsCode: false
            )
        }
    }

    static func prompt(for leg: Leg) -> Prompt {
        switch leg {
        case .code:
            Prompt(
                leg: leg,
                title: L10n.Pairing.Legs.promptCodeTitle,
                instruction: L10n.Pairing.Legs.promptCodeInstruction
            )
        case .first, .second, .third:
            Prompt(
                leg: leg,
                title: L10n.Pairing.Legs.promptMessageTitle,
                instruction: L10n.Pairing.Legs.promptMessageInstruction
            )
        case .compare:
            Prompt(leg: leg, title: "", instruction: "")
        case .offer:
            Prompt(
                leg: leg,
                title: L10n.Pairing.Legs.promptOfferTitle,
                instruction: L10n.Pairing.Legs.promptOfferInstruction
            )
        case .request:
            Prompt(
                leg: leg,
                title: L10n.Pairing.Legs.promptRequestTitle,
                instruction: L10n.Pairing.Legs.promptRequestInstruction
            )
        case .grant:
            Prompt(
                leg: leg,
                title: L10n.Pairing.Legs.promptGrantTitle,
                instruction: L10n.Pairing.Legs.promptGrantInstruction
            )
        }
    }
}

/// The failures that are this screen's, not the seam's.
enum PairingUIError: Error, Equatable, LocalizedError {
    case noSession
    case noPayload
    case noOpenVault

    var errorDescription: String? {
        switch self {
        case .noSession:
            L10n.Pairing.noSession
        case .noPayload:
            L10n.Pairing.noPayload
        case .noOpenVault:
            L10n.Pairing.noOpenVault
        }
    }
}
