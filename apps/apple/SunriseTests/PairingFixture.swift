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
/// Produced by `sunrise_pairing::encode_pairing_payload` over `ID_S_priv` =
/// 32 x 0x11, `ID_D_pub` = X25519(32 x 0x22), `D_S_priv` = 32 x 0x33,
/// `D_D_priv` = 32 x 0x44, one Stream key (the vault-meta stream, epoch 1,
/// 32 x 0xCD), `vault_root` = 32 x 0xAB, and `genesis_*` equal to the identity
/// in force — which is what an account that has never rotated looks like, and a
/// fresh vault is one. Fields 12-14 carry the device keys this device minted
/// for its own `PairingRequest` and the certificate its sponsor signed over
/// them; `ID_S_priv` and `ID_D_priv` are in neither, which is what #105 and
/// #76 closed.
///
/// **Do not edit this literal by hand.** `mise run apple-pairing-fixture`
/// rewrites it from the encoder, and
/// `crates/sunrise-pairing/tests/apple_fixture.rs` asserts on every
/// `cargo test` that what is written here is what that encoder produces today.
/// Regenerating by hand is how it went stale when fields 10 and 11 were added:
/// this file kept the nine-key shape and the failure surfaced as four opaque
/// XCTest assertions on a macOS runner instead of one named Rust test.
/// Before that it was key 2, `ID_D_priv`, being burned: the decoder refuses a
/// payload still carrying it, so the previous fixture stopped decoding.
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
