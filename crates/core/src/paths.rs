
use std::path::{Path, PathBuf};

use crate::util::sanitize_filename;

pub const UNKNOWN_AUTHOR: &str = "Unknown Author";

fn author_component(author: Option<&str>) -> String {
    let name = author
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(UNKNOWN_AUTHOR);
    sanitize_filename(name)
}

pub fn novel_dir(library: &Path, author: Option<&str>, title: &str) -> PathBuf {
    library
        .join(author_component(author))
        .join(sanitize_filename(title))
}

pub fn epub_path(library: &Path, author: Option<&str>, title: &str, volume: Option<u32>) -> PathBuf {
    let title_component = sanitize_filename(title);
    let filename = match volume {
        Some(v) => format!("{title_component} - Vol {v:02}.epub"),
        None => format!("{title_component}.epub"),
    };
    novel_dir(library, author, title).join(filename)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_volume_layout() {
        let p = epub_path(
            Path::new("/lib"),
            Some("MyLittleBrother"),
            "Cultivation Online",
            None,
        );
        assert!(p.ends_with("MyLittleBrother/Cultivation Online/Cultivation Online.epub"));
    }

    #[test]
    fn volume_layout() {
        let p = epub_path(Path::new("/lib"), Some("Author"), "Some Novel", Some(2));
        assert!(p.ends_with("Author/Some Novel/Some Novel - Vol 02.epub"));
    }

    #[test]
    fn missing_author_falls_back() {
        let p = epub_path(Path::new("/lib"), None, "Orphan Novel", None);
        assert!(p.ends_with("Unknown Author/Orphan Novel/Orphan Novel.epub"));
    }

    #[test]
    fn components_are_sanitized() {
        let p = epub_path(Path::new("/lib"), Some("A/B"), "Novel: Rise?", None);
        assert!(p.ends_with("A_B/Novel_ Rise_/Novel_ Rise_.epub"));
    }
}
