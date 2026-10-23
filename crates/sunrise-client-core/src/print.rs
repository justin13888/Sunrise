//! What a printed page says, with nothing about how it looks.
//!
//! "What does printing Today produce?" is a question a test can ask only if
//! everything decidable is decided before a renderer runs. Pagination counts
//! rows rather than measuring them: a pure function of the document, the
//! same on every machine, at the cost of some whitespace at the foot of a
//! page whose rows all happen to be one line tall. Ported from the Apple
//! app's `PrintDocument.swift` with its pagination tests; the renderer and the
//! words on the page stay the client's.

/// How many rows fit on a sheet at the sizes the Apple renderer draws.
pub const ROWS_PER_PAGE: usize = 34;

/// One printed line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintRow {
    /// A stable id, for the renderer's identity.
    pub id: String,
    /// The narrow left column: a checkbox, a time, a label. Empty prints as
    /// indentation.
    pub leading: String,
    /// The line.
    pub title: String,
    /// The grey second line; empty for a one-line row.
    pub detail: String,
}

/// A run of rows under a heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintSection {
    /// The heading.
    pub heading: String,
    /// Whether this is the rest of a section a page break split, which the
    /// renderer marks ("Overdue (continued)") so page two is not a column of
    /// rows under no heading.
    pub continued: bool,
    /// The rows.
    pub rows: Vec<PrintRow>,
}

/// A document, or one sheet of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintDocument {
    /// The title, repeated on every sheet.
    pub title: String,
    /// The line under it: the date printed, and whatever narrows the view.
    pub subtitle: String,
    /// The sections.
    pub sections: Vec<PrintSection>,
}

impl PrintDocument {
    /// How many rows there are.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.sections.iter().map(|s| s.rows.len()).sum()
    }

    /// Whether there is anything worth printing. Asked before a print panel
    /// is raised: a job that produces one blank page is found out at the
    /// printer.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.row_count() == 0
    }

    /// The file a PDF save proposes, from the title, so two lists cannot
    /// suggest the same one: lowercase letters and digits, every other run of
    /// characters one `-`, none at either end, `sunrise` if nothing is left.
    #[must_use]
    pub fn suggested_filename(&self) -> String {
        let mut stem = String::new();
        for c in self.title.chars().flat_map(char::to_lowercase) {
            if c.is_alphanumeric() {
                stem.push(c);
            } else if !stem.is_empty() && !stem.ends_with('-') {
                stem.push('-');
            }
        }
        let stem = stem.trim_end_matches('-');
        format!("{}.pdf", if stem.is_empty() { "sunrise" } else { stem })
    }

    /// Split into sheets of at most `rows_per_page` rows (at least one), each
    /// carrying the title and the subtitle. A section that runs past a page
    /// boundary goes on under its own heading, marked continued. An empty
    /// document is still one sheet, which says so.
    #[must_use]
    pub fn paginate(&self, rows_per_page: usize) -> Vec<Self> {
        let limit = rows_per_page.max(1);
        let sheet = |sections| Self {
            title: self.title.clone(),
            subtitle: self.subtitle.clone(),
            sections,
        };
        let mut pages = Vec::new();
        let mut current: Vec<PrintSection> = Vec::new();
        let mut used = 0;
        for section in &self.sections {
            let mut remaining = section.rows.as_slice();
            let mut continued = section.continued;
            while !remaining.is_empty() {
                if used >= limit {
                    pages.push(sheet(std::mem::take(&mut current)));
                    used = 0;
                }
                let take = (limit - used).min(remaining.len());
                current.push(PrintSection {
                    heading: section.heading.clone(),
                    continued,
                    rows: remaining[..take].to_vec(),
                });
                used += take;
                remaining = &remaining[take..];
                continued = true;
            }
        }
        if !current.is_empty() || pages.is_empty() {
            pages.push(sheet(current));
        }
        pages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(count: usize, prefix: &str) -> Vec<PrintRow> {
        (0..count)
            .map(|i| PrintRow {
                id: format!("{prefix}{i}"),
                leading: String::new(),
                title: format!("{prefix} {i}"),
                detail: String::new(),
            })
            .collect()
    }

    fn section(heading: &str, rows: Vec<PrintRow>) -> PrintSection {
        PrintSection {
            heading: heading.into(),
            continued: false,
            rows,
        }
    }

    fn document(sections: Vec<PrintSection>) -> PrintDocument {
        PrintDocument {
            title: "Today".into(),
            subtitle: "Monday".into(),
            sections,
        }
    }

    fn titled(title: &str) -> PrintDocument {
        PrintDocument {
            title: title.into(),
            subtitle: String::new(),
            sections: vec![],
        }
    }

    #[test]
    fn a_short_document_is_one_page() {
        let pages = document(vec![section("Now", rows(3, "a"))]).paginate(10);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].row_count(), 3);
    }

    #[test]
    fn every_page_repeats_the_title() {
        let pages = document(vec![section("Now", rows(25, "a"))]).paginate(10);
        assert!(pages
            .iter()
            .all(|p| p.title == "Today" && p.subtitle == "Monday"));
        let counts: Vec<_> = pages.iter().map(PrintDocument::row_count).collect();
        assert_eq!(counts, [10, 10, 5]);
    }

    #[test]
    fn a_split_section_goes_on_under_its_heading_marked_continued() {
        let pages = document(vec![section("Overdue", rows(15, "a"))]).paginate(10);
        assert_eq!(pages.len(), 2);
        let heads = |p: &PrintDocument| {
            p.sections
                .iter()
                .map(|s| (s.heading.clone(), s.continued))
                .collect::<Vec<_>>()
        };
        assert_eq!(heads(&pages[0]), [("Overdue".to_owned(), false)]);
        assert_eq!(heads(&pages[1]), [("Overdue".to_owned(), true)]);
    }

    #[test]
    fn sections_that_fit_share_a_page() {
        let pages = document(vec![
            section("Overdue", rows(3, "a")),
            section("Later", rows(4, "b")),
        ])
        .paginate(10);
        assert_eq!(pages.len(), 1);
        let headings: Vec<_> = pages[0]
            .sections
            .iter()
            .map(|s| s.heading.as_str())
            .collect();
        assert_eq!(headings, ["Overdue", "Later"]);
    }

    #[test]
    fn an_empty_document_still_paginates_to_one_page_and_a_zero_limit_is_one() {
        let pages = document(vec![]).paginate(10);
        assert_eq!(pages.len(), 1);
        assert!(pages[0].is_empty());
        assert_eq!(
            document(vec![section("Now", rows(2, "a"))])
                .paginate(0)
                .len(),
            2
        );
    }

    #[test]
    fn the_suggested_filename_is_derived_from_the_title() {
        assert_eq!(titled("Today").suggested_filename(), "today.pdf");
        assert_eq!(
            titled("Calendar \u{2014} Week").suggested_filename(),
            "calendar-week.pdf"
        );
        assert_eq!(titled("@errands").suggested_filename(), "errands.pdf");
        assert_eq!(titled("\u{2014}").suggested_filename(), "sunrise.pdf");
    }
}
