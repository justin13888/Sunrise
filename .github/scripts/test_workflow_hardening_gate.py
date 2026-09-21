#!/usr/bin/env python3
"""The exit-code contract of `workflow-hardening-gate.py`, as assertions.

Why this file exists
--------------------

The gate is a regex over YAML, not a YAML parser, because no parser ships with
CPython and the runners install nothing extra. That buys speed and costs
precision, and the cases below are where the precision has to be bought back.

Three failure modes are specific enough to name:

  1. **Reading a shell line as a step.** `run: |` blocks in this repository
     contain hundreds of lines of shell, and any one of them could contain the
     text `uses:`. A gate that read those would be red on a green tree, and a
     gate people learn to ignore protects nothing.

  2. **Reading a job-level `permissions:` as a top-level one.** These differ
     only by indentation. `release.yml` carries both, so a gate that confused
     them would pass a workflow whose token scope was declared for one job and
     left to a settings page for the other six.

  3. **Reading only the spelling this repository happens to use.** `uses:`
     is a mapping key, and YAML will take it in a flow mapping, under a quoted
     key, with its value on the next line, or as a block scalar — all of which
     Actions accepts and `actionlint` passes. A gate that only matched
     `- uses: x` would be green over every one of them, and the block-scalar
     spelling is the sharpest: the mechanism that makes `run: |` safe to skip
     is the one that hides a ref written the same way.

  4. **Passing a tree it could not read.** A gate pointed at the wrong
     directory must not translate "I found nothing" into "there is nothing
     wrong", which is how the determinism and redaction gates were both
     silently unenforceable before they were rewritten. The same rule applies
     within a file: a `uses:` whose value the gate cannot parse is a violation,
     never a skip.

Fixtures are synthesised trees
------------------------------

The gate takes a root argument, so every case writes a `.github/` tree in a
temp directory and points the gate at it. Nothing here reads this repository's
own workflows — that is what the `workflow-hardening` job in ci.yml is for. A
contract test that also did would go red for a dependabot bump rather than for
a change to the contract.

Run it with `mise run workflow-hardening-gate-test`, or directly.
"""

from __future__ import annotations

import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest

GATE = pathlib.Path(__file__).resolve().parent / "workflow-hardening-gate.py"

CLEAN = 0
VIOLATION = 1
CANNOT_RUN = 2

PIN = "actions/checkout@11d5960a326750d5838078e36cf38b85af677262"


class WorkflowHardeningGateContract(unittest.TestCase):
    def setUp(self):
        self.tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.workflows = self.tmp / ".github" / "workflows"
        self.workflows.mkdir(parents=True)

    # -- helpers ---------------------------------------------------------

    def workflow(self, body: str, name: str = "ci.yml"):
        (self.workflows / name).write_text(body)

    def composite(self, body: str, name: str = "rust-checks"):
        directory = self.tmp / ".github" / "actions" / name
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "action.yml").write_text(body)

    def run_gate(self, root: pathlib.Path | None = None):
        return subprocess.run(
            [sys.executable, str(GATE), str(root or self.tmp)],
            capture_output=True,
            text=True,
        )

    def assert_code(self, result, expected: int, *fragments: str):
        self.assertEqual(
            result.returncode,
            expected,
            f"expected {expected}, got {result.returncode}\n{result.stdout}\n{result.stderr}",
        )
        for fragment in fragments:
            self.assertIn(fragment, result.stdout)

    # -- assertion 1: pinning --------------------------------------------

    def test_a_pinned_tree_is_clean(self):
        self.workflow(f"name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses: {PIN}  # v4.4.0\n")
        self.assert_code(self.run_gate(), CLEAN, "OK: workflow-hardening clean")

    def test_a_floating_major_tag_is_a_violation(self):
        # The configuration CVE-2025-30066 exploited.
        self.workflow("name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses: actions/checkout@v4\n")
        self.assert_code(self.run_gate(), VIOLATION, "`actions/checkout@v4` is not pinned")

    def test_a_branch_ref_is_a_violation(self):
        # `dtolnay/rust-toolchain@stable` is a *branch*, which moves more often
        # than a tag does, and read identically by the old configuration.
        self.workflow("name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses: dtolnay/rust-toolchain@stable\n")
        self.assert_code(self.run_gate(), VIOLATION, "`dtolnay/rust-toolchain@stable` is not pinned")

    def test_a_short_sha_is_a_violation(self):
        # An abbreviated sha is ambiguous and grows ambiguous as the repo does.
        self.workflow("name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses: actions/checkout@11d5960\n")
        self.assert_code(self.run_gate(), VIOLATION, "is not pinned")

    def test_a_subpath_action_pinned_by_sha_is_clean(self):
        self.workflow(f"name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses: {PIN.replace('checkout', 'checkout/sub/dir')}  # v4.4.0\n")
        self.assert_code(self.run_gate(), CLEAN)

    def test_a_local_ref_is_exempt(self):
        # This repository's own tree, already covered by whatever gates the tree.
        self.workflow("name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses: ./.github/actions/rust-checks\n")
        self.assert_code(self.run_gate(), CLEAN)

    def test_a_docker_ref_is_exempt(self):
        # A sha is not what pins a `docker://` ref.
        self.workflow("name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses: docker://alpine:3.20\n")
        self.assert_code(self.run_gate(), CLEAN)

    def test_a_composite_action_is_read_too(self):
        # The easiest place for an unpinned ref to hide from a reviewer who
        # opened `workflows/` and nothing else.
        self.workflow(f"name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses: {PIN}  # v4.4.0\n")
        self.composite("name: rust\nruns:\n  using: composite\n  steps:\n    - uses: Swatinem/rust-cache@v2\n")
        self.assert_code(self.run_gate(), VIOLATION, "`Swatinem/rust-cache@v2` is not pinned", "action.yml")

    def test_a_composite_action_needs_no_permissions_block(self):
        # `permissions:` is not a key a composite action may carry, so the
        # second assertion must not be applied to one.
        self.workflow(f"name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses: {PIN}  # v4.4.0\n")
        self.composite(f"name: rust\nruns:\n  using: composite\n  steps:\n    - uses: {PIN}  # v4.4.0\n")
        self.assert_code(self.run_gate(), CLEAN)

    # -- assertion 2: declared token scope --------------------------------

    def test_a_workflow_with_no_permissions_is_a_violation(self):
        self.workflow(f"name: CI\njobs:\n  a:\n    steps:\n      - uses: {PIN}  # v4.4.0\n")
        self.assert_code(self.run_gate(), VIOLATION, "no top-level `permissions:` block")

    def test_a_job_level_permissions_block_is_not_a_top_level_one(self):
        # The two differ only by indentation, and this is the confusion that
        # would pass a workflow scoped for one job out of seven.
        self.workflow(
            f"name: CI\njobs:\n  a:\n    permissions:\n      contents: read\n    steps:\n      - uses: {PIN}  # v4.4.0\n"
        )
        self.assert_code(self.run_gate(), VIOLATION, "no top-level `permissions:` block")

    def test_an_inline_empty_permissions_map_counts(self):
        # `permissions: {}` is release.yml's shape and is a declaration.
        self.workflow(f"name: CI\npermissions: {{}}\njobs:\n  a:\n    permissions:\n      contents: read\n    steps:\n      - uses: {PIN}  # v4.4.0\n")
        self.assert_code(self.run_gate(), CLEAN)

    def test_every_workflow_is_checked_not_just_the_first(self):
        self.workflow(f"name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses: {PIN}  # v4.4.0\n")
        self.workflow(f"name: Release\njobs:\n  a:\n    steps:\n      - uses: {PIN}  # v4.4.0\n", name="release.yml")
        self.assert_code(self.run_gate(), VIOLATION, "release.yml", "no top-level `permissions:` block")

    # -- parsing: what the regex must not read ----------------------------

    def test_a_uses_line_inside_a_block_scalar_is_not_a_step(self):
        # The gate's central risk: `run: |` blocks here hold hundreds of lines
        # of shell, and one containing `uses:` must not turn a green tree red.
        self.workflow(
            "name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n"
            f"      - uses: {PIN}  # v4.4.0\n"
            "      - run: |\n"
            "          echo 'uses: evil/thing@v1'\n"
            "          uses: another/thing@v2\n"
        )
        self.assert_code(self.run_gate(), CLEAN)

    def test_a_blank_line_does_not_end_a_block_scalar(self):
        self.workflow(
            "name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n"
            f"      - uses: {PIN}  # v4.4.0\n"
            "      - run: |\n"
            "          echo one\n"
            "\n"
            "          uses: evil/thing@v1\n"
        )
        self.assert_code(self.run_gate(), CLEAN)

    def test_a_commented_out_uses_is_not_a_step(self):
        self.workflow(
            f"name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses: {PIN}  # v4.4.0\n      # - uses: actions/checkout@v4\n"
        )
        self.assert_code(self.run_gate(), CLEAN)

    def test_a_trailing_version_comment_does_not_become_part_of_the_ref(self):
        self.workflow(f"name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses: {PIN}  # v4.4.0\n")
        self.assert_code(self.run_gate(), CLEAN)

    def test_a_hash_inside_quotes_is_not_a_comment(self):
        self.workflow(
            f"name: CI\npermissions:\n  contents: read\nenv:\n  MSG: \"a # b\"\njobs:\n  a:\n    steps:\n      - uses: {PIN}  # v4.4.0\n"
        )
        self.assert_code(self.run_gate(), CLEAN)

    # -- parsing: spellings that are not `- uses: x` ----------------------

    def test_a_flow_mapping_is_read(self):
        self.workflow("name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - {uses: actions/checkout@v4}\n")
        self.assert_code(self.run_gate(), VIOLATION, "`actions/checkout@v4` is not pinned")

    def test_a_flow_mapping_with_a_sibling_key_is_read(self):
        self.workflow("name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - {name: x, uses: actions/checkout@v4}\n")
        self.assert_code(self.run_gate(), VIOLATION, "`actions/checkout@v4` is not pinned")

    def test_a_quoted_key_is_read(self):
        self.workflow("name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - \"uses\": actions/checkout@v4\n")
        self.assert_code(self.run_gate(), VIOLATION, "`actions/checkout@v4` is not pinned")

    def test_a_value_on_the_following_line_is_read(self):
        self.workflow("name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses:\n          actions/checkout@v4\n")
        self.assert_code(self.run_gate(), VIOLATION, "`actions/checkout@v4` is not pinned")

    def test_a_ref_written_as_a_folded_scalar_is_not_a_pass(self):
        # The gate cannot read it, and "cannot read" must never mean "absent".
        self.workflow("name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses: >-\n          actions/checkout@v4\n")
        self.assert_code(self.run_gate(), VIOLATION, "cannot read")

    def test_a_ref_written_as_a_literal_scalar_is_not_a_pass(self):
        self.workflow("name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses: |-\n          actions/checkout@v4\n")
        self.assert_code(self.run_gate(), VIOLATION, "cannot read")

    def test_a_uses_key_with_no_value_at_all_is_not_a_pass(self):
        self.workflow("name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses:\n      - run: echo\n")
        self.assert_code(self.run_gate(), VIOLATION, "cannot read")

    def test_a_sibling_key_after_a_block_scalar_first_step_is_read(self):
        # The block scalar's extent is measured from the key, not the dash, so
        # the step's own sibling keys are outside it.
        self.workflow(
            "name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n"
            "      - run: |\n          echo hi\n        uses: actions/checkout@v4\n"
        )
        self.assert_code(self.run_gate(), VIOLATION, "`actions/checkout@v4` is not pinned")

    def test_a_block_scalar_body_is_still_skipped_when_a_sibling_key_follows(self):
        # The mirror of the case above: the body is inside, `env:` ends it, and
        # the step after it is read normally.
        self.workflow(
            "name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n"
            f"      - uses: {PIN}  # v4.4.0\n"
            "      - run: |\n          uses: evil/thing@v1\n          echo done\n"
            "        env:\n          A: b\n"
            f"      - uses: {PIN}  # v4.4.0\n"
        )
        self.assert_code(self.run_gate(), CLEAN)

    # -- the pin has to say what it is ------------------------------------

    def test_a_pin_with_no_version_comment_is_a_violation(self):
        # Forty hex characters tell a reviewer nothing on their own.
        self.workflow(f"name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses: {PIN}\n")
        self.assert_code(self.run_gate(), VIOLATION, "pinned but unlabelled")

    # -- composite actions, wherever they are -----------------------------

    def test_a_nested_composite_action_is_read(self):
        # `uses: ./.github/actions/a/b` is legal, so the search cannot be one
        # level deep.
        self.workflow(f"name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses: {PIN}  # v4.4.0\n")
        self.composite("name: n\nruns:\n  using: composite\n  steps:\n    - uses: Swatinem/rust-cache@v2\n", name="a/b")
        self.assert_code(self.run_gate(), VIOLATION, "`Swatinem/rust-cache@v2` is not pinned")

    def test_a_composite_action_spelled_action_yaml_is_read(self):
        self.workflow(f"name: CI\npermissions:\n  contents: read\njobs:\n  a:\n    steps:\n      - uses: {PIN}  # v4.4.0\n")
        directory = self.tmp / ".github" / "actions" / "yamlish"
        directory.mkdir(parents=True)
        (directory / "action.yaml").write_text("name: y\nruns:\n  using: composite\n  steps:\n    - uses: Swatinem/rust-cache@v2\n")
        self.assert_code(self.run_gate(), VIOLATION, "`Swatinem/rust-cache@v2` is not pinned", "action.yaml")

    # -- fail closed ------------------------------------------------------

    def test_a_tree_with_no_workflow_directory_is_not_a_pass(self):
        empty = self.tmp / "elsewhere"
        empty.mkdir()
        self.assert_code(self.run_gate(root=empty), CANNOT_RUN, "is not a repository root")

    def test_an_empty_workflow_directory_is_not_a_pass(self):
        for path in self.workflows.iterdir():
            path.unlink()
        self.assert_code(self.run_gate(), CANNOT_RUN, "holds no workflow")


if __name__ == "__main__":
    unittest.main(verbosity=2)
