//! `crawler` CLI.
//!
//! Runs on a current-thread Tokio runtime: the SQLite connection is not `Send`,
//! and a poller has no need for a multi-threaded work-stealing runtime anyway.

mod service;

use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, ensure, Result};
use clap::{Parser, Subcommand};
use crawler_core::{
    build_epub, build_source, epub_path, sync_novel, Config, DerivedState, Source, Store,
    StoredNovel, StoredSource, SyncReport,
};

#[derive(Parser)]
#[command(name = "crawler", about = "Webnovel crawler -> EPUB", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Subscribe to a novel (registers it and its primary source).
    Subscribe {
        url: String,
        #[arg(long)]
        delay_ms: Option<u64>,
    },
    /// Add an alternate (fallback) source to an existing subscription.
    AddSource {
        novel: String,
        url: String,
        #[arg(long)]
        delay_ms: Option<u64>,
    },
    /// List all subscriptions.
    Subs,
    /// Remove a subscription and its downloaded chapters.
    Unsubscribe { novel: String },
    /// Download missing chapters for a subscribed novel (resume-aware).
    Fetch {
        novel: String,
        #[arg(long, default_value_t = 0)]
        limit: usize,
        #[arg(long)]
        delay_ms: Option<u64>,
    },
    /// Build an EPUB from a subscribed novel's stored chapters.
    Export {
        novel: String,
        /// Output path override (single file, ignores output_dir/splitting).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Purge exported chapters of completed novels to free space.
    Prune {
        #[arg(long)]
        retention_days: Option<u32>,
    },
    /// Sync all subscriptions (what the background task runs).
    Sync {
        #[arg(long, default_value_t = 0)]
        limit: usize,
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
    /// List a novel's discovered chapters (walks the full ToC; no DB, no bodies).
    List {
        url: String,
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
        Command::Export { novel, out } => export(&config, novel, out),
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
/// Returns the written paths; marks all chapters exported.
fn export_novel(store: &Store, novel: &StoredNovel, config: &Config) -> Result<Vec<PathBuf>> {
    let chapters = store.load_chapters(novel.id)?;
    if chapters.is_empty() {
        return Ok(Vec::new());
    }
    let meta = novel.to_meta();
    let mut paths = Vec::new();

    if config.split_every_chapters == 0 {
        let path = epub_path(&config.output_dir, meta.author.as_deref(), &meta.title, None);
        build_epub(&meta, &chapters, &path)?;
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
            build_epub(&meta, chunk, &path)?;
            paths.push(path);
        }
    }
    store.mark_all_exported(novel.id)?;
    Ok(paths)
}

/// After a sync/fetch: re-evaluate completion and run auto-export/append.
fn post_sync(
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
    let gained_new = report.newly_fetched > 0
        && matches!(prev_state, DerivedState::Live | DerivedState::LikelyComplete);

    let novel = store
        .find_novel(&novel_id.to_string())?
        .ok_or_else(|| anyhow!("novel #{novel_id} vanished"))?;

    let should_export = (config.auto_export && just_caught_up)
        || (config.auto_append && gained_new)
        || novel.export_pending;

    if should_export {
        match export_novel(store, &novel, config) {
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

fn acquire_sync_lock() -> Result<Option<File>> {
    let lock_path = crawler_core::default_db_path()?
        .parent()
        .map(|p| p.join("sync.lock"))
        .ok_or_else(|| anyhow!("cannot resolve lock path"))?;
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        match OpenOptions::new().write(true).create(true).share_mode(0).open(&lock_path) {
            Ok(f) => Ok(Some(f)),
            Err(e) if e.raw_os_error() == Some(32) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    #[cfg(not(windows))]
    {
        let f = OpenOptions::new().write(true).create(true).open(&lock_path)?;
        Ok(Some(f))
    }
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
    println!("Next: `crawler fetch {id}` to download chapters.");
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
    Ok(())
}

fn subs() -> Result<()> {
    let store = Store::open_default()?;
    let novels = store.list_subscriptions()?;
    if novels.is_empty() {
        println!("No subscriptions yet. Add one with `crawler subscribe <url>`.");
        return Ok(());
    }
    for n in novels {
        let author = n.author.as_deref().unwrap_or("Unknown Author");
        let pending = if n.export_pending { ", export pending" } else { "" };
        println!(
            "#{}  {} — {}  [{} chapters, {}{pending}]",
            n.id, n.title, author, n.chapter_count, n.derived_state.as_str()
        );
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

    let report = sync_novel(&store, found.id, found.derived_state, &sources, limit, |line| {
        eprintln!("  {line}")
    })
    .await?;

    for w in &report.warnings {
        eprintln!("  ! {w}");
    }
    post_sync(&store, config, found.id, found.derived_state, &report)?;

    let fallback_note = if report.from_fallback > 0 {
        format!(" ({} from fallback sources)", report.from_fallback)
    } else {
        String::new()
    };
    let mode = if report.delta_mode { "delta check" } else { "full scan" };
    println!(
        "Fetched {} new chapters for \"{}\"{fallback_note} (now {} stored) [{mode}].",
        report.newly_fetched,
        found.title,
        before + report.newly_fetched as usize
    );
    Ok(())
}

fn export(config: &Config, novel: String, out: Option<PathBuf>) -> Result<()> {
    let store = Store::open_default()?;
    let found = store
        .find_novel(&novel)?
        .ok_or_else(|| anyhow!("no subscription matches \"{novel}\""))?;
    let chapters = store.load_chapters(found.id)?;
    ensure!(
        !chapters.is_empty(),
        "\"{}\" has no downloaded chapters yet; run `crawler fetch {}` first",
        found.title,
        found.id
    );

    if let Some(out_path) = out {
        build_epub(&found.to_meta(), &chapters, &out_path)?;
        store.mark_all_exported(found.id)?;
        println!("Exported {} chapters of \"{}\" to {}", chapters.len(), found.title, out_path.display());
    } else {
        let paths = export_novel(&store, &found, config)?;
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
    if novels.is_empty() {
        println!("No subscriptions to sync.");
        return Ok(());
    }

    let mut total_new = 0u32;
    for novel in &novels {
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
        match sync_novel(&store, novel.id, novel.derived_state, &sources, limit, |line| {
            eprintln!("  {line}")
        })
        .await
        {
            Ok(report) => {
                for w in &report.warnings {
                    eprintln!("  ! {w}");
                }
                if let Err(e) = post_sync(&store, config, novel.id, novel.derived_state, &report) {
                    eprintln!("  ! post-sync for \"{}\" failed: {e}", novel.title);
                }
                total_new += report.newly_fetched;
                let mode = if report.delta_mode { "delta" } else { "full" };
                println!("  {}: +{} new chapters [{mode}]", novel.title, report.newly_fetched);
            }
            Err(e) => eprintln!("  ! sync failed for \"{}\": {e}", novel.title),
        }
    }

    // Auto-prune per config after the pass.
    match store.apply_retention(config.retention_days) {
        Ok(n) if n > 0 => println!("Pruned {n} exported chapter(s) from completed novels."),
        Ok(_) => {}
        Err(e) => eprintln!("! prune failed: {e}"),
    }

    println!("Sync complete: {total_new} new chapter(s) across {} novel(s).", novels.len());
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
        println!("Background sync is not installed. Install with `crawler service install`.");
    }
    Ok(())
}

fn config_show(config: &Config) -> Result<()> {
    let path = crawler_core::config::config_path()?;
    println!("Config file: {}", path.display());
    println!("  output_dir            = {}", config.output_dir.display());
    println!("  request_delay_ms      = {}", config.request_delay_ms);
    println!("  poll_interval_minutes = {}", config.poll_interval_minutes);
    println!("  retention_days        = {}", config.retention_days);
    println!("  quiet_grace_days      = {}", config.quiet_grace_days);
    println!("  auto_export           = {}", config.auto_export);
    println!("  auto_append           = {}", config.auto_append);
    println!("  split_every_chapters  = {}", config.split_every_chapters);
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
