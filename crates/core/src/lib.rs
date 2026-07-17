//! Core library for the webnovel crawler: fetching, source adapters, chapter
//! extraction, and EPUB packaging. See DESIGN.md for the architecture and the
//! decisions behind it.

pub mod epub;
pub mod fetch;
pub mod model;
pub mod paths;
pub mod profiles;
pub mod source;
pub mod util;

pub use epub::build_epub;
pub use fetch::{FetchConfig, Fetcher, ReqwestFetcher};
pub use model::{Chapter, ChapterRef, NovelMeta, NovelStatus};
pub use paths::{epub_path, novel_dir};
pub use source::{GenericSource, SiteProfile, Source};
pub use util::sanitize_filename;
