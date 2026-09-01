#!/usr/bin/env python3
"""Turn exported xcresult attachments into a readable screenshot directory.

`xcrun xcresulttool export attachments` writes every attachment under a bare
UUID — frequently with no extension at all — and drops a `manifest.json`
alongside mapping each to the name it was attached under. A directory of UUIDs
is not a design review, so this renames them.

Two things it does beyond renaming, both because the raw export is unusable
without them:

**It strips XCTest's uniquing suffix.** An attachment named `02-today` comes
back as `02-today_0_D34C48B4-2F93-4372-BE1C-EA0FC49D1170`. The suffix exists so
two attachments in one run cannot collide; it is noise in a filename a person
is going to read, and the walk's own numeric prefixes already sort correctly.

**It separates the walk's screenshots from XCTest's own.** A UI test run also
attaches "UI Snapshot", "Synthesized Event", an "App UI hierarchy" dump and a
screen recording — dozens of files, automatically, and far more of them than
the walk produces. They are genuinely useful when something failed and pure
noise when nothing did, so they go to `_xctest/` rather than into the bin: a
failed walk should still leave the recording that shows why.

Collisions are resolved rather than overwritten. Two attachments may carry one
name if a test retried, and the second is more likely to be the interesting
one, so neither is discarded.
"""

from __future__ import annotations

import json
import pathlib
import re
import sys

# XCTest appends `_<index>_<uuid>` to the name an attachment was added under.
UNIQUING_SUFFIX = re.compile(
    r"_\d+_[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-"
    r"[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$"
)

# Attachment names XCTest generates on its own, rather than ones a test asked
# for. Matched as prefixes because each carries a timestamp.
AUTOMATIC = (
    "UI Snapshot",
    "Synthesized Event",
    "App UI hierarchy",
    "Complete Issue Description",
    "Screen Recording",
    "kXCTAttachment",
)

# Enough of a sniff to give a file the extension its viewer needs. The export
# supplies none for most attachments.
MAGIC = (
    (b"\x89PNG\r\n\x1a\n", ".png"),
    (b"\xff\xd8\xff", ".jpg"),
    (b"GIF8", ".gif"),
)


def extension(path: pathlib.Path) -> str:
    if path.suffix:
        return path.suffix
    try:
        head = path.open("rb").read(12)
    except OSError:
        return ""
    for magic, suffix in MAGIC:
        if head.startswith(magic):
            return suffix
    # `ftyp` sits at offset 4 in an MP4 box header.
    if head[4:8] == b"ftyp":
        return ".mp4"
    return ""


def unique(target: pathlib.Path) -> pathlib.Path:
    if not target.exists():
        return target
    n = 2
    while True:
        candidate = target.with_name(f"{target.stem}-{n}{target.suffix}")
        if not candidate.exists():
            return candidate
        n += 1


def main(directory: str) -> int:
    root = pathlib.Path(directory)
    manifest = root / "manifest.json"
    if not manifest.is_file():
        print(f"{root}: no manifest.json; leaving filenames alone", file=sys.stderr)
        return 0

    entries = json.loads(manifest.read_text())
    if isinstance(entries, dict):
        entries = [entries]

    noise = root / "_xctest"
    kept = 0
    parked = 0

    for entry in entries:
        for attachment in entry.get("attachments", []):
            exported = attachment.get("exportedFileName")
            if not exported:
                continue
            source = next(root.glob(f"{exported}*"), None)
            if source is None or not source.is_file():
                continue

            wanted = attachment.get("suggestedHumanReadableName") or exported
            name = UNIQUING_SUFFIX.sub("", pathlib.Path(wanted).stem)
            suffix = extension(source)

            if wanted.startswith(AUTOMATIC):
                noise.mkdir(exist_ok=True)
                destination = unique(noise / f"{name}{suffix}")
                parked += 1
            else:
                destination = unique(root / f"{name}{suffix}")
                kept += 1
            source.rename(destination)

    manifest.unlink()
    print(f"{root}: {kept} screenshot(s), {parked} XCTest attachment(s) in _xctest/")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print("usage: name-shots.py <exported-attachments-directory>", file=sys.stderr)
        raise SystemExit(2)
    raise SystemExit(main(sys.argv[1]))
