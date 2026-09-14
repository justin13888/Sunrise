import Foundation
import Testing

/// One real, canonically encoded `PairingPayload`, for tests that need to hand
/// the seam a payload it will actually accept.
///
/// `SunriseCore.open` *decodes* what it is given, and refuses a payload whose
/// `identity_id` is not the id derived from its own `ID_S_pub`, or whose
/// `DeviceCert` does not verify under that key and name the device keys beside
/// it. So this cannot be a `Data(count:)` stand-in, and Swift has no
/// constructor for the type — a payload is assembled out of a completed
/// pairing, and a unit test has no second device.
///
/// Regenerate it with
///
///     cargo test -p sunrise-pairing --lib -- --ignored --nocapture apple_fixture
///
/// which is the generator that produced it, over `ID_S_priv` = 32 × 0x11,
/// `ID_D_pub` = X25519(32 × 0x22), `D_S_priv` = 32 × 0x33, `D_D_priv` =
/// 32 × 0x44, one Stream key (the vault-meta stream, epoch 1, 32 × 0xCD), and
/// `vault_root` = 32 × 0xAB. A stale one fails loudly here rather than quietly
/// somewhere else — which is what happened twice: when key 2, `ID_D_priv`, was
/// burned, and again when key 1, `ID_S_priv`, was (#105) and fields 12–14
/// arrived to carry this device's own keys and the certificate its sponsor
/// signed for them.
enum PairingFixture {
    /// The 32-byte vault root inside ``payload()``.
    static let vaultRoot = Data(repeating: 0xAB, count: 32)

    static let base64 =
        "qgNYINBKsjJ0K7SrOhNovUYV5ObQIkq3GgFrr4UgozLJd4c3BFggD6poTtKIZ7l/Smot7l34"
            + "zpdOdrcBjj8iocTPJnhXDyAFUIiXbcOPokLXyeP8WJiPVg0GoVAAAAAAAAAAAAAAAAAAAAAA"
            + "oQFYIM3Nzc3Nzc3Nzc3Nzc3Nzc3Nzc3Nzc3Nzc3Nzc3Nzc3NCVggq6urq6urq6urq6urq6ur"
            + "q6urq6urq6urq6urq6urq6sKUIiXbcOPokLXyeP8WJiPVg0LWCDQSrIydCu0qzoTaL1GFeTm"
            + "0CJKtxoBa6+FIKMyyXeHNwxYIDMzMzMzMzMzMzMzMzMzMzMzMzMzMzMzMzMzMzMzMzMzDVgg"
            + "REREREREREREREREREREREREREREREREREREREREREQOWMuiAagBAQJQcOvMRpf/oBT/eOlZ"
            + "KAVttwNYIBfLefsrQSDysexl5BmNbgiyjoE/6wHkpACDm4XhgIDOBFgg/y7kVgHsG2cxDHeQ"
            + "QEWFrmlzMe7hwfjPJBlzHB//PmsFUIiXbcOPokLXyeP8WJiPVg0GGwAAAYvP5WgAB2dmaXh0"
            + "dXJlCGR0ZXN0AlhA35Ys2QnwL/rEafgN6eqvJCB7exLUXnwClj1HOyPpJRlutf1uQkmj3f68"
            + "VGsnP6TxVlbArRRKgIzy6NS6KHqPBg=="

    /// The fixture as bytes. `#require` rather than `!`, so a mistyped literal
    /// is a test failure that names itself and not a crash in the suite.
    static func payload() throws -> Data {
        try #require(Data(base64Encoded: base64))
    }
}
