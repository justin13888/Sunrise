# 0038 — The macOS app updates itself through Sparkle, and the appcast's EdDSA key is subordinate to the Developer ID certificate in lifecycle, not in authority

**Status:** accepted

**Amends:** [`../07-clients/desktop.md`](../07-clients/desktop.md) §Update
channel, which said Sparkle-style signed updates were "specified, not built",
and [`../07-clients/releasing.md`](../07-clients/releasing.md), which gains the
seventh secret and the key-rotation procedure.
[`../07-clients/overview.md`](../07-clients/overview.md) §Distribution gains the
update channel beside the download channel.

**A new record rather than a clause inside
[`./0031-macos-distribution.md`](./0031-macos-distribution.md), on that ADR's
own instruction.** Its revisit trigger 4 reads:

> 4. **Sparkle, or any auto-update mechanism, landing.** An update channel is a
>    second thing that has to be signed and a second thing a user trusts. It
>    does not reverse this decision, but the appcast's signing key and its
>    hosting are decisions of the same kind and belong beside this record.

"Beside", not "inside", and the distinction is doing work: ADR-0031 decides how
a stranger *first gets* the app, and every argument in it is about Gatekeeper,
notarization and the Mac App Store. This decides what an *already installed*
copy will execute next, which is a different trust question with a different
threat model and a different key. Folding it in would also mean editing an
accepted ADR's Decision section so that it argued for something it does not
decide, which is how a record stops being a record. Nothing in ADR-0031 is
edited; its trigger 4 is answered underneath itself, where a reader of that
file will find it.

## Context

### The specification has outlived the gap it described

[`../07-clients/desktop.md`](../07-clients/desktop.md) §Update channel has said
the same thing since before ADR-0031:

> **Specified, not built.** Sparkle-style signed updates over the direct
> channel, applied on next launch — a running session is never interrupted by
> an update. Channels: `stable`, `beta`. There is no Sparkle dependency in the
> project today and no update path of any kind.

ADR-0031 made that buildable and deliberately did not build it, recording the
state plainly: *"An installed copy currently learns about a new version the way
it did before: it does not."* Everything an updater needs now exists — a
`.dmg` at a stable URL, a Developer ID signature to check it against, and a
`release.yml` that produces both from a tag.

### The part that is a security decision rather than a packaging detail

[#140](https://github.com/justin13888/Sunrise/issues/140) framed it as *"the
appcast signing key is a second trust root"*: whoever holds it can push code to
every installed copy, it is a different key from the Developer ID certificate,
and a compromise or rotation of one does not imply the other.

The first half is exactly right and this record does not soften it. The second
half needs correcting, and correcting it is the whole reason this ADR exists
rather than a paragraph in a workflow comment. Sparkle does not treat the two
credentials as independent. Its documentation says so directly, in the section
on rotation:

> For regular application updates (not installer package updates), if you both
> code-sign your application with Apple's Developer ID program and include a
> public EdDSA key for signing your update archive, Sparkle allows rotating
> keys by issuing a new update that **changes either your Apple code signing
> certificate or your EdDSA keys (but not both)**. For applications that opt
> into enabling `SUVerifyUpdateBeforeExtraction`, changing your EdDSA keys can
> only be done if the update archive is a Developer ID code signed disk image
> (dmg).

Read that as a statement about **authority over the credentials themselves**,
not as a statement that an update needs both signatures. It does not, and that
is worth being exact about, because the exactness is the difference between a
record somebody can act on and a comforting sentence. From Sparkle 2.10.0's
`SUUpdateValidator.m`, the doc comment on `validateUpdateForHost:`:

> If the update is a bundle, then it must meet **any one** of:
> * old and new Ed(DSA) public keys are the same and valid (it allows change of
>   Code Signing identity), or
> * old and new Code Signing identity are the same and valid

and, in the body, on the line that combines them:

> Either DSA must be valid, or Apple Code Signing must be valid. We allow
> failure of one of them, because this allows key rotation without breaking
> chain of trust.

So the relation is an **OR**, and the OR is not sloppiness — it is the
mechanism that makes rotation possible at all. **Whoever holds the EdDSA key
can ship code to every installed copy.** #140 is right, this ADR does not
pretend otherwise, and the key is stored and handled as what it is: a
code-signing credential.

The subordination is over each credential's **lifecycle**, and under this app's
configuration it runs one way:

* **Changing the EdDSA key requires the Apple code signature.** With
  `SUVerifyUpdateBeforeExtraction` enabled — which this app enables, Decision 2
  — an archive whose EdDSA signature does not verify against the *installed*
  copy's key is accepted only through
  `codeSignatureIsValidAtDownloadURL:andMatchesDeveloperIDTeamFromOldBundleURL:`
  (`SUUpdateValidator.m`, the fallback in
  `validateDownloadPathWithFallbackOnCodeSigning:`). In plain terms: a
  Developer ID signed `.dmg` from the same team, which is exactly what
  `release.yml` produces and nothing outside it can.
* **Neither credential can be removed.** `passesBasicUpdatePolicy…` rejects an
  update that drops the EdDSA key and one that drops code signing. So the pair
  cannot decay into a single credential by accident.
* **They cannot both change in one release.** Each change is authenticated by
  the one that did not move.

What that asymmetry decides is **recovery**, which is the question an operator
actually faces:

* **A compromised EdDSA key is recoverable in-band.** The certificate holder
  cuts a Developer-ID-signed release carrying a new public key, and every
  installed copy accepts it because the Apple signature it already trusts
  vouches for the change. No user action, no reinstall, no announcement nobody
  reads.
* **A compromised Developer ID certificate is not recoverable in-band.** It is
  Apple's to revoke and the operating system's to check on first launch, and
  recovering from it is an Apple process rather than a Sparkle one.
* **What this does not buy, said plainly.** Because validation is an OR, an
  attacker holding only the EdDSA key can ship an update signed with an
  identity of their own — Sparkle checks that the new bundle's signature is
  internally valid, not that it matches the installed copy's — and that update
  can carry a new public key and a new certificate. They can seize both. The
  window is until the real certificate holder ships a rotation and a user
  updates, and there is no mechanism here that shortens it. Anyone reading this
  record to decide how carefully to hold `SPARKLE_ED_PRIVATE_KEY` should read
  that paragraph rather than the one above it.

So "second co-equal trust root" is the wrong model, and not because the EdDSA
key is weaker in what it can do. It is the wrong model because the two
credentials are not interchangeable in *lifecycle*: the certificate can replace
the key in-band and nothing can replace the certificate in-band, which is what
decides where each is stored, how each is rotated, and which compromise is an
afternoon and which is an incident.

**This generalises unchanged.** Windows and Linux desktop clients, when they
exist, will have the same shape: a platform code-signing identity that the
operating system checks, and an update-feed key that the updater checks. The
rule to carry forward is *the feed key is subordinate to the platform signing
identity in lifecycle — the identity must be able to replace the key without a
user reinstalling — and an updater that cannot express that is the wrong
updater*. The rest of the analysis carries too: that an updater accepting
either credential makes the feed key a code-signing credential in its own
right, and that the asymmetry is about recovery. It is stated here in full so
that the later ADR can cite it rather than re-derive it.

### What a tag already computes

`release.yml`'s `verify` job derives one boolean from the tag —

```
case "$version" in
  *-*) echo "prerelease=true"  >> "$GITHUB_OUTPUT" ;;
  *)   echo "prerelease=false" >> "$GITHUB_OUTPUT" ;;
esac
```

— and the `release` job writes it onto the GitHub Release with
`-F prerelease="$PRERELEASE"`. The `stable` and `beta` channels this ADR adds
read **that** value, in both directions: for the release being published, from
`verify`'s output; for older releases, from the Release object the `release` job
stamped with it. There is no second place where a hyphen in a version string is
interpreted, and `test_appcast.py` asserts it — a tag that looks like a
prerelease but whose Release is not flagged as one follows the flag.

## Decision

**The macOS app updates itself through Sparkle. The feed is an
`appcast.xml`, generated and EdDSA-signed by `release.yml` and published to
GitHub Releases, with `stable` and `beta` channels keyed off the `prerelease`
value `verify` already computes. The EdDSA key is recorded as *subordinate to
the Developer ID certificate* — in lifecycle, per the Context above — rather
than as a co-equal second trust root, and is rotated accordingly. It is held
as what it also is: a credential that ships code.**

### 1. Sparkle, via SPM, on the macOS target only

`apps/apple/project.yml` gains a `packages:` block and one dependency on the
`Sunrise` target. `SunriseiOS` does not get it: iOS has no direct-download
channel to update over at all ([ADR-0038](./0039-ios-distribution.md)), so
Sparkle there would be a framework nothing can reach, embedded in a bundle that
App Store review looks at. `apps/apple/macOS/SoftwareUpdate.swift` is the only
file that imports it, and `macOS/` compiles into the Mac target alone — so the
shared tree stays shared, which [ADR-0028](./0028-ios-is-a-v1-client.md)'s
revisit trigger 4 asks for.

**Pinned with `exactVersion`, not a range.** `Sunrise.xcodeproj` is generated
and gitignored, so the `Package.resolved` Xcode writes inside it is not
committed either. A range would let every machine resolve a different Sparkle
with nothing in the repository recording which one shipped, and this is the one
dependency whose job is to authorise code that replaces the running app.
`.github/scripts/sparkle-tools.sh` pins the matching *tools* release and asserts
the two pins are equal, because signing a feed with a different release of the
tooling than the framework that verifies it is skew nobody notices until an
update fails.

### 2. Six Info.plist keys, and two of them are the security posture

| Key | Value | Why |
|---|---|---|
| `SUFeedURL` | `…/releases/latest/download/appcast.xml` | The feed, reached through the alias — see Decision 4 |
| `SUPublicEDKey` | *empty until the owner generates the pair* | Public, so it belongs in the committed bundle where it is reviewable, not injected at signing time |
| `SURequireSignedFeed` | `true` | The feed is authenticated, not merely transport-protected |
| `SUVerifyUpdateBeforeExtraction` | `true` | Required by the above, and the thing that makes Decision 5's rotation story hold |

`SUEnableAutomaticChecks` is on and `SUAutomaticallyUpdate` is off: the app
checks in the background and asks before installing. `desktop.md` requires that
a running session is never interrupted, which Sparkle satisfies either way by
installing when the app quits; it does not require installing without telling
anybody, and for an app holding a user's encrypted vault that is a bigger claim
than the specification makes.

**`SUPublicEDKey` is empty today and the app checks it.** The key pair does not
exist — only the repository owner can create it. `SoftwareUpdate.controller`
returns `nil` when the key is empty, so no updater is started and the Check for
Updates menu item is disabled with the reason attached. Starting Sparkle with no
public key would give the app an update path it cannot authenticate, which is
worse than having none.

### 3. The feed is generated by a tested script, not by a shell loop

`.github/scripts/appcast.py` is a pure function from two files — the Releases
API body and a per-release signature sidecar — to one XML document, with
`.github/scripts/test_appcast.py` asserting its rules and `ci.yml` running
them. Three of those rules are invisible when correct and catastrophic when
wrong: which release lands on which channel, whether the signature in hand
describes the bytes being advertised, and whether the cap on feed length is
applied per channel (a global cap after ten prereleases leaves the stable
channel empty). None is checkable by `actionlint`, and the only other way to
find out is to publish and watch.

The per-release sidecar, `appcast-item.json`, is the mechanism that makes a
*cumulative* feed possible without re-signing history. It is produced in the
`macos-app` job beside the `.sha256`, on the machine that built and stapled the
disk image and after stapling — stapling rewrites the file, so a signature taken
before it would describe something nobody downloads. It carries the EdDSA
signature, the length, and the bundle's own `CFBundleVersion`, which is the
workflow run number and is not recoverable from a tag. A later release's
`appcast` job reads the sidecars rather than downloading and re-signing old disk
images, so no past artifact is ever trusted by this pipeline on the strength of
having been downloaded.

### 4. Hosting: the Release the app already downloads from

The feed is an asset of the GitHub Release, reached through
`https://github.com/<owner>/<repo>/releases/latest/download/appcast.xml`.
No web server, no Pages deployment, no second thing to keep up. The `.dmg` is
already there and the enclosure URLs point at it.

**GitHub's `latest` alias skips prereleases, and that is why the `appcast` job
uploads twice.** When a stable tag publishes, it becomes `latest` and carries
the fresh feed. When a *prerelease* publishes, `latest` does not move, so the
regenerated feed is also written onto the current stable release — otherwise a
beta subscriber would never be offered the beta, because the file their Mac
fetches would predate it. The feed is one document covering both channels; the
alias is only how it is reached.

One hole, named rather than hidden: **before the first stable release exists,
the alias resolves to nothing** and no installed copy has a feed. Nothing is
lost — there are no installed copies yet — and cutting a stable release closes
it. The workflow says so with a `::warning::` rather than passing silently.

### 5. The key: where it lives, and how it is rotated

* **Storage.** The private key is one repository secret,
  `SPARKLE_ED_PRIVATE_KEY`, holding the base64 seed `generate_keys -x` exports.
  It is written to a `umask 077` file for the width of one `sign_update` call
  and deleted immediately, with the `always()` cleanup step naming it again for
  the path where the step dies in between. It is never written to an artifact,
  never echoed, and never leaves the two jobs that sign.
* **The public half is committed**, in `project.yml`'s `SUPublicEDKey`. What an
  installed copy trusts should be reviewable in the tree, not injected by a
  secret at signing time — a build whose trust anchor comes from CI cannot be
  reproduced or audited from a checkout.
* **Rotation is a release, not an incident procedure.** Generate a new pair,
  put the new public key in `project.yml`, replace the secret, and cut a
  release. Sparkle accepts the new key because the `.dmg` carrying it is
  Developer ID signed with the *unchanged* certificate. The two must not move
  in the same release — Sparkle permits changing one or the other, never both —
  so a certificate renewal and a key rotation are two tags.
* **What rotation cannot do.** It does not un-sign anything the old key already
  signed, and it does not take effect on a copy that has not updated. Between
  the theft and the user's next update, the old key still ships code — see the
  "what this does not buy" bullet in the Context. Rotation is how the exposure
  *ends*, not a reason to treat the exposure as small.
* **`verify` fails the release loudly when the secret is absent**, with a
  message naming it, beside the six ADR-0031 secrets and in the same step but
  under its **own** error title — the six are about the file a stranger
  downloads and this one is about the feed every installed copy reads, they are
  created in different places, and an operator who fixes one should not learn
  about the other on the next 90-minute run. A seventh check in `macos-app`
  fails the release if `SUPublicEDKey` is empty while the private key exists,
  because an app that ignores the feed is indistinguishable from an app that is
  up to date.

## Alternatives considered

**Apple code signing only, with no EdDSA key.** Rejected, and it is the
alternative worth pricing because it is the one that appears to remove a key
rather than add one. Sparkle does support verifying an update by comparing the
downloaded app's code signature against the running one — and it says what it
thinks of that, at runtime, in a string carried by the shipped 2.10.0 framework
binary:

> Error: Serving updates without an EdDSA key and only using Apple Code Signing
> is **deprecated and may be unsupported in a future release**.

Its documentation agrees in the positive direction: the security section lists
three recommendations and one of them is to *"Sign the published update archive
(dmg, zip, etc), binary delta updates, and installer packages with Sparkle's
EdDSA (ed25519) signature."* Three costs decide it, beyond the deprecation:

1. **Verification gets shallower, and specifically less precise.** An EdDSA
   signature binds *these exact bytes*. A Developer ID signature binds *the
   team that signed them* — Sparkle's own code-signing fallback is
   `…andMatchesDeveloperIDTeamFromOldBundleURL:`, a team comparison — so any
   artifact that team has ever signed satisfies it equally, and the check
   cannot tell the intended release from a different one. Without the EdDSA
   key that weaker check is the *only* check, and on a `.zip`, which cannot
   carry a code signature at all, there is nothing to check before extraction.
2. **There is no key-rotation path left.** Rotation in Sparkle is defined over
   *two* credentials: a release changes one and is authorised by the other. With
   only the Apple certificate there is nothing to rotate it against, so a
   certificate that has to change — expiry, compromise, a change of team — has
   no in-band way to reach installed copies at all.
3. **Delta updates are forgone.** A binary delta is a patch file, not a signed
   bundle, so there is nothing for a code-signature check to check. Sparkle
   signs deltas with EdDSA, and an app that has no EdDSA key cannot ship them.

The paradoxical-sounding conclusion is the right one: adding the EdDSA key
makes the system *more* recoverable, not less, because it gives the Developer ID
certificate something to authorise.

**No updater at all; the app queries the Releases API and links to the
download.** Rejected, and it is the honest-but-weaker option #140 named.
It adds no key and no feed to host, and it hands the install to the user — who
does it late, or not at all. For a product whose update path is also its
security-fix path, "the user does the install" is a policy of shipping fixes to
the subset of people who notice. It is also not free of the trust question it
appears to dodge: the app would still be deciding what to tell a user to run, on
the strength of an HTTPS response with nothing signed inside it, which is a
*weaker* position than a signed appcast and not a neutral one.

**Homebrew cask only.** Not rejected; out of scope and filed separately, exactly
as ADR-0031 left it. A cask makes updating somebody else's problem for the
subset of users who install that way, and that subset does not include anyone
who downloaded the `.dmg` from the Releases page.

**Hosting the feed on GitHub Pages, or on a fixed non-version tag.** Rejected as
more moving parts for the same result. Pages is a second deployment to keep
working and a second place a release can half-succeed. A permanent `appcast`
tag would avoid the `latest`-alias wrinkle in Decision 4, at the cost of a tag
that is not a release sitting in a repository whose release workflow is built on
"a tag is the release", and a spurious entry on the Releases page forever. The
two uploads are cheaper and stay inside the model.

**Signing only the enclosures, not the feed.** Rejected once
`SURequireSignedFeed` was on the table. The enclosure signature authorises the
*bytes that get installed* and would do its job with an unsigned feed; signing
the feed authenticates the *metadata* — which versions exist and on which
channel — against someone who can write this repository's release assets but
does not hold the EdDSA key, which is a strictly wider set than the key's
holders. It does not prevent replay of an older, genuinely signed feed, and this
record says so rather than implying otherwise.

## Consequences

- **A seventh repository secret becomes a release prerequisite**,
  `SPARKLE_ED_PRIVATE_KEY`. Until it exists, every tag push fails at `verify`
  with a message naming it — the same contract, and the same reasoning, as
  ADR-0031's six.
- **Two new things ship on every Release:** `appcast.xml` and
  `appcast-item.json`. Neither is an artifact a human downloads, and the
  release notes say so, because an unexplained JSON file beside a `.dmg` invites
  exactly one wrong guess.
- **`release.yml` gains an `appcast` job** that runs after `release`, on macOS
  because `sign_update` is a macOS binary. It is the cheapest macOS job in the
  file.
- **`ci.yml` gains an `appcast-contract` job.** The generator's rules are
  asserted on every pull request, in the style of the file-size and
  core-filesystem gate contracts.
- **The Mac app gains two menu items** under About: Check for Updates…, and an
  Include Beta Updates toggle that is the user-visible half of the channel
  decision. Both are disabled, with the reason attached, on a build whose
  `SUPublicEDKey` is empty — which is every build until the owner generates the
  pair.
- **`docs/07-clients/desktop.md` §Update channel stops saying the question is
  open**, which is what #140 asked for.
- **The beta channel has no feed until a stable release exists.** Decision 4
  names it; the workflow warns about it; cutting a stable release fixes it.
- **Nothing here is proved end to end yet.** The signing chain was exercised
  locally against a generated key — `sign_update` produced a signature over a
  file, `appcast.py` put it in a feed, `sign_update --verify` accepted it and
  rejected a tampered copy — but no release has run, no real key exists, and no
  installed copy has ever taken an update from this feed. The first tag after
  the owner creates the secret is what proves the rest.

## What would force revisiting this

1. **A second holder of the EdDSA key.** Decision 5's storage argument is
   levelled on one secret in one repository, written by one person. A team, a
   second signing machine or a hardware token changes where the key lives and
   therefore what rotating it costs.
2. **Sparkle changing what authorises a key change.** The whole subordination
   clause is read off Sparkle's rotation rule and off
   `SUVerifyUpdateBeforeExtraction` being on. If a future version allows
   changing both credentials in one update, or drops the code-signature
   requirement for an EdDSA change, the lifecycle asymmetry is gone — the
   certificate could no longer replace the key in-band — and the two become
   co-equal in the one respect where this record says they are not.
3. **Delta updates landing.** They are named here as a cost of the rejected
   alternative and are not built. Building them adds a second signed artifact
   per release and a `generate_appcast` run over a directory of disk images,
   which is a different feed-generation shape from the sidecar one in
   Decision 3.
4. **A second desktop platform.** The generalisation in the Context is a claim
   about Windows and Linux that no code tests. The ADR that brings one of them
   in should cite this clause or explain why the shape does not transfer.
5. **The App Sandbox landing.** Sparkle in a sandboxed app needs its installer
   launcher XPC service and an entitlement, which is a different integration
   from this one. ADR-0031's revisit trigger 1 covers the sandbox itself; this
   is what it would cost here.
