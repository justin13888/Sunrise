/**
 * The web client's messages, for the reader's languages.
 *
 * Negotiated once, at load, from `navigator.languages` against the locales
 * `i18n/*.toml` carries; a language the catalog has not been translated into
 * gets English. Every user-visible string in `src/` comes from here — see
 * `docs/10-cross-cutting/i18n.md` §String catalog.
 */

import { createMessages } from "@sunrise/i18n";

export const t = createMessages(
    typeof navigator === "undefined" ? [] : navigator.languages,
);

/**
 * Put the negotiated locale on `<html>`: `lang` for screen readers and
 * hyphenation, `dir` so a right-to-left locale mirrors the layout — which it
 * can only do because layout here is written in logical properties
 * (`paddingInline`, not `paddingLeft`). The build writes `<title>` in the
 * source locale; this re-titles the tab in the negotiated one.
 */
export function applyDocumentLocale(root: HTMLElement): void {
    root.lang = t.locale;
    root.dir = t.dir;
    root.ownerDocument.title = t.web.app.title();
}
