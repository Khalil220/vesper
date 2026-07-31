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
    pub genre: Option<String>,
    pub status_hint: NovelStatus,
    pub derived_state: DerivedState,
    /// A previous auto-export was blocked (e.g. the EPUB was locked); retry later.
    pub export_pending: bool,
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
            genre: self.genre.clone(),
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

/// Default library DB path: `%LOCALAPPDATA%/vesper/data/library.db`.
pub fn default_db_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "vesper")
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
                id             INTEGER PRIMARY KEY,
                title          TEXT NOT NULL,
                author         TEXT,
                cover_url      TEXT,
                genre          TEXT,
                status_hint    TEXT NOT NULL,
                derived_state  TEXT NOT NULL,
                export_pending INTEGER NOT NULL DEFAULT 0,
                created_at     INTEGER NOT NULL,
                updated_at     INTEGER NOT NULL
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
                novel_id    INTEGER NOT NULL REFERENCES novels(id) ON DELETE CASCADE,
                number      INTEGER NOT NULL,
                title       TEXT NOT NULL,
                body        TEXT NOT NULL,
                source_id   INTEGER REFERENCES sources(id),
                fetched_at  INTEGER NOT NULL,
                exported    INTEGER NOT NULL DEFAULT 0,
                exported_at INTEGER,
                PRIMARY KEY (novel_id, number)
            );

            -- Chapter numbers that discovery expects but no source can provide
            -- (a permanent 404 hole in the site). Tracked so a novel with such
            -- holes can still complete/export, and so the gap is visible.
            CREATE TABLE IF NOT EXISTS chapter_gaps (
                novel_id    INTEGER NOT NULL REFERENCES novels(id) ON DELETE CASCADE,
                number      INTEGER NOT NULL,
                detected_at INTEGER NOT NULL,
                PRIMARY KEY (novel_id, number)
            );
            "#,
        )?;
        // Upgrade older DBs (harmless no-ops if the columns already exist).
        let _ = self
            .conn
            .execute("ALTER TABLE chapters ADD COLUMN exported_at INTEGER", []);
        let _ = self.conn.execute(
            "ALTER TABLE novels ADD COLUMN export_pending INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = self
            .conn
            .execute("ALTER TABLE novels ADD COLUMN genre TEXT", []);
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
                "already following \"{}\" (novel #{existing}); use \
                 `vesper add-source {existing} <url>` to attach another source",
                meta.title
            );
        }

        let now = now_unix();
        self.conn.execute(
            "INSERT INTO novels (title, author, cover_url, genre, status_hint, derived_state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![
                meta.title,
                meta.author,
                meta.cover_url,
                meta.genre,
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

    /// Find a novel whose title matches `title` ignoring case, spacing, and
    /// punctuation (via `util::normalize_title`). Catches the same novel
    /// re-subscribed from a differently-formatted source so we can point the user
    /// at `add-source` instead of forking a duplicate. Normalization is done in
    /// Rust (SQLite can't strip punctuation), so this scans the novels table —
    /// fine for a personal library.
    pub fn find_novel_by_normalized_title(&self, title: &str) -> Result<Option<StoredNovel>> {
        let target = crate::util::normalize_title(title);
        if target.is_empty() {
            return Ok(None);
        }
        let candidates: Vec<(i64, String)> = {
            let mut stmt = self.conn.prepare("SELECT id, title FROM novels")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for (id, t) in candidates {
            if crate::util::normalize_title(&t) == target {
                return self.load_novel_by_id(id);
            }
        }
        Ok(None)
    }

    /// All subscriptions, ordered by id — so the `#N` labels `subs` and `status`
    /// print run in sequence and stay put as titles come and go.
    pub fn list_subscriptions(&self) -> Result<Vec<StoredNovel>> {
        let ids: Vec<i64> = {
            let mut stmt = self.conn.prepare("SELECT id FROM novels ORDER BY id")?;
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
                "SELECT title, author, cover_url, genre, status_hint, derived_state, export_pending
                 FROM novels WHERE id = ?1",
                params![id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, i64>(6)? != 0,
                    ))
                },
            )
            .optional()?;

        let Some((title, author, cover_url, genre, status, state, export_pending)) = row else {
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
            genre,
            status_hint: NovelStatus::from_str(&status),
            derived_state: DerivedState::from_str(&state),
            export_pending,
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

    /// Chapter numbers recorded as permanent gaps (source 404 holes) for a novel.
    pub fn gaps(&self, novel_id: i64) -> Result<BTreeSet<u32>> {
        let mut stmt = self
            .conn
            .prepare("SELECT number FROM chapter_gaps WHERE novel_id = ?1")?;
        let rows = stmt.query_map(params![novel_id], |r| r.get::<_, i64>(0))?;
        let mut set = BTreeSet::new();
        for n in rows {
            set.insert(n? as u32);
        }
        Ok(set)
    }

    /// Recorded gaps whose chapter is *not* stored from any source — i.e. truly
    /// missing chapters (a gap filled from a fallback is excluded). This is what
    /// the UI and EPUB notice should show.
    pub fn unfilled_gaps(&self, novel_id: i64) -> Result<BTreeSet<u32>> {
        let mut stmt = self.conn.prepare(
            "SELECT number FROM chapter_gaps
             WHERE novel_id = ?1
               AND number NOT IN (SELECT number FROM chapters WHERE novel_id = ?1)",
        )?;
        let rows = stmt.query_map(params![novel_id], |r| r.get::<_, i64>(0))?;
        let mut set = BTreeSet::new();
        for n in rows {
            set.insert(n? as u32);
        }
        Ok(set)
    }

    /// Mark a chapter number as a permanent gap (no-op if already recorded).
    pub fn record_gap(&self, novel_id: i64, number: u32) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO chapter_gaps (novel_id, number, detected_at)
             VALUES (?1, ?2, ?3)",
            params![novel_id, number, now_unix()],
        )?;
        Ok(())
    }

    /// Clear a recorded gap (e.g. the chapter became available or was filled).
    pub fn clear_gap(&self, novel_id: i64, number: u32) -> Result<()> {
        self.conn.execute(
            "DELETE FROM chapter_gaps WHERE novel_id = ?1 AND number = ?2",
            params![novel_id, number],
        )?;
        Ok(())
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

    /// Chapter numbers currently sourced from something other than
    /// `primary_source_id` — candidates to upgrade if the primary now has them.
    pub fn chapters_from_other_sources(
        &self,
        novel_id: i64,
        primary_source_id: i64,
    ) -> Result<Vec<u32>> {
        let mut stmt = self.conn.prepare(
            "SELECT number FROM chapters
             WHERE novel_id = ?1 AND (source_id IS NULL OR source_id != ?2)
             ORDER BY number",
        )?;
        let rows = stmt.query_map(params![novel_id, primary_source_id], |r| r.get::<_, i64>(0))?;
        let mut out = Vec::new();
        for n in rows {
            out.push(n? as u32);
        }
        Ok(out)
    }

    /// Replace a stored chapter's content (used when upgrading a fallback-sourced
    /// chapter to the primary's authoritative version). Re-attributes the source
    /// and clears the exported flag so the change propagates to a re-export.
    /// Leaves `fetched_at` intact so it doesn't disturb quiet/completion timing.
    pub fn update_chapter_content(
        &self,
        novel_id: i64,
        source_id: i64,
        chapter: &Chapter,
    ) -> Result<()> {
        let body = chapter.paragraphs.join("\n\n");
        self.conn.execute(
            "UPDATE chapters SET title = ?3, body = ?4, source_id = ?5, exported = 0, exported_at = NULL
             WHERE novel_id = ?1 AND number = ?2",
            params![novel_id, chapter.number, chapter.title, body, source_id],
        )?;
        Ok(())
    }

    /// Overwrite a stored chapter's title, leaving its body and source alone.
    /// Repair hatch for chapters saved while an adapter's title parsing was
    /// wrong — sync inserts are `OR IGNORE`, so a re-sync can't fix them.
    /// Returns whether the title actually changed; clears `exported` when it did
    /// so the next export rebuilds the EPUB.
    pub fn update_chapter_title(&self, novel_id: i64, number: u32, title: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE chapters SET title = ?3, exported = 0, exported_at = NULL
             WHERE novel_id = ?1 AND number = ?2 AND title != ?3",
            params![novel_id, number, title],
        )?;
        Ok(changed > 0)
    }

    /// Record a source's progress after a sync pass.
    pub fn update_source_progress(&self, source_id: i64, last_seen_chapter: u32) -> Result<()> {
        self.conn.execute(
            "UPDATE sources SET last_seen_chapter = ?2, last_synced_at = ?3 WHERE id = ?1",
            params![source_id, last_seen_chapter as i64, now_unix()],
        )?;
        Ok(())
    }

    /// Refresh a novel's mutable metadata (author, cover, genre, status hint) from
    /// a freshly-fetched `NovelMeta`. Leaves the title (the identity) and chapters
    /// untouched.
    pub fn update_novel_meta(&self, novel_id: i64, meta: &NovelMeta) -> Result<()> {
        self.conn.execute(
            "UPDATE novels SET author = ?2, cover_url = ?3, genre = ?4, status_hint = ?5, updated_at = ?6
             WHERE id = ?1",
            params![
                novel_id,
                meta.author,
                meta.cover_url,
                meta.genre,
                meta.status_hint.as_str(),
                now_unix(),
            ],
        )?;
        Ok(())
    }

    /// Mark every stored chapter of a novel as exported, stamping the time (for
    /// the retention grace period).
    pub fn mark_all_exported(&self, novel_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE chapters SET exported = 1, exported_at = ?2 WHERE novel_id = ?1",
            params![novel_id, now_unix()],
        )?;
        Ok(())
    }

    /// The most recent `fetched_at` across a novel's chapters — i.e. when we last
    /// stored a new chapter. Used to gauge how long a novel has been quiet.
    pub fn latest_fetch_time(&self, novel_id: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT max(fetched_at) FROM chapters WHERE novel_id = ?1",
                params![novel_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten())
    }

    /// The most recent `last_synced_at` across a novel's sources.
    pub fn last_synced_at(&self, novel_id: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT max(last_synced_at) FROM sources WHERE novel_id = ?1",
                params![novel_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Re-evaluate a novel's completion: a *Live* novel whose site status is
    /// Completed and which has stored no new chapter for `quiet_grace_days`
    /// becomes *LikelyComplete*. Returns the (possibly unchanged) state.
    ///
    /// The reverse (LikelyComplete -> Live on new activity) is handled by
    /// `sync_novel`, which sets Live whenever it fetches something.
    pub fn reevaluate_completion(
        &self,
        novel_id: i64,
        quiet_grace_days: u32,
    ) -> Result<DerivedState> {
        let (status, state): (String, String) = self.conn.query_row(
            "SELECT status_hint, derived_state FROM novels WHERE id = ?1",
            params![novel_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let status = NovelStatus::from_str(&status);
        let state = DerivedState::from_str(&state);

        if state != DerivedState::Live || status != NovelStatus::Completed {
            return Ok(state);
        }
        let latest = self.latest_fetch_time(novel_id)?.unwrap_or(0);
        let grace = quiet_grace_days as i64 * 86_400;
        if now_unix() - latest >= grace {
            self.set_derived_state(novel_id, DerivedState::LikelyComplete)?;
            Ok(DerivedState::LikelyComplete)
        } else {
            Ok(state)
        }
    }

    /// Purge exported chapters of *LikelyComplete* novels once they are at least
    /// `retention_days` old (0 = as soon as exported). Never touches un-exported
    /// chapters, nor novels in any other state (ongoing novels keep their working
    /// set). Returns how many chapters were deleted.
    pub fn apply_retention(&self, retention_days: u32) -> Result<usize> {
        let cutoff = now_unix() - retention_days as i64 * 86_400;
        let n = self.conn.execute(
            "DELETE FROM chapters
             WHERE exported = 1 AND exported_at IS NOT NULL AND exported_at <= ?1
               AND novel_id IN (SELECT id FROM novels WHERE derived_state = 'likely_complete')",
            params![cutoff],
        )?;
        Ok(n)
    }

    /// Flag (or clear) that a novel has a pending export to retry next pass.
    pub fn set_export_pending(&self, novel_id: i64, pending: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE novels SET export_pending = ?2 WHERE id = ?1",
            params![novel_id, pending as i64],
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
            genre: None,
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

    /// `subs` prints `#<id>` per line, so the listing has to come back in id
    /// order — sorting by title made those numbers jump around.
    #[test]
    fn subscriptions_list_in_id_order() {
        let s = mem_store();
        let mut zed = sample_meta("https://novgo.net/z.html");
        zed.title = "Zebra Chronicles".into();
        let mut abe = sample_meta("https://novgo.net/a.html");
        abe.title = "Abacus Diaries".into();

        let first = s.subscribe(&zed, "novgo").unwrap();
        let second = s.subscribe(&abe, "novgo").unwrap();

        let ids: Vec<i64> = s.list_subscriptions().unwrap().iter().map(|n| n.id).collect();
        assert_eq!(ids, vec![first, second]);
    }

    #[test]
    fn normalized_title_matches_across_formatting() {
        let s = mem_store();
        let id = s.subscribe(&sample_meta("https://novgo.net/a.html"), "novgo").unwrap();
        // "Test Novel" subscribed; a differently-formatted same title matches.
        let found = s.find_novel_by_normalized_title("test-novel!").unwrap();
        assert_eq!(found.map(|n| n.id), Some(id));
        // A genuinely different title does not.
        assert!(s.find_novel_by_normalized_title("Other Story").unwrap().is_none());
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

    /// Sync inserts are `OR IGNORE`, so chapters stored with a bad title need an
    /// explicit overwrite; a corrected title must also un-export the chapter.
    #[test]
    fn retitle_overwrites_and_marks_for_re_export() {
        let s = mem_store();
        let id = s.subscribe(&sample_meta("https://novgo.net/a.html"), "novgo").unwrap();
        let src = s.find_novel(&id.to_string()).unwrap().unwrap().primary_source().unwrap().id;
        s.insert_chapter_if_absent(id, src, &chapter(1)).unwrap();
        s.mark_all_exported(id).unwrap();

        assert!(s.update_chapter_title(id, 1, "The Three Wives").unwrap());
        assert_eq!(s.load_chapters(id).unwrap()[0].title, "The Three Wives");
        // Body survived the retitle.
        assert_eq!(s.load_chapters(id).unwrap()[0].paragraphs, vec!["Para one.", "Para two."]);

        // Same title again is a no-op, so a repeat pass reports nothing changed.
        assert!(!s.update_chapter_title(id, 1, "The Three Wives").unwrap());
        // Unknown chapter number is a no-op, not an error.
        assert!(!s.update_chapter_title(id, 99, "Nope").unwrap());
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

    fn completed_meta(url: &str) -> NovelMeta {
        NovelMeta {
            title: "Done Novel".into(),
            author: Some("A".into()),
            cover_url: None,
            genre: None,
            status_hint: NovelStatus::Completed,
            source_url: url.into(),
        }
    }

    fn primary_source_id(s: &Store, id: i64) -> i64 {
        s.find_novel(&id.to_string())
            .unwrap()
            .unwrap()
            .primary_source()
            .unwrap()
            .id
    }

    #[test]
    fn retention_purges_only_exported_likely_complete_chapters() {
        let s = mem_store();
        let id = s.subscribe(&sample_meta("https://novgo.net/a.html"), "novgo").unwrap();
        let src = primary_source_id(&s, id);
        s.insert_chapter_if_absent(id, src, &chapter(1)).unwrap();
        s.insert_chapter_if_absent(id, src, &chapter(2)).unwrap();

        // Not exported, not LikelyComplete: nothing purged.
        assert_eq!(s.apply_retention(0).unwrap(), 0);

        s.mark_all_exported(id).unwrap();
        // Exported but still Backfilling: nothing purged.
        assert_eq!(s.apply_retention(0).unwrap(), 0);

        s.set_derived_state(id, DerivedState::LikelyComplete).unwrap();
        // Exported + LikelyComplete + grace 0: purged.
        assert_eq!(s.apply_retention(0).unwrap(), 2);
        assert!(s.stored_chapter_numbers(id).unwrap().is_empty());
    }

    #[test]
    fn retention_never_purges_unexported_chapters() {
        let s = mem_store();
        let id = s.subscribe(&sample_meta("https://novgo.net/a.html"), "novgo").unwrap();
        let src = primary_source_id(&s, id);
        s.insert_chapter_if_absent(id, src, &chapter(1)).unwrap();
        s.set_derived_state(id, DerivedState::LikelyComplete).unwrap();
        assert_eq!(s.apply_retention(0).unwrap(), 0);
        assert_eq!(s.stored_chapter_numbers(id).unwrap().len(), 1);
    }

    #[test]
    fn retention_respects_grace_days() {
        let s = mem_store();
        let id = s.subscribe(&sample_meta("https://novgo.net/a.html"), "novgo").unwrap();
        let src = primary_source_id(&s, id);
        s.insert_chapter_if_absent(id, src, &chapter(1)).unwrap();
        s.mark_all_exported(id).unwrap();
        s.set_derived_state(id, DerivedState::LikelyComplete).unwrap();
        // Just exported; a 30-day grace keeps it.
        assert_eq!(s.apply_retention(30).unwrap(), 0);
        assert_eq!(s.stored_chapter_numbers(id).unwrap().len(), 1);
    }

    #[test]
    fn reevaluate_marks_quiet_completed_novel() {
        let s = mem_store();
        let id = s.subscribe(&completed_meta("https://novgo.net/done.html"), "novgo").unwrap();
        s.set_derived_state(id, DerivedState::Live).unwrap();
        assert_eq!(
            s.reevaluate_completion(id, 0).unwrap(),
            DerivedState::LikelyComplete
        );
    }

    #[test]
    fn reevaluate_keeps_ongoing_or_recent_novels_live() {
        let s = mem_store();
        // Ongoing status never becomes LikelyComplete.
        let ongoing = s.subscribe(&sample_meta("https://novgo.net/a.html"), "novgo").unwrap();
        s.set_derived_state(ongoing, DerivedState::Live).unwrap();
        assert_eq!(s.reevaluate_completion(ongoing, 0).unwrap(), DerivedState::Live);

        // Completed but recently active with a huge grace stays Live.
        let done = s.subscribe(&completed_meta("https://novgo.net/done.html"), "novgo").unwrap();
        let src = primary_source_id(&s, done);
        s.insert_chapter_if_absent(done, src, &chapter(1)).unwrap();
        s.set_derived_state(done, DerivedState::Live).unwrap();
        assert_eq!(s.reevaluate_completion(done, 3650).unwrap(), DerivedState::Live);
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
