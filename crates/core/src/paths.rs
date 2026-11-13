
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

pub fn relocate_epubs(
    library: &Path,
    from: (Option<&str>, &str),
    to: (Option<&str>, &str),
) -> std::io::Result<Vec<PathBuf>> {
    let mut moved = Vec::new();
    for volume in std::iter::once(None).chain((1..).map(Some)) {
        let old = epub_path(library, from.0, from.1, volume);
        if !old.is_file() {
            if volume.is_some() {
                break;
            }
            continue;
        }
        let new = epub_path(library, to.0, to.1, volume);
        if new == old {
            continue;
        }
        if new.exists() && std::fs::canonicalize(&new)? != std::fs::canonicalize(&old)? {
            continue;
        }
        if let Some(dir) = new.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::rename(&old, &new)?;
        moved.push(new);
    }

    let old_dir = novel_dir(library, from.0, from.1);
    if std::fs::remove_dir(&old_dir).is_ok() {
        if let Some(author_dir) = old_dir.parent() {
            let _ = std::fs::remove_dir(author_dir);
        }
    }
    Ok(moved)
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

    fn scratch_library(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vesper-paths-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn book(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn a_renamed_novel_takes_its_epub_along() {
        let lib = scratch_library("rename");
        let old_title = r"Online Game: I Have A 100\% Drop Rate";
        let new_title = "Online Game: I Have A 100% Drop Rate";
        let old = epub_path(&lib, Some("Champion_Dreamer"), old_title, None);
        book(&old, "the whole book");

        let moved = relocate_epubs(
            &lib,
            (Some("Champion_Dreamer"), old_title),
            (Some("Champion_Dreamer"), new_title),
        )
        .unwrap();

        let new = epub_path(&lib, Some("Champion_Dreamer"), new_title, None);
        assert_eq!(moved, vec![new.clone()]);
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "the whole book");
        assert!(!old.parent().unwrap().exists(), "the emptied old folder is removed");
        assert!(lib.join("Champion_Dreamer").exists(), "the author folder still has a book");
        std::fs::remove_dir_all(&lib).unwrap();
    }

    #[test]
    fn every_volume_moves_to_a_new_author() {
        let lib = scratch_library("volumes");
        for v in 1..=2 {
            book(&epub_path(&lib, Some("Old Name"), "Saga", Some(v)), "volume");
        }

        let moved =
            relocate_epubs(&lib, (Some("Old Name"), "Saga"), (Some("New Name"), "Saga")).unwrap();

        assert_eq!(moved.len(), 2);
        for v in 1..=2 {
            assert!(epub_path(&lib, Some("New Name"), "Saga", Some(v)).is_file());
        }
        assert!(!lib.join("Old Name").exists(), "the empty author folder goes too");
        std::fs::remove_dir_all(&lib).unwrap();
    }

    #[test]
    fn another_book_in_the_way_is_never_overwritten() {
        let lib = scratch_library("collision");
        let old = epub_path(&lib, Some("A"), "Old Title", None);
        let taken = epub_path(&lib, Some("A"), "New Title", None);
        book(&old, "mine");
        book(&taken, "someone else's");

        let moved = relocate_epubs(&lib, (Some("A"), "Old Title"), (Some("A"), "New Title")).unwrap();

        assert!(moved.is_empty());
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "mine");
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "someone else's");
        std::fs::remove_dir_all(&lib).unwrap();
    }

    #[test]
    fn a_novel_never_exported_has_nothing_to_move() {
        let lib = scratch_library("unexported");
        let moved = relocate_epubs(&lib, (Some("A"), "Old"), (Some("A"), "New")).unwrap();
        assert!(moved.is_empty());
        std::fs::remove_dir_all(&lib).unwrap();
    }

    #[test]
    fn components_are_sanitized() {
        let p = epub_path(Path::new("/lib"), Some("A/B"), "Novel: Rise?", None);
        assert!(p.ends_with("A_B/Novel_ Rise_/Novel_ Rise_.epub"));
    }
}
