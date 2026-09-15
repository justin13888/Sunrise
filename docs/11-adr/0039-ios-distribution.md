# 0039 — A tag uploads an iOS build to TestFlight; App Store submission is a separate, manual act

**Status:** accepted

**Amends:** [`../07-clients/overview.md`](../07-clients/overview.md)
§Distribution, whose iOS row said "TestFlight → App Store when a release is
cut" without saying what produces either.

**Takes half of the slot
[`./0028-ios-is-a-v1-client.md`](./0028-ios-is-a-v1-client.md) Decision 6
reserved, and leaves the other half where it is.** That decision reads:

> 6. **Promotion to MUST parity is a separate ADR**, written when an iOS
>    release is cut. This one deliberately does not pre-commit to it.

and [ADR-0031](./0031-macos-distribution.md) then pointed the channel question
at the same record: *"ADR-0028 reserves promotion of iOS to MUST parity for the
ADR written when an iOS release is cut; that same ADR is where its channel
belongs."*

So the reserved slot holds two things: **the channel** and **the promotion**.
This ADR decides the channel and builds it. It does **not** promote iOS to MUST
parity, and that is honouring Decision 6 rather than working around it: the
promotion is conditioned on a release being cut, and none has been. Nothing in
this change puts a build in front of a tester. The six secrets it depends on do
not exist, the App Store Connect app record does not exist, and only the
account holder can create either. The promotion ADR is the one written when a
build actually reaches somebody through TestFlight, and its evidence will be
that it did.

## Context

### iOS cannot use the answer macOS got

[ADR-0031](./0031-macos-distribution.md) settled macOS distribution and scoped
iOS out in terms that are worth repeating, because they are the reason this is
a separate record and not a second row in a table:

> iOS cannot ship by direct download at all: there is no equivalent of a
> Developer ID identity on the platform, so the only channels are TestFlight
> and the App Store, and the entitlement argument in Decision 1 does not
> transfer — iOS apps are sandboxed unconditionally and there is no global
> hotkey to lose.

Every load-bearing argument in ADR-0031 is about a choice iOS does not have.
The App Sandbox argument evaporates because iOS is always sandboxed. The
self-hosting argument — that a review queue puts a third party between a
self-hoster and their own client — survives as a *cost* rather than as a
deciding one, because on iOS there is no channel without that third party.
What is left is not "direct download versus the store"; it is "the store, or
nothing".

### The premise a review queue does not fit inside

`release.yml`'s own header states the property the whole file is built on:

> a release cannot happen without a tag object in the repository to point at
> afterwards

and ADR-0031 Decision 3 makes it sharper: *"A store submission has a queue, a
review outcome and a release date that are all decided after the tag is pushed
and by somebody else. There is no honest way to represent that in a workflow
whose premise is that pushing the tag *is* publishing."*

That argument is correct and it is about **App Store review**, not about
everything Apple. Uploading a build to App Store Connect has no queue, no human
and no rejection path: it succeeds or it fails, in the run, on the tag. The
build then appears in TestFlight once Apple's processing finishes, which is
minutes to hours of machine time rather than days of somebody's judgement.
[#141](https://github.com/justin13888/Sunrise/issues/141) put the two options
plainly and asked for one to be chosen deliberately. The line between them is
exactly the line between "a transfer that either happened or did not" and "a
decision somebody else makes later".

### What exists in the tree already

Read from the tree rather than from a plan:

- **`SunriseiOS` is a full application target** (`apps/apple/project.yml`),
  iPhone and iPad, sharing `Sunrise/` with the Mac and adding `iOS/`.
- **`SunriseiOSTests` and `SunriseiOSUITests` both run in CI on every pull
  request**, and ADR-0028 rests on that: iOS is the only Apple product where CI
  proves a tap reaches the core.
- **`mise run ios-app` and `ios-run` build and launch it on a simulator**, with
  `CODE_SIGN_IDENTITY: "-"` — ad-hoc signing, which
  [`../07-clients/overview.md`](../07-clients/overview.md) notes is not
  optional even on a simulator because iOS gates the Keychain on an
  application-identifier entitlement only a signed binary carries.

What has never existed is a signed *distribution* build. The gap is the same
class of gap ADR-0031 closed for macOS — not a narrowness inside something that
ships, but the absence of a way to ship it.

## Decision

**iOS ships through App Store Connect. Pushing a `vX.Y.Z` tag builds, signs and
uploads an iOS build, which lands in TestFlight. Submitting a build for App
Store review, and releasing it, are manual acts performed by the account holder
outside this repository — so the App Store version stream runs on its own
cadence, and this ADR says so rather than letting a workflow imply otherwise.**

### 1. The tag does the deterministic half and stops

`release.yml` gains an `ios-release` job: `mise run apple-xcframework`,
`xcodegen`, `xcodebuild archive` on the `SunriseiOS` scheme, then
`xcodebuild -exportArchive` with a `method: app-store-connect`,
`destination: upload` options plist and an App Store Connect API key.

`xcrun altool` is retired; `-exportArchive` with `-authenticationKeyPath` is
its supported replacement and does the export, the signing and the transfer in
one command, with no stored Apple ID and no app-specific password.

A successful run means App Store Connect **accepted** the upload, and the job
summary says exactly that. It does not wait for processing, and it does not
claim the build is in TestFlight — waiting on Apple's processing would be
putting the first minutes of the queue inside the tag, which is the thing this
decision refuses.

### 2. Nothing here submits anything for review

No step in this repository creates an App Store version, attaches a build to
one, answers an export-compliance question or presses Submit. That is deliberate
and it is the whole content of the cadence answer: the parts of shipping iOS
that a tag can do are in the workflow, and the parts that are somebody's
judgement are outside it, where their latency cannot be mistaken for a build
failure.

### 3. `release` does not depend on `ios-release`

The GitHub Release carries no iOS artifact — there is nowhere for one to go,
which is the whole reason iOS needed its own record — so gating the Release on
an App Store Connect upload would let an Apple-side outage withhold the CLI
tarballs and the container image. The job runs in parallel with the rest and
fails on its own.

### 4. It no-ops **visibly** until the account records exist

None of the six secrets below exists, and the App Store Connect app record they
authenticate against does not exist either; only the account holder can create
them. The owner's decision is to build the plumbing now and enrol later, so the
job has to be reviewable without being runnable.

A skipped job is the obvious way to do that and it is the wrong one: a skipped
job is a grey tick in a list of grey ticks, indistinguishable from one that was
never reached. Worse is a job that runs, does nothing and goes green, because a
green tick beside the words "iOS" reads as a successful upload. So:

* **The job's name changes.** `runs-on` and `name` both read `verify`'s
  `ios_configured` output, so the checks list says
  **iOS (not configured, no-op)** rather than **iOS (App Store Connect)**.
* **The summary says NOT UPLOADED**, in those words, followed by the list of
  missing secrets and a link to this ADR.
* **A `::notice::` is emitted**, so it surfaces in the run's annotations too.
* **The no-op runs on `ubuntu-latest`.** macOS minutes bill at 10x and the
  entire body of an unconfigured run is one `echo`.

All six or none: `verify` treats a partial set as unconfigured, because a
half-configured upload fails twenty minutes into a macOS runner with an Apple
error that names none of them.

### 5. The secrets the owner must create — this list is the handover

Every one is a **repository secret** (Settings → Secrets and variables →
Actions). All six, or `ios-release` does nothing.

| Secret | What it is | How |
|---|---|---|
| `IOS_APPSTORE_API_KEY_P8` | Base64 of an App Store Connect API key file, `AuthKey_<KEYID>.p8`, for a key with the **App Manager** role | App Store Connect → Users and Access → Integrations → App Store Connect API → Team Keys → **+**. Downloadable once. `base64 -i AuthKey_<KEYID>.p8 \| pbcopy` |
| `IOS_APPSTORE_API_KEY_ID` | That key's id — the `<KEYID>` in the filename, also in the KEY ID column | Copied from the same table |
| `IOS_APPSTORE_API_ISSUER_ID` | The team's Issuer ID, a UUID | Shown above the key table; the same for every key on the team |
| `IOS_DISTRIBUTION_CERTIFICATE_P12` | Base64 of a `.p12` holding the **Apple Distribution** certificate *and* its private key | Create the certificate at developer.apple.com → Certificates → **+** → Apple Distribution, install it, then export it with its key from Keychain Access → My Certificates. The same procedure `releasing.md` gives for the Developer ID `.p12`, including the `openssl pkcs12` check that the private key is really in there |
| `IOS_DISTRIBUTION_CERTIFICATE_PASSWORD` | The password set when exporting that `.p12` | — |
| `IOS_PROVISIONING_PROFILE` | Base64 of an **App Store** provisioning profile for `dev.sunrise.SunriseiOS` | developer.apple.com → Profiles → **+** → App Store, against the App ID for that bundle identifier and the certificate above. `base64 -i Sunrise_AppStore.mobileprovision \| pbcopy` |

And one prerequisite that is not a secret: **an App Store Connect app record
for the bundle identifier `dev.sunrise.SunriseiOS`**, which is what
`apps/apple/project.yml` ships. Without the record there is nothing for a build
to be uploaded *to*, and the upload fails with an Apple error rather than a
useful one. Renaming that identifier later is a new app record, not a rename.

**`MACOS_TEAM_ID` is reused, not duplicated.** It is the Apple Developer Team
ID, and this account has one. It is already a secret for the macOS job, and a
second copy under an iOS-flavoured name would be one more thing that can drift
out of step with the first for no benefit; the name is historical and this
record is the note that says so. Revisit trigger 5 below is what a second team
would fire.

**The App Store Connect key is *not* a reuse of the notarization key**
(`APPLE_API_KEY_P8`), and that is deliberate even though both are App Store
Connect API keys. The roles differ — `releasing.md` has the notarization key
created with the **Developer** role, which is all notarization asks for, while
uploading a build is an App Manager capability — but the decision does not rest
on the exact minimum role, because it would hold even if they overlapped: one
key would mean the credential that signs every `.dmg` also carries authority to
put binaries in front of users, and revoking either capability would take the
other with it. Two keys are two independent blast radii. If the account holder
finds a Developer-role key uploads fine, that is a note to add here, not a
reason to merge them.

### 6. Two App Store obligations are deliberately **not** answered here

Both are things a workflow can encode and neither is a thing a workflow should
decide, so they are named rather than guessed at:

- **Export compliance.** `ITSAppUsesNonExemptEncryption` is absent from
  `apps/apple/project.yml`, so App Store Connect will ask on the first upload.
  It is absent on purpose: the app bundles SQLCipher and does its own key
  handling, so the common `false` — "this app uses only encryption exempted by
  Apple's own APIs" — is not obviously true of it, and a committed `false` is a
  legal declaration made by whoever writes the line. The account holder answers
  it once, in App Store Connect; if the answer turns out to be stable it can
  then be committed with a record saying who decided it.
- **A privacy manifest.** There is no `PrivacyInfo.xcprivacy` in the tree. Apple
  requires one for apps using certain "required reason" APIs — `UserDefaults`
  is on that list and this app uses it — and an upload without one draws a
  warning before it draws a rejection. Writing the manifest means auditing what
  the app and its dependencies actually touch, which is its own piece of work
  and not something to fabricate in a distribution ADR.

Expect the first real upload to surface both. That is the correct place to
discover them: in App Store Connect's own response, with the account holder
reading it, rather than encoded as a guess in a committed file.

### 7. Manual signing, not `-allowProvisioningUpdates`

Xcode can create and download signing assets itself when given an API key. It
is rejected here because it makes a tag push mutate the team's signing assets on
Apple's side as a side effect — new certificates and profiles appearing because
a release ran — which is surprising, is not reversible from the workflow, and
is a poor fit for a credential set the owner is expected to audit. The
certificate and profile are inputs instead.

The profile's **name** and **UUID** are read out of the profile itself with
`security cms -D`, rather than carried as two more secrets that could disagree
with the file.

## Alternatives considered

**Decouple iOS from the tag entirely — a separate workflow, a separate
version stream, released on its own schedule.** Rejected, narrowly, and it is
the other option #141 named. It is honest about App Store review's latency, and
it costs the one property that makes the rest of this repository's release story
legible: that the artifacts for a version are produced by the same run, from the
same commit, with one thing to re-run when one of them fails. The chosen answer
gets the same honesty for free — App Store *submission* is already outside the
tag — without giving up the build's provenance. The version stream on the App
Store does drift from the tag stream, and Decision 2 says so; what does not
drift is which commit a given build came from.

**Make the tag submit for review as well.** Rejected on ADR-0031 Decision 3,
which this record does not reopen. A workflow that submits is a workflow whose
success means "somebody will decide later", and there is no honest way to
represent that in a green tick.

**`fastlane` (`match`, `pilot`, `deliver`).** Rejected. It is the industry
default and it would work. It also brings a Ruby toolchain, a Gemfile to keep
pinned, and — for `match` — a second private git repository holding signing
material, which is a whole new secret-management shape for a repository with
one maintainer and a working pattern for exactly this (`macos-app` already
imports a `.p12` into a temporary keychain and deletes it). `xcodebuild` does
the upload natively. Adding a dependency to avoid writing forty lines of YAML
that mirror forty lines already in the file next to it is the wrong trade here.

**Ship iOS ad-hoc or through an enterprise programme.** Not available. Ad-hoc
distribution caps at 100 registered devices and needs each one's UDID, which is
not a channel; the Apple Developer Enterprise Program is for in-house
distribution to an organisation's own employees and this is not that.

**Wait until the owner has enrolled, then write this.** Rejected on the owner's
own decision — build the plumbing, they will enrol — and on a practical point:
the shape of the job is the part worth reviewing, and reviewing it is cheaper
now, in a pull request, than in the hour after an App Store Connect record is
created.

## Consequences

- **`release.yml` gains an `ios-release` job** and `verify` gains two outputs,
  `ios_configured` and `ios_missing`. Neither is a gate: a missing iOS secret
  is the expected state and does not fail a release, unlike the seven macOS
  ones.
- **Six repository secrets and one App Store Connect app record are the
  handover.** Decision 5 is the list, and is why this ADR exists as much as the
  cadence answer is.
- **iOS still carries no MUSTs.** ADR-0028's parity column is untouched, its
  SHOULD marks stand, and its regression rule is unaffected. The promotion ADR
  its Decision 6 reserves is still unwritten and is still triggered by the same
  thing: a release actually reaching users.
- **`docs/07-clients/overview.md` §Distribution stops describing the iOS
  channel in the future tense.**
- **Nothing here is proved end to end, and cannot be.** The job has been linted
  and its structure asserted; no archive of the iOS target has been signed with
  a distribution certificate, no export options plist has been consumed by
  `-exportArchive` with `destination: upload`, and no build has been uploaded,
  because doing any of that requires credentials that do not exist and an
  account record only the owner can create. The first run after the six secrets
  exist is the test. This is stated here rather than discovered then.
- **A prerelease tag uploads too.** `v1.2.3-rc.1` produces a build like any
  other, which is right for TestFlight — that is what a TestFlight build is —
  and harmless for the App Store, which only ever sees a build somebody submits
  by hand.

## What would force revisiting this

1. **A build reaching a tester.** That fires ADR-0028's Decision 6 and the
   promotion ADR gets written. It also turns every "not proved end to end"
   sentence above into something that either held or did not, and the ADR that
   records the answer should say which.
2. **App Store review rejecting the app for a reason about the product rather
   than the package.** A self-hosted relay is exactly the kind of thing a
   reviewer can have an opinion about (ADR-0031 Decision 2 names the risk). A
   rejection is not a packaging bug and would reopen whether the App Store is a
   channel this project can rely on at all.
3. **`xcodebuild -exportArchive` losing `destination: upload`.** The whole
   upload mechanism is one plist key. Its removal, or an Apple requirement for
   a notarization-style wait, would put the transfer back into a separate tool
   and is worth re-deriving rather than patching.
4. **A capability arriving that the provisioning profile does not carry** —
   push notifications, App Groups, iCloud, HealthKit, a widget extension's own
   App ID. The profile is an input secret, so each one means regenerating and
   re-uploading it: an operational step with no signal in this repository until
   a build fails. If capabilities start changing regularly,
   `-allowProvisioningUpdates` becomes worth its cost after all and Decision 7
   should be re-argued.
5. **A second Apple account, or a change of team.** `MACOS_TEAM_ID` being one
   secret shared between the macOS and iOS jobs is levelled on there being one
   team. Two would make the shared name actively misleading.
