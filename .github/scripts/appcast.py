#!/usr/bin/env python3
"""Build the Sparkle appcast for the macOS app from published GitHub Releases.

Why this is a script and not twenty lines of YAML
-------------------------------------------------

The appcast is the file that tells every installed copy of Sunrise which build
to download and run next. It is therefore the most security-relevant artifact
this repository produces after the `.dmg` itself, and the rules it encodes —
which release goes on which channel, which download an item points at, whether
the signature in hand actually describes the bytes being advertised — are
decisions, not formatting. Decisions belong somewhere they can be tested, which
in this repository means a file with a `test_` twin beside it
(`test_appcast.py`), exactly as the file-size and core-filesystem gates are.

Nothing here touches the network. The workflow fetches — `gh api` for the
release list, `gh release download` for the per-release item files — and this
script is a pure function from those files to one XML document. That is what
makes `test_appcast.py` able to assert the whole contract with a tmpdir and no
fixtures beyond JSON.

The two inputs
--------------

**`--releases`** is the body of `GET /repos/{owner}/{repo}/releases`, either as
one JSON array or as one JSON object per line — see `read_releases` for why the
workflow hands over the second. The only fields read are `tag_name`,
`prerelease`, `draft`, `published_at`, `html_url` and
`assets[].{name,size,browser_download_url}`.

`prerelease` is the whole channel decision and it is deliberately read from
here rather than re-derived from the tag string. `release.yml`'s `verify` job
computes it once from the tag, the `release` job writes it onto the GitHub
Release with `-F prerelease=...`, and this script reads that back. A second
`case "$version" in *-*)` anywhere in the pipeline would be a second source of
truth that could disagree with the first; there is one, and it is `verify`.

**`--items-dir`** holds `<tag>/appcast-item.json`, one per release, produced by
the `macos-app` job on the machine that built and signed that release's `.dmg`
and uploaded to the Release as an asset. It carries exactly the things the
Releases API cannot tell us:

```json
{
  "schema": 1,
  "shortVersionString": "1.2.3",
  "version": "417",
  "dmg": "sunrise-1.2.3-aarch64-apple-darwin.dmg",
  "length": 12345678,
  "edSignature": "base64…",
  "minimumSystemVersion": "26.0"
}
```

`version` is the bundle's `CFBundleVersion`, which is the workflow run number —
the value Sparkle compares against the running copy. It cannot be recovered
from a tag, which is the reason this sidecar exists at all. `edSignature` is
the EdDSA signature over the `.dmg`, computed by `sign_update` beside the
`shasum` that produces the `.sha256`: on the machine that made the file, never
after an artifact round trip.

What is checked, and why each check is here
-------------------------------------------

* **The named `.dmg` must be an asset of that release.** An item advertising a
  download that 404s is an update mechanism that fails for every user at once.
* **`length` must equal the asset's size in the API.** The signature is over a
  specific byte sequence; if the file on the Releases page is not the file that
  was signed, Sparkle would reject the update after downloading it. Failing
  here says so before anybody's Mac finds out.
* **`edSignature` must be present and non-empty.** An item with no signature is
  one Sparkle refuses, and a feed full of them is a broken updater that looks
  like a working one.

A release with no item file is **skipped with a note**, not an error: releases
predating this pipeline exist and have no sidecar, and a feed that refuses to
build because of history nobody can change is a feed that never builds.

Channels
--------

A `prerelease` release emits `<sparkle:channel>beta</sparkle:channel>`. A
stable release emits no channel element at all, which is how Sparkle spells
"the default channel every installation is subscribed to". Only an installation
that opts in — `SPUUpdaterDelegate.allowedChannels`, wired to a user-visible
toggle in `apps/apple/macOS/SoftwareUpdate.swift` — is offered the beta items.

The cap is **per channel**, not over the merged list. Ten betas in a row would
otherwise push every stable item out of a globally-capped feed and strand the
users who never opted into anything.

See `docs/11-adr/0037-macos-update-feed.md` for the trust argument, and
`docs/07-clients/releasing.md` for the operator's view.
"""

from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from xml.sax.saxutils import escape, quoteattr

SPARKLE_NS = "http://www.andymatuschak.org/xml-namespaces/sparkle"

#: The name of the per-release sidecar, in one place because the workflow that
#: writes it and the job that downloads it both have to spell it identically.
ITEM_FILENAME = "appcast-item.json"

#: The channel name a prerelease lands on. Sparkle matches this string against
#: whatever `allowedChannels` returns, so it is shared vocabulary with
#: `apps/apple/macOS/SoftwareUpdate.swift` and changing it here alone would
#: silently empty the beta channel.
BETA_CHANNEL = "beta"

#: Weekday and month names spelled out rather than taken from `strftime`.
#: RFC 822 dates are English by definition and `%a`/`%b` follow the runner's
#: locale, so this is the difference between a deterministic document and one
#: that depends on an environment variable nobody set on purpose.
_WEEKDAYS = ("Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun")
_MONTHS = (
    "Jan", "Feb", "Mar", "Apr", "May", "Jun",
    "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
)


class AppcastError(Exception):
    """A condition that must stop the release rather than ship a broken feed."""


def rfc822(iso8601: str) -> str:
    """Render a GitHub `published_at` timestamp the way RSS spells a date.

    GitHub emits `2026-09-14T10:11:12Z`. `datetime.fromisoformat` handles the
    `Z` from Python 3.11, and the runners and this repository's gates are well
    past that; the explicit replacement keeps it working on 3.10 anyway rather
    than failing on a version difference nobody would look for here.
    """
    parsed = datetime.fromisoformat(iso8601.replace("Z", "+00:00"))
    parsed = parsed.astimezone(timezone.utc)
    return (
        f"{_WEEKDAYS[parsed.weekday()]}, {parsed.day:02d} "
        f"{_MONTHS[parsed.month - 1]} {parsed.year:04d} "
        f"{parsed.hour:02d}:{parsed.minute:02d}:{parsed.second:02d} +0000"
    )


def read_releases(path: Path) -> list[dict[str, Any]]:
    """Accept either a JSON array or one JSON object per line.

    `gh api --paginate` over a paged array endpoint does not reliably emit one
    array: depending on the version it concatenates a document per page. The
    workflow therefore asks for `--jq '.[]'`, which is newline-delimited
    objects and is unambiguous on every version. A plain array is still
    accepted because that is what a hand-saved API response looks like and
    what the tests are clearest written against.
    """
    text = path.read_text(encoding="utf-8").strip()
    if not text:
        return []
    if text.startswith("["):
        parsed = json.loads(text)
        if not isinstance(parsed, list):
            raise AppcastError(f"{path}: expected a JSON array")
        return parsed
    releases = []
    for number, line in enumerate(text.splitlines(), start=1):
        line = line.strip()
        if not line:
            continue
        try:
            releases.append(json.loads(line))
        except json.JSONDecodeError as exc:
            raise AppcastError(f"{path}:{number}: not valid JSON: {exc}") from exc
    return releases


def load_item(items_dir: Path, tag: str) -> dict[str, Any] | None:
    """Read one release's sidecar, or `None` when it has none."""
    path = items_dir / tag / ITEM_FILENAME
    if not path.is_file():
        return None
    try:
        item = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise AppcastError(f"{path}: not valid JSON: {exc}") from exc
    if not isinstance(item, dict):
        raise AppcastError(f"{path}: expected a JSON object")
    return item


def build_entry(release: dict[str, Any], item: dict[str, Any]) -> dict[str, Any]:
    """Cross-check one release against its sidecar and flatten the pair.

    Every `AppcastError` below is a case where continuing would publish a feed
    that looks fine and does not work.
    """
    tag = release["tag_name"]

    required = ("shortVersionString", "version", "dmg", "length", "edSignature")
    missing = [key for key in required if not item.get(key)]
    if missing:
        raise AppcastError(f"{tag}: {ITEM_FILENAME} is missing {', '.join(missing)}")

    assets = {asset["name"]: asset for asset in release.get("assets", [])}
    asset = assets.get(item["dmg"])
    if asset is None:
        raise AppcastError(
            f"{tag}: {ITEM_FILENAME} names {item['dmg']}, which is not an asset "
            f"of that release. The feed would advertise a download that 404s."
        )

    if int(asset["size"]) != int(item["length"]):
        raise AppcastError(
            f"{tag}: {item['dmg']} is {asset['size']} bytes on the Release and "
            f"{item['length']} bytes in {ITEM_FILENAME}. The EdDSA signature "
            f"describes the bytes that were signed, so these disagreeing means "
            f"the asset is not the file that was signed."
        )

    return {
        "tag": tag,
        "prerelease": bool(release.get("prerelease")),
        "published_at": release["published_at"],
        "html_url": release.get("html_url", ""),
        "url": asset["browser_download_url"],
        "length": int(item["length"]),
        "ed_signature": item["edSignature"],
        "version": str(item["version"]),
        "short_version": str(item["shortVersionString"]),
        "minimum_system_version": str(item.get("minimumSystemVersion", "")),
    }


def select(entries: list[dict[str, Any]], max_items: int) -> list[dict[str, Any]]:
    """Newest first, capped per channel rather than over the merged list."""
    ordered = sorted(entries, key=lambda e: (e["published_at"], e["tag"]), reverse=True)
    stable = [e for e in ordered if not e["prerelease"]][:max_items]
    beta = [e for e in ordered if e["prerelease"]][:max_items]
    kept = {id(e) for e in stable + beta}
    return [e for e in ordered if id(e) in kept]


def render_item(entry: dict[str, Any]) -> str:
    """One `<item>`, as text.

    Hand-built rather than `xml.etree`: the namespaced element names Sparkle
    requires (`sparkle:edSignature` on an attribute, in particular) come out of
    `ElementTree` as generated `ns0:` prefixes, and the document Sparkle parses
    should be the document a human reads in the Releases page.
    """
    lines = [
        "    <item>",
        f"      <title>{escape(entry['short_version'])}</title>",
    ]
    if entry["html_url"]:
        lines.append(f"      <link>{escape(entry['html_url'])}</link>")
    lines += [
        f"      <sparkle:version>{escape(entry['version'])}</sparkle:version>",
        "      <sparkle:shortVersionString>"
        f"{escape(entry['short_version'])}</sparkle:shortVersionString>",
    ]
    if entry["prerelease"]:
        lines.append(f"      <sparkle:channel>{BETA_CHANNEL}</sparkle:channel>")
    if entry["minimum_system_version"]:
        lines.append(
            "      <sparkle:minimumSystemVersion>"
            f"{escape(entry['minimum_system_version'])}"
            "</sparkle:minimumSystemVersion>"
        )
    lines.append(f"      <pubDate>{rfc822(entry['published_at'])}</pubDate>")
    if entry["html_url"]:
        # Escaped HTML rather than CDATA, which is what RSS readers and Sparkle
        # both expect in `<description>`. Inline, so the release notes a user
        # is shown are covered by the feed's own signature instead of being a
        # second URL somebody would have to authenticate separately.
        notes = (
            f'&lt;p&gt;Release notes for &lt;a href="{escape(entry["html_url"])}"&gt;'
            f"{escape(entry['tag'])}&lt;/a&gt;.&lt;/p&gt;"
        )
        lines.append(f"      <description>{notes}</description>")
    lines.append(
        "      <enclosure "
        f"url={quoteattr(entry['url'])} "
        f"length={quoteattr(str(entry['length']))} "
        'type="application/octet-stream" '
        f"sparkle:edSignature={quoteattr(entry['ed_signature'])} />"
    )
    lines.append("    </item>")
    return "\n".join(lines)


def render(entries: list[dict[str, Any]], repository: str) -> str:
    """The whole document."""
    title = f"{repository.split('/')[-1]} macOS updates"
    parts = [
        '<?xml version="1.0" encoding="utf-8"?>',
        f'<rss version="2.0" xmlns:sparkle="{SPARKLE_NS}">',
        "  <channel>",
        f"    <title>{escape(title)}</title>",
        f"    <link>https://github.com/{escape(repository)}/releases</link>",
        "    <description>Sunrise for macOS. Stable items carry no channel; "
        f"prereleases carry the {BETA_CHANNEL} channel.</description>",
        "    <language>en</language>",
    ]
    parts += [render_item(entry) for entry in entries]
    parts += ["  </channel>", "</rss>", ""]
    return "\n".join(parts)


def generate(
    releases: list[dict[str, Any]],
    items_dir: Path,
    repository: str,
    max_items: int,
    require_tag: str | None = None,
) -> str:
    """Releases plus sidecars in, one appcast out.

    `require_tag` is the tag currently being released. It is the one release
    whose absence from the feed is a bug rather than history: every other
    release may legitimately have no sidecar, but if the run that just built
    and signed a `.dmg` cannot get it into the feed, publishing the feed anyway
    would quietly ship an update nobody is offered.
    """
    entries = []
    for release in releases:
        if release.get("draft"):
            continue
        tag = release["tag_name"]
        item = load_item(items_dir, tag)
        if item is None:
            print(f"note: {tag} has no {ITEM_FILENAME}; not in the feed", file=sys.stderr)
            continue
        entries.append(build_entry(release, item))

    if require_tag is not None and not any(e["tag"] == require_tag for e in entries):
        raise AppcastError(
            f"{require_tag} is the release being published and it produced no "
            f"feed item. Every installed copy would be told this version does "
            f"not exist."
        )

    return render(select(entries, max_items), repository)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--releases", required=True, type=Path,
                        help="JSON body of GET /repos/{owner}/{repo}/releases")
    parser.add_argument("--items-dir", required=True, type=Path,
                        help=f"directory of <tag>/{ITEM_FILENAME} sidecars")
    parser.add_argument("--repository", required=True, help="owner/repo")
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--max-items", type=int, default=10,
                        help="per channel, not over the merged list (default: 10)")
    parser.add_argument("--require-tag", default=None,
                        help="fail unless this tag produced an item")
    args = parser.parse_args(argv)

    try:
        releases = read_releases(args.releases)
    except AppcastError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    try:
        xml = generate(
            releases=releases,
            items_dir=args.items_dir,
            repository=args.repository,
            max_items=args.max_items,
            require_tag=args.require_tag,
        )
    except AppcastError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    args.output.write_text(xml, encoding="utf-8")
    print(f"wrote {args.output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
