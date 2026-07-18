//! `vesper` CLI.
//!
//! Runs on a current-thread Tokio runtime: the SQLite connection is not `Send`,
//! and a poller has no need for a multi-threaded work-stealing runtime anyway.

mod service;

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, ensure, Result};
use clap::{Parser, Subcommand};
use vesper_core::{
    build_epub, build_source, download_cover, epub_path, sync_novel, Config, DerivedState, Source,
    Store, StoredNovel, StoredSource, SyncProgress, SyncReport,
};

#[derive(Parser)]
#[command(name = "vesper", about = "Vesper: webnovel crawler -> EPUB", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Subscribe to a novel (registers it and its primary source).
    Subscribe {
        /// Novel landing-page URL.
        url: String,
        /// Override the request delay, in milliseconds (default: config value).
        #[arg(long)]
        delay_ms: Option<u64>,
    },
    /// Add an alternate (fallback) source to an existing subscription.
    AddSource {
        /// Existing novel to attach the source to (id or title).
        novel: String,
        /// New source URL for the same novel.
        url: String,
        /// Override the request delay, in milliseconds (default: config value).
        #[arg(long)]
        delay_ms: Option<u64>,
    },
    /// List all subscriptions.
    Subs,
    /// Remove a subscription and its downloaded chapters.
    Unsubscribe {
        /// Novel to remove (id or title).
        novel: String,
    },
    /// Download missing chapters for a subscribed novel (resume-aware).
    Fetch {
        /// Novel to fetch (id or title).
        novel: String,
        /// Max new chapters to fetch this run (0 = all missing).
        #[arg(long, default_value_t = 0)]
        limit: usize,
        /// Override the request delay, in milliseconds (default: config value).
        #[arg(long)]
        delay_ms: Option<u64>,
    },
    /// Build an EPUB from a subscribed novel's stored chapters.
    Export {
        /// Novel to export (id or title).
        novel: String,
        /// Output path override (single file, ignores output_dir/splitting).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Purge exported chapters of completed novels to free space.
    Prune {
        /// Days to keep exported chapters before purging (default: config value).
        #[arg(long)]
        retention_days: Option<u32>,
    },
    /// Sync all subscriptions (what the background task runs).
    Sync {
        /// Max new chapters per novel this run (0 = all missing).
        #[arg(long, default_value_t = 0)]
        limit: usize,
        /// Override the request delay, in milliseconds (default: config value).
        #[arg(long)]
        delay_ms: Option<u64>,
    },
    /// Manage the background sync task (install/uninstall/status).
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Show the config file path and current settings.
    Config,
    /// Show library status: subscriptions, last sync, and recent log lines.
    Status,
    /// List loaded site profiles and where to add custom ones.
    Profiles,
    /// List a novel's discovered chapters (walks the full ToC; no DB, no bodies).
    List {
        /// Novel landing-page URL.
        url: String,
        /// Override the request delay, in milliseconds (default: config value).
        #[arg(long)]
        delay_ms: Option<u64>,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install the background sync task (Windows Task Scheduler).
    Install {
        /// Interval in minutes (defaults to poll_interval_minutes from config).
        #[arg(long)]
        interval_minutes: Option<u32>,
    },
    /// Remove the background sync task.
    Uninstall,
    /// Show whether the background sync task is installed.
    Status,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Loading also generates config.ini with defaults on first run.
    let config = Config::load_or_create()?;

    match cli.cmd {
        Command::Subscribe { url, delay_ms } => subscribe(&config, url, delay_ms).await,
        Command::AddSource { novel, url, delay_ms } => {
            add_source(&config, novel, url, delay_ms).await
        }
        Command::Subs => subs(),
        Command::Unsubscribe { novel } => unsubscribe(novel),
        Command::Fetch { novel, limit, delay_ms } => fetch(&config, novel, limit, delay_ms).await,
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

fn source_for(url: &str, delay_ms: u64) -> Result<Box<dyn Source>> {
    build_source(url, Duration::from_millis(delay_ms))
        .ok_or_else(|| anyhow!("no known source handles this URL: {url}"))
}

/// Build a source adapter per stored source (priority order), skipping any whose
/// host we have no adapter for.
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

/// Build a novel's EPUB(s) from stored chapters, honouring the split setting.
/// Returns the written paths; marks all chapters exported. Embeds the cover if
/// one can be downloaded (best-effort).
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
    let gaps: Vec<u32> = store.gaps(novel.id)?.into_iter().collect();
    let mut paths = Vec::new();

    if config.split_every_chapters == 0 {
        let path = epub_path(&config.output_dir, meta.author.as_deref(), &meta.title, None);
        build_epub(&meta, &chapters, &path, cover.as_ref(), &gaps)?;
        paths.push(path);
    } else {
        let size = config.split_every_chapters as usize;
        for (i, chunk) in chapters.chunks(size).enumerate() {
            let path = epub_path(
                &config.output_dir,
                meta.author.as_deref(),
                &meta.title,
                Some((i + 1) as u32),
            );
            // Only list gaps that fall within this volume's chapter range.
            let vol_gaps: Vec<u32> = match (chunk.first(), chunk.last()) {
                (Some(f), Some(l)) => {
                    gaps.iter().copied().filter(|g| *g >= f.number && *g <= l.number).collect()
                }
                _ => Vec::new(),
            };
            build_epub(&meta, chunk, &path, cover.as_ref(), &vol_gaps)?;
            paths.push(path);
        }
    }
    store.mark_all_exported(novel.id)?;
    Ok(paths)
}

/// After a sync/fetch: re-evaluate completion and run auto-export/append.
async fn post_sync(
    store: &Store,
    config: &Config,
    novel_id: i64,
    prev_state: DerivedState,
    report: &SyncReport,
) -> Result<()> {
    let final_state = store.reevaluate_completion(novel_id, config.quiet_grace_days)?;
    if final_state != prev_state {
        eprintln!("  state: {} -> {}", prev_state.as_str(), final_state.as_str());
    }

    let just_caught_up =
        prev_state == DerivedState::Backfilling && report.new_state == DerivedState::Live;
    // New chapters or content upgrades both make an existing EPUB stale.
    let content_changed = report.newly_fetched > 0 || report.upgraded > 0;
    let gained_new =
        content_changed && matches!(prev_state, DerivedState::Live | DerivedState::LikelyComplete);

    let novel = store
        .find_novel(&novel_id.to_string())?
        .ok_or_else(|| anyhow!("novel #{novel_id} vanished"))?;

    let should_export = (config.auto_export && just_caught_up)
        || (config.auto_append && gained_new)
        || novel.export_pending;

    if should_export {
        match export_novel(store, &novel, config).await {
            Ok(paths) if !paths.is_empty() => {
                store.set_export_pending(novel_id, false)?;
                eprintln!("  auto-exported {} file(s) to {}", paths.len(), config.output_dir.display());
            }
            Ok(_) => {}
            Err(e) => {
                // Likely the EPUB is open/locked elsewhere; retry next pass.
                store.set_export_pending(novel_id, true)?;
                eprintln!("  ! auto-export deferred (will retry next sync): {e}");
            }
        }
    }
    Ok(())
}

/// Append a timestamped line to the log file (best-effort). Used by `sync` so
/// windowless background runs leave a trail (their stderr is discarded).
fn log_line(config: &Config, msg: &str) {
    if let Some(parent) = config.log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&config.log_path) {
        let _ = writeln!(f, "[{}] {msg}", local_timestamp());
    }
}

/// Current local wall-clock time, e.g. `2026-07-17 21:39:12`. Local (not UTC) so
/// the log reads naturally; DST is handled by the OS via `chrono::Local`.
fn local_timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// One-line summary of a novel's unavailable (404-gap) chapters, or "" if none.
fn describe_gaps(gaps: &BTreeSet<u32>) -> String {
    if gaps.is_empty() {
        return String::new();
    }
    const CAP: usize = 15;
    let mut list = gaps.iter().take(CAP).map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
    if gaps.len() > CAP {
        list.push_str(&format!(", … (+{} more)", gaps.len() - CAP));
    }
    format!("{} unavailable at source (404): ch. {}", gaps.len(), list)
}

/// A single-line, self-overwriting `n/m` progress indicator on stderr. It draws
/// only when stderr is a terminal, so piped, redirected, and windowless
/// background runs stay clean (they get the final summary line instead). The
/// line updates in place with a carriage return and is erased on `finish`, so
/// the summary that follows starts on a clean line.
struct ProgressBar {
    enabled: bool,
    last_len: usize,
}

impl ProgressBar {
    fn new() -> Self {
        // Draw when stderr is a terminal; `VESPER_FORCE_PROGRESS` forces it on
        // for cases where detection is wrong (some multiplexers / CI) or to make
        // the raw output observable when capturing.
        let forced = std::env::var_os("VESPER_FORCE_PROGRESS").is_some();
        Self {
            enabled: forced || std::io::stderr().is_terminal(),
            last_len: 0,
        }
    }

    fn update(&mut self, p: SyncProgress) {
        if !self.enabled {
            return;
        }
        let msg = match p {
            SyncProgress::Fetching { done, total } => format!("  Fetching {done}/{total}..."),
            SyncProgress::Upgrading { done, total } => format!("  Upgrading {done}/{total}..."),
        };
        // Pad over any leftover from a previously longer line, then reset to col 0.
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

/// Take an exclusive advisory lock on `path`, returning the held `File` on
/// success or `None` if another process already holds it. Cross-platform via
/// `fs2` (LockFileEx on Windows, `flock` on Unix) — this is what makes
/// overlapping `sync` runs skip on every platform, not just Windows. The lock is
/// released when the returned `File` is dropped (i.e. at the end of the run).
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

/// Whether a `try_lock_exclusive` error means "already locked by someone else".
/// On Unix `flock` returns `EWOULDBLOCK`, which std maps to `WouldBlock`; on
/// Windows `LockFileEx` returns `ERROR_LOCK_VIOLATION` (os error 33), which it
/// does not, so match that explicitly.
fn lock_would_block(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::WouldBlock || (cfg!(windows) && e.raw_os_error() == Some(33))
}

async fn subscribe(config: &Config, url: String, delay_ms: Option<u64>) -> Result<()> {
    let source = source_for(&url, delay_ms.unwrap_or(config.request_delay_ms))?;
    eprintln!("Fetching novel metadata...");
    let meta = source.fetch_novel(&url).await?;

    let store = Store::open_default()?;
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
        if !meta.title.eq_ignore_ascii_case(&found.title) {
            eprintln!(
                "  warning: this source's title is \"{}\", but the novel is \"{}\". \
                 Proceeding since you asked to link them.",
                meta.title, found.title
            );
        }
    }

    let sid = store.add_source(found.id, source.name(), &url)?;
    println!("Added {} as a fallback source (#{sid}) for \"{}\".", source.name(), found.title);

    // If the novel has gaps, re-open its backfill so the next sync does a full
    // walk and tries to fill those holes from the new source.
    if !store.gaps(found.id)?.is_empty() {
        store.set_derived_state(found.id, DerivedState::Backfilling)?;
        println!("  It has unavailable chapters; the next sync will try to fill them from this source.");
    }
    Ok(())
}

fn subs() -> Result<()> {
    let store = Store::open_default()?;
    let novels = store.list_subscriptions()?;
    if novels.is_empty() {
        println!("No subscriptions yet. Add one with `vesper subscribe <url>`.");
        return Ok(());
    }
    for n in novels {
        let author = n.author.as_deref().unwrap_or("Unknown Author");
        let pending = if n.export_pending { ", export pending" } else { "" };
        println!(
            "#{}  {} — {}  [{} chapters, {}{pending}]",
            n.id, n.title, author, n.chapter_count, n.derived_state.as_str()
        );
        let gaps = store.gaps(n.id)?;
        if !gaps.is_empty() {
            println!("    gaps: {}", describe_gaps(&gaps));
        }
        for s in &n.sources {
            let seen = s
                .last_seen_chapter
                .map(|c| format!("last seen ch.{c}"))
                .unwrap_or_else(|| "not yet synced".into());
            let role = if s.priority == 1 { "primary" } else { "fallback" };
            println!("    [{role}] {} — {} ({seen})", s.name, s.url);
        }
    }
    Ok(())
}

fn unsubscribe(novel: String) -> Result<()> {
    let store = Store::open_default()?;
    let found = store
        .find_novel(&novel)?
        .ok_or_else(|| anyhow!("no subscription matches \"{novel}\""))?;
    store.remove_subscription(found.id)?;
    println!(
        "Unsubscribed from \"{}\" (novel #{}); removed {} stored chapters.",
        found.title, found.id, found.chapter_count
    );
    Ok(())
}

async fn fetch(config: &Config, novel: String, limit: usize, delay_ms: Option<u64>) -> Result<()> {
    let store = Store::open_default()?;
    let found = store
        .find_novel(&novel)?
        .ok_or_else(|| anyhow!("no subscription matches \"{novel}\""))?;

    let sources = build_sources(&found, delay_ms.unwrap_or(config.request_delay_ms))?;
    ensure!(!sources.is_empty(), "no usable sources for \"{}\"", found.title);

    let before = store.stored_chapter_numbers(found.id)?.len();
    eprintln!("Syncing \"{}\" from {} source(s)...", found.title, sources.len());

    // Ctrl+C during a manual fetch pauses gracefully: a watcher flips this flag,
    // the progress callback returns Break, and sync_novel stops after the current
    // (already-saved) chapter. A second Ctrl+C hits the default handler and hard-
    // aborts. Downloaded chapters are durable regardless, so the run resumes.
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
    let report = sync_novel(&store, found.id, found.derived_state, &sources, limit, |p| {
        bar.update(p);
        if cancel.load(Ordering::SeqCst) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .await?;
    bar.finish();
    watcher.abort();

    for w in &report.warnings {
        eprintln!("  ! {w}");
    }

    if report.interrupted {
        println!(
            "Paused \"{}\": {} new chapter(s) saved (now {} stored). Resume with `vesper fetch {}`.",
            found.title,
            report.newly_fetched,
            before + report.newly_fetched as usize,
            found.id
        );
        return Ok(());
    }

    post_sync(&store, config, found.id, found.derived_state, &report).await?;

    let fallback_note = if report.from_fallback > 0 {
        format!(" ({} from fallback sources)", report.from_fallback)
    } else {
        String::new()
    };
    let mode = if report.delta_mode { "delta check" } else { "full scan" };
    let upgrade_note = if report.upgraded > 0 {
        format!(", {} upgraded from primary", report.upgraded)
    } else {
        String::new()
    };
    println!(
        "Fetched {} new chapters for \"{}\"{fallback_note}{upgrade_note} (now {} stored) [{mode}].",
        report.newly_fetched,
        found.title,
        before + report.newly_fetched as usize
    );
    if !report.gaps.is_empty() {
        let gaps: BTreeSet<u32> = report.gaps.iter().copied().collect();
        println!(
            "Note: {} — the source has no page for these. They're marked in the EPUB; \
             add an alternate source (`vesper add-source {}`) to try to fill them.",
            describe_gaps(&gaps),
            found.id
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
        let gaps: Vec<u32> = store.gaps(found.id)?.into_iter().collect();
        build_epub(&meta, &chapters, &out_path, cover.as_ref(), &gaps)?;
        store.mark_all_exported(found.id)?;
        println!("Exported {} chapters of \"{}\" to {}", chapters.len(), found.title, out_path.display());
    } else {
        let paths = export_novel(&store, &found, config).await?;
        println!("Exported {} chapters of \"{}\" to {} file(s):", chapters.len(), found.title, paths.len());
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
    println!("Pruned {n} exported chapter(s) from completed novels (retention {days}d).");
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
        // Poll finished novels less often: skip a LikelyComplete novel that was
        // re-checked within the recheck window.
        if novel.derived_state == DerivedState::LikelyComplete {
            if let Ok(Some(last)) = store.last_synced_at(novel.id) {
                let window = config.likely_complete_recheck_days as i64 * 86_400;
                if vesper_core::util::now_unix() - last < window {
                    eprintln!("[{}] finished; re-checked recently, skipping", novel.title);
                    continue;
                }
            }
        }
        let sources = match build_sources(novel, delay) {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => {
                eprintln!("[{}] no usable sources; skipping", novel.title);
                continue;
            }
            Err(e) => {
                eprintln!("[{}] {e}; skipping", novel.title);
                continue;
            }
        };
        eprintln!("Syncing \"{}\"...", novel.title);
        let mut bar = ProgressBar::new();
        let result = sync_novel(&store, novel.id, novel.derived_state, &sources, limit, |p| {
            bar.update(p);
            ControlFlow::Continue(())
        })
        .await;
        bar.finish();
        match result {
            Ok(report) => {
                for w in &report.warnings {
                    eprintln!("  ! {w}");
                }
                if let Err(e) =
                    post_sync(&store, config, novel.id, novel.derived_state, &report).await
                {
                    eprintln!("  ! post-sync for \"{}\" failed: {e}", novel.title);
                }
                total_new += report.newly_fetched;
                let mode = if report.delta_mode { "delta" } else { "full" };
                let mut extra = if report.upgraded > 0 {
                    format!(", {} upgraded", report.upgraded)
                } else {
                    String::new()
                };
                // Surface gaps in the log (background stderr is discarded) and on
                // screen for a manual/visible run.
                if !report.gaps.is_empty() {
                    let gaps: BTreeSet<u32> = report.gaps.iter().copied().collect();
                    extra.push_str(&format!(", {}", describe_gaps(&gaps)));
                    eprintln!("  ! {}: {}", novel.title, describe_gaps(&gaps));
                }
                println!("  {}: +{} new chapters [{mode}]", novel.title, report.newly_fetched);
                log_line(
                    config,
                    &format!("  {}: +{} new [{mode}]{extra}", novel.title, report.newly_fetched),
                );
            }
            Err(e) => {
                eprintln!("  ! sync failed for \"{}\": {e}", novel.title);
                log_line(config, &format!("  ERROR {}: {e}", novel.title));
            }
        }
    }

    // Auto-prune per config after the pass.
    match store.apply_retention(config.retention_days) {
        Ok(n) if n > 0 => {
            println!("Pruned {n} exported chapter(s) from completed novels.");
            log_line(config, &format!("pruned {n} exported chapters"));
        }
        Ok(_) => {}
        Err(e) => eprintln!("! prune failed: {e}"),
    }

    println!("Sync complete: {total_new} new chapter(s) across {} novel(s).", novels.len());
    log_line(config, &format!("sync complete: {total_new} new across {} novels", novels.len()));
    Ok(())
}

fn service_install(config: &Config, interval_minutes: Option<u32>) -> Result<()> {
    let interval = interval_minutes.unwrap_or(config.poll_interval_minutes);
    let exe = std::env::current_exe()?;
    service::manager()?.install(&exe, interval)?;
    println!(
        "Installed background sync: \"{}\" runs `{}` every {interval} min, windowless.",
        service::TASK_NAME,
        exe.display()
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
        println!("Background sync is INSTALLED.\n{}", st.detail);
    } else {
        println!("Background sync is not installed. Install with `vesper service install`.");
    }
    Ok(())
}

fn config_show(config: &Config) -> Result<()> {
    let path = vesper_core::config::config_path()?;
    println!("Config file: {}", path.display());
    println!("  output_dir            = {}", config.output_dir.display());
    println!("  request_delay_ms      = {}", config.request_delay_ms);
    println!("  poll_interval_minutes = {}", config.poll_interval_minutes);
    println!("  retention_days        = {}", config.retention_days);
    println!("  quiet_grace_days      = {}", config.quiet_grace_days);
    println!("  likely_complete_recheck_days = {}", config.likely_complete_recheck_days);
    println!("  auto_export           = {}", config.auto_export);
    println!("  auto_append           = {}", config.auto_append);
    println!("  split_every_chapters  = {}", config.split_every_chapters);
    println!("  log_path              = {}", config.log_path.display());
    Ok(())
}

fn status(config: &Config) -> Result<()> {
    let store = Store::open_default()?;
    println!("Library DB: {}", vesper_core::default_db_path()?.display());
    println!("Log file:   {}", config.log_path.display());

    let novels = store.list_subscriptions()?;
    println!("\nSubscriptions: {}", novels.len());
    for n in &novels {
        let pending = if n.export_pending { ", export pending" } else { "" };
        println!(
            "  #{} {} — {} chapters [{}{pending}]",
            n.id, n.title, n.chapter_count, n.derived_state.as_str()
        );
        let gaps = store.gaps(n.id)?;
        if !gaps.is_empty() {
            println!("      gaps: {}", describe_gaps(&gaps));
        }
    }

    match std::fs::read_to_string(&config.log_path) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            if let Some(last) = lines.iter().rev().find(|l| l.contains("sync complete")) {
                println!("\nLast completed sync: {last}");
            }
            let tail: Vec<&&str> = lines.iter().rev().take(8).collect();
            if !tail.is_empty() {
                println!("Recent log:");
                for l in tail.into_iter().rev() {
                    println!("  {l}");
                }
            }
        }
        Err(_) => println!("\nNo log yet (the background sync hasn't run)."),
    }
    Ok(())
}

fn profiles_show() -> Result<()> {
    use vesper_core::profiles;
    // Calling all() also generates the README in the profiles folder.
    let loaded = profiles::all();
    if let Some(dir) = profiles::profiles_dir() {
        println!("Add custom site profiles (.ini) in: {}", dir.display());
        println!("(see README.txt there for the format)\n");
    }
    println!("Config-driven profiles:");
    for p in &loaded {
        println!("  {} — {}", p.host, p.name);
    }
    println!("Built-in hand-written adapters:");
    println!("  freewebnovel.com — freewebnovel (Tier-2 curl)");
    Ok(())
}

async fn list(config: &Config, url: String, delay_ms: Option<u64>) -> Result<()> {
    let source = source_for(&url, delay_ms.unwrap_or(config.request_delay_ms))?;

    eprintln!("Fetching novel metadata...");
    let meta = source.fetch_novel(&url).await?;
    eprintln!("  Title:  {}", meta.title);
    if let Some(author) = &meta.author {
        eprintln!("  Author: {author}");
    }

    eprintln!("Walking full table of contents...");
    let refs = source.discover_chapters(&url, None).await?;
    ensure!(!refs.is_empty(), "no chapters found for {url}");

    println!("{} chapters discovered.", refs.len());
    if let (Some(first), Some(last)) = (refs.first(), refs.last()) {
        println!("  first: ch.{} {}", first.number, first.title);
        println!("  last:  ch.{} {}", last.number, last.title);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_timestamp_has_expected_shape() {
        // e.g. "2026-07-17 21:39:12" — 19 chars, no offset suffix.
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

    #[test]
    fn sync_lock_is_single_holder() {
        let path = std::env::temp_dir().join(format!("vesper-locktest-{}.lock", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let first = try_lock_file(&path).unwrap();
        assert!(first.is_some(), "first acquisition should succeed");

        // A second acquisition while the first is held must be refused — this is
        // the guarantee that was silently missing on non-Windows.
        let second = try_lock_file(&path).unwrap();
        assert!(second.is_none(), "second acquisition must be refused while held");

        drop(first);
        let third = try_lock_file(&path).unwrap();
        assert!(third.is_some(), "acquisition succeeds again once released");

        drop(third);
        let _ = std::fs::remove_file(&path);
    }
}
