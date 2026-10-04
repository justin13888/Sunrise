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
            title: "Scan this with the device that has your vault",
            instruction: """
                On that device, open Settings › Vaults › Add a device and \
                choose Scan, or paste the text below there. The code works \
                for five minutes; this screen moves on by itself once it has \
                been read.
                """,
            text: payload,
            drawsCode: true
        )
    }

    var doneSummary: String {
        intent == .addThisMac
            ? "This \(Platform.deviceName) is paired. Your vault is open here."
            : "The other device has a certificate from your account and a copy of your vault key."
    }

    /// What `.working` says while the last messages cross.
    var finishingLabel: String {
        intent == .addThisMac
            ? "Opening your vault on this \(Platform.deviceName)…"
            : "Sealing your vault key for the other device…"
    }

    static let reachingRelay = "Reaching your relay…"
    static let connecting = "Connecting to the other device through your relay…"

    // MARK: - Why this pairing is copy and paste

    static var noRelayConfigured: String {
        """
        This \(Platform.deviceName) has no relay set up, so this pairing runs by \
        copy and paste. Add your relay under Settings › Sync to pair by scanning \
        a code instead.
        """
    }

    static var notSignedIn: String {
        """
        Pairing over your relay needs this \(Platform.deviceName) signed in to \
        your account, so this pairing runs by copy and paste.
        """
    }

    static func relayDidNotAnswer(_ relayURL: String) -> String {
        """
        Your relay at \(relayURL) did not answer, so this pairing runs by copy \
        and paste. The other device should choose “Copy and paste instead” too.
        """
    }

    static func relayDidNotAnswerHere(_ relayURL: String) -> String {
        """
        Your relay at \(relayURL) did not answer from this \(Platform.deviceName), \
        so pair by copy and paste: on the device you are adding, choose “Copy and \
        paste instead”, then scan or paste the new code it shows.
        """
    }

    static func relayRefused(_ error: any Error) -> String {
        """
        Your relay could not start this pairing (\(error.localizedDescription)), \
        so it runs by copy and paste. The other device should choose “Copy and \
        paste instead” too.
        """
    }

    func handOff(for leg: Leg, text: String) -> HandOff {
        switch leg {
        case .code:
            HandOff(
                leg: leg,
                title: "Show this to the device that has your vault",
                instruction: """
                    Scan the code, or copy the text below and paste it into \
                    Settings › Vaults › Add a device on your other device — \
                    the one that already has your vault.
                    """,
                text: text,
                drawsCode: true
            )
        case .first, .second, .third:
            HandOff(
                leg: leg,
                title: "Copy this to the other device",
                instruction: """
                    Paste it into the field the other device is showing, then \
                    come back here and continue.
                    """,
                text: text,
                drawsCode: false
            )
        case .compare:
            HandOff(leg: leg, title: "", instruction: "", text: text, drawsCode: false)
        case .offer:
            HandOff(
                leg: leg,
                title: "Copy this to the device you are adding",
                instruction: """
                    This says who your account is. It carries no keys — the \
                    other device replies with keys of its own, and only then \
                    does anything of yours leave this one.
                    """,
                text: text,
                drawsCode: false
            )
        case .request:
            HandOff(
                leg: leg,
                title: "Copy this back to the device with your vault",
                instruction: """
                    This \(Platform.deviceName) has just made itself a pair of \
                    keys. Only the public halves are in this block; the private \
                    ones stay here and are never sent anywhere.
                    """,
                text: text,
                drawsCode: false
            )
        case .grant:
            HandOff(
                leg: leg,
                title: "Copy this last block to the other device",
                instruction: """
                    This is the certificate admitting that device to your \
                    account, and your vault key, sealed so that only the device \
                    whose digits you just confirmed can open it. Anything it \
                    passes through on the way — a message, a clipboard, a relay \
                    — sees nothing usable.
                    """,
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
                title: "Scan the code on the device you are adding",
                instruction: """
                    That device is showing a QR code with the same text \
                    underneath it. Scan it with the camera, or paste the text \
                    here.
                    """
            )
        case .first, .second, .third:
            Prompt(
                leg: leg,
                title: "Paste what the other device is showing",
                instruction: "Copy the block from the other device's screen and paste it here."
            )
        case .compare:
            Prompt(leg: leg, title: "", instruction: "")
        case .offer:
            Prompt(
                leg: leg,
                title: "Paste the block that names the account",
                instruction: """
                    The other device is showing a block that says which account \
                    you are joining. Pasting it makes this device a pair of keys \
                    for that account to certify.
                    """
            )
        case .request:
            Prompt(
                leg: leg,
                title: "Paste the keys from the device you are adding",
                instruction: """
                    That device replied with the public half of the keys it just \
                    made. Pasting them here signs a certificate for them.
                    """
            )
        case .grant:
            Prompt(
                leg: leg,
                title: "Paste the sealed certificate and key",
                instruction: """
                    The other device is showing one last block, now that you have \
                    both confirmed the digits. It is the only thing in this \
                    whole exchange that carries your vault key.
                    """
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
            "This pairing is over. Start it again from the beginning."
        case .noPayload:
            "Sunrise could not produce a pairing code for this device."
        case .noOpenVault:
            "There is no open vault on this device to share."
        }
    }
}
