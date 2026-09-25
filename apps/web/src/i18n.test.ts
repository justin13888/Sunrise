import { describe, expect, it } from "vitest";
import { applyDocumentLocale, t } from "./i18n";

describe("the web client's locale", () => {
    it("falls back to the source catalog outside a browser", () => {
        expect(t.locale).toBe("en");
        expect(t.web.app.empty()).toBe("Nothing on the list.");
    });

    it("writes lang, dir and the tab title onto <html>", () => {
        const doc = { title: "" };
        const root = { lang: "", dir: "", ownerDocument: doc };
        applyDocumentLocale(root as unknown as HTMLElement);
        expect(root).toMatchObject({ lang: "en", dir: "ltr" });
        expect(doc.title).toBe(t.web.app.title());
    });
});
