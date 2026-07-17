//! Core domain types shared across the crawler.

/// A novel's completion status as reported by a site.
///
/// This is only ever a *hint*: site labels are unreliable (see DESIGN.md).
/// Observed chapter activity — not this field — is the authority on whether a
/// novel is still ongoing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NovelStatus {
    Ongoing,
    Completed,
    Unknown,
}

/// Metadata about a novel, extracted from its landing page.
#[derive(Debug, Clone)]
pub struct NovelMeta {
    pub title: String,
    pub author: Option<String>,
    pub cover_url: Option<String>,
    /// Hint only — never treated as ground truth for completion.
    pub status_hint: NovelStatus,
    pub source_url: String,
}

/// A reference to a chapter discovered from a table of contents: enough to
/// fetch and order it, without its body.
#[derive(Debug, Clone)]
pub struct ChapterRef {
    pub number: u32,
    pub title: String,
    pub url: String,
}

/// A fully fetched chapter: its prose split into paragraphs.
#[derive(Debug, Clone)]
pub struct Chapter {
    pub number: u32,
    pub title: String,
    pub paragraphs: Vec<String>,
}
