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
                title: "Paste the code from the device you are adding",
                instruction: """
                    That device is showing a QR code with the same text \
                    underneath it. Paste the text here.
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
