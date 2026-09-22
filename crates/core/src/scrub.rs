use anyhow::Result;

use crate::freewebnovel::strip_promo;
use crate::store::Store;

#[derive(Debug, Default)]
pub struct ScrubReport {
    pub chapters: Vec<u32>,
    pub marks: usize,
}

impl ScrubReport {
    pub fn is_empty(&self) -> bool {
        self.chapters.is_empty()
    }
}

pub fn scrub_novel(store: &Store, novel_id: i64, dry_run: bool) -> Result<ScrubReport> {
    let mut report = ScrubReport::default();

    for chapter in store.load_chapters(novel_id)? {
        let mut cleaned = Vec::with_capacity(chapter.paragraphs.len());
        let mut marks = 0usize;
        for paragraph in &chapter.paragraphs {
            match strip_promo(paragraph) {
                None => marks += 1,
                Some(kept) => {
                    if kept != paragraph.trim() {
                        marks += 1;
                    }
                    cleaned.push(kept);
                }
            }
        }
        if marks == 0 || cleaned.is_empty() {
            continue;
        }
        if !dry_run {
            store.update_chapter_body(novel_id, chapter.number, &cleaned)?;
        }
        report.chapters.push(chapter.number);
        report.marks += marks;
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Chapter, NovelMeta, NovelStatus};

    fn store_with_chapter(paragraphs: &[&str]) -> (Store, i64) {
        let store = Store::open_in_memory().unwrap();
        let meta = NovelMeta {
            title: "Marked Novel".into(),
            author: Some("An Author".into()),
            cover_url: None,
            genre: None,
            status_hint: NovelStatus::Ongoing,
            source_url: "https://freewebnovel.com/novel/marked".into(),
        };
        let id = store.subscribe(&meta, "freewebnovel").unwrap();
        let source = store
            .find_novel(&id.to_string())
            .unwrap()
            .unwrap()
            .primary_source()
            .unwrap()
            .id;
        let chapter = Chapter {
            number: 1,
            title: "One".into(),
            paragraphs: paragraphs.iter().map(|s| s.to_string()).collect(),
        };
        store.insert_chapter_if_absent(id, source, &chapter).unwrap();
        (store, id)
    }

    #[test]
    fn removes_both_forms_and_leaves_the_prose() {
        let (store, id) = store_with_chapter(&[
            "He drew his sword.",
            "Enjoy more content from freewebnovel",
            "The blade sang. Stay updated with freewebnovel",
        ]);

        let report = scrub_novel(&store, id, false).unwrap();
        assert_eq!(report.chapters, vec![1]);
        assert_eq!(report.marks, 2);
        assert_eq!(
            store.load_chapters(id).unwrap()[0].paragraphs,
            vec!["He drew his sword.", "The blade sang."]
        );

        assert!(scrub_novel(&store, id, false).unwrap().is_empty());
    }

    #[test]
    fn a_dry_run_reports_without_writing() {
        let (store, id) = store_with_chapter(&["Prose.", "Read the latest on freewebnovel"]);

        let report = scrub_novel(&store, id, true).unwrap();
        assert_eq!(report.marks, 1);
        assert_eq!(store.load_chapters(id).unwrap()[0].paragraphs.len(), 2);
    }

    #[test]
    fn a_clean_novel_is_untouched() {
        let (store, id) = store_with_chapter(&[
            "The Empire had fallen.",
            "\"Was it just the nine of you against the entire Empire?\"",
        ]);
        assert!(scrub_novel(&store, id, false).unwrap().is_empty());
        assert_eq!(store.load_chapters(id).unwrap()[0].paragraphs.len(), 2);
    }
}
