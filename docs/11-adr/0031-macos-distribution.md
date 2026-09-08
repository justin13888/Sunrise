# 0031 — macOS ships as a Developer ID-signed, notarized `.dmg` on the GitHub Release, not through the Mac App Store

**Status:** accepted

**Amends:** [`../07-clients/overview.md`](../07-clients/overview.md)
§Distribution and [`../07-clients/desktop.md`](../07-clients/desktop.md)
§Sandboxing, both of which recorded the Mac App Store as "under evaluation",
plus that file's §Update channel, which gains what this decision changes about
it. Adds [`../07-clients/releasing.md`](../07-clients/releasing.md), the
operator runbook.

**Answers a question left open by**
[`./0019-swiftui-macos-client.md`](./0019-swiftui-macos-client.md), which
committed to a SwiftUI macOS client and never settled how it reaches a user.
Nothing in that ADR is edited: it says nothing about distribution to correct.

## Context

### The release workflow can ship two of the three clients

[`../07-clients/parity-matrix.md`](../07-clients/parity-matrix.md) opens by
saying that **three** clients ship in v1 — the macOS app, the iOS / iPadOS app
and the CLI — with macOS and the CLI carrying the MUSTs and iOS carrying
SHOULDs and no MUSTs until a release is cut
([ADR-0028](./0028-ios-is-a-v1-client.md)). Its macOS column records 23 MUSTs
and the audit grades all 23 met.

[#111](https://github.com/justin13888/Sunrise/issues/111) quotes an earlier
revision of that file — *"Two clients ship in v1"* — which ADR-0028 has since
superseded. The correction matters here, because "two clients" would make this
ADR look like it was settling the distribution of half the v1 client set. It is
settling **one** of three, and the third one's channel is a decision this
record deliberately does not take; see the iOS consequence below.

`.github/workflows/release.yml` before this change built two Rust targets and
copied exactly two executables into a tarball, pushed one container image, and
created the GitHub Release. Over the whole file,

```
grep -n 'xcodebuild\|macos-app\|xcframework\|\.app\|dmg\|notariz\|codesign' .github/workflows/release.yml
```

returned **one** hit, and that hit was the comment `# macos-26 is the arm64
Tahoe image; see the macos-app job in ci.yml.` Drop `macos-app` and `\.app`
from the pattern and it returns nothing at all. So a `vX.Y.Z` tag produced the
relay and the CLI, and the client carrying 23 of the 32 v1 MUSTs had no way to
reach anybody. The macOS app was built only by `ci.yml`'s `macos-app` job,
which runs `xcodebuild test`: it proves the app, and produces nothing
installable.

That is a different class of gap from the rest of the backlog. The other open
issues are narrownesses inside something that ships. This was the absence of a
way to ship it, and it is
[#111](https://github.com/justin13888/Sunrise/issues/111).

### The question that had to be answered first

Direct download and the Mac App Store are not two packagings of one build. They
need different entitlements, different signing identities, different review
lead times and a different answer to how an installed copy learns about a new
version. Nothing could be built until one was chosen, and two documents had
already recorded the choice as open:

- [`../07-clients/overview.md`](../07-clients/overview.md) §Distribution:
  "Direct signed & notarized `.dmg`; **Mac App Store under evaluation**".
- [`../07-clients/desktop.md`](../07-clients/desktop.md) §Sandboxing: "A Mac
  App Store build would have to trade that away; it is **under evaluation, not
  committed**."

"Under evaluation" in two accepted specs is exactly the state the ADR directory
exists to end.

## Decision

**The macOS app ships as a Developer ID Application-signed, Apple-notarized,
stapled `.dmg`, attached to the same GitHub Release as the CLI tarballs and
announced beside the container image. The Mac App Store is not a v1 channel and
is not "under evaluation" any more.**

Four pieces of evidence decided it, and each one is in the tree rather than in
a preference. A fifth subsection records what the decision then forces, which
is not evidence for it.

### 1. The App Store would reverse an accepted decision, not implement one

`apps/apple/project.yml` records, on the macOS target's `settings.base`:

> `# No App Sandbox in v1. Sandboxing moves Keychain items into an app-scoped
> group, which would have to land together with the entitlement and a signing
> identity; both are release work.`

Mac App Store distribution **requires** `com.apple.security.app-sandbox`. So
choosing the App Store is not a distribution decision that leaves the app
alone; it is a decision to sandbox, which is a decision the project has already
taken the other way and written down.
[`../07-clients/desktop.md`](../07-clients/desktop.md) §Sandboxing gives the
reason: a sandboxed build cannot register a reliable system-wide hotkey, and
quick capture is the feature the persona uses most. It is a macOS **MUST** in
the parity matrix — *"Quick capture (global hotkey / system surface) | MUST"* —
and the global hotkey is the first item of the first step of the first core
workflow in
[`../00-product/core-workflows.md`](../00-product/core-workflows.md). An App
Store leg would therefore have to either ship a second, differently-behaving
build, or withdraw a MUST.

### 2. Self-hosting is first-class, and a reviewer has an opinion about that

[ADR-0027](./0027-v1-self-host-first.md) makes the self-host single binary the
*only* server shape v1 ships. The app must therefore point at an arbitrary
user-supplied relay URL: that is the normal case, not an escape hatch.

Direct download has no review lead time and nobody's opinion about where the
app connects. An App Store submission has both. This is not a prediction that
review would refuse it — it is that a channel which can refuse it puts a third
party between a self-hoster and the client they self-host *for*, which is the
one relationship ADR-0027 is about.

### 3. A tag is the release, and a review queue cannot be a tag

`release.yml` was tag-driven with no manual dispatch at all, and its own header
comment said why: "a release cannot happen without a tag object in the
repository to point at afterwards." A store submission has a queue, a review
outcome and a release date that are all decided after the tag is pushed and by
somebody else. There is no honest way to represent that in a workflow whose
premise is that pushing the tag *is* publishing.

The dry-run dispatch this change adds does not weaken that property, and it is
worth being explicit about why: it publishes nothing. It builds an unsigned
`.dmg`, leaves it as a workflow artifact, and `verify` marks the run so that
`binaries`, `image` and `release` all skip. The invariant is not "no manual
trigger exists"; it is "nothing is published without a tag", and that still
holds.

### 4. A third artifact on one Release is coherent; a second channel is not

The CLI tarballs and the server image already ship from one GitHub Release,
with one checksum block and one set of release notes. A `.dmg` beside them is
the same thing again. A second channel, gated differently, updating on a
different schedule and reachable only through an Apple account, is a second
release process — and this repository has one maintainer.

### 5. What that decision then forces

- **Hardened runtime on, App Sandbox still off.** Notarization rejects a
  submission without the hardened runtime, so it is not a preference.
  `project.yml` sets `ENABLE_HARDENED_RUNTIME: YES` in `settings.base` and the
  workflow overrides nothing, so a local archive is the bundle that ships. (It
  first landed as a command-line override in the release job; #142 moved it to
  the project file.) The two settings are independent — the comment quoted in
  Decision 1 is about the sandbox, and the sandbox stays off.
- **Two notarization submissions, not one.** The `.app` is notarized and
  stapled, and then the `.dmg` built around the stapled copy is notarized and
  stapled too. A ticket is stapled to the thing that carries the quarantine
  flag; stapling only the disk image leaves the app with no ticket of its own
  the moment a user drags it to `/Applications`, and Gatekeeper's first-launch
  check then has to go online. For a product whose users are explicitly
  expected to run without anybody's cloud (ADR-0027), an app that will not
  launch offline the first time is a defect.
- **The xcframework is rebuilt from the tagged source, not consumed from a CI
  artifact.** #111 raised this as an open supply-chain question. The answer is
  the conservative one: the only inputs to the signed bundle are the tag and
  the release workflow, so no artifact produced by a differently-triggered
  workflow can end up inside a signed binary. The cost is one release build of
  the three-slice xcframework per tag, on a runner that bills at 10x, which is
  the correct thing to spend that money on.
- **The file is named like its neighbours.**
  `sunrise-<version>-aarch64-apple-darwin.dmg`, matching the
  `sunrise-<version>-<target triple>.tar.gz` the `binaries` job emits, with a
  `.sha256` beside it produced the same way and on the same machine. The two
  macOS artifacts sort together and a reader can see they are the same slice —
  and the app inside is arm64-only, which is the same decision the `binaries`
  matrix already records as *"x86_64-apple-darwin — no Intel Mac target is
  claimed by the app"*. It has to be stated on the archive rather than
  inherited: an `xcodebuild archive` at `generic/platform=macOS` defaults to
  `arm64 x86_64` and the Intel half will not link against a `macos_slices`
  that holds one triple.
- **The workflow fails loudly rather than shipping something unsigned.** None
  of the six secrets exists in this repository yet and only its owner can
  create them. A preflight names the missing secret and stops, because the
  alternative — an unsigned `.dmg` on the Releases page — is discovered by a
  user rather than by the maintainer. It lives on `verify` rather than on
  `macos-app`, because `binaries` and `image` do not depend on `macos-app`: a
  guard inside that job would fail only after the image had been pushed to
  GHCR, which is precisely the half-published tag `release.yml`'s own
  `concurrency` comment calls worse than a duplicate run. `verify` already
  exists to "refuse early, before anything is built or pushed". Only the
  boolean `secrets.X != ''` crosses into it; no key material does.

## Alternatives considered

**Mac App Store.** Rejected on Decisions 1–4 above. The strongest thing that
can be said for it is discovery, and discovery is not what a v1 for one persona
needs from its distribution channel.

**Both channels.** Rejected, and it is the alternative worth spelling out
because "do both eventually" is the reflex. Both means two builds that differ
in a security-relevant entitlement, two identities, two version streams, and a
support surface where the answer to "does the hotkey work" is "which copy do
you have". The App Store copy would be the one whose MUST is unmet, and the
parity matrix has no way to grade a capability that depends on where the binary
came from.

**A signed `.zip` instead of a `.dmg`.** Rejected, narrowly. A `.zip` is
simpler to produce and notarizes identically. A `.dmg` gets the drag-to-
`/Applications` window every Mac user already knows, and — the deciding
difference — a `.zip` cannot be stapled, so the app inside it is the *only*
thing carrying a ticket and there is nothing to validate about the download
itself. `spctl -a -t install` on the file a user actually downloaded is worth
the extra `hdiutil` call.

**Homebrew cask.** Not rejected; out of scope. A cask is a pointer at a
download, so it is a thing to add *after* there is a signed `.dmg` at a stable
URL, not instead of one.
[`../07-clients/overview.md`](../07-clients/overview.md) already lists Homebrew
under the CLI's channels; a cask for the app is a follow-on.

**Unsigned, with instructions to right-click → Open.** Rejected outright. On
any current macOS that path is a dialog most people read as "this is malware",
and telling a user to defeat Gatekeeper is not a distribution strategy for an
application whose entire pitch is that it holds their data locally and
encrypted.

## Consequences

- **`release.yml` gains a `macos-app` job** and the `release` job depends on
  it, so a tag now publishes three artifacts and no tag publishes a subset of
  them silently. The release notes carry the `.dmg`, its verification commands
  and its checksum in the same block as the rest.
- **Six repository secrets become a release prerequisite** —
  `MACOS_CERTIFICATE_P12`, `MACOS_CERTIFICATE_PASSWORD`, `MACOS_TEAM_ID`,
  `APPLE_API_KEY_P8`, `APPLE_API_KEY_ID` and `APPLE_API_ISSUER_ID`. Until they
  exist, **every tag push fails** at `verify`'s preflight step, with a message
  naming what is missing, before anything is built or pushed. That is the intended behaviour and not a regression: before
  this change a tag succeeded and shipped nothing for macOS, which is the worse
  of the two failures because it is silent.
  [`../07-clients/releasing.md`](../07-clients/releasing.md) is how they get
  created.
- **A `workflow_dispatch` dry run makes the job testable before the secrets
  exist.** It builds and packages an `-UNSIGNED.dmg` and stops. Everything
  except the four credential-bearing steps is exercised, including the archive,
  the packaging, the naming and the checksum.
- **`apps/apple/project.yml` still carries `DEVELOPMENT_TEAM: ""`.** It is
  overridden on the release build's command line, which is correct: a team id
  is a secret in CI and must not be committed. `ENABLE_HARDENED_RUNTIME` was
  overridden the same way when this ADR was written and no longer is — it is
  `YES` in the project file, so a developer's local archive matches what
  ships.
- **iOS distribution is untouched and is a separate decision.** iOS cannot ship
  by direct download at all: there is no equivalent of a Developer ID identity
  on the platform, so the only channels are TestFlight and the App Store, and
  the entitlement argument in Decision 1 does not transfer — iOS apps are
  sandboxed unconditionally and there is no global hotkey to lose. ADR-0028
  reserves promotion of iOS to MUST parity for the ADR written when an iOS
  release is cut; that same ADR is where its channel belongs. Nothing here
  should be read as having decided it.
- **There is still no update mechanism.**
  [`../07-clients/desktop.md`](../07-clients/desktop.md) §Update channel
  specifies Sparkle-style signed updates over the direct channel with `stable`
  and `beta` channels, and says outright that none of it is built. This ADR
  makes that specification buildable for the first time — Sparkle needs a
  stable download URL, a Developer ID signature and an appcast, and the first
  two now exist — and deliberately does not build it. An installed copy
  currently learns about a new version the way it did before: it does not.
- **The `.dmg` goes in the GitHub Release and nowhere else.** Not the container
  image, which ships `sunrise-server` and has no business carrying a Mac app,
  and not the CLI tarball, whose contents are two executables plus `LICENSE`
  and `README.md`.
- **Every job in `release.yml` now carries a `timeout-minutes`** with its basis
  in a comment, following the convention `ci.yml` adopted. That is
  [#100](https://github.com/justin13888/Sunrise/issues/100) and it is folded in
  here rather than filed separately, because this change adds the most
  expensive job in the workflow and adding it without a bound would be adding
  the exact problem #100 describes.

## What would force revisiting this

1. **The App Sandbox landing for another reason.** Decision 1 rests on the
   sandbox being off. If it goes on — because Keychain access moves to an
   app-scoped group, or because a capability arrives that needs it — the
   strongest argument against the App Store is gone and the question is open
   again. Note the direction: the sandbox is what would change first, and this
   ADR would follow it, not the other way around.
2. **A second maintainer, or a second person who can hold the identity.**
   Decision 4's "this repository has one maintainer" is a real input. A team
   that can carry two release processes can reconsider carrying two channels.
3. **An Apple policy change that puts direct download behind a gate.** The
   decision assumes a Developer ID-signed, notarized app opens on a stock Mac
   without the user being asked to weaken anything. macOS has narrowed that
   path repeatedly. If it narrows to the point where a first launch needs a
   Settings visit, the calculation changes.
4. **Sparkle, or any auto-update mechanism, landing.** An update channel is a
   second thing that has to be signed and a second thing a user trusts. It does
   not reverse this decision, but the appcast's signing key and its hosting are
   decisions of the same kind and belong beside this record.
5. **An iOS release being cut.** Not because it changes anything here, but
   because it is the moment somebody will be tempted to fold both platforms
   into one App Store answer. The consequence above says why that does not
   follow.
