//! `$EDITOR` round trip for long-form note bodies.
//!
//! `docs/07-clients/tui.md`: "Long-form note editing: `e` opens the note body
//! in `$EDITOR` (vim/helix/nano), saves on exit." The TUI binds this to `E`,
//! leaving `e` as the inline title edit.
//!
//! The terminal suspend/restore is the binary's job (it owns the `Terminal`);
//! everything decidable without a terminal lives here, behind an injected
//! runner, so the rules that actually matter — *don't* write back an aborted
//! edit, *don't* write back an unchanged one — are unit-tested rather than
//! hand-verified.

use std::path::{Path, PathBuf};

/// Fallback when neither `$EDITOR` nor `$VISUAL` is set. POSIX guarantees it.
pub const FALLBACK_EDITOR: &str = "vi";

/// Resolve the editor invocation from `$EDITOR`, then `$VISUAL`, then `vi`.
///
/// The value may carry arguments (`code -w`, `emacsclient -nw`), so it is split
/// on whitespace rather than treated as a bare program name. Blank or
/// whitespace-only values are treated as unset, because an exported-but-empty
/// `EDITOR` is a common shell accident and "" is not a program.
///
/// Returns `(program, args)`; never empty.
#[must_use]
pub fn resolve_editor(editor: Option<&str>, visual: Option<&str>) -> (String, Vec<String>) {
    let spec = [editor, visual]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or(FALLBACK_EDITOR);
    let mut parts = spec.split_whitespace().map(ToString::to_string);
    let program = parts.next().unwrap_or_else(|| FALLBACK_EDITOR.to_string());
    (program, parts.collect())
}

/// What the editor process did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorExit {
    /// Exited zero: the user saved and quit normally.
    Ok,
    /// Exited non-zero (`:cq`, a crash, a missing binary invoked by the shell).
    Failed,
}

/// Seed a temp file with `original`, hand it to `run`, and report the edited
/// bytes — or `None` when nothing should be written back.
///
/// Nothing is written back when the editor exits non-zero or leaves the file
/// byte-identical: an aborted edit (`:cq`, `:q!`) must not clobber a body, and
/// an unchanged one must not manufacture an op that changes nothing (which
/// would still cost a sync round trip and bump `updated_at`).
///
/// The temp file is removed on every path, including the error ones.
pub fn edit_bytes<F>(
    dir: &Path,
    name: &str,
    original: &[u8],
    run: F,
) -> Result<Option<Vec<u8>>, String>
where
    F: FnOnce(&Path) -> Result<EditorExit, String>,
{
    let path: PathBuf = dir.join(name);
    std::fs::write(&path, original).map_err(|e| format!("{}: {e}", path.display()))?;
    let outcome = match run(&path) {
        Ok(EditorExit::Ok) => match std::fs::read(&path) {
            Ok(edited) if edited == original => Ok(None),
            Ok(edited) => Ok(Some(edited)),
            Err(e) => Err(format!("could not read back {}: {e}", path.display())),
        },
        Ok(EditorExit::Failed) => Err("editor exited non-zero; nothing was saved".into()),
        Err(e) => Err(e),
    };
    std::fs::remove_file(&path).ok();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A private scratch directory for one test.
    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sunrise-editor-test-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn editor_beats_visual_beats_vi() {
        assert_eq!(
            resolve_editor(Some("hx"), Some("vim")),
            ("hx".into(), vec![])
        );
        assert_eq!(resolve_editor(None, Some("vim")), ("vim".into(), vec![]));
        assert_eq!(resolve_editor(None, None), ("vi".into(), vec![]));
        // Exported-but-empty is treated as unset.
        assert_eq!(resolve_editor(Some("  "), None), ("vi".into(), vec![]));
    }

    #[test]
    fn an_editor_with_arguments_is_split() {
        let (program, args) = resolve_editor(Some("code -w --new-window"), None);
        assert_eq!(program, "code");
        assert_eq!(args, vec!["-w".to_string(), "--new-window".to_string()]);
    }

    #[test]
    fn edited_bytes_come_back() {
        let dir = scratch("edited");
        let out = edit_bytes(&dir, "note.md", b"first draft", |p| {
            std::fs::write(p, b"second draft").unwrap();
            Ok(EditorExit::Ok)
        })
        .expect("edit succeeded");
        assert_eq!(out.as_deref(), Some(&b"second draft"[..]));
        // The temp file does not outlive the edit.
        assert!(!dir.join("note.md").exists());
    }

    #[test]
    fn the_editor_sees_the_current_body() {
        let dir = scratch("seed");
        let seen = std::cell::RefCell::new(Vec::new());
        let _ = edit_bytes(&dir, "note.md", b"existing body", |p| {
            *seen.borrow_mut() = std::fs::read(p).unwrap();
            Ok(EditorExit::Ok)
        });
        assert_eq!(seen.into_inner(), b"existing body");
    }

    #[test]
    fn an_unchanged_file_is_not_an_edit() {
        let dir = scratch("unchanged");
        let out = edit_bytes(&dir, "note.md", b"first draft", |_| Ok(EditorExit::Ok))
            .expect("edit succeeded");
        assert_eq!(out, None);
    }

    #[test]
    fn a_failed_editor_writes_nothing_back() {
        let dir = scratch("failed");
        // Even though the file on disk changed, a non-zero exit means the user
        // abandoned the edit.
        let err = edit_bytes(&dir, "note.md", b"first draft", |p| {
            std::fs::write(p, b"scratch work").unwrap();
            Ok(EditorExit::Failed)
        })
        .expect_err("a non-zero exit is an error");
        assert!(err.contains("nothing was saved"), "got {err}");
        assert!(!dir.join("note.md").exists());
    }

    #[test]
    fn a_launch_failure_is_reported_verbatim() {
        let dir = scratch("launch");
        let err = edit_bytes(&dir, "note.md", b"x", |_| {
            Err("could not run the editor: no such file".into())
        })
        .expect_err("a launch failure is an error");
        assert!(err.contains("no such file"), "got {err}");
    }

    #[test]
    fn utf8_round_trips_through_the_opaque_body() {
        // NoteBody is opaque bytes, so plain text has to survive unchanged —
        // including multi-byte characters.
        let dir = scratch("utf8");
        let text = "café — naïve\nsecond line\n";
        let out = edit_bytes(&dir, "note.md", b"", |p| {
            std::fs::write(p, text).unwrap();
            Ok(EditorExit::Ok)
        })
        .expect("edit succeeded")
        .expect("changed");
        assert_eq!(String::from_utf8(out).unwrap(), text);
    }
}
