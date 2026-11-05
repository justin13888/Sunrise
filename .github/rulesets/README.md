# Repository rulesets

Each JSON file here records one of this repository's rulesets as it should be,
in the shape `gh api` accepts for an update. The live ruleset is a repository
setting: it changes with no commit, no diff and no review. A file here gives
it a diff, and the `Workflow hardening gate` checks every context the file
requires against the workflows (see that gate's docstring, assertion 3).

| file | live ruleset | protects |
| --- | --- | --- |
| `protect-master.json` | `Protect master`, id `13571319` | the default branch |

## Changing a ruleset is two acts, and only one is a commit

1. **The commit.** Edit the file in a pull request. The gate fails if a
   required context names no job, names a job built from `${{ matrix.* }}`,
   or names a job whose workflow does not run on every pull request.
2. **The admin act.** After that pull request merges, a repository admin
   applies the file:

   ```sh
   gh api -X PUT repos/justin13888/Sunrise/rulesets/13571319 \
     --input .github/rulesets/protect-master.json
   ```

   and confirms that what the API returns matches the file:

   ```sh
   gh api repos/justin13888/Sunrise/rulesets/13571319
   ```

Order matters for required contexts:

- **Adding one:** the job must already be on the default branch and must
  report on pull requests before the context is applied. Otherwise every open
  pull request waits for a check that never arrives.
- **Renaming or removing a job:** take its context out of the live ruleset
  first, then merge the rename.

`integration_id` `15368` is the GitHub Actions app. Pinning it means only a
check run from Actions satisfies the context. A status posted under the same
name by another app or token does not.

## What the gate does not check

The gate is offline. It reads this file, never the live ruleset, so it cannot
tell whether the admin act happened. A file that disagrees with the live
ruleset fails no check. The second command above is the only way to compare
them.

A required context also blocks merging only while its workflow is enabled and
running. A workflow disabled in the repository's Actions settings reports
nothing, so every required context it owns stays pending. Disable the workflow
only after removing its contexts from the live ruleset, and apply them again
only after it runs again.

## Current state

`protect-master.json` requires `Appcast generator contract` (#304). That is
not the whole set of contexts the rule required in the past, and nothing in
the tree records that set. The file lists only what a pull request has
justified, and other contexts join through pull requests of their own.
