---
status: living
---

# Releasing the macOS app

Operator runbook for the `.dmg` half of a Sunrise release: the secrets it
needs, how to produce each one, what the pipeline does with them, and how to
check by hand that what came out is what a stranger's Mac will accept.

Everything else about a release — the tag, the CLI tarballs, the container
image, the notes — is `.github/workflows/release.yml` and needs nothing from
you beyond pushing the tag.

**Why this shape:** [ADR-0031](../11-adr/0031-macos-distribution.md). The short
version is that the app is not sandboxed
([`desktop.md`](./desktop.md) §Sandboxing), the Mac App Store requires the
sandbox, and a self-hosted relay should not be somebody else's review decision.

## The six secrets

All six are **repository secrets** (Settings → Secrets and variables → Actions
→ Repository secrets). Only the repository owner can create them. Until all six
exist, **every tag push fails** — in the `verify` job, with a message naming
the ones that are missing, before anything is built and before the container
image is pushed. That placement is deliberate twice over: the alternative to
failing is an unsigned `.dmg` on the Releases page that nobody notices until a
user tries to open it, and the alternative to failing *early* is a tag whose
image reached GHCR and whose Release never appeared.

| Secret | What it is | Used by |
|---|---|---|
| `MACOS_CERTIFICATE_P12` | Base64 of a `.p12` holding the **Developer ID Application** certificate *and* its private key | `security import` into the job's temporary keychain |
| `MACOS_CERTIFICATE_PASSWORD` | The password set when exporting that `.p12` | the same `security import` |
| `MACOS_TEAM_ID` | The 10-character Apple Developer Team ID | `DEVELOPMENT_TEAM` on the archive, `teamID` in the export options plist |
| `APPLE_API_KEY_P8` | Base64 of the App Store Connect API key file, `AuthKey_<KEYID>.p8` | `xcrun notarytool --key` |
| `APPLE_API_KEY_ID` | That key's id — the `<KEYID>` in the filename | `xcrun notarytool --key-id` |
| `APPLE_API_ISSUER_ID` | The issuer UUID for the App Store Connect account | `xcrun notarytool --issuer` |

`MACOS_TEAM_ID` is not confidential — a team id is embedded in every signature
this pipeline produces and is readable from any released build. It is a secret
anyway so that there is exactly one place to look for "the things a release
needs from Apple", and so that adding it does not also mean explaining when to
use a repository variable instead.

### Prerequisite: an Apple Developer Program membership

A Developer ID Application certificate and the notary service both require a
paid membership. There is no free path to a notarized build.

### `MACOS_CERTIFICATE_P12` and `MACOS_CERTIFICATE_PASSWORD`

You need the certificate **and** its private key in one file. The private key
exists only on the Mac that generated the signing request, so do this on that
Mac.

1. If you have no Developer ID Application certificate yet, create one at
   [developer.apple.com](https://developer.apple.com/account/resources/certificates/list)
   → Certificates → **+** → **Developer ID Application**, uploading a
   certificate signing request from Keychain Access (Keychain Access →
   Certificate Assistant → Request a Certificate From a Certificate
   Authority → *Saved to disk*). Download the resulting `.cer` and open it,
   which installs it into your login keychain beside its key.
2. In **Keychain Access**, select the *login* keychain and the **My
   Certificates** category. Find `Developer ID Application: <your name>
   (<TEAMID>)` and expand its disclosure triangle — if there is no private key
   underneath it, you are on the wrong Mac and the export will not contain what
   the pipeline needs.
3. Right-click the certificate → **Export "Developer ID Application: …"**,
   choose *Personal Information Exchange (.p12)*, save it as
   `developer-id.p12`, and set an export password. That password is
   `MACOS_CERTIFICATE_PASSWORD`. Use a real one; it protects the private key in
   transit and at rest in GitHub.
4. Check the export before you trust it, so a `.p12` with no key fails here
   rather than twenty minutes into a macOS runner:

   ```
   openssl pkcs12 -in developer-id.p12 -passin pass:'<the password>' -info -nodes \
     | grep -E 'friendlyName|BEGIN PRIVATE KEY'
   ```

   You must see a `friendlyName` naming
   `Developer ID Application: … (<TEAMID>)` **and** a `BEGIN PRIVATE KEY`
   line. If the key is absent the export did not include it — go back to
   step 2 and check the disclosure triangle.
5. Base64 it and put the result in `MACOS_CERTIFICATE_P12`. macOS's `base64`
   emits one unwrapped line, which is what you want:

   ```
   base64 -i developer-id.p12 | pbcopy
   ```

6. Delete the `.p12`. The copy in your keychain is the one you keep, and a
   private key in a Downloads folder is a private key somebody else will find.

### `APPLE_API_KEY_P8`, `APPLE_API_KEY_ID`, `APPLE_API_ISSUER_ID`

Notarization uses an App Store Connect API key rather than an Apple ID and an
app-specific password. The key is revocable on its own, carries no access to
your Apple ID, and does not stop working when a password changes.

1. [App Store Connect](https://appstoreconnect.apple.com/access/integrations/api)
   → Users and Access → **Integrations** → App Store Connect API → Team Keys.
2. **+**, name it something like `notarization`, and give it the **Developer**
   access role. Notarization needs no more than that.
3. Download the `AuthKey_<KEYID>.p8`. **Apple lets you download it once.**
4. `APPLE_API_KEY_ID` is the `<KEYID>` in the filename, also shown in the KEY ID
   column.
5. `APPLE_API_ISSUER_ID` is the **Issuer ID** shown above the key table. It is a
   UUID and it is the same for every key on the team.
6. `APPLE_API_KEY_P8` is the file, base64'd in one line:

   ```
   base64 -i AuthKey_<KEYID>.p8 | pbcopy
   ```

Check the three work together before trusting a tag to them:

```
xcrun notarytool history --key AuthKey_<KEYID>.p8 --key-id <KEYID> --issuer <ISSUER-UUID>
```

An empty history is a pass. An authentication error means one of the three is
wrong, and the message does not say which.

## What the pipeline does with them

`.github/workflows/release.yml`, job `macos-app`, on `macos-26`:

0. **Preflight**, in `verify` rather than in this job. Checks all six secrets
   are non-empty and stops the whole workflow if any is not. Only the boolean
   `secrets.X != ''` crosses into that job — no key material — because all it
   has to know is whether the secret exists.
1. **`mise run apple-xcframework`.** The repository's own task, building the
   three-slice `SunriseCore.xcframework` and the generated Swift bindings from
   *this tag's* source. Nothing is consumed from another workflow.
2. **A temporary keychain.** `security create-keychain` under `$RUNNER_TEMP`,
   the `.p12` imported into it, `set-key-partition-list` so `codesign` is not
   blocked on a UI prompt that a runner cannot answer, and the keychain
   prepended to the search list. It is deleted in an `always()` step, so a
   failed archive or a rejected notarization does not leave a private key
   behind.
3. **`xcodegen generate` then `xcodebuild archive`** on the `Sunrise` macOS
   scheme, `-destination 'generic/platform=macOS'`, `-configuration Release`,
   with `ARCHS=arm64`, `CODE_SIGN_STYLE=Manual`, `CODE_SIGN_IDENTITY="Developer
   ID Application"`, `DEVELOPMENT_TEAM=$MACOS_TEAM_ID` and
   `OTHER_CODE_SIGN_FLAGS=--timestamp`. The hardened runtime is **not** passed
   here; `project.yml` carries it (see below).
   `MARKETING_VERSION` comes from the tag rather than from `project.yml`'s
   pinned `0.1.0`, so the About box and the file name agree.

   **`ARCHS=arm64` is not optional**, and it is the one setting a local
   `mise run macos-app` does not teach you. That task builds Debug at
   `-destination 'platform=macOS,arch=arm64'`, where `ONLY_ACTIVE_ARCH=YES`
   settles the architecture. An archive is Release at
   `generic/platform=macOS`, where `ARCHS` falls back to `$(ARCHS_STANDARD)` —
   `arm64 x86_64` — and the Intel half fails to link:

   ```
   ld: symbol(s) not found for architecture x86_64
   note: '…/out/SunriseCore.xcframework' is missing architecture(s)
         required by this target (x86_64)
   ```

   `mise.toml`'s `macos_slices` is `aarch64-apple-darwin` alone, and
   `release.yml`'s `binaries` matrix records why: *"x86_64-apple-darwin — no
   Intel Mac target is claimed by the app"*. A universal build means adding
   that triple to `macos_slices` first. Until then, arm64 is the product, and
   the artifact's name says so.
4. **`xcodebuild -exportArchive`** with a `method: developer-id`,
   `signingStyle: manual` export options plist.
5. **Notarize and staple the app**, then build the `.dmg` around the stapled
   copy, sign it, **notarize and staple the `.dmg` too**. Two submissions, and
   the first is the one that makes an offline first launch work — see ADR-0031.
6. **Verify**, with the same commands listed below, so a signature that does
   not satisfy Gatekeeper fails the release rather than the user.
7. **Checksum and upload.** `sunrise-<version>-aarch64-apple-darwin.dmg` plus a
   `.sha256`, attached to the Release by the `release` job and listed in its
   notes.

### One setting that is overridden rather than committed

- **`DEVELOPMENT_TEAM`.** `project.yml` has it empty, which is correct: a team
  id belongs in the secret, not in a committed file, and an empty value is what
  keeps `mise run macos-app` working for a contributor who has no Apple
  account.

`ENABLE_HARDENED_RUNTIME` used to be the second, overridden to `YES` on the
archive command line over a `NO` in the project file. It is now
`ENABLE_HARDENED_RUNTIME: YES` in `project.yml`'s `settings.base` and is passed
nowhere, which is what makes a local `xcodebuild archive` the same bundle this
job signs. The setting changes how the process runs — library loading is
restricted and the DYLD environment variables are ignored — so a difference
there was one nobody could see until the notary service or a user's crash
report reported it. It is unrelated to the App Sandbox, which stays off; see
[`desktop.md`](./desktop.md) §Sandboxing.

## Verifying a release by hand

Do this on the file you downloaded from the Releases page, not on a local
build, and ideally on a Mac that has never seen the app. Downloading through a
browser is what sets the quarantine flag, which is what makes the check mean
something.

```
shasum -a 256 sunrise-<version>-aarch64-apple-darwin.dmg
```

Compare with the `### Checksums` block in the release notes.

### The disk image

```
spctl -a -vvv -t install sunrise-<version>-aarch64-apple-darwin.dmg
xcrun stapler validate sunrise-<version>-aarch64-apple-darwin.dmg
```

`spctl` must print `accepted` and `source=Notarized Developer ID`.
`source=Developer ID` without `Notarized` means the file is signed but its
ticket is missing — notarization or stapling did not happen. `stapler validate`
must print `The validate action worked!`.

### The app inside it

Mount the image and check the bundle:

```
hdiutil attach sunrise-<version>-aarch64-apple-darwin.dmg
codesign -dv --verbose=4 /Volumes/Sunrise*/Sunrise.app
spctl -a -vvv -t exec /Volumes/Sunrise*/Sunrise.app
xcrun stapler validate /Volumes/Sunrise*/Sunrise.app
hdiutil detach /Volumes/Sunrise*
```

In the `codesign -dv --verbose=4` output, four fields carry the whole claim:

| Field | Must say | Why it matters |
|---|---|---|
| `Authority` | `Developer ID Application: … (<TEAMID>)`, then `Developer ID Certification Authority`, then `Apple Root CA` | An ad-hoc or self-signed build has one `Authority` line or none |
| `TeamIdentifier` | your team id, **not** `not set` | `not set` means ad-hoc; Gatekeeper will refuse it after a download |
| `flags` | includes `runtime` | The hardened runtime. Without it the build could not have been notarized, so its absence means the artifact is not the one CI produced |
| `Timestamp` | a real date | A secure timestamp from Apple's TSA. Without it the signature stops verifying the day the certificate expires |

`spctl -a -vvv -t exec` must again print `accepted` and
`source=Notarized Developer ID`.

Note the two assessment types: **`-t install` for the disk image** and **`-t
exec` for the app**. They are different Gatekeeper policies and a file can pass
one and fail the other, which is exactly the failure mode the two-submission
flow above exists to avoid.

### The real test

Drag `Sunrise.app` out of the mounted image to `/Applications`, **turn off
Wi-Fi**, and double-click it. It must open with no dialog. If it opens only
with the network on, the app's own notarization ticket is missing and the
`.dmg`'s ticket is doing the work — see ADR-0031 on why both are stapled.

## Dry runs while the secrets do not exist

The packaging path is testable without any Apple credential:

Actions → **Release** → *Run workflow* → tick **`unsigned_macos_dmg`**.

That builds the xcframework, archives, packages
`sunrise-<version>-aarch64-apple-darwin-UNSIGNED.dmg`, checksums it and
uploads it as a workflow artifact. It creates no tag, no Release and no
registry push — `verify` marks the run as a dry run and the `binaries`, `image`
and `release` jobs skip.

What it does **not** exercise, and cannot: the keychain import, `xcodebuild
-exportArchive` (there is no export method that produces an unsigned Developer
ID export, so the dry run lifts the `.app` straight out of the archive
instead), both notarization submissions, and the stapling. Those four steps are
the ones the secrets exist for.

The artifact it produces will not open on a Mac after a download. That is the
correct behaviour and it is why the file name says `UNSIGNED`.

## Troubleshooting

| Symptom | Cause |
|---|---|
| `Missing repository secret(s): …` | The `verify` job's preflight step. Create the named secrets above, then delete and re-push the tag. |
| `errSecInternalComponent` from `codesign` | `security set-key-partition-list` did not run or did not match the keychain password. The workflow does it; a local reproduction usually has not. |
| `No signing certificate "Developer ID Application" found` | The `.p12` has the certificate and not its private key. Re-export from the Mac that generated the signing request. |
| notarytool: `Team is not yet configured for notarization` | The Apple Developer Program membership is not active, or the account has not accepted the current agreements. |
| notarytool status `Invalid`, log says `The executable does not have the hardened runtime enabled` | `ENABLE_HARDENED_RUNTIME: YES` is missing from `apps/apple/project.yml`'s `settings.base`, or a target overrode it back to `NO`. |
| notarytool status `Invalid`, log says `The signature does not include a secure timestamp` | `--timestamp` did not reach `codesign`, usually because `OTHER_CODE_SIGN_FLAGS` was overridden elsewhere. |
| `stapler` fails with `Error 65` | The submission is notarized but Apple's ticket has not propagated yet. It is a retry, not a rebuild. |
| A tag pushed an image to GHCR but created no Release | `macos-app` (or `binaries`) failed after `image` had already pushed. `release` needs all three, so it did not run — which is the intended failure mode: no Release is better than one advertising an artifact that does not exist. Fix the cause and re-run the failed jobs; `release` runs on the same tag. The preflight on `verify` exists so that the commonest reason for this — no signing secrets — cannot reach that state at all. |
| A published Release has the tarballs but no `.dmg` | Not reachable: `release` needs `macos-app` and uploads `dist/*.dmg`. A Release like that predates this pipeline. |

## Related

- [ADR-0031](../11-adr/0031-macos-distribution.md) — why direct download, why
  not the App Store, and what that forced.
- [ADR-0027](../11-adr/0027-v1-self-host-first.md) — self-host-first, which is
  half of the argument.
- [`desktop.md`](./desktop.md) — §Sandboxing and §Update channel.
- [`overview.md`](./overview.md) §Distribution — the per-client channel table.
- [`../06-server/self-hosting.md`](../06-server/self-hosting.md) — the other
  half of a release, for the operator running the relay.
