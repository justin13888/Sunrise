//! Terminal image previews (behind the `images` cargo feature).
//!
//! Wraps `ratatui-image` 2.x (the release line pinned to ratatui 0.28): a
//! [`Picker`] chooses the best terminal graphics protocol (Kitty / iTerm2 /
//! Sixel) with a unicode-halfblocks fallback, and [`load_preview`] decodes an
//! image file into a resize-aware render state for the `StatefulImage`
//! widget.

use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use std::path::Path;

/// Render state for one loaded preview image (resize-aware, not `Clone`;
/// owned by the runtime and passed to the render functions by `&mut`).
pub type Preview = Box<dyn StatefulProtocol>;

/// Font size assumed when terminal font-size detection is unavailable.
const FALLBACK_FONT_SIZE: (u16, u16) = (8, 16);

/// Build a [`Picker`] for the current terminal.
///
/// Detects the terminal font size via termios where possible and then
/// guesses the graphics protocol from the environment / terminal probing
/// (`ratatui-image` 2.x has no `from_query_stdio`; `from_termios` +
/// `guess_protocol` is its equivalent). Every failure path degrades to
/// unicode halfblocks at a default font size — this never panics.
///
/// This queries the live terminal, so call it only from the runtime (after
/// entering the alternate screen, before reading events). Tests must build
/// a `Picker` manually (`Picker::new(font_size)` defaults to halfblocks).
#[must_use]
pub fn init_picker() -> Picker {
    #[cfg(unix)]
    let mut picker = Picker::from_termios().unwrap_or_else(|_| Picker::new(FALLBACK_FONT_SIZE));
    #[cfg(not(unix))]
    let mut picker = Picker::new(FALLBACK_FONT_SIZE);
    picker.guess_protocol();
    picker
}

/// Load an image file and prepare it for rendering with the picker's
/// current protocol. Errors (missing file, unsupported/corrupt format) are
/// returned as status-line-ready strings.
pub fn load_preview(picker: &mut Picker, path: &Path) -> Result<Preview, String> {
    let img = image::ImageReader::open(path)
        .map_err(|e| format!("preview: {e}"))?
        .decode()
        .map_err(|e| format!("preview: {e}"))?;
    Ok(picker.new_resize_protocol(img))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_preview_reports_missing_file() {
        let mut picker = Picker::new(FALLBACK_FONT_SIZE);
        match load_preview(&mut picker, Path::new("/nonexistent/nope.png")) {
            Err(err) => assert!(err.starts_with("preview: ")),
            Ok(_) => panic!("expected an error for a missing file"),
        }
    }
}
