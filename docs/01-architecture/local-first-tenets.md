---
status: accepted
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
- *Target state:* export emits human-readable JSON + Markdown notes + ICS for blocks, encrypted with a user-held key only. What exists is narrower — the `sunrise` CLI's `export` subcommand and the iCalendar export in `sunrise-integrations`; there is no `Core::export` and no whole-vault archive.
- Open-source client and server, AGPL-licensed.

## 5. Privacy and security by default

**Definition.** Plaintext content is never on a Sunrise-operated server. Plaintext is never in plaintext logs, metrics, or analytics. Encryption is on; there is no "off" mode.

**Implication.** No "free tier without encryption." No analytics SDK that ships event payloads with content fields.

## 6. Retain ownership and control

**Definition.** The user can leave Sunrise at any time with a complete, decrypted, structured copy of their data.

**Implication.** *Target state.* Export is a first-class feature, not a hidden setting, and account deletion wipes server-side blobs and metadata. Neither is met yet: export covers datasets rather than the vault (above), and account deletion has no route at all ([`../06-server/api.md`](../06-server/api.md)).

## 7. Multi-device collaboration without coordination

**Definition.** Two devices can each make changes offline for an arbitrary period, and merge cleanly when they reconnect. "Cleanly" means: no manual conflict UIs.

**Implication.** State is **op-shaped**, and every entity carries a merge rule. In v1 that rule is entity-level last-writer-wins ([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)), not a CRDT: the workspace ships no CRDT library. This tenet is therefore met with a caveat worth stating plainly — LWW *does* lose a concurrent edit to the same entity, keeping one side rather than merging both. What it guarantees is that every device converges on the same survivor with no conflict popup, which is the property the tenet is really about. Per-field merge, which would narrow the loss to genuinely-colliding fields, is the deferred design in [`05-sync/crdt-design.md`](../05-sync/crdt-design.md). See [`05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md).

---

## How we test these tenets

- **Tenet 1:** automated perf budget on every release; flame chart attached to PRs that touch hot paths.
- **Tenets 2/7:** integration tests with simulated multi-device divergence (3-way splits, week-long offline windows).
- **Tenet 3:** "airplane mode setup" smoke test for every release.
- **Tenet 4:** export round-trip test — export, then re-import into a fresh install, must produce equivalent state. Partially in place: `exporting_and_re_importing_is_the_identity` covers the iCalendar path across the vault and the seam, not a whole-vault archive.
- **Tenet 5:** a CI check rejects any code path that logs or transmits anything from the `Plain<T>` typed wrapper outside the local process.
- **Tenet 6:** tested via tenet 4.
