//! Tabular export per `docs/08-features/reviews-and-stats.md` §Export:
//! "Stats are computable from the op log; we export them on request as JSON or
//! CSV via the export pipeline."
//!
//! Everything here is a **pure value → text** transform. A [`Table`] is built
//! by the stats/review folds, and its two renderers turn it into either
//! RFC 4180 CSV or JSON. Neither renderer reads a clock, a database, or a
//! locale, so an export is a deterministic function of the numbers it was
//! handed.
//!
//! # Why the escaping is the interesting part
//!
//! A review export carries *user text* — task titles, stream names, note
//! excerpts — into a format with structural delimiters. A title containing a
//! comma, a double quote, a newline, or a lone `\r` is ordinary, and an export
//! that silently shifts a column when it meets one is worse than no export at
//! all. So:
//!
//! * **CSV** quotes any field containing `"`, `,`, `\r`, `\n`, or leading /
//!   trailing whitespace, and doubles embedded quotes ([RFC 4180] §2.6–2.7).
//!   Records are terminated with CRLF, as the RFC specifies.
//! * **JSON** escapes `"` and `\`, uses the short forms for the five
//!   whitespace controls, and `\u00XX` for every other C0 control — the set
//!   [RFC 8259] §7 requires to be escaped.
//!
//! Neither renderer can produce a document whose field count differs from its
//! header; that is what the hostile-content tests in this module pin down.
//!
//! [RFC 4180]: https://www.rfc-editor.org/rfc/rfc4180
//! [RFC 8259]: https://www.rfc-editor.org/rfc/rfc8259

mod datasets;
mod table;
#[cfg(test)]
mod tests;

pub use datasets::{activity_table, focus_table, streaks_table, trends_table, ExportDataset};
pub use table::{Cell, ExportFormat, Table};
