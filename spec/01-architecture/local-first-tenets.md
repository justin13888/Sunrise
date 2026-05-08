---
status: draft
---

# Local-First Tenets

Sunrise commits to the seven principles described in Kleppmann et al.'s "Local-first software" (2019), with concrete operational definitions for each.

## 1. No spinners

**Definition.** Every read of state the user owns completes in ≤16ms (one frame at 60Hz) on a 5-year-old device.

**Implication.** All reads hit the local DB. The UI never blocks on the network. A "loading" indicator is only ever for a remote integration (Google Calendar fetch), never for the user's own data.

## 2. Work is not stuck on one device

**Definition.** Every change made on Device A becomes visible on Device B once both are online. There is no "primary" device. There is no manual export/import to move between devices.

**Implication.** Sync is automatic, continuous, and resilient to multi-week offline gaps.

## 3. The network is optional

**Definition.** Sunrise on a brand-new device can be set up *without* a network connection if a paired device is on the same LAN (or via QR + USB tether). For ongoing operation, network is needed only to move ops between devices.

**Implication.** Pairing supports an out-of-band channel (QR over camera). Setup is not blocked on a server reachability check.

## 4. Long now

**Definition.** Sunrise data created today must be readable in 10 years, even if the company / project is gone.

**Implication.**
- Data format is fully documented (CDDL-described).
- Export emits human-readable JSON + Markdown notes + ICS for blocks, encrypted with a user-held key only.
- Open-source client and server, AGPL-licensed.

## 5. Privacy and security by default

**Definition.** Plaintext content is never on a Sunrise-operated server. Plaintext is never in plaintext logs, metrics, or analytics. Encryption is on; there is no "off" mode.

**Implication.** No "free tier without encryption." No analytics SDK that ships event payloads with content fields.

## 6. Retain ownership and control

**Definition.** The user can leave Sunrise at any time with a complete, decrypted, structured copy of their data.

**Implication.** Export is a first-class feature, not a hidden setting. Account deletion wipes server-side blobs and metadata.

## 7. Multi-device collaboration without coordination

**Definition.** Two devices can each make changes offline for an arbitrary period, and merge cleanly when they reconnect. "Cleanly" means: no lost changes, no manual conflict UIs for any field a CRDT can merge.

**Implication.** State is CRDT-shaped. The few fields where merge is genuinely ambiguous (e.g. "due date" set on two devices) get a documented merge rule, not a conflict popup. See [`05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md).

---

## How we test these tenets

- **Tenet 1:** automated perf budget on every release; flame chart attached to PRs that touch hot paths.
- **Tenets 2/7:** integration tests with simulated multi-device divergence (3-way splits, week-long offline windows).
- **Tenet 3:** "airplane mode setup" smoke test for every release.
- **Tenet 4:** export round-trip test — export, then re-import into a fresh install, must produce equivalent state.
- **Tenet 5:** a CI check rejects any code path that logs or transmits anything from the `Plain<T>` typed wrapper outside the local process.
- **Tenet 6:** tested via tenet 4.
