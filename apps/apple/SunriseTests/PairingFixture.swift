import Foundation
import Testing

/// One real, canonically encoded `PairingPayload`, for tests that need to hand
/// the seam a payload it will actually accept.
///
/// `DevicePairing.openPairingPayload` and `SunriseCore.open` both *decode* what
/// they are given, and the decoder refuses a payload whose `identity_id` is not
/// the id derived from its own `ID_S_pub`. So this cannot be a `Data(count:)`
/// stand-in, and Swift has no constructor for the type — a payload is built
/// inside the core, out of a vault, and a unit test has no vault.
///
/// Produced by `sunrise_pairing::encode_pairing_payload` over `ID_S_priv` =
/// 32 x 0x11, `ID_D_pub` = X25519(32 x 0x22), one Stream key (the vault-meta
/// stream, epoch 1, 32 x 0xCD), and `vault_root` = 32 x 0xAB. Regenerate it if
/// the payload's CDDL changes; a stale one fails loudly here rather than
/// quietly somewhere else — which is what happened when key 2, `ID_D_priv`, was
/// burned: the decoder refuses a payload still carrying it, so the previous
/// fixture stopped decoding and said so.
enum PairingFixture {
    /// The 32-byte vault root inside ``payload()``.
    static let vaultRoot = Data(repeating: 0xAB, count: 32)

    static let base64 =
        "qAFYIBERERERERERERERERERERERERERERERERERERERERERA1gg0EqyMnQrtKs6E2i9RhXk"
            + "5tAiSrcaAWuvhSCjMsl3hzcEWCAPqmhO0ohnuX9Kai3uXfjOl052twGOPyKhxM8meFcPIAVQ"
            + "iJdtw4+iQtfJ4/xYmI9WDQahUAAAAAAAAAAAAAAAAAAAAAChAVggzc3Nzc3Nzc3Nzc3Nzc3N"
            + "zc3Nzc3Nzc3Nzc3Nzc3Nzc0HZ2ZpeHR1cmUIZHRlc3QJWCCrq6urq6urq6urq6urq6urq6ur"
            + "q6urq6urq6urq6urqw=="

    /// The fixture as bytes. `#require` rather than `!`, so a mistyped literal
    /// is a test failure that names itself and not a crash in the suite.
    static func payload() throws -> Data {
        try #require(Data(base64Encoded: base64))
    }
}
