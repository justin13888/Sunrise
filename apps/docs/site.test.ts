import { describe, expect, it } from "vitest";
import { REPO, sidebar, siteHref } from "./site";

describe("siteHref", () => {
    it("keeps pages, anchors and absolute URLs as written", () => {
        expect(
            siteHref("./i18n.md", "10-cross-cutting/testing.md"),
        ).toBeUndefined();
        expect(
            siteHref("#plurals", "10-cross-cutting/i18n.md"),
        ).toBeUndefined();
        expect(siteHref("https://example.com", "README.md")).toBeUndefined();
        expect(siteHref("/roadmap", "README.md")).toBeUndefined();
    });

    it("sends a section with a README to its index page", () => {
        expect(siteHref("./11-adr", "README.md")).toBe("/11-adr/");
        expect(siteHref("../11-adr/#index", "10-cross-cutting/i18n.md")).toBe(
            "/11-adr/#index",
        );
        expect(siteHref("../", "11-adr/README.md")).toBe("/");
    });

    it("sends a directory with no page here to GitHub's listing of it", () => {
        expect(siteHref("./00-product", "README.md")).toBe(
            `${REPO}/tree/master/docs/00-product`,
        );
        expect(siteHref("../../crates/sunrise-cli", "07-clients/cli.md")).toBe(
            `${REPO}/tree/master/crates/sunrise-cli`,
        );
    });

    it("sends a file outside docs/ to its source on GitHub", () => {
        expect(
            siteHref(
                "../../crates/sunrise-cli/src/i18n.rs#L1",
                "07-clients/cli.md",
            ),
        ).toBe(`${REPO}/blob/master/crates/sunrise-cli/src/i18n.rs#L1`);
        expect(siteHref("../README.md", "README.md")).toBe(
            `${REPO}/blob/master/README.md`,
        );
    });
});

describe("sidebar", () => {
    it("has one group per docs/ section, linking the sections that have a README", () => {
        const groups = sidebar();
        expect(groups.map((g) => g.text)).toContain("10-cross-cutting");
        const adr = groups.find((g) => g.text === "11-adr");
        expect(adr?.link).toBe("/11-adr/");
        expect(
            adr?.items.some(
                (i) => i.link === "/11-adr/0029-design-token-pipeline",
            ),
        ).toBe(true);
        const cross = groups.find((g) => g.text === "10-cross-cutting");
        expect(cross?.link).toBeUndefined();
        expect(cross?.items).toContainEqual({
            text: "Internationalization",
            link: "/10-cross-cutting/i18n",
        });
    });
});
