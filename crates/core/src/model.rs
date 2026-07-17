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

impl NovelStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            NovelStatus::Ongoing => "ongoing",
            NovelStatus::Completed => "completed",
            NovelStatus::Unknown => "unknown",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "ongoing" => NovelStatus::Ongoing,
            "completed" => NovelStatus::Completed,
            _ => NovelStatus::Unknown,
        }
    }
}

/// A novel's lifecycle state, *derived* from observed activity (not the site
/// label). Drives auto-export and when it is safe to purge chapters. See
/// DESIGN.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivedState {
    /// Initial bulk download in progress.
    Backfilling,
    /// Caught up; receiving new chapters normally.
    Live,
    /// Labelled complete AND observably quiet — polled at reduced cadence.
    LikelyComplete,
}

impl DerivedState {
    pub fn as_str(&self) -> &'static str {
        match self {
            DerivedState::Backfilling => "backfilling",
            DerivedState::Live => "live",
            DerivedState::LikelyComplete => "likely_complete",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "live" => DerivedState::Live,
            "likely_complete" => DerivedState::LikelyComplete,
            _ => DerivedState::Backfilling,
        }
    }
}

/// Metadata about a novel, extracted from its landing page.
#[derive(Debug, Clone)]
pub struct NovelMeta {
    pub title: String,
    pub author: Option<String>,
    pub cover_url: Option<String>,
    /// Comma-separated genres, if the source exposes them.
    pub genre: Option<String>,
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
