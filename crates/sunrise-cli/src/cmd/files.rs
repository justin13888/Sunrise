//! Attachments: list, attach, detach, download, cancel.

use sunrise_cli::i18n::strings;
use sunrise_core::{AttachmentFetchState, Command, Core, Query, QueryResult};
use sunrise_id::EntityKind;

use super::{flag_value, id_of, positionals, task_id, CmdResult, Ctx};

/// How long `attachment get` waits before handing the request back to the next
/// session.
///
/// Ten times `sync --once`'s deadline, because they are waiting for different
/// things: that one drains an outbox of small ops, and this one may be moving
/// 100 MB. `Core::fetch_attachment` itself has no deadline — a GUI caller holds
/// a Cancel button and a person — so the CLI supplies its own, and supplies it
/// as a *report* rather than a failure: the request row is durable, so a
/// download that outlasts this is still a download, and the next
/// `sunrise sync --once` finishes it.
const DOWNLOAD_DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);

/// `attachments <task-id>` — one task's attachments and where their bytes are.
pub(super) async fn attachments(core: &Core, rest: &[String]) -> CmdResult {
    let raw = rest.first().ok_or_else(strings::attachments::list_usage)?;
    let task = task_id(raw)?;
    let QueryResult::Attachments(rows) = core.query(Query::TaskAttachments(task)).await? else {
        return Err(strings::general::unexpected_result().into());
    };
    for att in rows {
        println!(
            "{}  {:<32} {:>10}  {}",
            att.id.to_str(),
            att.filename,
            att.size_bytes,
            where_the_bytes_are(core, &att)?
        );
    }
    Ok(())
}

/// The state column of `sunrise attachments`, in the vocabulary the user acts
/// on rather than the vocabulary the schema uses.
///
/// "here" is the only one that needs no further action, and the other three
/// each name the next step: `get` it, wait for it, or `get` it again.
pub(super) fn where_the_bytes_are(
    core: &Core,
    att: &sunrise_domain::Attachment,
) -> CmdResult<String> {
    if core.attachment_is_local(att)? {
        return Ok(strings::attachments::here());
    }
    Ok(match core.attachment_fetch_state(att.id)? {
        AttachmentFetchState::Requested => strings::attachments::downloading(),
        AttachmentFetchState::Partial => strings::attachments::partial(),
        AttachmentFetchState::Idle if att.is_fetchable() => strings::attachments::fetchable(),
        // No `ciphertext_hash`, so this replica cannot name the blob on the
        // relay and no amount of asking will produce it. Said plainly rather
        // than offered as a button that cannot work.
        AttachmentFetchState::Idle => strings::attachments::never_uploaded(),
    })
}

/// `attachment get|cancel|add|remove …`.
pub(super) async fn attachment(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let args = rest.get(1..).unwrap_or_default();
    match rest.first().map(String::as_str) {
        Some("get") => attachment_get(ctx.core, args).await,
        Some("cancel") => attachment_cancel(ctx.core, args),
        Some("add") => attach(ctx, args).await,
        Some("remove") => detach(ctx, args).await,
        _ => Err(strings::attachments::usage().into()),
    }
}

/// A MIME type from a file's extension, for the few a task commonly carries.
/// Anything else is `application/octet-stream`, which every client can still
/// download.
fn mime_of(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        Some("md") => "text/markdown",
        _ => "application/octet-stream",
    }
}

/// `attachment add <task-id> <path> [--name <filename>] [--mime <type>]` —
/// attach a file.
///
/// The bytes are sealed and stored in this vault first, then the metadata op
/// is written ([`sunrise_core::Core::attach_file`]); the upload to the relay
/// is the sync driver's, so this works offline and the next `sync --once`
/// sends it.
async fn attach(ctx: &Ctx<'_>, args: &[String]) -> CmdResult {
    let words = positionals(args, &["--name", "--mime"], &[]);
    let [task, path] = words.as_slice() else {
        return Err(strings::attachments::add_usage().into());
    };
    let task = task_id(task)?;
    let p = std::path::Path::new(path);
    let bytes = std::fs::read(p).map_err(|e| format!("{path}: {e}"))?;
    let name = flag_value(args, "--name").unwrap_or_else(|| {
        p.file_name()
            .map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned())
    });
    let mime = flag_value(args, "--mime").unwrap_or_else(|| mime_of(p).to_owned());
    let att = ctx
        .core
        .attach_file(task, name.clone(), mime, &bytes)
        .await?;
    println!("{}  {name}", att.id.to_str());
    Ok(())
}

/// `attachment remove <attachment-id>` — tombstone the attachment.
async fn detach(ctx: &Ctx<'_>, args: &[String]) -> CmdResult {
    let [raw] = args else {
        return Err(strings::attachments::remove_usage().into());
    };
    let id = id_of(raw, EntityKind::Attachment)?;
    ctx.submit(Command::DetachFile(id), strings::undo::delete(raw))
        .await?;
    println!("removed  {raw}");
    Ok(())
}

/// `attachment get <attachment-id> [path]` — the CLI's Download button.
///
/// The bytes go to `path`, or to stdout when none is given, so
/// `sunrise attachment get att_… > scan.png` works the way `ical export`
/// already does. Written with `write_all` rather than printed: this is the one
/// output in the CLI that is not text.
async fn attachment_get(core: &Core, rest: &[String]) -> CmdResult {
    use std::io::Write as _;
    let raw = rest.first().ok_or_else(strings::attachments::get_usage)?;
    let id = sunrise_id::EntityRef::parse(raw, EntityKind::Attachment)
        .map_err(|e| strings::general::not_an_attachment_id(raw, &e.to_string()))?;

    match tokio::time::timeout(DOWNLOAD_DEADLINE, core.fetch_attachment(id)).await {
        Ok(r) => r?,
        // Not an error. The row is durable and still says what this device
        // wants, so the honest report is that the work outlived the command.
        Err(_) => {
            return Err(strings::attachments::deadline(
                i64::try_from(DOWNLOAD_DEADLINE.as_secs()).unwrap_or(i64::MAX),
            )
            .into())
        }
    }

    let bytes = core.attachment_bytes(id).await?;
    match rest.get(1) {
        Some(path) => {
            std::fs::write(path, &bytes).map_err(|e| format!("{path}: {e}"))?;
            // stdout carries the bytes when there is no path, so the note
            // about them always goes to stderr.
            eprintln!(
                "{}",
                strings::attachments::written(i64::try_from(bytes.len()).unwrap_or(i64::MAX), path)
            );
        }
        None => std::io::stdout().write_all(&bytes)?,
    }
    Ok(())
}

/// `attachment cancel <attachment-id>` — stop a download this vault has
/// outstanding.
///
/// Useful across processes rather than within one: a `get` interrupted at the
/// terminal leaves its request standing, which is deliberate — the next session
/// finishes it — and this is how to say that was not what you wanted.
fn attachment_cancel(core: &Core, rest: &[String]) -> CmdResult {
    let raw = rest
        .first()
        .ok_or_else(strings::attachments::cancel_usage)?;
    let id = sunrise_id::EntityRef::parse(raw, EntityKind::Attachment)
        .map_err(|e| strings::general::not_an_attachment_id(raw, &e.to_string()))?;
    core.cancel_attachment_fetch(id)?;
    // A confirmation, not data: nothing should be piping this.
    eprintln!("{}  {}", id.to_str(), strings::attachments::cancelled());
    Ok(())
}
