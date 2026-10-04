/**
 * Identifier spelling for the three bindings, and the tree every emitter walks.
 *
 * A catalog key is `surface.group.name`; each binding nests the groups and
 * spells the leaf in its own convention. Keeping the tree and the spelling in
 * one file is what makes "the same key has the same shape everywhere" a fact
 * about one function rather than three.
 */

/** `task_count` → `taskCount`. */
export function camel(segment: string): string {
    return segment.replace(/_([a-z0-9])/g, (_, c: string) => c.toUpperCase());
}

/** `task_count` → `TaskCount`. */
export function pascal(segment: string): string {
    const c = camel(segment);
    return c.charAt(0).toUpperCase() + c.slice(1);
}

/** A node of the key tree: a leaf is a full key, a branch is named children. */
export interface Tree {
    readonly leaves: Map<string, string>;
    readonly branches: Map<string, Tree>;
}

/**
 * Nest `keys` under their dotted segments, after dropping the first `strip`
 * segments of each (the surface, for a binding that carries only one).
 */
export function tree(keys: readonly string[], strip: number): Tree {
    const root: Tree = { leaves: new Map(), branches: new Map() };
    for (const key of keys) {
        const segments = key.split(".").slice(strip);
        let node = root;
        for (const segment of segments.slice(0, -1)) {
            let next = node.branches.get(segment);
            if (next === undefined) {
                next = { leaves: new Map(), branches: new Map() };
                node.branches.set(segment, next);
            }
            node = next;
        }
        node.leaves.set(segments.at(-1) ?? "", key);
    }
    return root;
}

/** Swift's reserved words, which need backticks as identifiers. */
const SWIFT_KEYWORDS = new Set(
    "associatedtype class deinit enum extension fileprivate func import init inout internal let open operator private precedencegroup protocol public rethrows static struct subscript typealias var break case catch continue default defer do else fallthrough for guard if in repeat return throw switch where while as false is nil self Self super throws true try Type".split(
        " ",
    ),
);

/** A Swift identifier, backticked when it is a keyword. */
export function swiftIdent(name: string): string {
    return SWIFT_KEYWORDS.has(name) ? `\`${name}\`` : name;
}

/** Rust's strict and reserved keywords, which need `r#` as identifiers. */
const RUST_KEYWORDS = new Set(
    "as break const continue crate else enum extern false fn for if impl in let loop match mod move mut pub ref return self static struct super trait true type unsafe use where while async await dyn abstract become box do final macro override priv typeof unsized virtual yield try gen".split(
        " ",
    ),
);

/** A Rust identifier, raw when it is a keyword. */
export function rustIdent(name: string): string {
    return RUST_KEYWORDS.has(name) ? `r#${name}` : name;
}
