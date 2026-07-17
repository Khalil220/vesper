# CLAUDE.md

Rust tool that crawls webnovel sites, downloads chapters, and packages them into
EPUBs. One binary, two faces: a CLI (control + export) and a lightweight
background sync that keeps subscribed novels current.

**Status: design phase, pre-scaffold.** No code yet. See `DESIGN.md` for the
full decisions and rationale; this file is the short rules-of-the-road. Fill in
the Build / Test / Run and module-map sections below once the workspace exists.

## Load-bearing constraints (don't re-litigate — see DESIGN.md for why)

- **One binary, Cargo workspace:** a `core` lib crate plus a single binary with
  subcommands. The background "service" is the same binary in a sync mode, not a
  separate program.
- **Background sync = scheduled invocation (Windows Task Scheduler), not a
  resident daemon.** Zero memory when idle. `install`/`uninstall` register/remove
  the task (shell out to `schtasks.exe`). Put service-management behind a trait
  for later Linux/macOS. Take a process lock per run to prevent double-fire.
- **SQLite (WAL) is the single source of truth**, in `%LOCALAPPDATA%` (via
  `directories::data_local_dir`) — **never roaming `%APPDATA%`**. Store cleaned
  text/XHTML, not raw HTML.
- **Multi-source by design.** A `Source` trait abstracts each site; a generic
  config-driven adapter handles the common server-rendered + CSS + `?page=N`
  case, so most sites (novgo included) are just declarative profiles, no
  recompile. Hand-written adapters only for weird sites. novgo is the first
  source, not the only one. URL-to-source resolves by host.
- **One logical novel, multiple ranked sources** (not per-source subscriptions).
  A novel (author+title) has a primary source plus optional fallbacks.
  Subscribing to an already-followed novel from a new site adds an alternate
  source (user-confirmed), never a duplicate — one novel => one EPUB at the
  author/novel path. **Active fallback:** sync gap-fills chapters the primary
  lacks from fallbacks by priority/chapter-number; primary is authoritative for
  content. Schema: `novels` / `sources` / `chapters` (see DESIGN.md).
- **Tiered fetcher behind a trait.** Tier 1 = `ReqwestFetcher` (browser UA +
  headers); novgo needs only this. Tier 2 = `CurlFetcher` (shells out to system
  `curl`, on Windows 10+), used for freewebnovel, whose Cloudflare challenges
  reqwest's TLS fingerprint even though both use Schannel. `build_source` picks
  the tier + adapter per host. Escalate further (`rquest`, headless browser)
  only if a site needs it.
- **Adaptive, per-host politeness:** modest delay + jitter, one request in
  flight per host, back off on 429/503, honor `Retry-After`, resume-on-disk.
  Parallelism only across distinct hosts. **No proxy-rotation / ban-evasion.**
- **Completion detection — don't trust the label.** The site status field is a
  *hint*; observed activity is authoritative. New chapters observed => Ongoing,
  overriding any "completed" label. The label only lowers poll cadence; never
  stop polling until unsubscribed. Hiatus != completed.
- **Retention resolves delete-vs-append:** ongoing novels keep chapters in the
  DB (so append = regenerate-from-DB); only *Likely complete* novels (labeled
  complete AND observably quiet for the grace window AND finally exported) get
  purged. Never fires on the label alone. Retention **never** deletes an
  un-exported chapter. Revival after purge re-hydrates the working set.
- **Auto-export via a Backfilling -> Caught-up/Live state machine**, not a
  "majority of chapters" heuristic. `auto_export` and `auto_append` are separate
  toggles.
- **EPUB writes are atomic** (temp file + rename). A sharing-violation on
  replace means the file is locked (Calibre/e-reader/OneDrive) -> mark export
  `pending`, retry next cycle.
- **Filename sanitization is mandatory** on Windows (strip `<>:"/\|?*`, trailing
  dots/spaces, reserved names).
- **Output layout: `<library>/<author>/<novel>/<novel>.epub`** (library defaults
  to `Documents/lightnovels`); volumes are `<novel> - Vol NN.epub` in the novel
  folder. See `core::paths`.
- Config: global flat `config.ini` (defaults) + per-novel overrides in the DB.

## Site quick reference

- **novgo.net** (generic profile): Cloudflare CDN-only, no challenge, Tier 1.
  Server-rendered; ToC paginated `?page=N` (~50/page); chapter URLs
  `/<slug>/chapter-<n>-<slug>.html`; content `div#chapter-content.chapter-c`
  (strip `div.ads*`); metadata/cover `og:novel:*` + `og:image`; status "1"/"2".
- **freewebnovel.com** (hand-written `freewebnovel` adapter, Tier 2 curl):
  AJAX/JS ToC (no scrapable pagination), so discovery reads `data-total-chapters`
  and generates sequential `/novel/<slug>/chapter-<n>` URLs from one request;
  chapter title comes from the chapter page `<title>`; content `.txt`; metadata
  `og:novel:*`; status word form ("Completed"/"Ongoing").

## Build / Test / Run

From the repo root:

- Build: `cargo build`
- Test: `cargo test` (unit tests live inline in `core`'s modules)
- Run (subscription workflow, all DB-backed):
  - `crawler subscribe <novel-url>` — register a novel + its primary source.
  - `crawler add-source <novel> <novel-url>` — add a fallback source to an
    existing novel (warns if the source's title differs; proceeds anyway).
  - `crawler subs` — list subscriptions (shows primary + fallback sources).
  - `crawler fetch <novel> [--limit N]` — download missing chapters into the DB
    (resume-aware; `<novel>` is an id or title; `--limit 0` = all missing).
  - `crawler export <novel> [--out PATH]` — build an EPUB from stored chapters.
  - `crawler unsubscribe <novel>` — remove a subscription (cascades chapters).
  - `crawler sync [--limit N]` — sync ALL subscriptions (what the background task
    runs). Single-instance lock: overlapping runs skip. Re-evaluates completion.
  - `crawler prune [--retention-days N]` — purge exported chapters of
    LikelyComplete novels (never un-exported chapters or ongoing novels).
  - `crawler config` — show the config.ini path and current settings.
  - `crawler profiles` — list loaded site profiles + the folder for custom ones.
  - `crawler service install|uninstall|status [--interval-minutes N]` — manage
    the Windows Task Scheduler job that runs `crawler sync`.
  - `crawler list <novel-url>` — discovery check: walk the full ToC, report
    count + first/last (no DB, no bodies fetched).
- DB + `sync.lock` live at `%LOCALAPPDATA%/webnovel-crawler/data/`.
- Built binary: `target/debug/crawler.exe`
- Live smoke test: export a few chapters from a novgo novel and validate the
  EPUB (unzip; check `mimetype` == `application/epub+zip`, `content.opf`, and
  that chapter XHTML holds real prose). `cargo test` does NOT rebuild the
  binary — run `cargo build` before invoking `target/debug/crawler.exe`.

## Module map

Cargo workspace, two crates under `crates/`:

- `core` (lib `crawler-core`):
  - `fetch` — `Fetcher` trait; Tier-1 `ReqwestFetcher` (adaptive per-host backoff
    that grows on 429/503/`Retry-After` and relaxes on success, jitter, bounded
    retries; browser UA + headers) and Tier-2 `CurlFetcher` (shells out to
    `curl`). `FetchConfig` tunes base/max delay and retry count.
  - `source` — `Source` trait, declarative `SiteProfile`, `GenericSource`
    adapter, and shared HTML extraction (`parse_novel_meta`, `parse_chapter_body`,
    status parsing) reused by hand-written adapters. Keep parsing synchronous so
    the non-`Send` `scraper::Html` never crosses an `.await`.
  - `freewebnovel` — hand-written `FreewebnovelSource` (AJAX-ToC site). Reuses
    the shared extractors; discovery generates sequential chapter URLs.
  - `profiles` — `SiteProfile`s: built-in (novgo) plus user `.ini` files loaded
    from `<config_dir>/profiles/` (`all()` merges them; self-documents via a
    generated README; bad files skipped with a warning). `crate::build_source`
    (in lib.rs) resolves a URL to its adapter + fetch tier by host.
  - `model` — domain types (`NovelMeta`, `ChapterRef`, `Chapter`, `NovelStatus`).
  - `epub` — EPUB packaging (reconstructed XHTML; atomic temp-file+rename).
  - `paths` — library layout (`epub_path`, `novel_dir`): author/novel/epub tree.
  - `store` — SQLite persistence (`Store`, `StoredNovel`): novels/sources/chapters
    schema, WAL, resume-aware chapter insert, DB-backed load for export. rusqlite
    pinned to 0.31 (cfg_select workaround). Connection is not `Send`, so the CLI
    uses a current-thread runtime.
  - `config` — `Config`: flat `config.ini` (`<config_dir>/config.ini`),
    self-generating with commented defaults; tolerant reads. Drives output_dir,
    delays, retention/grace days, auto_export/auto_append, split_every_chapters,
    poll interval.
  - `sync` — `sync_novel`: the shared multi-source engine. Discovers each ranked
    source (best-effort), gap-fills missing chapter numbers from the
    highest-priority source that has them (primary authoritative), returns a
    `SyncReport`. Used by `fetch` now and the future scheduled `sync`.
  - `util` — filename sanitization, chapter number/title parsing, `now_unix`.
- `cli` (bin `crawler`): clap subcommands on a current-thread Tokio runtime.
  subscribe / add-source / subs / fetch / export / unsubscribe / sync / service /
  list. `sync` takes a single-instance file lock (Windows `share_mode(0)`).
  - `cli::service` — `ServiceManager` trait + Windows Task Scheduler impl (shells
    out to `schtasks`); other platforms stubbed for later. Install registers
    `wscript.exe <sync-hidden.vbs>` so the periodic run is windowless (no console
    flash); the VBS is generated in the data dir and removed on uninstall.

Core design phases (design docs -> EPUB pipeline -> storage -> multi-source ->
scheduled sync -> state machine/delta -> retention -> config/auto-export) are all
implemented, and a second site (freewebnovel) validates the Source + Fetcher
abstractions. Remaining are the smaller deferred items in DESIGN.md's open list
(e.g. windowless scheduled task, fallback content upgrade).
