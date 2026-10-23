# 0067 — Windows secret files get a protected, one-entry DACL, set by the workspace's one `unsafe` module

**Status:** accepted

**Amends** the workspace-wide `unsafe_code = "forbid"` (`Cargo.toml`
`[workspace.lints.rust]`) by one exception: `crates/sunrise-private-file`'s
`src/windows.rs`, compiled on Windows only.

**Tracked by** [#486](https://github.com/justin13888/Sunrise/issues/486), the
remainder of [#369](https://github.com/justin13888/Sunrise/issues/369).

## Context

Every secret Sunrise writes to disk goes through one routine: the CLI's vault
key, parked pairing secret, exported pairing grant and sealed undo history,
and the credential file `sunrise_auth::FileStore` keeps. On Unix that routine
creates the file with mode `0600`. On Windows it set nothing, so the file took
whatever ACL its directory passed down. Under the user's profile that is the
user, `SYSTEM` and `Administrators`. A vault directory created under `C:\`
passes down `Authenticated Users` modify, so every account on the machine
could read the vault key.

Restricting a file's DACL needs Win32 calls: the process token's user SID, a
security descriptor, and `CreateFileW` with it. Every Rust binding for them is
`unsafe`. The workspace forbids `unsafe_code`, and a member crate cannot
inherit `[workspace.lints]` and relax one entry, because `forbid` cannot be
lifted by an `#[allow]`.

## Decision

1. **A new crate, `sunrise-private-file`, holds the one owner-only write.**
   `write_private(path, bytes)` stages the bytes in a fresh hidden sibling
   that is created owner-only, flushes it, and renames it over `path`. Both
   `sunrise-auth` and `sunrise-cli` call it. They each had a copy before, and
   the copies already differed in their staging name.
2. **The crate is the workspace's only exception to `forbid(unsafe_code)`,
   and the exception covers one module on one platform.** Its `Cargo.toml`
   copies the workspace lint table with `unsafe_code = "deny"` and adds
   `clippy::undocumented_unsafe_blocks = "deny"`. `src/lib.rs` forbids
   `unsafe_code` again everywhere but Windows. `src/windows.rs` alone carries
   `#![allow(unsafe_code)]`. Each `unsafe` block in it is one FFI call or one
   pointer read, with a `SAFETY:` comment. Every Windows-allocated buffer and
   every handle is owned by a value that frees it on drop. No raw pointer
   crosses the module boundary.
3. **What owner-only means on Windows.** The DACL is `D:P(A;;FA;;;<sid>)`:
   protected from inheritance, with exactly one entry, which allows full
   control to the SID the process token names as its user. It is set at
   creation, through `CreateFileW`'s `SECURITY_ATTRIBUTES` with `CREATE_NEW`,
   so no secret byte is ever written to a file with any other DACL. The
   rename that follows moves the file together with its descriptor.
4. **`SYSTEM` and `Administrators` get no entry.** This is the product
   decision this ADR records. The Unix twin is `0600`, which names no group
   either, and the point is that the file is the user's. An administrator can
   still take ownership and rewrite the DACL, and a process holding
   `SeBackupPrivilege` (backup software, for one) reads past a DACL by
   design. Neither can be prevented at file level, and neither is the threat
   this closes, which is other ordinary accounts on the same machine.
5. **The owner is left to the token's default.** For an elevated
   administrator, Windows may make `Administrators` the owner. An owner can
   always read and change the DACL, so that is no wider than decision 4
   already accepts. Setting the owner explicitly would add a failure mode, a
   refused owner SID, for no protection.
6. **The protection is checked on every platform.**
   `sunrise_private_file::verify_owner_only(path)` reads it back: mode `0600`
   on Unix; on Windows a protected DACL, one entry, an allow entry, full
   control, and a SID `EqualSid` to the current user's. SIDs are compared as
   SIDs, not as SDDL text, because SDDL prints some accounts by alias (`LA`
   for the built-in Administrator). The tests in `sunrise-private-file`,
   `sunrise-auth` and `sunrise-cli` call it unconditionally. CI's
   `rust-windows` job runs all three. One of the tests gives the temporary
   directory an inheritable `Everyone` full-control DACL first, which is
   wider than the `C:\` case, and still finds the file owner-only.

## Alternatives considered

- **A safe third-party crate for Windows ACLs.** The ones we know of
  (`windows-acl`, `windows-permissions`) wrap these same calls in their own
  `unsafe`, and each is small and has a single maintainer. Depending on one
  would move the `unsafe` out of this repository's review, not out of the
  binary. `cargo deny` vets licences and advisories, not soundness. The calls
  needed here fit in one module.
- **The Windows Credential Manager (`keyring`) for the credential file.** It
  caps a blob at 2560 bytes, and a bearer plus a refresh token can exceed
  that. It would also cover one of the five secrets: the vault key, the
  pairing files and the undo history would still need this.
- **`icacls` after creation.** It cannot act until the file exists, so the
  DACL is not set at creation. It needs a subprocess and `icacls` on `PATH`,
  and the account has to be named in a form that differs across locales and
  domains.
- **An `unsafe` block in each of `sunrise-auth` and `sunrise-cli`.** That
  makes two exceptions instead of one, in crates whose other code has no
  reason to be near one, and keeps the two copies of the routine.
- **Restricting the vault directory instead of the files.** The exported
  grant goes wherever `--out` names, which is not under the vault directory.
  A directory's DACL also reaches only files that inherit from it, and a file
  moved in from elsewhere keeps its own.

## Consequences

- `unsafe_code = "forbid"` no longer holds for every crate on every platform.
  It holds everywhere except `sunrise-private-file`'s `src/windows.rs` on
  Windows. A second exception needs its own ADR, and the place for any
  further Win32 security call is this module.
- `sunrise-private-file` is the one crate whose `[lints]` table is not
  `workspace = true`. A lint added to the workspace has to be added there too.
  Its `Cargo.toml` says so.
- Files written before this change keep the DACL they inherited until they
  are next written. The credential file, the undo history and the pairing
  files are rewritten in normal use. The vault key is written once, when the
  vault is created, so a Windows vault created before this change keeps an
  inherited DACL on its key. No migration rewrites it: no Windows client
  ships, and the CLI has run on Windows only since #369. A vault created or
  joined (`pair accept`) after this change has its key written through this
  path.
- Directories are not restricted. A vault directory still takes its parent's
  DACL, so file names and sizes in it are as visible as their parent makes
  them. The contents of the secret files are not.
- A volume with no ACLs (FAT32, exFAT) ignores the descriptor.
  `verify_owner_only` then reports the file as not owner-only, which is the
  truth.
