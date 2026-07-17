//! SQLite persistence: the single source of truth.
//!
//! Schema is multi-source aware from the start (see DESIGN.md): a logical
//! `novels` row has one or more `sources` (primary + fallbacks), and `chapters`
//! are keyed by (novel, number) with a note of which source supplied each.
//!
//! `rusqlite::Connection` is not `Send`, so the CLI runs on a current-thread
//! Tokio runtime and never holds the connection across a `spawn`. All methods
//! here are synchronous.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use directories::ProjectDirs;
use rusqlite::{params, Connection, OptionalExtension};

use crate::model::{Chapter, DerivedState, NovelMeta, NovelStatus};
use crate::util::now_unix;

/// A source feeding a novel: primary (priority 1) or a fallback.
#[derive(Debug, Clone)]
pub struct StoredSource {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub priority: i64,
    pub last_seen_chapter: Option<u32>,
}

/// A subscribed novel with its sources and a downloaded-chapter count.
#[derive(Debug, Clone)]
pub struct StoredNovel {
    pub id: i64,
    pub title: String,
    pub author: Option<String>,
    pub cover_url: Option<String>,
    pub status_hint: NovelStatus,
    pub derived_state: DerivedState,
    pub sources: Vec<StoredSource>,
    pub chapter_count: i64,
}

impl StoredNovel {
    /// The primary (highest-priority) source, if any.
    pub fn primary_source(&self) -> Option<&StoredSource> {
        self.sources.iter().min_by_key(|s| s.priority)
    }

    /// Reconstruct novel metadata for EPUB packaging.
    pub fn to_meta(&self) -> NovelMeta {
        NovelMeta {
            title: self.title.clone(),
            author: self.author.clone(),
            cover_url: self.cover_url.clone(),
            status_hint: self.status_hint.clone(),
            source_url: self
                .primary_source()
                .map(|s| s.url.clone())
                .unwrap_or_default(),
        }
    }
}

pub struct Store {
    conn: Connection,
}

/// Default library DB path: `%LOCALAPPDATA%/webnovel-crawler/data/library.db`.
pub fn default_db_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "webnovel-crawler")
        .ok_or_else(|| anyhow!("could not resolve a local data directory"))?;
    Ok(dirs.data_local_dir().join("library.db"))
}

impl Store {
    /// Open (creating if needed) the DB at `path`, enabling WAL + foreign keys
    /// and applying the schema.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating data dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        // journal_mode returns a row; consume it so it isn't treated as an error.
        conn.query_row("PRAGMA journal_mode=WAL;", [], |_| Ok(()))?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Open the default library DB.
    pub fn open_default() -> Result<Self> {
        Self::open(&default_db_path()?)
    }

    /// An ephemeral in-memory DB (tests, dry runs).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS novels (
                id            INTEGER PRIMARY KEY,
                title         TEXT NOT NULL,
                author        TEXT,
                cover_url     TEXT,
                status_hint   TEXT NOT NULL,
                derived_state TEXT NOT NULL,
                created_at    INTEGER NOT NULL,
                updated_at    INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sources (
                id                INTEGER PRIMARY KEY,
                novel_id          INTEGER NOT NULL REFERENCES novels(id) ON DELETE CASCADE,
                source_name       TEXT NOT NULL,
                url               TEXT NOT NULL UNIQUE,
                priority          INTEGER NOT NULL,
                last_seen_chapter INTEGER,
                last_synced_at    INTEGER
            );

            CREATE TABLE IF NOT EXISTS chapters (
                novel_id   INTEGER NOT NULL REFERENCES novels(id) ON DELETE CASCADE,
                number     INTEGER NOT NULL,
                title      TEXT NOT NULL,
                body       TEXT NOT NULL,
                source_id  INTEGER REFERENCES sources(id),
                fetched_at INTEGER NOT NULL,
                exported   INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (novel_id, number)
            );
            "#,
        )?;
        Ok(())
    }

    /// Create a new subscription: a novel plus its primary source. Errors if the
    /// URL is already subscribed, or if the same novel (title+author) already
    /// exists from another source (that is the future `add-source` path).
    pub fn subscribe(&self, meta: &NovelMeta, source_name: &str) -> Result<i64> {
        if self.source_id_for_url(&meta.source_url)?.is_some() {
            bail!("already subscribed to this source URL");
        }
        if let Some(existing) = self.novel_id_for(&meta.title, meta.author.as_deref())? {
            bail!(
                "already following \"{}\" (novel #{existing}) from another source; \
                 adding alternate sources will land in a later step",
                meta.title
            );
        }

        let now = now_unix();
        self.conn.execute(
            "INSERT INTO novels (title, author, cover_url, status_hint, derived_state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                meta.title,
                meta.author,
                meta.cover_url,
                meta.status_hint.as_str(),
                DerivedState::Backfilling.as_str(),
                now,
            ],
        )?;
        let novel_id = self.conn.last_insert_rowid();

        self.conn.execute(
            "INSERT INTO sources (novel_id, source_name, url, priority, last_seen_chapter, last_synced_at)
             VALUES (?1, ?2, ?3, 1, NULL, NULL)",
            params![novel_id, source_name, meta.source_url],
        )?;
        Ok(novel_id)
    }

    /// Add an alternate (fallback) source to an existing novel, at the next
    /// priority. The caller is responsible for confirming it is the same novel
    /// (cross-site titles differ). Errors if the URL is already in the library.
    pub fn add_source(&self, novel_id: i64, source_name: &str, url: &str) -> Result<i64> {
        if self.source_id_for_url(url)?.is_some() {
            bail!("that source URL is already in the library");
        }
        let next_priority: i64 = self.conn.query_row(
            "SELECT coalesce(max(priority), 0) + 1 FROM sources WHERE novel_id = ?1",
            params![novel_id],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO sources (novel_id, source_name, url, priority, last_seen_chapter, last_synced_at)
             VALUES (?1, ?2, ?3, ?4, NULL, NULL)",
            params![novel_id, source_name, url, next_priority],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    fn source_id_for_url(&self, url: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM sources WHERE url = ?1", params![url], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?)
    }

    fn novel_id_for(&self, title: &str, author: Option<&str>) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM novels
                 WHERE lower(title) = lower(?1) AND ifnull(author,'') = ifnull(?2,'')",
                params![title, author],
                |r| r.get::<_, i64>(0),
            )
            .optional()?)
    }

    /// All subscriptions, ordered by title.
    pub fn list_subscriptions(&self) -> Result<Vec<StoredNovel>> {
        let ids: Vec<i64> = {
            let mut stmt = self.conn.prepare("SELECT id FROM novels ORDER BY lower(title)")?;
            let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        ids.into_iter()
            .map(|id| {
                self.load_novel_by_id(id)?
                    .ok_or_else(|| anyhow!("novel #{id} vanished mid-query"))
            })
            .collect()
    }

    /// Resolve a novel by selector: a numeric id, or a case-insensitive title.
    pub fn find_novel(&self, selector: &str) -> Result<Option<StoredNovel>> {
        let id = if let Ok(n) = selector.parse::<i64>() {
            Some(n)
        } else {
            self.conn
                .query_row(
                    "SELECT id FROM novels WHERE lower(title) = lower(?1)",
                    params![selector],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
        };
        match id {
            Some(id) => self.load_novel_by_id(id),
            None => Ok(None),
        }
    }

    fn load_novel_by_id(&self, id: i64) -> Result<Option<StoredNovel>> {
        let row = self
            .conn
            .query_row(
                "SELECT title, author, cover_url, status_hint, derived_state FROM novels WHERE id = ?1",
                params![id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?;

        let Some((title, author, cover_url, status, state)) = row else {
            return Ok(None);
        };

        let sources = self.sources_for(id)?;
        let chapter_count: i64 =
            self.conn
                .query_row("SELECT count(*) FROM chapters WHERE novel_id = ?1", params![id], |r| {
                    r.get(0)
                })?;

        Ok(Some(StoredNovel {
            id,
            title,
            author,
            cover_url,
            status_hint: NovelStatus::from_str(&status),
            derived_state: DerivedState::from_str(&state),
            sources,
            chapter_count,
        }))
    }

    fn sources_for(&self, novel_id: i64) -> Result<Vec<StoredSource>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source_name, url, priority, last_seen_chapter
             FROM sources WHERE novel_id = ?1 ORDER BY priority",
        )?;
        let rows = stmt.query_map(params![novel_id], |r| {
            Ok(StoredSource {
                id: r.get(0)?,
                name: r.get(1)?,
                url: r.get(2)?,
                priority: r.get(3)?,
                last_seen_chapter: r.get::<_, Option<i64>>(4)?.map(|n| n as u32),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Remove a subscription and all its sources/chapters (cascade).
    pub fn remove_subscription(&self, novel_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM novels WHERE id = ?1", params![novel_id])?;
        Ok(())
    }

    /// Chapter numbers already stored for a novel (for resume / skip).
    pub fn stored_chapter_numbers(&self, novel_id: i64) -> Result<BTreeSet<u32>> {
        let mut stmt = self
            .conn
            .prepare("SELECT number FROM chapters WHERE novel_id = ?1")?;
        let rows = stmt.query_map(params![novel_id], |r| r.get::<_, i64>(0))?;
        let mut set = BTreeSet::new();
        for n in rows {
            set.insert(n? as u32);
        }
        Ok(set)
    }

    /// Insert a chapter if that number isn't already stored. Returns whether it
    /// was newly inserted (false = already present, left untouched).
    pub fn insert_chapter_if_absent(
        &self,
        novel_id: i64,
        source_id: i64,
        chapter: &Chapter,
    ) -> Result<bool> {
        let body = chapter.paragraphs.join("\n\n");
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO chapters (novel_id, number, title, body, source_id, fetched_at, exported)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            params![novel_id, chapter.number, chapter.title, body, source_id, now_unix()],
        )?;
        Ok(changed > 0)
    }

    /// Load all stored chapters for a novel, ascending by number.
    pub fn load_chapters(&self, novel_id: i64) -> Result<Vec<Chapter>> {
        let mut stmt = self.conn.prepare(
            "SELECT number, title, body FROM chapters WHERE novel_id = ?1 ORDER BY number",
        )?;
        let rows = stmt.query_map(params![novel_id], |r| {
            let number: i64 = r.get(0)?;
            let title: String = r.get(1)?;
            let body: String = r.get(2)?;
            Ok(Chapter {
                number: number as u32,
                title,
                paragraphs: body.split("\n\n").map(str::to_string).collect(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Record a source's progress after a sync pass.
    pub fn update_source_progress(&self, source_id: i64, last_seen_chapter: u32) -> Result<()> {
        self.conn.execute(
            "UPDATE sources SET last_seen_chapter = ?2, last_synced_at = ?3 WHERE id = ?1",
            params![source_id, last_seen_chapter as i64, now_unix()],
        )?;
        Ok(())
    }

    /// Mark every stored chapter of a novel as exported (retention bookkeeping).
    pub fn mark_all_exported(&self, novel_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE chapters SET exported = 1 WHERE novel_id = ?1",
            params![novel_id],
        )?;
        Ok(())
    }

    pub fn set_derived_state(&self, novel_id: i64, state: DerivedState) -> Result<()> {
        self.conn.execute(
            "UPDATE novels SET derived_state = ?2, updated_at = ?3 WHERE id = ?1",
            params![novel_id, state.as_str(), now_unix()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NovelStatus;

    fn mem_store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn sample_meta(url: &str) -> NovelMeta {
        NovelMeta {
            title: "Test Novel".into(),
            author: Some("An Author".into()),
            cover_url: None,
            status_hint: NovelStatus::Ongoing,
            source_url: url.into(),
        }
    }

    fn chapter(n: u32) -> Chapter {
        Chapter {
            number: n,
            title: format!("Chapter {n}"),
            paragraphs: vec!["Para one.".into(), "Para two.".into()],
        }
    }

    #[test]
    fn subscribe_creates_novel_and_primary_source() {
        let s = mem_store();
        let id = s.subscribe(&sample_meta("https://novgo.net/a.html"), "novgo").unwrap();
        let novel = s.find_novel(&id.to_string()).unwrap().unwrap();
        assert_eq!(novel.title, "Test Novel");
        assert_eq!(novel.sources.len(), 1);
        assert_eq!(novel.primary_source().unwrap().priority, 1);
        assert_eq!(novel.derived_state, DerivedState::Backfilling);
    }

    #[test]
    fn duplicate_url_is_rejected() {
        let s = mem_store();
        s.subscribe(&sample_meta("https://novgo.net/a.html"), "novgo").unwrap();
        let err = s.subscribe(&sample_meta("https://novgo.net/a.html"), "novgo").unwrap_err();
        assert!(err.to_string().contains("already subscribed"));
    }

    #[test]
    fn chapters_insert_is_idempotent_for_resume() {
        let s = mem_store();
        let id = s.subscribe(&sample_meta("https://novgo.net/a.html"), "novgo").unwrap();
        let src = s.find_novel(&id.to_string()).unwrap().unwrap().primary_source().unwrap().id;

        assert!(s.insert_chapter_if_absent(id, src, &chapter(1)).unwrap());
        // Second insert of the same number is a no-op.
        assert!(!s.insert_chapter_if_absent(id, src, &chapter(1)).unwrap());
        assert!(s.insert_chapter_if_absent(id, src, &chapter(2)).unwrap());

        assert_eq!(s.stored_chapter_numbers(id).unwrap(), [1, 2].into_iter().collect());
        let loaded = s.load_chapters(id).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].paragraphs, vec!["Para one.", "Para two."]);
    }

    #[test]
    fn add_source_appends_at_next_priority() {
        let s = mem_store();
        let id = s.subscribe(&sample_meta("https://novgo.net/a.html"), "novgo").unwrap();
        s.add_source(id, "othersite", "https://other.example/a.html").unwrap();

        let novel = s.find_novel(&id.to_string()).unwrap().unwrap();
        assert_eq!(novel.sources.len(), 2);
        assert_eq!(novel.primary_source().unwrap().priority, 1);
        let fallback = novel.sources.iter().find(|s| s.priority == 2).unwrap();
        assert_eq!(fallback.name, "othersite");

        // Duplicate URL is rejected.
        let err = s
            .add_source(id, "novgo", "https://novgo.net/a.html")
            .unwrap_err();
        assert!(err.to_string().contains("already in the library"));
    }

    #[test]
    fn find_by_title_is_case_insensitive() {
        let s = mem_store();
        s.subscribe(&sample_meta("https://novgo.net/a.html"), "novgo").unwrap();
        assert!(s.find_novel("test novel").unwrap().is_some());
        assert!(s.find_novel("NONEXISTENT").unwrap().is_none());
    }

    #[test]
    fn remove_cascades_chapters() {
        let s = mem_store();
        let id = s.subscribe(&sample_meta("https://novgo.net/a.html"), "novgo").unwrap();
        let src = s.find_novel(&id.to_string()).unwrap().unwrap().primary_source().unwrap().id;
        s.insert_chapter_if_absent(id, src, &chapter(1)).unwrap();
        s.remove_subscription(id).unwrap();
        assert!(s.find_novel(&id.to_string()).unwrap().is_none());
        assert!(s.stored_chapter_numbers(id).unwrap().is_empty());
    }
}
