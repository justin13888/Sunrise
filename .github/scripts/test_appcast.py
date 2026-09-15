#!/usr/bin/env python3
"""The contract of `appcast.py`, as assertions.

Why this file exists
--------------------

The appcast decides what every installed copy of the macOS app downloads and
runs next. Three of its rules are invisible in the output when they are right
and catastrophic when they are wrong:

* a **prerelease goes on the beta channel and a stable release goes on no
  channel at all** — get that backwards and every stable user is handed a
  release candidate, silently, on next launch;
* an item's **`length` must describe the bytes that were signed** — if it does
  not, Sparkle downloads the update and rejects it, and the only symptom is an
  updater that has quietly stopped working;
* the cap is **per channel** — a globally capped feed after ten prereleases in
  a row contains no stable item, and users who opted into nothing are offered
  nothing.

None of the three is visible to `actionlint`, and the only other way to check
them is to publish a release and see. So they are asserted here, against the
pure function the script is built around: files in, XML string out, no network.

Run it with `python3 .github/scripts/test_appcast.py`, or through the
`appcast-gate-test` step in `ci.yml`.
"""

from __future__ import annotations

import importlib.util
import json
import pathlib
import sys
import tempfile
import unittest
from xml.etree import ElementTree

SCRIPT = pathlib.Path(__file__).with_name("appcast.py")

_spec = importlib.util.spec_from_file_location("appcast", SCRIPT)
assert _spec is not None and _spec.loader is not None
appcast = importlib.util.module_from_spec(_spec)
sys.modules["appcast"] = appcast
_spec.loader.exec_module(appcast)

SPARKLE = "{http://www.andymatuschak.org/xml-namespaces/sparkle}"


def release(
    tag: str,
    *,
    prerelease: bool = False,
    published_at: str = "2026-09-14T10:11:12Z",
    dmg: str | None = None,
    size: int = 1234,
    draft: bool = False,
    assets: list[dict] | None = None,
) -> dict:
    """One element of the Releases API body, with only the fields read."""
    if assets is None:
        name = dmg or f"sunrise-{tag.lstrip('v')}-aarch64-apple-darwin.dmg"
        assets = [{
            "name": name,
            "size": size,
            "browser_download_url":
                f"https://github.com/o/r/releases/download/{tag}/{name}",
        }]
    return {
        "tag_name": tag,
        "prerelease": prerelease,
        "draft": draft,
        "published_at": published_at,
        "html_url": f"https://github.com/o/r/releases/tag/{tag}",
        "assets": assets,
    }


def item(
    tag: str,
    *,
    dmg: str | None = None,
    length: int = 1234,
    version: str = "7",
    short: str | None = None,
    signature: str = "c2ln",
    minimum: str | None = "26.0",
) -> dict:
    body = {
        "schema": 1,
        "shortVersionString": short or tag.lstrip("v"),
        "version": version,
        "dmg": dmg or f"sunrise-{tag.lstrip('v')}-aarch64-apple-darwin.dmg",
        "length": length,
        "edSignature": signature,
    }
    if minimum is not None:
        body["minimumSystemVersion"] = minimum
    return body


class AppcastTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.items = pathlib.Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    def write_item(self, tag: str, body: dict | None) -> None:
        if body is None:
            return
        directory = self.items / tag
        directory.mkdir(parents=True, exist_ok=True)
        (directory / appcast.ITEM_FILENAME).write_text(
            json.dumps(body), encoding="utf-8"
        )

    def generate(self, pairs, *, max_items: int = 10, require_tag: str | None = None):
        releases = []
        for rel, body in pairs:
            releases.append(rel)
            self.write_item(rel["tag_name"], body)
        return appcast.generate(
            releases=releases,
            items_dir=self.items,
            repository="justin13888/Sunrise",
            max_items=max_items,
            require_tag=require_tag,
        )

    @staticmethod
    def items_of(xml: str) -> list[ElementTree.Element]:
        return list(ElementTree.fromstring(xml).find("channel").findall("item"))

    # -- channels ---------------------------------------------------------

    def test_a_stable_release_carries_no_channel_element(self):
        # The whole meaning of "stable" in Sparkle: an item with no channel is
        # offered to every installation. Emitting `<sparkle:channel>stable`
        # would offer it to nobody, because nothing opts into that name.
        xml = self.generate([(release("v1.0.0"), item("v1.0.0"))])
        (entry,) = self.items_of(xml)
        self.assertIsNone(entry.find(f"{SPARKLE}channel"))

    def test_a_prerelease_carries_the_beta_channel(self):
        xml = self.generate([
            (release("v1.1.0-rc.1", prerelease=True), item("v1.1.0-rc.1")),
        ])
        (entry,) = self.items_of(xml)
        self.assertEqual(entry.find(f"{SPARKLE}channel").text, "beta")

    def test_prerelease_comes_from_the_release_not_from_the_tag(self):
        # `verify` owns the prerelease decision and the `release` job persists
        # it onto the Release. A tag that looks like a prerelease but is not
        # flagged as one must follow the flag, or this script becomes a second
        # source of truth that can disagree with the first.
        xml = self.generate([
            (release("v1.1.0-rc.1", prerelease=False), item("v1.1.0-rc.1")),
        ])
        (entry,) = self.items_of(xml)
        self.assertIsNone(entry.find(f"{SPARKLE}channel"))

    # -- integrity --------------------------------------------------------

    def test_a_length_that_disagrees_with_the_asset_is_fatal(self):
        with self.assertRaises(appcast.AppcastError) as raised:
            self.generate([
                (release("v1.0.0", size=999), item("v1.0.0", length=1234)),
            ])
        self.assertIn("not the file that was signed", str(raised.exception))

    def test_an_item_naming_an_absent_asset_is_fatal(self):
        with self.assertRaises(appcast.AppcastError) as raised:
            self.generate([
                (release("v1.0.0"), item("v1.0.0", dmg="something-else.dmg")),
            ])
        self.assertIn("404", str(raised.exception))

    def test_an_empty_signature_is_fatal(self):
        with self.assertRaises(appcast.AppcastError) as raised:
            self.generate([(release("v1.0.0"), item("v1.0.0", signature=""))])
        self.assertIn("edSignature", str(raised.exception))

    def test_the_release_being_published_must_reach_the_feed(self):
        with self.assertRaises(appcast.AppcastError) as raised:
            self.generate([(release("v1.0.0"), None)], require_tag="v1.0.0")
        self.assertIn("v1.0.0", str(raised.exception))

    # -- what is skipped rather than fatal --------------------------------

    def test_a_release_with_no_sidecar_is_skipped(self):
        # Releases predating this pipeline have no sidecar and cannot grow
        # one. Refusing to build the feed because of them would mean never
        # building it.
        xml = self.generate([
            (release("v0.9.0", published_at="2026-01-01T00:00:00Z"), None),
            (release("v1.0.0"), item("v1.0.0")),
        ])
        titles = [e.find("title").text for e in self.items_of(xml)]
        self.assertEqual(titles, ["1.0.0"])

    def test_a_draft_release_is_skipped(self):
        xml = self.generate([
            (release("v2.0.0", draft=True), item("v2.0.0")),
            (release("v1.0.0"), item("v1.0.0")),
        ])
        titles = [e.find("title").text for e in self.items_of(xml)]
        self.assertEqual(titles, ["1.0.0"])

    # -- ordering and the cap ---------------------------------------------

    def test_items_are_newest_first(self):
        xml = self.generate([
            (release("v1.0.0", published_at="2026-01-01T00:00:00Z"), item("v1.0.0")),
            (release("v1.2.0", published_at="2026-03-01T00:00:00Z"), item("v1.2.0")),
            (release("v1.1.0", published_at="2026-02-01T00:00:00Z"), item("v1.1.0")),
        ])
        titles = [e.find("title").text for e in self.items_of(xml)]
        self.assertEqual(titles, ["1.2.0", "1.1.0", "1.0.0"])

    def test_the_cap_is_per_channel(self):
        # Two betas newer than the only stable release, with a cap of one. A
        # cap over the merged list would keep one beta and leave the stable
        # channel empty; per channel keeps one of each.
        pairs = [
            (release("v1.0.0", published_at="2026-01-01T00:00:00Z"), item("v1.0.0")),
            (release("v1.1.0-rc.1", prerelease=True,
                     published_at="2026-02-01T00:00:00Z"), item("v1.1.0-rc.1")),
            (release("v1.1.0-rc.2", prerelease=True,
                     published_at="2026-03-01T00:00:00Z"), item("v1.1.0-rc.2")),
        ]
        xml = self.generate(pairs, max_items=1)
        titles = [e.find("title").text for e in self.items_of(xml)]
        self.assertEqual(titles, ["1.1.0-rc.2", "1.0.0"])

    # -- the document ------------------------------------------------------

    def test_the_enclosure_carries_url_length_and_signature(self):
        xml = self.generate([(release("v1.0.0", size=4096),
                              item("v1.0.0", length=4096, signature="AbC+/="))])
        (entry,) = self.items_of(xml)
        enclosure = entry.find("enclosure")
        self.assertEqual(enclosure.get("length"), "4096")
        self.assertEqual(enclosure.get(f"{SPARKLE}edSignature"), "AbC+/=")
        self.assertTrue(enclosure.get("url").endswith(".dmg"))

    def test_version_is_the_bundle_version_and_short_version_is_the_tag(self):
        # Sparkle compares `sparkle:version` against the running copy's
        # CFBundleVersion, which is the workflow run number. Swapping the two
        # would make every comparison meaningless.
        xml = self.generate([(release("v1.0.0"),
                              item("v1.0.0", version="417", short="1.0.0"))])
        (entry,) = self.items_of(xml)
        self.assertEqual(entry.find(f"{SPARKLE}version").text, "417")
        self.assertEqual(entry.find(f"{SPARKLE}shortVersionString").text, "1.0.0")

    def test_the_output_is_well_formed_and_parses_as_rss(self):
        xml = self.generate([
            (release("v1.0.0"), item("v1.0.0")),
            (release("v1.1.0-rc.1", prerelease=True,
                     published_at="2026-10-01T00:00:00Z"), item("v1.1.0-rc.1")),
        ])
        root = ElementTree.fromstring(xml)
        self.assertEqual(root.tag, "rss")
        self.assertEqual(root.get("version"), "2.0")
        self.assertIsNotNone(root.find("channel/title"))

    def test_a_signature_with_xml_metacharacters_is_quoted(self):
        # Base64 has no `<` or `&`, so this is a guard against the day
        # something else lands in that attribute rather than an observed bug:
        # an unquoted attribute would make the whole feed unparseable, which
        # is an updater outage rather than a bad item.
        xml = self.generate([(release("v1.0.0"), item("v1.0.0", signature='a"&<b'))])
        (entry,) = self.items_of(xml)
        self.assertEqual(entry.find("enclosure").get(f"{SPARKLE}edSignature"), 'a"&<b')

    # -- reading the release list ------------------------------------------

    def test_a_json_array_of_releases_is_read(self):
        path = self.items / "releases.json"
        path.write_text(json.dumps([release("v1.0.0")]), encoding="utf-8")
        self.assertEqual(len(appcast.read_releases(path)), 1)

    def test_newline_delimited_objects_are_read(self):
        # What `gh api --paginate --jq '.[]'` emits. A `--paginate` over a
        # paged array endpoint does not reliably produce one array, and a
        # second page of releases silently dropped is a feed that forgets
        # history.
        path = self.items / "releases.ndjson"
        path.write_text(
            "\n".join(json.dumps(release(t)) for t in ("v1.0.0", "v1.1.0")) + "\n",
            encoding="utf-8",
        )
        self.assertEqual(
            [r["tag_name"] for r in appcast.read_releases(path)],
            ["v1.0.0", "v1.1.0"],
        )

    def test_an_empty_release_list_is_not_an_error(self):
        path = self.items / "empty.json"
        path.write_text("", encoding="utf-8")
        self.assertEqual(appcast.read_releases(path), [])

    def test_dates_are_rfc822_in_english_regardless_of_locale(self):
        self.assertEqual(
            appcast.rfc822("2026-09-14T10:11:12Z"),
            "Mon, 14 Sep 2026 10:11:12 +0000",
        )

    def test_an_offset_timestamp_is_normalised_to_utc(self):
        self.assertEqual(
            appcast.rfc822("2026-09-14T12:11:12+02:00"),
            "Mon, 14 Sep 2026 10:11:12 +0000",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
