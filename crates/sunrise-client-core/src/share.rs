//! What a share becomes: shared text, a link and images, as an Inbox task.
//!
//! `docs/07-clients/mobile-ios.md` §Sharing extension. The derivation is the
//! same on any client a share sheet reaches, so it lives here; the filing —
//! creating the task, attaching each image, resuming an interrupted pass — is
//! the client's IO and stays there. Ported from the Apple app's
//! `SharedCapture.swift`.
//!
//! **No capture parser.** A shared paragraph is somebody else's prose, and
//! reading a `#` or a `^` in it as an annotation would file a quoted article
//! under a stream it happened to mention. What is shared is filed as written.
//!
//! Lengths count Unicode scalar values. The Swift original clipped a title by
//! grapheme cluster, so a title of combined emoji clips a few characters
//! earlier here; the attachment name was already counted in scalars there.

use sunrise_domain::note_body::{Inline, NoteBlock};

/// The longest title a shared line becomes. The core allows more; a title is
/// a line in a list, and the whole line is in the note.
pub const TITLE_LIMIT: usize = 200;

/// The largest image the core attaches: `AttachError::TooLarge` past it.
pub const MAX_IMAGE_BYTES: u64 = sunrise_domain::MAX_ATTACHMENT_BYTES;

/// The longest attachment name the core takes, in Unicode scalars.
pub const MAX_IMAGE_NAME_LEN: usize = sunrise_domain::MAX_FILENAME_LEN;

/// The task a share is filed as, before anything touches the vault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareDraft {
    /// The task's title.
    pub title: String,
    /// Its note, in blocks; empty for no note.
    pub body: Vec<NoteBlock>,
}

/// Why the core would refuse an image on every attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageRefusal {
    /// `AttachError::Empty`: no bytes.
    Empty,
    /// `AttachError::TooLarge`: over [`MAX_IMAGE_BYTES`].
    TooLarge,
}

/// The task one share becomes.
///
/// The title is the text's first non-blank line; failing that, the link's
/// host and path; failing that, what was shared (`image_count` images). Every
/// other line of the text is a paragraph of the note, and the link follows
/// them — so nothing shared is lost to the title's length. Last come the
/// `refused` notices, one paragraph each, for the images the core would never
/// take ([`refusal_notice`] words one).
#[must_use]
pub fn share_draft(
    text: Option<&str>,
    url: Option<&str>,
    image_count: usize,
    refused: &[String],
) -> ShareDraft {
    let lines: Vec<&str> = text
        .unwrap_or("")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let url = url.map(str::trim).filter(|u| !u.is_empty());
    let mut body = Vec::new();
    let title = if let Some((first, rest)) = lines.split_first() {
        let title = clipped(first);
        // A clipped first line goes into the note whole.
        if title != *first {
            body.push(paragraph(first));
        }
        body.extend(rest.iter().map(|l| paragraph(l)));
        title
    } else if let Some(url) = url {
        clipped(&link_title(url))
    } else if image_count > 1 {
        format!("Shared images ({image_count})")
    } else {
        "Shared image".to_owned()
    };
    if let Some(url) = url {
        body.push(NoteBlock::Paragraph {
            inline: vec![Inline::Link {
                href: url.to_owned(),
                label: url.to_owned(),
            }],
        });
    }
    body.extend(refused.iter().map(|n| paragraph(n)));
    ShareDraft { title, body }
}

/// The refusal an image of `size` bytes meets, or `None` if the core takes
/// it. The core's own two checks, asked first, so an image it can never take
/// is told apart from an attach that failed for now.
#[must_use]
pub const fn image_refusal(size: u64) -> Option<ImageRefusal> {
    if size == 0 {
        Some(ImageRefusal::Empty)
    } else if size > MAX_IMAGE_BYTES {
        Some(ImageRefusal::TooLarge)
    } else {
        None
    }
}

/// The sentence a refused image leaves in the task's note, which is where the
/// user looks for the image and so where they learn it is not coming.
///
/// English, like the title fallbacks above: it is written into the vault as
/// note content, not shown and thrown away, and every client writes the same.
#[must_use]
pub fn refusal_notice(name: &str, refusal: ImageRefusal) -> String {
    match refusal {
        ImageRefusal::Empty => format!("Not attached: \u{201c}{name}\u{201d} was empty."),
        ImageRefusal::TooLarge => format!(
            "Not attached: \u{201c}{name}\u{201d} is larger than the 100 MB an attachment can be."
        ),
    }
}

/// The name an image is attached under: what the sharing app called it,
/// trimmed, and shortened to [`MAX_IMAGE_NAME_LEN`] with its extension kept.
/// The core refuses a longer name on every attempt, so it is fitted rather
/// than refused.
#[must_use]
pub fn attachment_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return "Shared image".to_owned();
    }
    if trimmed.chars().count() <= MAX_IMAGE_NAME_LEN {
        return trimmed.to_owned();
    }
    let (base, suffix) = match trimmed.rsplit_once('.') {
        // An "extension" this long is not one, and keeping it whole would
        // leave no room for the name. A leading dot names no extension.
        Some((base, ext)) if !base.is_empty() && !ext.is_empty() && ext.chars().count() <= 16 => {
            (base, format!(".{ext}"))
        }
        _ => (trimmed, String::new()),
    };
    let keep = MAX_IMAGE_NAME_LEN - suffix.chars().count();
    let kept: String = base.chars().take(keep).collect();
    format!("{}{suffix}", kept.trim())
}

/// A link, as a title: `example.com/some/article`. A leading `www.` goes; a
/// link with no host is its own text.
#[must_use]
pub fn link_title(link: &str) -> String {
    let Ok(parsed) = url::Url::parse(link) else {
        return link.to_owned();
    };
    let Some(host) = parsed.host_str().filter(|h| !h.is_empty()) else {
        return link.to_owned();
    };
    let bare = host.strip_prefix("www.").unwrap_or(host);
    match parsed.path() {
        "" | "/" => bare.to_owned(),
        path => format!("{bare}{path}"),
    }
}

/// `line`, clipped to [`TITLE_LIMIT`] with an ellipsis when longer.
#[must_use]
pub fn clipped(line: &str) -> String {
    if line.chars().count() <= TITLE_LIMIT {
        return line.to_owned();
    }
    let mut out: String = line.chars().take(TITLE_LIMIT - 1).collect();
    out.push('\u{2026}');
    out
}

fn paragraph(text: &str) -> NoteBlock {
    NoteBlock::Paragraph {
        inline: vec![Inline::text(text)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use sunrise_domain::validation::validate_title;
    use sunrise_domain::MAX_FILENAME_LEN;

    fn link_block(href: &str) -> NoteBlock {
        NoteBlock::Paragraph {
            inline: vec![Inline::Link {
                href: href.into(),
                label: href.into(),
            }],
        }
    }

    #[test]
    fn shared_text_becomes_a_title_and_a_note() {
        let text = "Book the ferry\n\nCheck the times first.\nAnd the price.";
        let draft = share_draft(Some(text), None, 0, &[]);
        assert_eq!(draft.title, "Book the ferry");
        assert_eq!(
            draft.body,
            vec![
                paragraph("Check the times first."),
                paragraph("And the price.")
            ]
        );
    }

    /// A browser shares the page title beside the link.
    #[test]
    fn a_shared_page_is_its_title_with_the_link_in_the_note() {
        let link = "https://www.example.com/articles/1";
        let draft = share_draft(Some("A long read"), Some(link), 0, &[]);
        assert_eq!(draft.title, "A long read");
        assert_eq!(draft.body, vec![link_block(link)]);
    }

    #[test]
    fn a_bare_link_is_titled_by_its_host_and_path() {
        let title = |u: &str| share_draft(None, Some(u), 0, &[]).title;
        assert_eq!(
            title("https://www.example.com/articles/1"),
            "example.com/articles/1"
        );
        assert_eq!(title("https://example.com/"), "example.com");
        assert_eq!(
            link_title("mailto:someone@example.com"),
            "mailto:someone@example.com"
        );
    }

    #[test]
    fn an_image_alone_is_titled_as_one() {
        assert_eq!(share_draft(None, None, 1, &[]).title, "Shared image");
        assert_eq!(share_draft(None, None, 2, &[]).title, "Shared images (2)");
        assert!(share_draft(None, None, 1, &[]).body.is_empty());
    }

    #[test]
    fn shared_text_is_not_parsed_for_tags() {
        let draft = share_draft(Some("Read #general ^tomorrow !1"), None, 0, &[]);
        assert_eq!(draft.title, "Read #general ^tomorrow !1");
    }

    #[test]
    fn a_long_first_line_is_clipped_and_kept_in_the_note() {
        let line = vec!["word"; 80].join(" ");
        let draft = share_draft(Some(&line), None, 0, &[]);
        assert_eq!(draft.title.chars().count(), TITLE_LIMIT);
        assert!(draft.title.ends_with('\u{2026}'));
        assert_eq!(draft.body, vec![paragraph(&line)]);
    }

    #[test]
    fn refusal_notices_close_the_note() {
        let notice = refusal_notice("Scan.jpg", ImageRefusal::Empty);
        assert_eq!(notice, "Not attached: \u{201c}Scan.jpg\u{201d} was empty.");
        let draft = share_draft(
            None,
            Some("https://example.com/r"),
            1,
            std::slice::from_ref(&notice),
        );
        assert_eq!(
            draft.body,
            vec![link_block("https://example.com/r"), paragraph(&notice)]
        );
    }

    #[test]
    fn the_refusals_are_the_cores_own() {
        assert_eq!(image_refusal(0), Some(ImageRefusal::Empty));
        assert_eq!(image_refusal(1), None);
        assert_eq!(image_refusal(100_000_000), None);
        assert_eq!(image_refusal(100_000_001), Some(ImageRefusal::TooLarge));
        assert_eq!(
            image_refusal(100 * 1024 * 1024),
            Some(ImageRefusal::TooLarge)
        );
    }

    #[test]
    fn a_long_image_name_is_shortened_not_refused() {
        let long = format!("{}.png", "a".repeat(300));
        let fitted = attachment_name(&long);
        assert_eq!(fitted.chars().count(), MAX_IMAGE_NAME_LEN);
        assert_eq!(
            std::path::Path::new(&fitted).extension(),
            Some("png".as_ref())
        );
        assert_eq!(attachment_name("  Receipt.png "), "Receipt.png");
        assert_eq!(attachment_name("   "), "Shared image");
        let emoji = "\u{1f305}".repeat(300);
        assert_eq!(attachment_name(&emoji).chars().count(), MAX_IMAGE_NAME_LEN);
        let odd = format!("{}.{}", "b".repeat(300), "x".repeat(20));
        assert_eq!(attachment_name(&odd).chars().count(), MAX_IMAGE_NAME_LEN);
    }

    /// Names a sharing app could hand over: anything at all, and long runs
    /// over the characters the fitting treats specially, multi-byte ones and
    /// whitespace that a cut can leave at the end of what is kept.
    fn image_name() -> impl Strategy<Value = String> {
        prop_oneof![
            any::<String>(),
            proptest::collection::vec(
                proptest::sample::select(vec![
                    "a",
                    "東",
                    "\u{1f305}",
                    " ",
                    "\t",
                    "\u{3000}",
                    "\u{a0}",
                    ".",
                    ".png",
                ]),
                0..600,
            )
            .prop_map(|parts| parts.concat()),
        ]
    }

    proptest! {
        // `Direct`, the shape docs/10-cross-cutting/testing.md section 2 names.
        #![proptest_config(ProptestConfig {
            cases: 2048,
            rng_seed: sunrise_test_seed::proptest_rng_seed(),
            failure_persistence: Some(Box::new(
                proptest::test_runner::FileFailurePersistence::Direct(
                    "proptest-regressions/share.txt",
                ),
            )),
            ..ProptestConfig::default()
        })]

        /// Whatever an image was called, the name it is attached under passes
        /// the check `Attachment::validate` makes of a filename, unchanged, so
        /// the core never refuses a share for its name.
        #[test]
        fn an_attachment_name_always_validates(name in image_name()) {
            let fitted = attachment_name(&name);
            let validated = validate_title(&fitted, "attachment.filename", MAX_FILENAME_LEN);
            prop_assert_eq!(validated, Ok(fitted));
        }
    }
}
