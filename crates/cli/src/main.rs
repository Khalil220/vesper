
mod service;

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Result};
use clap::{Parser, Subcommand};
use vesper_core::paths::UNKNOWN_AUTHOR;
use vesper_core::{
    build_epub, build_source, download_cover, epub_path, migrate_lightnovelworld, sync_novel,
    Chapter, Config, Cover, DerivedState, MigrationOutcome, MigrationReport, NovelMeta,
    RefetchReport, Source, Store, StoredNovel, StoredSource, SyncProgress, SyncReport,
};

#[derive(Parser)]
#[command(
    name = "vesper",
    bin_name = "vesper",
    about = "Download web novels and keep them as EPUBs",
    version,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Follow a novel so sync keeps downloading its new chapters.
    #[command(visible_alias = "sub")]
    Subscribe {
        /// The novel's main page on a supported site.
        url: String,
        /// Follow it as a separate novel even if it looks like one you already follow.
        #[arg(long)]
        force: bool,
        /// Milliseconds to wait between requests, instead of request_delay_ms from the config.
        #[arg(long, value_name = "MS")]
        delay_ms: Option<u64>,
    },
    /// Add another site as a fallback source for a novel you follow.
    AddSource {
        /// Novel id (see `vesper subs`) or exact title, in quotes if it has spaces.
        novel: String,
        /// The same novel's main page on the other site.
        url: String,
        /// Milliseconds to wait between requests, instead of request_delay_ms from the config.
        #[arg(long, value_name = "MS")]
        delay_ms: Option<u64>,
    },
    /// List the novels you follow and their sources.
    Subs {
        /// Show only novels with chapters that no source has.
        #[arg(long)]
        gaps: bool,
    },
    /// Update a novel's author, cover, genre and status from its primary source.
    Refresh {
        /// Novel id (see `vesper subs`) or exact title, in quotes if it has spaces, or `all`.
        novel: String,
        /// Also re-read every stored chapter's title, one request per chapter. Only needed
        /// to fix titles that were saved wrong.
        #[arg(long)]
        titles: bool,
        /// Milliseconds to wait between requests, instead of request_delay_ms from the config.
        #[arg(long, value_name = "MS")]
        delay_ms: Option<u64>,
    },
    /// Download chapters you already have again and replace their text.
    Refetch {
        /// Novel id (see `vesper subs`) or exact title, in quotes if it has spaces, or `all`.
        novel: String,
        /// Only these chapters, such as 152, 152-154 or 1,5,10-20.
        #[arg(long, value_name = "LIST")]
        chapters: Option<String>,
        /// Show what would change without saving anything.
        #[arg(long)]
        dry_run: bool,
        /// Milliseconds to wait between requests, instead of request_delay_ms from the config.
        #[arg(long, value_name = "MS")]
        delay_ms: Option<u64>,
    },
    /// Download chapters again that were saved as a site's "log in to read" notice.
    Repair {
        /// Novel id (see `vesper subs`) or exact title, in quotes if it has spaces, or `all`.
        novel: String,
        /// Repair this one chapter, whatever its saved text looks like.
        #[arg(long, value_name = "NUMBER")]
        chapter: Option<u32>,
        /// Show what would change without saving anything.
        #[arg(long)]
        dry_run: bool,
        /// Milliseconds to wait between requests, instead of request_delay_ms from the config.
        #[arg(long, value_name = "MS")]
        delay_ms: Option<u64>,
    },
    /// Make another of a novel's sources its primary one, for example after a site shuts down.
    SetPrimary {
        /// Novel id (see `vesper subs`) or exact title, in quotes if it has spaces.
        novel: String,
        /// The source's URL, or its site name as `vesper subs` shows it.
        source: String,
    },
    /// Stop following a novel and delete its downloaded chapters.
    #[command(visible_alias = "unsub")]
    Unsubscribe {
        /// Novel id (see `vesper subs`) or exact title, in quotes if it has spaces.
        novel: String,
    },
    /// Download new chapters for a novel you follow, or a novel's URL straight to an EPUB.
    Fetch {
        /// Novel id (see `vesper subs`), exact title in quotes if it has spaces, or the URL
        /// of a novel you don't follow, which is saved as an EPUB without joining your library.
        novel: String,
        /// Stop after this many new chapters. 0, the default, means no limit.
        #[arg(long, value_name = "COUNT", default_value_t = 0, hide_default_value = true)]
        limit: usize,
        /// Where to save the EPUB. Only for the URL of a novel you don't follow.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
        /// Milliseconds to wait between requests, instead of request_delay_ms from the config.
        #[arg(long, value_name = "MS")]
        delay_ms: Option<u64>,
    },
    /// Remove site adverts from chapters you already have, without going online.
    Scrub {
        /// Novel id (see `vesper subs`) or exact title, in quotes if it has spaces, or `all`.
        novel: String,
        /// Show what would change without saving anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Build the EPUB for a novel you follow from its downloaded chapters.
    Export {
        /// Novel id (see `vesper subs`) or exact title, in quotes if it has spaces.
        novel: String,
        /// Save one EPUB at this path, instead of the library folder and any volume split.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
    /// Delete the downloaded chapters of finished novels once they are exported, to save space.
    Prune {
        /// Days to keep exported chapters, instead of retention_days from the config.
        #[arg(long, value_name = "DAYS")]
        retention_days: Option<u32>,
    },
    /// Check every novel you follow for new chapters. The background task runs this.
    Sync {
        /// Stop after this many new chapters per novel. 0, the default, means no limit.
        #[arg(long, value_name = "COUNT", default_value_t = 0, hide_default_value = true)]
        limit: usize,
        /// Milliseconds to wait between requests, instead of request_delay_ms from the config.
        #[arg(long, value_name = "MS")]
        delay_ms: Option<u64>,
    },
    /// Install, remove or check the background sync task.
    #[command(disable_help_subcommand = true)]
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Show where the config file is and the settings in it.
    Config,
    /// Show which novels need attention and how the last sync went.
    Status,
    /// List the sites Vesper can read and where to add your own.
    Profiles,
    /// Show how many chapters a novel's page lists, without saving anything.
    List {
        /// The novel's main page on a supported site.
        url: String,
        /// Milliseconds to wait between requests, instead of request_delay_ms from the config.
        #[arg(long, value_name = "MS")]
        delay_ms: Option<u64>,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Run `vesper sync` on a schedule under your user account.
    Install {
        /// Minutes between runs, instead of poll_interval_minutes from the config.
        #[arg(long, value_name = "MINUTES")]
        interval_minutes: Option<u32>,
    },
    /// Remove the background sync task.
    Uninstall,
    /// Show whether the background sync task is installed and when it runs next.
    Status,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load_or_create()?;

    if cli.cmd.touches_library() {
        run_pending_migrations(&config).await;
    }

    match cli.cmd {
        Command::Subscribe { url, force, delay_ms } => {
            subscribe(&config, url, force, delay_ms).await
        }
        Command::AddSource { novel, url, delay_ms } => {
            add_source(&config, novel, url, delay_ms).await
        }
        Command::Subs { gaps } => subs(gaps),
        Command::Refresh { novel, titles, delay_ms } => {
            refresh(&config, novel, titles, delay_ms).await
        }
        Command::Refetch { novel, chapters, dry_run, delay_ms } => {
            refetch(&config, novel, chapters, dry_run, delay_ms).await
        }
        Command::Repair { novel, chapter, dry_run, delay_ms } => {
            repair(&config, novel, chapter, dry_run, delay_ms).await
        }
        Command::SetPrimary { novel, source } => set_primary(novel, source),
        Command::Unsubscribe { novel } => unsubscribe(novel),
        Command::Fetch { novel, limit, out, delay_ms } => {
            fetch(&config, novel, limit, out, delay_ms).await
        }
        Command::Scrub { novel, dry_run } => scrub(&config, novel, dry_run).await,
        Command::Export { novel, out } => export(&config, novel, out).await,
        Command::Prune { retention_days } => prune(&config, retention_days),
        Command::Sync { limit, delay_ms } => sync_all(&config, limit, delay_ms).await,
        Command::Service { action } => match action {
            ServiceAction::Install { interval_minutes } => {
                service_install(&config, interval_minutes)
            }
            ServiceAction::Uninstall => service_uninstall(),
            ServiceAction::Status => service_status(),
        },
        Command::Config => config_show(&config),
        Command::Status => status(&config),
        Command::Profiles => profiles_show(),
        Command::List { url, delay_ms } => list(&config, url, delay_ms).await,
    }
}

impl Command {
    fn touches_library(&self) -> bool {
        !matches!(
            self,
            Command::Config | Command::Profiles | Command::Service { .. } | Command::List { .. }
        )
    }
}

async fn run_pending_migrations(config: &Config) {
    let store = match Store::open_default() {
        Ok(s) => s,
        Err(_) => return,
    };
    let delay = Duration::from_millis(config.request_delay_ms);
    match migrate_lightnovelworld(&store, delay).await {
        Ok(Some(report)) => report_migration(&report),
        Ok(None) => {}
        Err(e) => eprintln!("  (library migration deferred: {e}; it will retry next run)"),
    }
}

fn report_migration(report: &MigrationReport) {
    if report.is_empty() {
        return;
    }
    let moved = report.moved().count();
    if moved > 0 {
        eprintln!(
            "Light Novel World has moved to chikari.moe; \
             updated {moved} subscription{} to the new site.",
            if moved == 1 { "" } else { "s" }
        );
    }
    for outcome in &report.outcomes {
        match outcome {
            MigrationOutcome::Moved { novel_id, title, via_title_search, .. } => {
                let note = if *via_title_search {
                    " (matched by title — its address changed)"
                } else {
                    ""
                };
                eprintln!("  #{novel_id} {title}{note}");
            }
            MigrationOutcome::NotOnChikari { novel_id, title, promoted: Some(source) } => {
                eprintln!(
                    "  #{novel_id} {title}: did not make the move to chikari.moe, so \
                     {source} is now its primary source. Nothing was re-downloaded."
                )
            }
            MigrationOutcome::NotOnChikari { novel_id, title, promoted: None } => eprintln!(
                "  #{novel_id} {title}: did not make the move to chikari.moe, and has no \
                 other source. lightnovelworld.org is shutting down, so this novel will \
                 stop updating; the chapters you already have are safe. Export it while \
                 you can: `vesper export {novel_id}`."
            ),
            MigrationOutcome::Undetermined { novel_id, title, reason } => eprintln!(
                "  #{novel_id} {title}: could not be checked ({reason}); \
                 left as-is and will be retried next run."
            ),
        }
    }
}

fn source_for(url: &str, delay_ms: u64) -> Result<Box<dyn Source>> {
    build_source(url, Duration::from_millis(delay_ms))
        .ok_or_else(|| anyhow!("no known source handles this URL: {url}"))
}

fn build_sources(novel: &StoredNovel, delay_ms: u64) -> Result<Vec<(StoredSource, Box<dyn Source>)>> {
    let mut sources = Vec::new();
    for s in &novel.sources {
        match build_source(&s.url, Duration::from_millis(delay_ms)) {
            Some(src) => sources.push((s.clone(), src)),
            None => eprintln!("  (skipping source {} — no adapter for its host)", s.url),
        }
    }
    Ok(sources)
}

async fn export_novel(store: &Store, novel: &StoredNovel, config: &Config) -> Result<Vec<PathBuf>> {
    let chapters = store.load_chapters(novel.id)?;
    if chapters.is_empty() {
        return Ok(Vec::new());
    }
    let meta = novel.to_meta();
    let cover = match &meta.cover_url {
        Some(url) => download_cover(url).await,
        None => None,
    };
    let gaps: Vec<u32> = store.unfilled_gaps(novel.id)?.into_iter().collect();
    let paths = write_epubs(config, &meta, &chapters, cover.as_ref(), &gaps, None)?;
    store.mark_all_exported(novel.id)?;
    Ok(paths)
}

fn write_epubs(
    config: &Config,
    meta: &NovelMeta,
    chapters: &[Chapter],
    cover: Option<&Cover>,
    gaps: &[u32],
    out: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let existing = match out {
        Some(path) => vec![path.to_path_buf()],
        None => existing_epubs(config, meta),
    };
    let dropped = vesper_core::epub::chapters_dropped(&existing, chapters);
    ensure!(
        dropped.is_empty(),
        "not replacing {}: it has {} that this copy would leave out (ch. {}). For a novel in \
         your library, prune most likely deleted them after export, and `vesper fetch` on it \
         downloads them again. Otherwise, save this copy somewhere else with --out",
        existing.first().map(|p| p.display().to_string()).unwrap_or_default(),
        count(dropped.len(), "chapter"),
        describe_numbers(&dropped)
    );

    if let Some(path) = out {
        build_epub(meta, chapters, path, cover, gaps)?;
        return Ok(vec![path.to_path_buf()]);
    }
    if config.split_every_chapters == 0 {
        let path = epub_path(&config.output_dir, meta.author.as_deref(), &meta.title, None);
        build_epub(meta, chapters, &path, cover, gaps)?;
        return Ok(vec![path]);
    }

    let size = config.split_every_chapters as usize;
    let mut paths = Vec::new();
    for (i, chunk) in chapters.chunks(size).enumerate() {
        let path = epub_path(
            &config.output_dir,
            meta.author.as_deref(),
            &meta.title,
            Some((i + 1) as u32),
        );
        let vol_gaps: Vec<u32> = match (chunk.first(), chunk.last()) {
            (Some(f), Some(l)) => {
                gaps.iter().copied().filter(|g| *g >= f.number && *g <= l.number).collect()
            }
            _ => Vec::new(),
        };
        build_epub(meta, chunk, &path, cover, &vol_gaps)?;
        paths.push(path);
    }
    Ok(paths)
}

fn existing_epubs(config: &Config, meta: &NovelMeta) -> Vec<PathBuf> {
    let author = meta.author.as_deref();
    let single = epub_path(&config.output_dir, author, &meta.title, None);
    let volumes = (1..)
        .map(|v| epub_path(&config.output_dir, author, &meta.title, Some(v)))
        .take_while(|p| p.is_file());
    std::iter::once(single).filter(|p| p.is_file()).chain(volumes).collect()
}

async fn export_and_track(store: &Store, config: &Config, novel: &StoredNovel) -> Vec<PathBuf> {
    match export_novel(store, novel, config).await {
        Ok(paths) => {
            if !paths.is_empty() {
                let _ = store.set_export_pending(novel.id, false);
            }
            paths
        }
        Err(e) => {
            let _ = store.set_export_pending(novel.id, true);
            eprintln!("  ! EPUB not written, will retry next sync: {e}");
            Vec::new()
        }
    }
}

async fn post_sync(
    store: &Store,
    config: &Config,
    novel_id: i64,
    prev_state: DerivedState,
    report: &SyncReport,
) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    let final_state = store.reevaluate_completion(novel_id, config.quiet_grace_days)?;
    if final_state != prev_state {
        notes.push(format!("now {}, was {}", state_label(final_state), state_label(prev_state)));
    }

    let just_caught_up =
        prev_state == DerivedState::Backfilling && report.new_state == DerivedState::Live;
    let content_changed = report.newly_fetched > 0 || report.upgraded > 0;
    let gained_new =
        content_changed && matches!(prev_state, DerivedState::Live | DerivedState::LikelyComplete);

    let novel = store
        .find_novel(&novel_id.to_string())?
        .ok_or_else(|| anyhow!("novel #{novel_id} vanished"))?;

    let should_export = (config.auto_export && just_caught_up)
        || (config.auto_append && gained_new)
        || novel.export_pending;

    if should_export && !export_and_track(store, config, &novel).await.is_empty() {
        notes.push("EPUB updated".to_string());
    }
    Ok(notes)
}

fn log_line(config: &Config, msg: &str) {
    if let Some(parent) = config.log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&config.log_path) {
        let _ = writeln!(f, "[{}] {msg}", local_timestamp());
    }
}

fn local_timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn describe_numbers(numbers: &BTreeSet<u32>) -> String {
    const CAP: usize = 15;
    let mut list = numbers.iter().take(CAP).map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
    if numbers.len() > CAP {
        list.push_str(&format!(" and {} more", numbers.len() - CAP));
    }
    list
}

fn describe_gaps(gaps: &BTreeSet<u32>) -> String {
    format!("no source has ch. {}", describe_numbers(gaps))
}

fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

fn state_label(state: DerivedState) -> &'static str {
    match state {
        DerivedState::Backfilling => "backfilling",
        DerivedState::Live => "live",
        DerivedState::LikelyComplete => "likely complete",
    }
}

fn chapter_label(number: u32, title: &str) -> String {
    if title.is_empty() || title == format!("Chapter {number}") {
        format!("ch.{number}")
    } else {
        format!("ch.{number} {title}")
    }
}

struct ProgressBar {
    enabled: bool,
    last_len: usize,
}

impl ProgressBar {
    fn new() -> Self {
        let forced = std::env::var_os("VESPER_FORCE_PROGRESS").is_some();
        Self {
            enabled: forced || std::io::stderr().is_terminal(),
            last_len: 0,
        }
    }

    fn sync(&mut self, title: &str, p: SyncProgress) {
        match p {
            SyncProgress::Fetching { done, total } => {
                self.update(&format!("{title}: downloading"), done, total)
            }
            SyncProgress::Upgrading { done, total } => {
                self.update(&format!("{title}: re-downloading from the primary source"), done, total)
            }
        }
    }

    fn update(&mut self, label: &str, done: usize, total: usize) {
        if !self.enabled {
            return;
        }
        let msg = format!("  {label} {done}/{total}...");
        let pad = " ".repeat(self.last_len.saturating_sub(msg.len()));
        let mut err = std::io::stderr();
        let _ = write!(err, "\r{msg}{pad}");
        let _ = err.flush();
        self.last_len = msg.len();
    }

    fn finish(&mut self) {
        if self.enabled && self.last_len > 0 {
            let mut err = std::io::stderr();
            let _ = write!(err, "\r{}\r", " ".repeat(self.last_len));
            let _ = err.flush();
        }
        self.last_len = 0;
    }
}

fn acquire_sync_lock() -> Result<Option<File>> {
    let lock_path = vesper_core::default_db_path()?
        .parent()
        .map(|p| p.join("sync.lock"))
        .ok_or_else(|| anyhow!("cannot resolve lock path"))?;
    try_lock_file(&lock_path)
}

fn try_lock_file(path: &Path) -> Result<Option<File>> {
    use fs2::FileExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let file = OpenOptions::new().write(true).create(true).open(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(e) if lock_would_block(&e) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn lock_would_block(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::WouldBlock || (cfg!(windows) && e.raw_os_error() == Some(33))
}

async fn subscribe(config: &Config, url: String, force: bool, delay_ms: Option<u64>) -> Result<()> {
    let source = source_for(&url, delay_ms.unwrap_or(config.request_delay_ms))?;
    eprintln!("Fetching novel metadata...");
    let meta = source.fetch_novel(&url).await?;

    let store = Store::open_default()?;

    if !force {
        if let Some(existing) = store.find_novel_by_normalized_title(&meta.title)? {
            let from = existing
                .primary_source()
                .map(|s| format!(" (from {})", s.name))
                .unwrap_or_default();
            bail!(
                "\"{}\" looks like #{} \"{}\"{from}, which you already follow.\n\
                 To add this URL as a fallback source: vesper add-source {} {url}\n\
                 To follow it as a separate novel anyway, run the same command with --force.",
                meta.title,
                existing.id,
                existing.title,
                existing.id,
            );
        }
    }

    let id = store.subscribe(&meta, source.name())?;

    println!("Subscribed to \"{}\" (novel #{id}).", meta.title);
    if let Some(author) = &meta.author {
        println!("  Author: {author}");
    }
    println!("  Source: {} ({url})", source.name());
    println!("Next: `vesper fetch {id}` to download chapters.");
    Ok(())
}

async fn add_source(config: &Config, novel: String, url: String, delay_ms: Option<u64>) -> Result<()> {
    let store = Store::open_default()?;
    let found = store
        .find_novel(&novel)?
        .ok_or_else(|| anyhow!("no subscription matches \"{novel}\""))?;
    let source = source_for(&url, delay_ms.unwrap_or(config.request_delay_ms))?;

    eprintln!("Checking new source...");
    if let Ok(meta) = source.fetch_novel(&url).await {
        use vesper_core::util::normalize_title;
        if normalize_title(&meta.title) != normalize_title(&found.title) {
            eprintln!(
                "  warning: this source's title is \"{}\", but the novel is \"{}\". \
                 Proceeding since you asked to link them.",
                meta.title, found.title
            );
        }
    }

    store.add_source(found.id, source.name(), &url)?;
    println!("Added {} as a fallback source for \"{}\".", source.name(), found.title);

    if !store.unfilled_gaps(found.id)?.is_empty() {
        store.set_derived_state(found.id, DerivedState::Backfilling)?;
        println!("  The next sync will try it for the chapters no source had.");
    }
    Ok(())
}

async fn refresh_one(
    store: &Store,
    config: &Config,
    novel: &StoredNovel,
    delay_ms: u64,
) -> Result<(StoredNovel, Vec<String>)> {
    let primary = novel
        .primary_source()
        .ok_or_else(|| anyhow!("\"{}\" has no source to refresh from", novel.title))?;
    let source = source_for(&primary.url, delay_ms)?;
    let meta = source.fetch_novel(&primary.url).await?;
    store.update_novel_meta(novel.id, &meta)?;
    let current = store
        .find_novel(&novel.id.to_string())?
        .ok_or_else(|| anyhow!("novel #{} vanished", novel.id))?;

    let mut changes = Vec::new();
    if meta.title != novel.title {
        changes.push(format!("renamed to \"{}\"", meta.title));
    }
    let old_author = novel.author.as_deref().unwrap_or(UNKNOWN_AUTHOR);
    let new_author = meta.author.as_deref().unwrap_or(UNKNOWN_AUTHOR);
    if old_author != new_author {
        changes.push(format!("author changed from {old_author} to {new_author}"));
    }
    if changes.is_empty() {
        return Ok((current, changes));
    }

    let moved = vesper_core::paths::relocate_epubs(
        &config.output_dir,
        (novel.author.as_deref(), &novel.title),
        (meta.author.as_deref(), &meta.title),
    )?;
    let exported = !moved.is_empty()
        || epub_path(&config.output_dir, meta.author.as_deref(), &meta.title, None).is_file()
        || epub_path(&config.output_dir, meta.author.as_deref(), &meta.title, Some(1)).is_file();
    let rebuilt = exported
        && current.derived_state != DerivedState::LikelyComplete
        && !export_and_track(store, config, &current).await.is_empty();
    match (moved.is_empty(), rebuilt) {
        (false, true) => changes.push("EPUB moved and rebuilt under the new name".into()),
        (false, false) => changes.push(
            "EPUB moved to the new folder; the title inside it updates on its next export".into(),
        ),
        (true, true) => changes.push("EPUB rebuilt".into()),
        (true, false) => {}
    }
    Ok((current, changes))
}

async fn retitle_one(store: &Store, novel: &StoredNovel, delay_ms: u64) -> Result<usize> {
    let primary = novel
        .primary_source()
        .ok_or_else(|| anyhow!("\"{}\" has no source to re-read titles from", novel.title))?;
    let source = source_for(&primary.url, delay_ms)?;

    let have = store.stored_chapter_numbers(novel.id)?;
    if have.is_empty() {
        return Ok(0);
    }
    let refs = source.discover_chapters(&primary.url, None).await?;
    let mut progress = ProgressBar::new();
    let total = have.len();
    let mut done = 0usize;
    let mut fixed = 0usize;

    for r in refs.iter().filter(|r| have.contains(&r.number)) {
        done += 1;
        progress.update(&format!("{}: reading titles", novel.title), done, total);
        match source.fetch_chapter(r).await {
            Ok(ch) => {
                if store.update_chapter_title(novel.id, ch.number, &ch.title)? {
                    fixed += 1;
                }
            }
            Err(e) => {
                progress.finish();
                eprintln!("  ! ch.{}: {e}", r.number);
            }
        }
    }
    progress.finish();
    Ok(fixed)
}

async fn refresh(
    config: &Config,
    novel: String,
    titles: bool,
    delay_ms: Option<u64>,
) -> Result<()> {
    let store = Store::open_default()?;
    let delay = delay_ms.unwrap_or(config.request_delay_ms);

    if novel.eq_ignore_ascii_case("all") {
        let novels = store.list_subscriptions()?;
        if novels.is_empty() {
            println!("You don't follow any novels yet.");
            return Ok(());
        }
        eprintln!("Refreshing {}...", count(novels.len(), "novel"));
        let mut retitled = 0usize;
        let mut failed = 0usize;
        for n in &novels {
            let (current, mut notes) = match refresh_one(&store, config, n, delay).await {
                Ok(refreshed) => refreshed,
                Err(e) => {
                    eprintln!("  ! {}: {e}", n.title);
                    failed += 1;
                    (n.clone(), Vec::new())
                }
            };
            if titles {
                match retitle_one(&store, &current, delay).await {
                    Ok(0) => {}
                    Ok(fixed) => {
                        notes.push(format!("{} corrected", count(fixed, "chapter title")));
                        retitled += fixed;
                        if config.auto_append
                            && !export_and_track(&store, config, &current).await.is_empty()
                        {
                            notes.push("EPUB updated".to_string());
                        }
                    }
                    Err(e) => eprintln!("  ! {}: titles not checked, {e}", n.title),
                }
            }
            if !notes.is_empty() {
                println!("  {}: {}", n.title, notes.join("; "));
            }
        }
        println!("Updated {}.", count(novels.len() - failed, "novel"));
        if retitled > 0 && !config.auto_append {
            println!("Run `vesper export <id>` for each novel with corrected titles to update its EPUB.");
        }
        return Ok(());
    }

    let found = store
        .find_novel(&novel)?
        .ok_or_else(|| anyhow!("no subscription matches \"{novel}\""))?;
    eprintln!("Refreshing \"{}\"...", found.title);
    let (current, changes) = refresh_one(&store, config, &found, delay).await?;
    if changes.is_empty() {
        println!("Updated \"{}\". The title and author are unchanged.", found.title);
    } else {
        println!("Updated \"{}\": {}.", found.title, changes.join("; "));
    }
    if titles {
        eprintln!("Re-reading chapter titles, one request each...");
        match retitle_one(&store, &current, delay).await? {
            0 => println!("Chapter titles were already correct."),
            fixed => {
                let updated = config.auto_append
                    && !export_and_track(&store, config, &current).await.is_empty();
                println!(
                    "Corrected {}{}.",
                    count(fixed, "chapter title"),
                    if updated { "; EPUB updated" } else { "" }
                );
                if !config.auto_append {
                    println!("Run `vesper export {}` to update the EPUB.", current.id);
                }
            }
        }
    }
    Ok(())
}

fn subs(gaps_only: bool) -> Result<()> {
    let store = Store::open_default()?;
    let novels = store.list_subscriptions()?;
    if novels.is_empty() {
        println!("You don't follow any novels yet. Add one with `vesper sub <url>`.");
        return Ok(());
    }
    let mut shown = 0usize;
    for n in novels {
        let gaps = store.unfilled_gaps(n.id)?;
        if gaps_only && gaps.is_empty() {
            continue;
        }
        shown += 1;
        let author = n.author.as_deref().unwrap_or("Unknown Author");
        let pending = if n.export_pending { ", EPUB not written yet" } else { "" };
        let pruned = store.pruned_chapters(n.id)?.len();
        let held = if pruned > 0 {
            format!(
                "{} stored and {pruned} pruned after export",
                count(n.chapter_count as usize, "chapter")
            )
        } else {
            count(n.chapter_count as usize, "chapter")
        };
        println!(
            "#{} {} by {}: {held}, {}{pending}",
            n.id,
            n.title,
            author,
            state_label(n.derived_state)
        );
        if !gaps.is_empty() {
            println!("    {}", describe_gaps(&gaps));
        }
        for s in &n.sources {
            let seen = s
                .last_seen_chapter
                .map(|c| format!("last seen ch.{c}"))
                .unwrap_or_else(|| "not synced yet".into());
            let role = if s.priority == 1 { "primary" } else { "fallback" };
            println!("    {role}: {}, {}, {seen}", s.name, s.url);
        }
    }
    if gaps_only && shown == 0 {
        println!("No novel is missing chapters.");
    }
    Ok(())
}

async fn refetch(
    config: &Config,
    novel: String,
    chapters: Option<String>,
    dry_run: bool,
    delay_ms: Option<u64>,
) -> Result<()> {
    let store = Store::open_default()?;
    let delay = delay_ms.unwrap_or(config.request_delay_ms);
    let every = novel.eq_ignore_ascii_case("all");

    let targets = match chapters.as_deref() {
        Some(spec) => {
            ensure!(!every, "--chapters needs a single novel, not `all`");
            Some(vesper_core::util::parse_chapter_spec(spec).map_err(|e| anyhow!("{e}"))?)
        }
        None => None,
    };

    let novels = if every {
        let subs = store.list_subscriptions()?;
        ensure!(!subs.is_empty(), "no subscriptions to re-download");
        subs
    } else {
        vec![store
            .find_novel(&novel)?
            .ok_or_else(|| anyhow!("no subscription matches \"{novel}\""))?]
    };

    if every {
        let total: i64 = novels.iter().map(|n| n.chapter_count).sum();
        eprintln!(
            "Re-downloading {} across {}{}, one request per chapter. This takes a while.",
            count(total as usize, "chapter"),
            count(novels.len(), "novel"),
            if dry_run { " (dry run)" } else { "" }
        );
    }

    let mut changed = Vec::new();
    for n in &novels {
        if !every {
            let scope = match &targets {
                Some(t) => count(t.len(), "chapter"),
                None => format!("all {}", count(n.chapter_count as usize, "chapter")),
            };
            eprintln!(
                "Re-downloading {scope} of \"{}\"{}...",
                n.title,
                if dry_run { " (dry run)" } else { "" }
            );
        }

        let sources = build_sources(n, delay)?;
        let mut bar = ProgressBar::new();
        let label = format!("{}: downloading", n.title);
        let report = vesper_core::refetch_novel(
            &store,
            n.id,
            &sources,
            targets.as_ref(),
            dry_run,
            |_, done, total| bar.update(&label, done, total),
        )
        .await;
        bar.finish();

        let report = match report {
            Ok(r) => r,
            Err(e) if every => {
                eprintln!("  ! {}: {e}", n.title);
                continue;
            }
            Err(e) => return Err(e),
        };
        if report.changed() {
            changed.push((n.id, n.title.clone()));
        }
        report_refetch(&report, n, every, dry_run);
    }

    if changed.is_empty() {
        println!("{}", if dry_run { "Nothing would change." } else { "Nothing changed." });
    } else if !dry_run {
        println!();
        for (id, title) in &changed {
            let Some(novel) = store.find_novel(&id.to_string())? else {
                continue;
            };
            if config.auto_append {
                if !export_and_track(&store, config, &novel).await.is_empty() {
                    println!("{title}: EPUB updated.");
                }
            } else {
                println!("Run `vesper export {id}` to update the EPUB for {title}.");
            }
        }
    }
    Ok(())
}

fn report_refetch(report: &RefetchReport, novel: &StoredNovel, quiet: bool, dry_run: bool) {
    let verb = if dry_run { "would be " } else { "" };
    let interesting = report.changed() || !report.skipped.is_empty();
    if quiet && !interesting {
        return;
    }
    if quiet {
        println!("{}", novel.title);
    }
    if !report.replaced.is_empty() {
        println!(
            "  {} {verb}rewritten: ch. {}",
            count(report.replaced.len(), "chapter"),
            describe_numbers(&report.replaced.iter().copied().collect())
        );
    }
    if !report.added.is_empty() {
        println!(
            "  {} {verb}added: ch. {}",
            count(report.added.len(), "chapter"),
            describe_numbers(&report.added.iter().copied().collect())
        );
    }
    if !report.unchanged.is_empty() && !quiet {
        println!("  {} already matched the source", count(report.unchanged.len(), "chapter"));
    }
    for (number, why) in &report.skipped {
        if *number == 0 {
            eprintln!("  ! {why}");
        } else {
            eprintln!("  ! ch.{number} left as it was: {why}");
        }
    }
}

async fn repair(
    config: &Config,
    novel: String,
    chapter: Option<u32>,
    dry_run: bool,
    delay_ms: Option<u64>,
) -> Result<()> {
    let store = Store::open_default()?;
    let delay = delay_ms.unwrap_or(config.request_delay_ms);

    let novels = if novel.eq_ignore_ascii_case("all") {
        ensure!(chapter.is_none(), "--chapter needs a single novel, not `all`");
        store.list_subscriptions()?
    } else {
        vec![store
            .find_novel(&novel)?
            .ok_or_else(|| anyhow!("no subscription matches \"{novel}\""))?]
    };

    let mut total_repaired = 0usize;
    for n in &novels {
        let sources = build_sources(n, delay)?;
        let mut bar = ProgressBar::new();
        let label = format!("{}: checking", n.title);
        let report = vesper_core::repair_novel(
            &store,
            n.id,
            &sources,
            chapter,
            dry_run,
            |_, done, total| bar.update(&label, done, total),
        )
        .await;
        bar.finish();

        match report {
            Ok(r) if r.is_empty() => {
                if novels.len() == 1 {
                    println!("{}: nothing to repair.", n.title);
                }
            }
            Ok(r) => {
                if !r.repaired.is_empty() {
                    total_repaired += r.repaired.len();
                    let updated = !dry_run
                        && config.auto_append
                        && !export_and_track(&store, config, n).await.is_empty();
                    println!(
                        "{}: {} {}: ch. {}{}",
                        n.title,
                        count(r.repaired.len(), "chapter"),
                        if dry_run { "would be repaired" } else { "repaired" },
                        describe_numbers(&r.repaired.iter().copied().collect()),
                        if updated { "; EPUB updated" } else { "" }
                    );
                }
                for (number, why) in &r.skipped {
                    if *number == 0 {
                        eprintln!("  ! {}: {why}", n.title);
                    } else {
                        eprintln!("  ! {}, ch.{number} left as it was: {why}", n.title);
                    }
                }
            }
            Err(e) => eprintln!("  ! {}: {e}", n.title),
        }
    }

    if total_repaired > 0 && !dry_run && !config.auto_append {
        println!(
            "\n{} replaced. Run `vesper export <id>` to update the EPUBs.",
            count(total_repaired, "chapter")
        );
    }
    Ok(())
}

fn resolve_source<'a>(sources: &'a [StoredSource], needle: &str) -> Result<&'a StoredSource> {
    let needle = needle.trim();
    if let Some(exact) = sources.iter().find(|s| s.url == needle) {
        return Ok(exact);
    }
    let by_name: Vec<&StoredSource> = sources
        .iter()
        .filter(|s| s.name.eq_ignore_ascii_case(needle))
        .collect();
    match by_name.as_slice() {
        [one] => Ok(one),
        [] => {
            let available = sources
                .iter()
                .map(|s| format!("{} ({})", s.name, s.url))
                .collect::<Vec<_>>()
                .join(", ");
            bail!("\"{needle}\" is not a source; available: {available}")
        }
        many => {
            let urls = many.iter().map(|s| s.url.as_str()).collect::<Vec<_>>().join(", ");
            bail!("\"{needle}\" matches {} sources; name one by URL: {urls}", many.len())
        }
    }
}

fn set_primary(novel: String, source: String) -> Result<()> {
    let store = Store::open_default()?;
    let found = store
        .find_novel(&novel)?
        .ok_or_else(|| anyhow!("no subscription matches \"{novel}\""))?;

    let chosen = resolve_source(&found.sources, &source)
        .map_err(|e| anyhow!("{e} for \"{}\"", found.title))?;

    let previous = found.primary_source().map(|s| s.name.clone());
    if !store.promote_source(found.id, chosen.id)? {
        println!("\"{}\" is already the primary source for \"{}\".", chosen.name, found.title);
        return Ok(());
    }
    println!(
        "\"{}\" is now the primary source for \"{}\" (novel #{}){}.",
        chosen.name,
        found.title,
        found.id,
        previous.map(|p| format!("; {p} is now a fallback")).unwrap_or_default()
    );
    println!("  Stored chapters were kept and re-attributed, so nothing will be re-downloaded.");
    Ok(())
}

fn unsubscribe(novel: String) -> Result<()> {
    let store = Store::open_default()?;
    let found = store
        .find_novel(&novel)?
        .ok_or_else(|| anyhow!("no subscription matches \"{novel}\""))?;
    store.remove_subscription(found.id)?;
    println!(
        "Unsubscribed from \"{}\" (novel #{}) and deleted its {}.",
        found.title,
        found.id,
        count(found.chapter_count as usize, "stored chapter")
    );
    Ok(())
}

fn is_url(arg: &str) -> bool {
    arg.starts_with("http://") || arg.starts_with("https://")
}

async fn fetch(
    config: &Config,
    novel: String,
    limit: usize,
    out: Option<PathBuf>,
    delay_ms: Option<u64>,
) -> Result<()> {
    let store = Store::open_default()?;
    let subscribed = if is_url(&novel) {
        store.find_novel_by_source_url(&novel)?
    } else {
        Some(
            store
                .find_novel(&novel)?
                .ok_or_else(|| anyhow!("no subscription matches \"{novel}\""))?,
        )
    };

    let Some(found) = subscribed else {
        return fetch_to_epub(config, &novel, limit, out, delay_ms).await;
    };
    ensure!(
        out.is_none(),
        "--out is for a URL you don't follow; \"{}\" is in your library, so \
         `vesper export {} --out <path>` writes its EPUB",
        found.title,
        found.id
    );

    let sources = build_sources(&found, delay_ms.unwrap_or(config.request_delay_ms))?;
    ensure!(!sources.is_empty(), "no usable sources for \"{}\"", found.title);

    let before = store.stored_chapter_numbers(found.id)?.len();
    eprintln!("Checking \"{}\" on {}...", found.title, count(sources.len(), "source"));
    let pruned = store.pruned_chapters(found.id)?.len();
    let mut state = found.derived_state;
    if pruned > 0 {
        eprintln!("Downloading the {} that prune deleted after export, too.", count(pruned, "chapter"));
        if state == DerivedState::LikelyComplete {
            state = DerivedState::Live;
            store.set_derived_state(found.id, state)?;
        }
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let watcher = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.store(true, Ordering::SeqCst);
            }
        })
    };

    let mut bar = ProgressBar::new();
    let report = sync_novel(&store, found.id, state, &sources, limit, |p| {
        bar.sync(&found.title, p);
        if cancel.load(Ordering::SeqCst) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .await?;
    bar.finish();
    watcher.abort();

    let total = before + (report.newly_fetched + report.restored) as usize;
    if report.interrupted {
        println!(
            "Paused with {} saved, {total} stored in all. Run `vesper fetch {}` to carry on.",
            count((report.newly_fetched + report.restored) as usize, "chapter"),
            found.id
        );
        for w in &report.warnings {
            eprintln!("  ! {w}");
        }
        return Ok(());
    }

    let notes = post_sync(&store, config, found.id, state, &report).await?;

    let fallback_note = if report.from_fallback > 0 {
        format!(" ({} from fallback sources)", report.from_fallback)
    } else {
        String::new()
    };
    let mut upgrade_note = if report.upgraded > 0 {
        format!(
            ", {} re-downloaded from the primary source",
            count(report.upgraded as usize, "chapter")
        )
    } else {
        String::new()
    };
    if report.restored > 0 {
        upgrade_note.push_str(&format!(", {} restored", count(report.restored as usize, "pruned chapter")));
    }
    println!(
        "{}{fallback_note}{upgrade_note}, {total} stored in all.",
        count(report.newly_fetched as usize, "new chapter")
    );
    for w in &report.warnings {
        eprintln!("  ! {w}");
    }
    for note in &notes {
        println!("  {note}");
    }
    if !report.gaps.is_empty() {
        let gaps: BTreeSet<u32> = report.gaps.iter().copied().collect();
        println!(
            "  {}. The EPUB lists them on a Missing Chapters page; `vesper add-source {} <url>` \
             adds another site to try.",
            describe_gaps(&gaps),
            found.id
        );
    }
    Ok(())
}

async fn fetch_to_epub(
    config: &Config,
    url: &str,
    limit: usize,
    out: Option<PathBuf>,
    delay_ms: Option<u64>,
) -> Result<()> {
    let source = source_for(url, delay_ms.unwrap_or(config.request_delay_ms))?;

    eprintln!("Fetching novel metadata...");
    let meta = source.fetch_novel(url).await?;
    eprintln!(
        "  {} by {}",
        meta.title,
        meta.author.as_deref().unwrap_or("Unknown Author")
    );

    eprintln!("Reading the chapter list...");
    let mut refs = source.discover_chapters(url, None).await?;
    ensure!(!refs.is_empty(), "no chapters found for {url}");
    if limit > 0 {
        refs.truncate(limit);
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let watcher = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.store(true, Ordering::SeqCst);
            }
        })
    };

    let total = refs.len();
    let mut chapters = Vec::with_capacity(total);
    let mut failed: Vec<u32> = Vec::new();
    let mut bar = ProgressBar::new();
    let mut interrupted = false;

    for (done, chapter_ref) in refs.iter().enumerate() {
        bar.update("Downloading", done + 1, total);
        match source.fetch_chapter(chapter_ref).await {
            Ok(c) => chapters.push(c),
            Err(e) => {
                bar.finish();
                eprintln!("  ! ch.{}: {e}", chapter_ref.number);
                failed.push(chapter_ref.number);
            }
        }
        if cancel.load(Ordering::SeqCst) {
            interrupted = true;
            break;
        }
    }
    bar.finish();
    watcher.abort();
    ensure!(
        !chapters.is_empty(),
        "no chapters could be downloaded from {url}; no EPUB was written"
    );

    let cover = match &meta.cover_url {
        Some(u) => download_cover(u).await,
        None => None,
    };
    let paths = write_epubs(config, &meta, &chapters, cover.as_ref(), &[], out.as_deref())?;

    let got = if chapters.len() == total {
        count(total, "chapter")
    } else {
        format!("{} of {}", chapters.len(), count(total, "chapter"))
    };
    println!("Downloaded {got} of \"{}\". Saved to:", meta.title);
    for p in &paths {
        println!("  {}", p.display());
    }
    if !failed.is_empty() {
        let numbers: Vec<String> = failed.iter().map(u32::to_string).collect();
        println!("Missing from the EPUB: ch. {}", numbers.join(", "));
    }
    if interrupted {
        println!("Stopped early on Ctrl+C, so the EPUB holds only what was downloaded.");
    }
    println!("Nothing was added to your library.");
    Ok(())
}

async fn scrub(config: &Config, novel: String, dry_run: bool) -> Result<()> {
    let store = Store::open_default()?;
    let novels = if novel.eq_ignore_ascii_case("all") {
        store.list_subscriptions()?
    } else {
        vec![store
            .find_novel(&novel)?
            .ok_or_else(|| anyhow!("no subscription matches \"{novel}\""))?]
    };

    let mut total_marks = 0usize;
    let mut total_chapters = 0usize;
    for n in &novels {
        let report = vesper_core::scrub_novel(&store, n.id, dry_run)?;
        if report.is_empty() {
            if novels.len() == 1 {
                println!("{}: no site marks found.", n.title);
            }
            continue;
        }
        total_marks += report.marks;
        total_chapters += report.chapters.len();
        let updated =
            !dry_run && config.auto_append && !export_and_track(&store, config, n).await.is_empty();
        println!(
            "{}: {} {} in {}{}.",
            n.title,
            count(report.marks, "mark"),
            if dry_run { "found" } else { "removed" },
            count(report.chapters.len(), "chapter"),
            if updated { "; EPUB updated" } else { "" }
        );
    }

    if total_chapters == 0 {
        if novels.len() > 1 {
            println!("No site marks found.");
        }
    } else if dry_run {
        println!(
            "\n{} in {}. Run without --dry-run to remove them.",
            count(total_marks, "mark"),
            count(total_chapters, "chapter")
        );
    } else if !config.auto_append {
        println!(
            "\n{} removed from {}. Run `vesper export <id>` to update the EPUBs.",
            count(total_marks, "mark"),
            count(total_chapters, "chapter")
        );
    }
    Ok(())
}

async fn export(config: &Config, novel: String, out: Option<PathBuf>) -> Result<()> {
    let store = Store::open_default()?;
    let found = store
        .find_novel(&novel)?
        .ok_or_else(|| anyhow!("no subscription matches \"{novel}\""))?;
    let chapters = store.load_chapters(found.id)?;
    ensure!(
        !chapters.is_empty(),
        "\"{}\" has no downloaded chapters yet; run `vesper fetch {}` first",
        found.title,
        found.id
    );

    if let Some(out_path) = out {
        let meta = found.to_meta();
        let cover = match &meta.cover_url {
            Some(url) => download_cover(url).await,
            None => None,
        };
        let gaps: Vec<u32> = store.unfilled_gaps(found.id)?.into_iter().collect();
        write_epubs(config, &meta, &chapters, cover.as_ref(), &gaps, Some(&out_path))?;
        store.mark_all_exported(found.id)?;
        println!(
            "Exported {} of \"{}\" to {}",
            count(chapters.len(), "chapter"),
            found.title,
            out_path.display()
        );
    } else {
        let paths = export_novel(&store, &found, config).await?;
        println!("Exported {} of \"{}\" to:", count(chapters.len(), "chapter"), found.title);
        for p in &paths {
            println!("  {}", p.display());
        }
    }
    Ok(())
}

fn prune(config: &Config, retention_days: Option<u32>) -> Result<()> {
    let store = Store::open_default()?;
    let days = retention_days.unwrap_or(config.retention_days);
    let n = store.apply_retention(days)?;
    println!(
        "Deleted {} of finished novels that were exported more than {days} days ago.",
        count(n, "chapter")
    );
    Ok(())
}

async fn sync_all(config: &Config, limit: usize, delay_ms: Option<u64>) -> Result<()> {
    let _lock = match acquire_sync_lock()? {
        Some(f) => f,
        None => {
            eprintln!("Another sync is already running; skipping.");
            return Ok(());
        }
    };
    let delay = delay_ms.unwrap_or(config.request_delay_ms);

    let store = Store::open_default()?;
    let novels = store.list_subscriptions()?;
    log_line(config, &format!("sync started ({} novels)", novels.len()));
    if novels.is_empty() {
        println!("No subscriptions to sync.");
        log_line(config, "sync complete: no subscriptions");
        return Ok(());
    }

    let mut total_new = 0u32;
    for novel in &novels {
        if novel.derived_state == DerivedState::LikelyComplete {
            if let Ok(Some(last)) = store.last_synced_at(novel.id) {
                let window = config.likely_complete_recheck_days as i64 * 86_400;
                if vesper_core::util::now_unix() - last < window {
                    continue;
                }
            }
        }
        let sources = match build_sources(novel, delay) {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => {
                println!("{}: skipped, no supported source.", novel.title);
                continue;
            }
            Err(e) => {
                println!("{}: skipped, {e}", novel.title);
                continue;
            }
        };
        let mut bar = ProgressBar::new();
        let result = sync_novel(&store, novel.id, novel.derived_state, &sources, limit, |p| {
            bar.sync(&novel.title, p);
            ControlFlow::Continue(())
        })
        .await;
        bar.finish();
        match result {
            Ok(report) => {
                total_new += report.newly_fetched;
                let mode = if report.delta_mode { "delta" } else { "full" };
                let mut extra = if report.upgraded > 0 {
                    format!(", {} upgraded", report.upgraded)
                } else {
                    String::new()
                };
                let mut line = format!(
                    "{}: {}",
                    novel.title,
                    count(report.newly_fetched as usize, "new chapter")
                );
                if report.upgraded > 0 {
                    line.push_str(&format!(
                        ", {} re-downloaded from the primary source",
                        count(report.upgraded as usize, "chapter")
                    ));
                }
                if report.restored > 0 {
                    line.push_str(&format!(
                        ", {} restored",
                        count(report.restored as usize, "pruned chapter")
                    ));
                    extra.push_str(&format!(", {} restored", report.restored));
                }
                if !report.gaps.is_empty() {
                    let gaps: BTreeSet<u32> = report.gaps.iter().copied().collect();
                    extra.push_str(&format!(", {}", describe_gaps(&gaps)));
                    line.push_str(&format!("; {}", describe_gaps(&gaps)));
                }
                let notes = match post_sync(&store, config, novel.id, novel.derived_state, &report).await {
                    Ok(notes) => notes,
                    Err(e) => vec![format!("! after syncing: {e}")],
                };
                let news = report.newly_fetched > 0
                    || report.upgraded > 0
                    || report.restored > 0
                    || !report.gaps.is_empty()
                    || !report.warnings.is_empty()
                    || !notes.is_empty();
                if news {
                    println!("{line}");
                    for w in &report.warnings {
                        eprintln!("  ! {w}");
                    }
                    for note in &notes {
                        println!("  {note}");
                    }
                }
                log_line(
                    config,
                    &format!("  {}: +{} new [{mode}]{extra}", novel.title, report.newly_fetched),
                );
            }
            Err(e) => {
                println!("{}: failed, {e}", novel.title);
                log_line(config, &format!("  ERROR {}: {e}", novel.title));
            }
        }
    }

    match store.apply_retention(config.retention_days) {
        Ok(n) if n > 0 => {
            println!("Deleted {} of finished novels to save space.", count(n, "exported chapter"));
            log_line(config, &format!("pruned {n} exported chapters"));
        }
        Ok(_) => {}
        Err(e) => eprintln!("! prune failed: {e}"),
    }

    println!(
        "Sync complete: {} across {}.",
        count(total_new as usize, "new chapter"),
        count(novels.len(), "novel")
    );
    log_line(
        config,
        &format!(
            "sync complete: {} across {}",
            count(total_new as usize, "new chapter"),
            count(novels.len(), "novel")
        ),
    );
    Ok(())
}

fn service_install(config: &Config, interval_minutes: Option<u32>) -> Result<()> {
    let interval = interval_minutes.unwrap_or(config.poll_interval_minutes);
    let exe = std::env::current_exe()?;
    service::manager()?.install(&exe, interval)?;
    println!(
        "Installed the background sync task \"{}\". It runs `{} sync` every {}.",
        service::TASK_NAME,
        exe.display(),
        count(interval as usize, "minute")
    );
    Ok(())
}

fn service_uninstall() -> Result<()> {
    service::manager()?.uninstall()?;
    println!("Removed the background sync task.");
    Ok(())
}

fn service_status() -> Result<()> {
    let st = service::manager()?.status()?;
    if st.installed {
        println!("Background sync is installed: {}.", st.detail);
    } else {
        println!("Background sync is not installed. Install with `vesper service install`.");
    }
    Ok(())
}

fn config_show(config: &Config) -> Result<()> {
    let path = vesper_core::config::config_path()?;
    println!("Config file: {}", path.display());
    println!("  output_dir = {}", config.output_dir.display());
    println!("  request_delay_ms = {}", config.request_delay_ms);
    println!("  poll_interval_minutes = {}", config.poll_interval_minutes);
    println!("  retention_days = {}", config.retention_days);
    println!("  quiet_grace_days = {}", config.quiet_grace_days);
    println!("  likely_complete_recheck_days = {}", config.likely_complete_recheck_days);
    println!("  auto_export = {}", config.auto_export);
    println!("  auto_append = {}", config.auto_append);
    println!("  split_every_chapters = {}", config.split_every_chapters);
    println!("  log_path = {}", config.log_path.display());
    Ok(())
}

fn status(config: &Config) -> Result<()> {
    let store = Store::open_default()?;
    println!("Library: {}", vesper_core::default_db_path()?.display());
    println!("Log: {}", config.log_path.display());

    let novels = store.list_subscriptions()?;
    let mut states: Vec<(&str, usize)> = Vec::new();
    for n in &novels {
        let label = state_label(n.derived_state);
        match states.iter_mut().find(|(s, _)| *s == label) {
            Some((_, c)) => *c += 1,
            None => states.push((label, 1)),
        }
    }
    let breakdown = match states.as_slice() {
        [(only, _)] => format!("all {only}"),
        _ => states
            .iter()
            .map(|(s, c)| format!("{c} {s}"))
            .collect::<Vec<_>>()
            .join(", "),
    };
    println!("\nFollowing {}{}", count(novels.len(), "novel"), if novels.is_empty() {
        ".".to_string()
    } else {
        format!(", {breakdown}.")
    });

    let mut attention = Vec::new();
    for n in &novels {
        let mut notes = Vec::new();
        if n.export_pending {
            notes.push("EPUB not written yet, the next sync retries it".to_string());
        }
        let gaps = store.unfilled_gaps(n.id)?;
        if !gaps.is_empty() {
            notes.push(describe_gaps(&gaps));
        }
        if !notes.is_empty() {
            attention.push(format!("  #{} {}: {}", n.id, n.title, notes.join("; ")));
        }
    }
    if !attention.is_empty() {
        println!("Needs attention:");
        for line in attention {
            println!("{line}");
        }
    }

    match std::fs::read_to_string(&config.log_path) {
        Ok(content) => {
            let summary = last_sync(&content);
            match summary.finished {
                Some((at, what)) => println!("\nLast sync finished {at}: {what}."),
                None => println!("\nNo sync has finished yet."),
            }
            if let Some(at) = summary.unfinished {
                println!("A sync started {at} and has not finished.");
            }
            if summary.errors.is_empty() {
                println!("No errors in the most recent sync.");
            } else {
                println!("Errors in the most recent sync:");
                for e in summary.errors {
                    println!("  {e}");
                }
            }
        }
        Err(_) => println!("\nNo log yet; the background sync has not run."),
    }
    Ok(())
}

struct LastSync<'a> {
    finished: Option<(&'a str, &'a str)>,
    unfinished: Option<&'a str>,
    errors: Vec<&'a str>,
}

fn stamped(line: &str) -> Option<(&str, &str)> {
    line.strip_prefix('[')?.split_once("] ")
}

fn last_sync(log: &str) -> LastSync<'_> {
    let lines: Vec<&str> = log.lines().collect();
    let position = |prefix: &str| {
        lines
            .iter()
            .rposition(|l| stamped(l).is_some_and(|(_, rest)| rest.starts_with(prefix)))
    };
    let completed = position("sync complete: ");
    let started = position("sync started");

    let finished = completed.and_then(|i| {
        let (at, rest) = stamped(lines[i])?;
        Some((at, rest.strip_prefix("sync complete: ")?))
    });
    let unfinished = match (started, completed) {
        (Some(s), done) if done.map_or(true, |d| s > d) => stamped(lines[s]).map(|(at, _)| at),
        _ => None,
    };
    let errors = lines[started.unwrap_or(0)..]
        .iter()
        .filter_map(|l| l.split_once("ERROR ").map(|(_, e)| e))
        .collect();
    LastSync {
        finished,
        unfinished,
        errors,
    }
}

fn profiles_show() -> Result<()> {
    use vesper_core::profiles;
    println!("Sites Vesper can read, with the name `vesper subs` shows for each:");
    println!("  chikari.moe (chikari)");
    println!("  freewebnovel.com (freewebnovel)");
    println!("  royalroad.com (royalroad)");
    println!("  scribblehub.com (scribblehub)");
    for p in &profiles::all() {
        println!("  {} ({}, from a profile)", p.host, p.name);
    }
    if let Some(dir) = profiles::profiles_dir() {
        println!("\nAdd your own site profiles as .ini files in {}", dir.display());
        println!("README.txt in that folder describes the format.");
    }
    Ok(())
}

async fn list(config: &Config, url: String, delay_ms: Option<u64>) -> Result<()> {
    let source = source_for(&url, delay_ms.unwrap_or(config.request_delay_ms))?;

    eprintln!("Fetching novel metadata...");
    let meta = source.fetch_novel(&url).await?;
    println!(
        "{} by {}",
        meta.title,
        meta.author.as_deref().unwrap_or("Unknown Author")
    );

    eprintln!("Reading the chapter list...");
    let refs = source.discover_chapters(&url, None).await?;
    ensure!(!refs.is_empty(), "no chapters found for {url}");

    println!("{} listed.", count(refs.len(), "chapter"));
    if let (Some(first), Some(last)) = (refs.first(), refs.last()) {
        println!("  First: {}", chapter_label(first.number, &first.title));
        println!("  Last: {}", chapter_label(last.number, &last.title));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_timestamp_has_expected_shape() {
        let ts = local_timestamp();
        println!("sample log timestamp: {ts}");
        assert_eq!(ts.len(), 19, "unexpected shape: {ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], " ");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[16..17], ":");
        assert!(ts[0..4].parse::<u32>().is_ok(), "year: {ts}");
        assert!(ts[11..13].parse::<u32>().is_ok(), "hour: {ts}");
    }

    fn source(name: &str, url: &str, priority: i64) -> StoredSource {
        StoredSource {
            id: priority,
            name: name.into(),
            url: url.into(),
            priority,
            last_seen_chapter: None,
        }
    }

    #[test]
    fn set_primary_accepts_either_column_of_subs() {
        let sources = vec![
            source("lightnovelworld", "https://lightnovelworld.org/novel/a/", 1),
            source("freewebnovel", "https://freewebnovel.com/novel/a", 2),
        ];
        assert_eq!(resolve_source(&sources, "freewebnovel").unwrap().priority, 2);
        assert_eq!(resolve_source(&sources, "FreeWebNovel").unwrap().priority, 2);
        assert_eq!(
            resolve_source(&sources, "https://freewebnovel.com/novel/a").unwrap().priority,
            2
        );
        let err = resolve_source(&sources, "royalroad").unwrap_err().to_string();
        assert!(err.contains("freewebnovel"), "{err}");
    }

    #[test]
    fn set_primary_refuses_an_ambiguous_site_name() {
        let sources = vec![
            source("royalroad", "https://www.royalroad.com/fiction/1/a", 1),
            source("freewebnovel", "https://freewebnovel.com/novel/a", 2),
            source("freewebnovel", "https://freewebnovel.com/novel/a-alt", 3),
        ];
        let err = resolve_source(&sources, "freewebnovel").unwrap_err().to_string();
        assert!(err.contains("matches 2 sources"), "{err}");
        assert_eq!(
            resolve_source(&sources, "https://freewebnovel.com/novel/a-alt").unwrap().priority,
            3
        );
    }

    #[test]
    fn only_library_commands_trigger_migrations() {
        assert!(Command::Subs { gaps: false }.touches_library());
        assert!(Command::Sync { limit: 0, delay_ms: None }.touches_library());
        assert!(Command::Export { novel: "x".into(), out: None }.touches_library());
        assert!(Command::Status.touches_library());

        assert!(!Command::Config.touches_library());
        assert!(!Command::Profiles.touches_library());
        assert!(!Command::Service { action: ServiceAction::Status }.touches_library());
        assert!(!Command::List { url: "x".into(), delay_ms: None }.touches_library());
    }

    #[test]
    fn the_cli_definition_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn sub_and_unsub_are_short_forms() {
        let parsed = Cli::try_parse_from(["vesper", "unsub", "14"]).unwrap();
        assert!(matches!(parsed.cmd, Command::Unsubscribe { novel } if novel == "14"));
        let parsed = Cli::try_parse_from(["vesper", "sub", "https://example.com/n"]).unwrap();
        assert!(matches!(parsed.cmd, Command::Subscribe { .. }));
    }

    #[test]
    fn help_is_a_flag_not_a_command() {
        assert!(Cli::try_parse_from(["vesper", "help"]).is_err());
        let Err(err) = Cli::try_parse_from(["vesper", "--help"]) else {
            panic!("--help should print help");
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = err.to_string();
        assert!(help.contains("Usage: vesper <COMMAND>"), "{help}");
        assert!(!help.contains("vesper.exe"), "{help}");
        assert!(!help.contains("help         Print"), "{help}");
    }

    #[test]
    fn counts_use_real_plurals() {
        assert_eq!(count(1, "chapter"), "1 chapter");
        assert_eq!(count(0, "chapter"), "0 chapters");
        assert_eq!(count(3, "chapter title"), "3 chapter titles");
    }

    #[test]
    fn states_and_chapters_read_as_words() {
        assert_eq!(state_label(DerivedState::LikelyComplete), "likely complete");
        assert_eq!(chapter_label(5, "Chapter 5"), "ch.5");
        assert_eq!(chapter_label(5, ""), "ch.5");
        assert_eq!(chapter_label(1, "Nightmare Begins"), "ch.1 Nightmare Begins");
        let gaps: BTreeSet<u32> = (1..=17).collect();
        assert_eq!(
            describe_gaps(&gaps),
            "no source has ch. 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15 and 2 more"
        );
    }

    #[test]
    fn status_reads_the_last_sync_from_the_log() {
        let log = "\
[2026-11-13 05:14:00] sync started (2 novels)
[2026-11-13 05:14:01]   ERROR Old Novel: timed out
[2026-11-13 05:14:39] sync complete: 0 new across 2 novels
[2026-11-13 06:14:00] sync started (2 novels)
[2026-11-13 06:14:05]   ERROR Shadow Slave: 503 Service Unavailable
";
        let summary = last_sync(log);
        assert_eq!(summary.finished, Some(("2026-11-13 05:14:39", "0 new across 2 novels")));
        assert_eq!(summary.unfinished, Some("2026-11-13 06:14:00"));
        assert_eq!(summary.errors, vec!["Shadow Slave: 503 Service Unavailable"]);

        let done = format!("{log}[2026-11-13 06:14:40] sync complete: 1 new across 2 novels\n");
        let summary = last_sync(&done);
        assert_eq!(summary.finished, Some(("2026-11-13 06:14:40", "1 new across 2 novels")));
        assert_eq!(summary.unfinished, None);
        assert_eq!(summary.errors.len(), 1);
    }

    fn book_chapters(numbers: std::ops::RangeInclusive<u32>) -> Vec<Chapter> {
        numbers
            .map(|n| Chapter {
                number: n,
                title: format!("Chapter {n}"),
                paragraphs: vec![format!("Text {n}.")],
            })
            .collect()
    }

    #[test]
    fn an_epub_is_never_replaced_by_a_shorter_book() {
        let dir = std::env::temp_dir().join(format!("vesper-shrink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = Config {
            output_dir: dir.clone(),
            ..Config::default()
        };
        let meta = NovelMeta {
            title: "Pruned Saga".into(),
            author: Some("Someone".into()),
            cover_url: None,
            genre: None,
            status_hint: vesper_core::NovelStatus::Completed,
            source_url: "https://example.com/saga".into(),
        };

        let written = write_epubs(&config, &meta, &book_chapters(1..=10), None, &[], None).unwrap();
        let before = std::fs::read(&written[0]).unwrap();

        let err = write_epubs(&config, &meta, &book_chapters(9..=11), None, &[], None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("8 chapters that this copy would leave out"), "{err}");
        assert_eq!(std::fs::read(&written[0]).unwrap(), before, "the full book is untouched");

        let out = dir.join("elsewhere.epub");
        std::fs::copy(&written[0], &out).unwrap();
        assert!(write_epubs(&config, &meta, &book_chapters(1..=3), None, &[], Some(&out)).is_err());

        write_epubs(&config, &meta, &book_chapters(1..=11), None, &[], None).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sync_lock_is_single_holder() {
        let path = std::env::temp_dir().join(format!("vesper-locktest-{}.lock", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let first = try_lock_file(&path).unwrap();
        assert!(first.is_some(), "first acquisition should succeed");

        let second = try_lock_file(&path).unwrap();
        assert!(second.is_none(), "second acquisition must be refused while held");

        drop(first);
        let third = try_lock_file(&path).unwrap();
        assert!(third.is_some(), "acquisition succeeds again once released");

        drop(third);
        let _ = std::fs::remove_file(&path);
    }
}
