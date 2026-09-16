# The vault the keychain must open

One encrypted SQLCipher vault, written by this build, that
[`../src/keychain/fixture.rs`](../src/keychain/fixture.rs) opens through the
ordinary `Db::open` + `Keychain::open` on every `cargo test`. Read that module
— it generates this file, states its contents, and asserts what has to come
back out of it.

| File | Pins | Why it exists |
|---|---|---|
| `keychain_vault_suite_v5.db` | `CRYPTO_SUITE_V = 5` | The wrapping side of the key hierarchy had no test that was not self-consistent. Both "legacy vault" tests in `keychain/mod.rs` wrap their secrets in-process with the same AAD they then unwrap with, and `sunrise-storage`'s fixtures assert rows without ever unwrapping one. |

Named for `CRYPTO_SUITE_V` and not for `STORAGE_V`: what it pins is the key
hierarchy. The migration chain has its own fixtures next door in
[`../../sunrise-storage/fixtures/`](../../sunrise-storage/fixtures/README.md),
and they are the ones named by storage version.

## What breaks it

Opening this file needs ten constants to still be what they were — the
SQLCipher key context, the four wrapped-secret AAD prefixes, the device-id and
identity-id contexts, the device-cert signature domain, the stream-key wrap
AAD, and the vault-meta genesis context — plus `WRAPPED_SECRET_LEN` and
`WRAPPED_STREAM_KEY_LEN`. None of them is stored in the file. That is the whole
point: a build that changed one wraps and unwraps its own vaults perfectly and
cannot open this one.

## The key is committed, deliberately

The vault root is **thirty-two `0x6c` bytes**, spelled out as
`FIXTURE_VAULT_ROOT` in `../src/keychain/fixture.rs`, and the account identity
inside is the public one from `sunrise-crypto-test-vectors`.

That is not a leak. A checked-in encrypted fixture is worthless without its
key, and an unencrypted one would skip the `PRAGMA key` path that is half of
what is under test. Nothing is behind it: the identity and both device seeds
are constants in this repository, and no user, device or account has ever been
keyed with any of them. Please do not "fix" it by rotating it — the replacement
would be exactly as public.

## Regenerating

```
mise run keychain-fixture
```

**Rarely, and for exactly one reason:** a deliberate `CRYPTO_SUITE_V` bump that
moves one of the constants above, landing with a rotation plan per
[`../../../docs/03-crypto/key-rotation.md`](../../../docs/03-crypto/key-rotation.md).
Regenerate then, rename the file to the new suite version, and keep the old one
if the build still claims to open vaults written under the old suite.

Regenerating to make a red test green is the one thing this fixture exists to
prevent. If `the_committed_vault_opens_and_its_keys_are_the_frozen_ones` fails
and no suite bump was intended, the change under review is the bug.

The bytes are not reproducible: SQLCipher writes a random salt into the file
header and every wrap draws a fresh nonce, so every regeneration produces a
different file with identical contents. Expect a whole-file diff, and review
the module rather than the binary.
