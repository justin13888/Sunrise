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
/// 32 x 0x11, `ID_D_priv` = 32 x 0x22, one Stream key (the vault-meta stream,
/// epoch 1, 32 x 0xCD), and `vault_root` = 32 x 0xAB. Regenerate it if the
/// payload's CDDL changes; a stale one fails loudly here rather than quietly
/// somewhere else.
enum PairingFixture {
    /// The 32-byte vault root inside ``payload()``.
    static let vaultRoot = Data(repeating: 0xAB, count: 32)

    static let base64 =
        "qQFYIBERERERERERERERERERERERERERERERERERERERERERAlggIiIiIiIiIiIiIiIiIiIi"
            + "IiIiIiIiIiIiIiIiIiIiIiIDWCDQSrIydCu0qzoTaL1GFeTm0CJKtxoBa6+FIKMyyXeHNwRY"
            + "IA+qaE7SiGe5f0pqLe5d+M6XTna3AY4/IqHEzyZ4Vw8gBVCIl23Dj6JC18nj/FiYj1YNBqFQ"
            + "AAAAAAAAAAAAAAAAAAAAAKEBWCDNzc3Nzc3Nzc3Nzc3Nzc3Nzc3Nzc3Nzc3Nzc3Nzc3NzQdn"
            + "Zml4dHVyZQhkdGVzdAlYIKurq6urq6urq6urq6urq6urq6urq6urq6urq6urq6ur"

    /// The fixture as bytes. `#require` rather than `!`, so a mistyped literal
    /// is a test failure that names itself and not a crash in the suite.
    static func payload() throws -> Data {
        try #require(Data(base64Encoded: base64))
    }
}
