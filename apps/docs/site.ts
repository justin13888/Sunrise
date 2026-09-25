/**
 * What the docs site derives from the `docs/` tree: where a Markdown link
 * goes, and the sidebar.
 *
 * Its own module, outside `.vitepress/`, so vitest (which does not descend
 * into dot-directories) runs `site.test.ts` against it.
 */

import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { basename, join } from "node:path";
import { fileURLToPath } from "node:url";

/** Where a link this site has no page for resolves: the source on GitHub. */
export const REPO = "https://github.com/justin13888/Sunrise";

/** The repository root on disk, which relative links are resolved against. */
export const ROOT = fileURLToPath(new URL("../../", import.meta.url));

/**
 * The site's href for a relative Markdown link in `docs/<from>`, or
 * `undefined` to keep it as written.
 *
 * - Into `docs/`, to a directory with a `README.md`: that directory's index
 *   page, which VitePress addresses with a trailing slash.
 * - Anywhere else that is a directory — a `docs/` section with no README, or
 *   `crates/…` — GitHub's listing of it, which is what GitHub shows there.
 * - A file outside `docs/`: its source on GitHub.
 * - Everything else (a page, an anchor, an absolute URL): unchanged.
 */
export function siteHref(
    href: string,
    from: string,
    root: string = ROOT,
): string | undefined {
    if (/^[a-z][a-z0-9+.-]*:|^#|^\//i.test(href)) {
        return undefined;
    }
    const target = new URL(href, `file:///docs/${from}`);
    const path = decodeURIComponent(target.pathname)
        .slice(1)
        .replace(/\/$/, "");
    const onDisk = join(root, path);
    const isDir = existsSync(onDisk) && statSync(onDisk).isDirectory();
    const inDocs = path === "docs" || path.startsWith("docs/");
    if (inDocs && isDir && existsSync(join(onDisk, "README.md"))) {
        const page = path === "docs" ? "" : `${path.slice("docs/".length)}/`;
        return `/${page}${target.hash}`;
    }
    if (isDir) {
        return `${REPO}/tree/master/${path}${target.hash}`;
    }
    if (!inDocs) {
        return `${REPO}/blob/master/${path}${target.hash}`;
    }
    return undefined;
}

/** A page's title: its first `# ` heading, else its file name. */
function titleOf(file: string): string {
    const heading = /^# (.+)$/m.exec(readFileSync(file, "utf8"));
    return heading?.[1]?.trim() ?? basename(file, ".md");
}

/** One sidebar entry: a section of `docs/` and its pages. */
export interface SidebarGroup {
    text: string;
    collapsed: boolean;
    link?: string;
    items: { text: string; link: string }[];
}

/**
 * One sidebar group per section of `docs/`, in the numbered order
 * `docs/README.md` says to read them, each listing its pages by title. Read
 * from the tree at build time, so a new page is in the sidebar without an
 * edit here.
 */
export function sidebar(root: string = ROOT): SidebarGroup[] {
    const docs = join(root, "docs");
    return readdirSync(docs, { withFileTypes: true })
        .filter((d) => d.isDirectory())
        .map((d) => d.name)
        .sort()
        .map((dir) => {
            const pages = readdirSync(join(docs, dir))
                .filter((f) => f.endsWith(".md") && f !== "README.md")
                .sort();
            return {
                text: dir,
                collapsed: true,
                ...(existsSync(join(docs, dir, "README.md"))
                    ? { link: `/${dir}/` }
                    : {}),
                items: pages.map((f) => ({
                    text: titleOf(join(docs, dir, f)),
                    link: `/${dir}/${f.replace(/\.md$/, "")}`,
                })),
            };
        });
}
