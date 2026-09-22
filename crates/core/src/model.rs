
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivedState {
    Backfilling,
    Live,
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

#[derive(Debug, Clone)]
pub struct NovelMeta {
    pub title: String,
    pub author: Option<String>,
    pub cover_url: Option<String>,
    pub genre: Option<String>,
    pub status_hint: NovelStatus,
    pub source_url: String,
}

#[derive(Debug, Clone)]
pub struct ChapterRef {
    pub number: u32,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct Chapter {
    pub number: u32,
    pub title: String,
    pub paragraphs: Vec<String>,
}
