#!/usr/bin/env bash
#
# Reject a variable-time `==` / `!=` / `.eq(` / `.ne(` over a value whose name
# says it is a MAC, tag, digest, hash, signature, secret, key, nonce, checksum,
# token or proof, in the crates whose comparisons run on attacker-supplied
# bytes. ADR-0061 and docs/03-crypto/audit-scope.md §Constant-time gate.
#
# Usage: constant-time-gate.sh
#   Run from the repository root (CI does). CONSTANT_TIME_ALLOWLIST overrides
#   the allowlist path, which is how the contract test points it at a fixture.
#
# # What it is and what it is not
#
# A name heuristic, not a type analysis. `subtle::ConstantTimeEq` is how these
# crates compare a secret, and nothing enforced it: clippy's
# `disallowed-methods` sees method calls and paths, never the `==` operator, so
# `clippy.toml` has no spelling that bans `[u8; 32] == [u8; 32]`. What a grep
# can do is make every comparison of something *named* like a secret a line
# somebody has to justify, which is the property the issue asked for: today
# nothing distinguishes a harmless `!=` over a public digest from a harmful
# one, and after this one of them carries a written reason.
#
# A hit is either rewritten to `ct_eq`, or entered in the allowlist beside this
# script with the exact source line and the reason it is not a timing oracle.
# An allowlist entry that no longer matches a line fails the gate, so the list
# cannot outlive the code it excuses.
#
# The heuristic's blind spots are stated rather than hidden: a secret bound to
# a name with none of these words in it, a comparison split across lines, an
# operand that is a call with arguments or a method chain
# (`compute_mac(k, m) == received`, `mac.finalize().into_bytes() == expected`:
# only the last path segment is read, and only an empty `()` is looked
# through), and `matches!`/`assert_eq!` (deliberately: neither is how a
# verifier decides). The external audit (docs/03-crypto/audit-scope.md) is the
# backstop for all four.
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
allowlist="${CONSTANT_TIME_ALLOWLIST:-$here/constant-time-allowlist.tsv}"

# The names. Lower- and upper-case spellings both, since grep-gate.sh is
# case-sensitive and constants are SCREAMING_CASE. `sig` covers `signature`,
# `mac` covers `hmac`.
words='(mac|tag|digest|hash|sig|secret|key|nonce|checksum|token|proof|MAC|TAG|DIGEST|HASH|SIG|SECRET|KEY|NONCE|CHECKSUM|TOKEN|PROOF)'
ident='[[:alnum:]_]*'

# Three shapes, each anchored on the *last* path segment of an operand so that
# `self.tag == x` hits and `signing.public_bytes() == x` does not:
#   1. the left operand:   `tag == `, `expected_mac[..] != `, `digest() == `
#   2. the right operand:  ` == &other.tag`, ` != checksum(`
#   3. a method call:      `tag.eq(`, `digest().ne(` -- but not `ct_eq(`
left="(^|[^[:alnum:]_])${ident}${words}${ident}(\[[^]]*\])?(\(\))?[[:space:]]*(==|!=)"
right="(==|!=)[[:space:]]*[&*]*(${ident}(\.|::))*${ident}${words}${ident}"
method="(^|[^[:alnum:]_])${ident}${words}${ident}(\(\))?\.(eq|ne)\("

exec "$here/grep-gate.sh" --allowlist "$allowlist" \
  "constant-time" \
  "${left}|${right}|${method}" \
  crates/sunrise-crypto/src \
  crates/sunrise-pairing/src \
  crates/sunrise-http-sig/src \
  crates/sunrise-server/src/auth
