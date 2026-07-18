# CLAUDE.md

**Vesper** — a Rust tool that crawls webnovel sites, downloads chapters, and
packages them into EPUBs. One binary, two faces: a CLI (control + export) and a
lightweight background sync that keeps subscribed novels current.

**Status: implemented (v1).** All planned phases plus five sources (novgo
profile; hand-written freewebnovel, lightnovelworld, royalroad + scribblehub
adapters), a second fetch tier (curl, with POST support), a windowless scheduled
task, fallback content upgrade, external config-driven profiles, EPUB cover +
genre embedding, and a status command + sync logging are in place and tested
(59 unit + 2 CLI tests). Linux/macOS service
impls are verified in CI (GitHub Actions builds/tests on ubuntu/macos/windows and
round-trips the service install). See `DESIGN.md` for the full decisions and
rationale; this file is the short rules-of-the-road.

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
  `curl`, on Windows 10+, and supports GET *and* form POST), used for
  freewebnovel and scribblehub, whose Cloudflare challenges reqwest's TLS
  fingerprint even though both use Schannel. `build_source` picks the tier +
  adapter per host. Escalate further (`rquest`, headless browser) only if a site
  needs it.
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
- **lightnovelworld.org** (hand-written `lightnovelworld` adapter, Tier 1):
  JS-rendered ToC, so discovery reads the total from `og:title` ("… - N
  Chapters") and generates sequential `/novel/<slug>/chapter/<n>/` URLs.
  Metadata from page elements (`h1.novel-title`, `a.author-link`,
  `.status-badge`) + `og:image` — NOT `og:novel:*`. Content `#chapterText`;
  `data-protected` is JS copy-blocking only (prose is plain `<p>` text, no
  decoys observed).
- **royalroad.com** (hand-written `royalroad` adapter, Tier 1): whole chapter
  list is a `window.chapters = [...]` JSON array in the fiction page (parsed with
  serde_json) — 1-request discovery, but chapter URLs use non-sequential DB ids
  so the list must be read, not generated. Metadata: `<title>` (minus
  "| Royal Road"), `twitter:creator`, `og:image`, status from a `span.label`.
  Content `.chapter-inner`; **decoy paragraphs** are filtered — a `<style>` marks
  a randomized class `display:none` and decoy `<p>`s use it, so collect those
  classes and skip matching paragraphs.
- **scribblehub.com** (hand-written `scribblehub` adapter, Tier 2 curl): the
  hardest source. Cloudflare 403s without a full browser header set *and* a
  `Referer` (so the curl tier sends both). ToC is a WordPress `admin-ajax.php`
  **POST** (`action=wi_getreleases_pagination&pagenum=N&mypostid=<id>`), 15/page,
  newest-first, `a.toc_a` links — an out-of-range page returns 403, so page count
  is derived from the total (`span.cnt_toc`) and paging stops at it. Chapter URLs
  use non-sequential ids and the "Chapter N" labels don't match the count, so
  chapters are numbered by **position**, oldest-first. Series page gives
  `#mypostid` + total; metadata `og:title` / `a[href*="/profile/"]` /
  `span.rnd_stats` + `og:image` (minus `noimagefound`); content `#chp_raw`.

## Build / Test / Run

From the repo root:

- Build: `cargo build`
- Test: `cargo test` (unit tests live inline in `core`'s modules)
- Run (subscription workflow, all DB-backed):
  - `vesper subscribe <novel-url>` — register a novel + its primary source.
  - `vesper add-source <novel> <novel-url>` — add a fallback source to an
    existing novel (warns if the source's title differs; proceeds anyway).
  - `vesper subs` — list subscriptions (shows primary + fallback sources).
  - `vesper fetch <novel> [--limit N]` — download missing chapters into the DB
    (resume-aware; `<novel>` is an id or title; `--limit 0` = all missing).
  - `vesper export <novel> [--out PATH]` — build an EPUB from stored chapters.
  - `vesper unsubscribe <novel>` — remove a subscription (cascades chapters).
  - `vesper sync [--limit N]` — sync ALL subscriptions (what the background task
    runs). Single-instance lock: overlapping runs skip. Re-evaluates completion.
  - `vesper prune [--retention-days N]` — purge exported chapters of
    LikelyComplete novels (never un-exported chapters or ongoing novels).
  - `vesper config` — show the config.ini path and current settings.
  - `vesper status` — DB/log paths, per-novel state, last sync, recent log tail.
  - `vesper profiles` — list loaded site profiles + the folder for custom ones.
  - `vesper service install|uninstall|status [--interval-minutes N]` — manage
    the Windows Task Scheduler job that runs `vesper sync`.
  - `vesper list <novel-url>` — discovery check: walk the full ToC, report
    count + first/last (no DB, no bodies fetched).
- DB + `sync.lock` live at `%LOCALAPPDATA%/vesper/data/`.
- Built binary: `target/debug/vesper.exe`
- Live smoke test: export a few chapters from a novgo novel and validate the
  EPUB (unzip; check `mimetype` == `application/epub+zip`, `content.opf`, and
  that chapter XHTML holds real prose). `cargo test` does NOT rebuild the
  binary — run `cargo build` before invoking `target/debug/vesper.exe`.

## Module map

Cargo workspace, two crates under `crates/`:

- `core` (lib `vesper-core`):
  - `fetch` — `Fetcher` trait; Tier-1 `ReqwestFetcher` (adaptive per-host backoff
    that grows on 429/503/`Retry-After` and relaxes on success, jitter, bounded
    retries; browser UA + headers) and Tier-2 `CurlFetcher` (shells out to
    `curl`; `get` sends a full browser header set + `Referer`, and `post` sends a
    form-encoded AJAX POST — both needed by scribblehub). `FetchConfig` tunes
    base/max delay and retry count.
  - `source` — `Source` trait, declarative `SiteProfile`, `GenericSource`
    adapter, and shared HTML extraction (`parse_novel_meta`, `parse_chapter_body`,
    status parsing) reused by hand-written adapters. Keep parsing synchronous so
    the non-`Send` `scraper::Html` never crosses an `.await`.
  - `freewebnovel` — hand-written `FreewebnovelSource` (AJAX-ToC site). Reuses
    the shared extractors; discovery generates sequential chapter URLs.
  - `lightnovelworld` — hand-written `LightNovelWorldSource` (JS-ToC site).
    Element-based metadata (no `og:novel:*`); discovery generates sequential
    `/chapter/<n>/` URLs from the count in `og:title`; content `#chapterText`.
  - `royalroad` — hand-written `RoyalRoadSource`. Discovery parses the
    `window.chapters` JSON array (serde_json); content `.chapter-inner` with
    `display:none` decoy `<p>`s filtered out.
  - `scribblehub` — hand-written `ScribbleHubSource` (curl tier). Discovery POSTs
    the `admin-ajax.php` ToC pages (newest-first, stops at the `span.cnt_toc`
    total to avoid an out-of-range 403); chapters numbered by position,
    oldest-first; content `#chp_raw`.
  - `profiles` — `SiteProfile`s: built-in (novgo) plus user `.ini` files loaded
    from `<config_dir>/profiles/` (`all()` merges them; self-documents via a
    generated README; bad files skipped with a warning). `crate::build_source`
    (in lib.rs) resolves a URL to its adapter + fetch tier by host.
  - `model` — domain types (`NovelMeta`, `ChapterRef`, `Chapter`, `NovelStatus`).
  - `epub` — EPUB packaging (reconstructed XHTML; atomic temp-file+rename;
    embeds a downloaded cover image and the genre as `dc:subject`).
  - `paths` — library layout (`epub_path`, `novel_dir`): author/novel/epub tree.
  - `store` — SQLite persistence (`Store`, `StoredNovel`): novels/sources/chapters
    schema, WAL, resume-aware chapter insert, DB-backed load for export. rusqlite
    pinned to 0.31 (cfg_select workaround). Connection is not `Send`, so the CLI
    uses a current-thread runtime.
  - `config` — `Config`: flat `config.ini` (`<config_dir>/config.ini`),
    self-generating with commented defaults; tolerant reads (escape-disabled so
    Windows `C:\` paths round-trip). Drives output_dir, delays, retention/grace/
    recheck days, auto_export/auto_append, split_every_chapters, poll interval,
    log_path.
  - `sync` — `sync_novel`: the shared multi-source engine used by both `fetch`
    and the scheduled `sync`. Backfilling walks the full ToC; a caught-up (Live)
    novel does a cheap delta check (landing page) with a full-walk fallback on a
    gap. Gap-fills missing chapters from the highest-priority source (primary
    authoritative), upgrades fallback-sourced chapters once the primary catches
    up, drives the Backfilling->Live transition, returns a `SyncReport`.
  - `util` — filename sanitization, chapter number/title parsing, `now_unix`.
- `cli` (bin `vesper`): clap subcommands on a current-thread Tokio runtime.
  subscribe / add-source / subs / fetch / export / unsubscribe / sync / prune /
  service / config / status / profiles / list. `sync` takes a single-instance
  file lock (Windows `share_mode(0)`) and appends to a log file.
  - `cli::service` — `ServiceManager` trait + Windows Task Scheduler impl (shells
    out to `schtasks`); other platforms stubbed for later. Install registers
    `wscript.exe <sync-hidden.vbs>` so the periodic run is windowless (no console
    flash); the VBS is generated in the data dir and removed on uninstall.

All planned phases are implemented (design docs -> EPUB pipeline -> storage ->
multi-source + active fallback -> scheduled sync -> state machine/delta ->
retention -> config/auto-export), four hand-written adapters (freewebnovel,
lightnovelworld, royalroad, scribblehub) validate the Source + Fetcher
abstractions (spanning AJAX/JS ToCs, embedded-JSON chapter lists, decoy-paragraph
filtering, and POST-based pagination), and the polish items
(windowless task, fallback content upgrade, external profiles) plus the wrap-up
pass (cover + genre embedding, status command + logging, reduced poll cadence,
verified auto_append, cross-platform service impls) are done, and CI (GitHub
Actions) builds/tests on ubuntu/macos/windows — which verified the Linux/macOS
service impls. Nothing functional is outstanding; zstd and same-site-name
disambiguation are documented decisions against.
