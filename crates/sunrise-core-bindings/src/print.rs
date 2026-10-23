//! Print pagination, lowered onto UniFFI: [`sunrise_client_core::print`].
//!
//! The records carry a `Core` prefix because the Apple app already declares
//! `PrintDocument`, `PrintSection` and `PrintRow`, which these replace in a
//! later pass.

use sunrise_client_core::print::{PrintDocument, PrintRow, PrintSection, ROWS_PER_PAGE};

/// See [`sunrise_client_core::print::PrintRow`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CorePrintRow {
    /// A stable id.
    pub id: String,
    /// The narrow left column.
    pub leading: String,
    /// The line.
    pub title: String,
    /// The grey second line.
    pub detail: String,
}

/// See [`sunrise_client_core::print::PrintSection`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CorePrintSection {
    /// The heading.
    pub heading: String,
    /// The rest of a section a page break split.
    pub continued: bool,
    /// The rows.
    pub rows: Vec<CorePrintRow>,
}

/// See [`sunrise_client_core::print::PrintDocument`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CorePrintDocument {
    /// The title.
    pub title: String,
    /// The line under it.
    pub subtitle: String,
    /// The sections.
    pub sections: Vec<CorePrintSection>,
}

impl From<CorePrintDocument> for PrintDocument {
    fn from(d: CorePrintDocument) -> Self {
        Self {
            title: d.title,
            subtitle: d.subtitle,
            sections: d
                .sections
                .into_iter()
                .map(|s| PrintSection {
                    heading: s.heading,
                    continued: s.continued,
                    rows: s
                        .rows
                        .into_iter()
                        .map(|r| PrintRow {
                            id: r.id,
                            leading: r.leading,
                            title: r.title,
                            detail: r.detail,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

impl From<PrintDocument> for CorePrintDocument {
    fn from(d: PrintDocument) -> Self {
        Self {
            title: d.title,
            subtitle: d.subtitle,
            sections: d
                .sections
                .into_iter()
                .map(|s| CorePrintSection {
                    heading: s.heading,
                    continued: s.continued,
                    rows: s
                        .rows
                        .into_iter()
                        .map(|r| CorePrintRow {
                            id: r.id,
                            leading: r.leading,
                            title: r.title,
                            detail: r.detail,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

/// How many rows the Apple renderer fits on a sheet.
#[uniffi::export]
#[must_use]
pub fn print_rows_per_page() -> u32 {
    u32::try_from(ROWS_PER_PAGE).unwrap_or(u32::MAX)
}

/// `document` split into sheets of at most `rows_per_page` rows, each with
/// the title; a split section goes on marked `continued`.
#[uniffi::export]
#[must_use]
pub fn print_paginate(document: CorePrintDocument, rows_per_page: u32) -> Vec<CorePrintDocument> {
    PrintDocument::from(document)
        .paginate(rows_per_page as usize)
        .into_iter()
        .map(CorePrintDocument::from)
        .collect()
}

/// The file name a PDF save proposes for a document titled `title`.
#[uniffi::export]
#[must_use]
pub fn print_suggested_filename(title: String) -> String {
    PrintDocument {
        title,
        subtitle: String::new(),
        sections: Vec::new(),
    }
    .suggested_filename()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exports_lower_the_client_layer_unchanged() {
        let row = |i: u32| CorePrintRow {
            id: format!("r{i}"),
            leading: String::new(),
            title: format!("row {i}"),
            detail: String::new(),
        };
        let doc = CorePrintDocument {
            title: "Today".into(),
            subtitle: "Monday".into(),
            sections: vec![CorePrintSection {
                heading: "Overdue".into(),
                continued: false,
                rows: (0..15).map(row).collect(),
            }],
        };
        let pages = print_paginate(doc, 10);
        assert_eq!(pages.len(), 2);
        assert!(pages[1].sections[0].continued);
        assert_eq!(
            print_suggested_filename("Calendar \u{2014} Week".into()),
            "calendar-week.pdf"
        );
        assert_eq!(print_rows_per_page(), 34);
    }
}
