# Design

**Vesper** is a Rust tool that crawls webnovel sites, downloads chapters, and
packages them into EPUB files. It has two faces sharing one codebase: a CLI for
control and export, and a lightweight background sync that keeps subscribed
novels current.

This document records the decisions made and *why*, so they don't get
re-litigated later. CLAUDE.md holds the short rules-of-the-road and points here
for depth.

## Shape of the system

- **One binary, Cargo workspace.** A `core` library crate (fetch, extract,
  store, EPUB) plus a single binary exposing subcommands. The "service" is not a
  separate program — it is the same binary invoked in a sync mode by the OS
  scheduler. This keeps the CLI and the background worker sharing identical code
  instead of drifting apart, and it is one artifact to distribute.

- **The CLI owns:** subscription management (subscribe/add-source/unsubscribe/
  subs), fetch, EPUB export, prune, `config`/`profiles` display, and service
  install/uninstall/status. No TUI — plain commands and arguments.

- **The background sync is a dumb poller:** check subscribed novels for new
  chapters, fetch the deltas, store them, and (conditionally) trigger export. It
  never makes policy decisions the CLI hasn't configured.

## Sources (multi-site)

novgo is the *first* source, not the only one. The system is built around a
**source adapter** abstraction so new sites are added without rewriting the core.

- A `Source` trait defines what any site must provide: match a URL to this
  source (by host), discover the chapter list, extract chapter content, extract
  metadata (title / author / cover / status), and declare its fetch tier.
- **Two ways to implement a source:**
  1. A **generic, config-driven adapter** for the common case — server-rendered
     pages, CSS-selectable content, `?page=N`-style ToC pagination. Most novel
     aggregators (including novgo) fit this, so they become a declarative site
     *profile* (selectors + pagination pattern + tier + rate limits), addable
     without recompiling.
  2. A **hand-written Rust adapter** implementing the same trait, for sites too
     weird for the generic one (JS-rendered, AJAX ToC, odd auth).
- Start with the trait plus one generic config-driven adapter; **novgo is just a
  profile** for it. Reach for a bespoke adapter only when a site earns it.
- **The data model is a logical novel with one or more ranked sources**, not a
  per-source subscription. A novel (identified by author + title) is fed by a
  primary source plus optional fallbacks, ordered by preference. This revises an
  earlier call (independent per-source subscriptions), which would have collided
  on the author/title output path — two sources of the same novel resolve to the
  same `<author>/<novel>/<novel>.epub`.
- **Subscribing to an already-followed novel from a new site adds an alternate
  source** to the existing novel rather than creating a duplicate. This is
  **user-confirmed** — cross-site titles differ (romanizations, alternate
  names), so duplicate detection is never silent. One logical novel => one EPUB
  at the author/novel path, regardless of source count.
- **Active fallback sync.** Sync pulls from the primary source; for chapters the
  primary is missing, it gap-fills from fallbacks by priority, matched on chapter
  number. The **primary is authoritative for content** — fallbacks only supply
  chapters the primary lacks, never overwrite. Warn when sources' chapter counts
  diverge substantially: cross-source numbering is only approximately aligned
  (sites split, group, and number prologues/bonus chapters differently), so
  gap-fill by number is best-effort, not a guaranteed 1:1 mapping.
- Chapter URLs are source-specific; URL-to-source resolution is by host.

## Background sync: scheduled invocation, not a resident daemon

We run the sync as a **scheduled invocation**, not a long-lived process. The OS
scheduler launches the binary in sync mode every N minutes; it syncs, then
exits. When it is not actively syncing it uses **zero** memory, because nothing
is resident. This directly serves the "as little memory as possible, runs
unnoticed" goal.

A novel poller has no reason to hold state between runs — "last-seen chapter"
lives in the DB, not in RAM — so the main thing a resident daemon would buy
(in-memory state, sub-interval reactivity) is wasted here.

- **Windows (primary target):** Task Scheduler, every N minutes. `install`
  registers a windowless task via a hidden VBS launcher (see the "Windowless
  scheduled task" resolved item); `uninstall` removes it. Shelling out to
  `schtasks.exe` is far less code than implementing a real Windows Service.
- **Portability:** put service install/uninstall behind a small trait so a
  systemd user unit (Linux) or launchd plist (macOS) can slot in later without
  touching the rest. Do Windows first; do not let this one component's
  portability block the core.
- **Future option:** a resident daemon (tokio `current_thread` flavor) can be
  added later for staggered per-host workers with shared live rate-limit state,
  without changing the CLI or DB. Not now.

- **Overlap guard:** scheduled invocation can double-fire if one run runs long.
  Take a process-level lock (lockfile or SQLite lock) at the start of a sync run
  so two runs can't stomp each other.

## Storage

- **SQLite (WAL mode) is the single source of truth.** One file, transactional,
  no thousands-of-tiny-files overhead. WAL lets the CLI and a sync run touch it
  concurrently from separate processes. Chapters are rows, not loose files.

- **Location: `%LOCALAPPDATA%`, not roaming `%APPDATA%`.** In domain/enterprise
  setups roaming `%APPDATA%` is synced to a server on login; we do not want
  gigabytes of novel text roaming. Use the `directories` crate's
  `data_local_dir()`.

- **Store cleaned text / minimal XHTML, not raw HTML.** The raw page is ~50 KB
  of ads and boilerplate around ~15 KB of actual chapter, so extracting at fetch
  time is roughly a 3x space win before anything else. Optional zstd compression
  on the stored text.

- **Schema sketch** (multi-source aware from day one, so fallbacks need no later
  migration):
  - `novels` — the logical book: id, title, author, cover_url, status_hint,
    derived_state (backfilling / live / likely-complete), timestamps.
  - `sources` — id, novel_id, source_name, url, priority (1 = primary),
    last_seen_chapter, last_synced_at. A novel has one or more.
  - `chapters` — novel_id, number, title, body, source_id (provenance which
    source supplied it), fetched_at, exported flag. Keyed by (novel_id, number):
    the primary's content wins; fallbacks only fill missing numbers.

- **Right-sizing the storage fear:** a chapter is ~2,000–4,000 words ≈ 12–25 KB
  plaintext, ~5–8 KB compressed. A 2,500-chapter novel is ~15–25 MB compressed.
  A hundred such novels is a few GB. This is text, not video. The real
  ballooning risk is **images** (illustrated novels) — if image support is ever
  added, that is where a hard cap and aggressive retention earn their keep.

## Fetching and politeness

- **Tiered fetcher behind a trait.** Pick the cheapest tier that works per-site;
  `build_source` selects it by host:
  1. `ReqwestFetcher` — plain `reqwest` with a browser UA + consistent browser
     headers (novgo needs only this).
  2. `CurlFetcher` — shells out to the system `curl` (built into Windows 10+).
     Used for freewebnovel, whose Cloudflare challenges reqwest's TLS ClientHello
     even with browser headers/HTTP-1.1 and even though both use Schannel; curl's
     ClientHello passes. Chosen over pulling in a heavyweight TLS-impersonation
     stack (originally sketched as `rquest`).
  3. Further escalation, if a site ever needs it: `rquest` fingerprint
     impersonation, or a headless browser / FlareSolverr used once to obtain a
     `cf_clearance` cookie handed to a fast client — never a browser per chapter.
     Not currently needed.

- **Adaptive rate control, per host.** Start at a modest delay (~1–2s) with a
  single request in flight per host and jitter on the delay. On 429/503 or a
  `Retry-After` header, back off immediately and honor `Retry-After` exactly,
  then settle back down. One worker per host; parallelism only *across* distinct
  hosts, never within one host.

- **Resume on disk.** Every chapter is persisted as it lands, so a throttle or
  crash costs a pause, not a restart. This also makes it cheap to experiment
  with a faster delay.

- **Hard line:** no proxy-rotation / IP-cycling / ban-evasion machinery. That
  exists to keep hammering a site that is trying to make you stop; against small
  operators it is the harmful case, not personal archival. Adaptive
  fast-with-backoff is both the polite and the optimal strategy — they coincide.

## Completion detection (don't trust the label)

Site status labels are unreliable — a novel is sometimes marked "Completed"
while still receiving updates (and occasionally the reverse). So the label is a
**hint, never ground truth.** The authoritative signal is **observed activity.**

- **Observed new chapters always win.** If the poller sees new chapters appear,
  the novel is ongoing whatever the label says. New chapters immediately set the
  state to *Ongoing* and reset the quiet timer.
- **The label only modulates poll cadence.** A "Completed" label plus a long
  quiet period lets us poll *less often* — it never makes us stop.
- **Never stop polling until the user unsubscribes.** A novel believed complete
  is still polled at a reduced cadence, so a surprise revival (bonus chapters,
  the author returns) is caught. This handles mislabeling in both directions.
- **Derived state**, roughly:
  - *Ongoing* — recent new chapters observed, or labeled ongoing.
  - *Likely complete* — labeled complete AND no new chapters for a long grace
    window. Polled at reduced cadence, not abandoned.
- Where a site exposes no status at all, "no new chapters for 30+ days" means
  *dormant*, not *complete* — a hiatus is not completion; never mark a novel
  finished on silence alone.
- The site's status field (novgo: `og:novel:status`) still feeds this as a hint
  — knowing which value means "completed" is useful, but it is never the sole
  basis for the *Likely complete* state or for purging (see retention).

## Retention (resolves the delete-vs-append collision)

Two desired features fight: "delete raw chapters after export" and "append new
chapters to the existing EPUB later." The clean, robust way to update an EPUB is
to **regenerate it from the DB** (an EPUB is a ZIP with an internal spine/nav;
splicing into it in place is fragile). But if chapters were deleted after
export, regeneration is impossible. Resolution:

- **Ongoing novels keep their chapters in the DB** (the working set). Text is
  cheap, and this keeps appends as simple regenerate-from-DB operations.
- **Retention/deletion applies only to *Likely complete* novels** (see
  Completion detection) — labeled complete AND observably quiet for the grace
  window AND a final export done. It deliberately does **not** fire on the label
  alone, so a mislabeled-but-still-updating novel is never purged: its observed
  activity keeps it *Ongoing*. Once purged, the EPUB is the archive of record.
- **Revival after purge:** if a purged novel later gets new chapters, it reverts
  to *Ongoing* and its working set is re-hydrated (re-fetch the tail from source,
  or read existing chapters back from the EPUB) before appending. The generous
  quiet grace window makes this rare.

- **Safe retention semantics:** delete a raw chapter only after it has been
  successfully exported AND `retention_days` have passed. `0` = purge on export;
  a positive N = keep exported chapters N days as a safety buffer, then purge. An
  **un-exported chapter is never deleted by retention**, regardless of age.

## EPUB export

- **Per-subscription state machine drives automatic export** (replaces a fuzzy
  "majority of chapters exist" threshold):
  - **Backfilling** — initial bulk download in progress. No auto-export, no
    auto-append (don't rebuild the EPUB on every batch).
  - **Caught up / Live** — backfill finished. On the *transition* into this
    state, do one automatic full export. Thereafter each delta of new chapters
    triggers an auto-append.
  - `auto_export` and `auto_append` are separate config toggles.

- **"Append" = regenerate from DB, not ZIP surgery.** Cheap and always correct.

- **Atomic writes.** Write to a temp file, then rename over the target. A crash
  mid-write never corrupts a good existing EPUB.

- **Locked-file safeguard:** if the rename/replace fails with a sharing
  violation (Calibre has it open, an e-reader is mounted, OneDrive is syncing),
  mark that novel's export `pending` and retry next sync cycle. The failed
  replace *is* the lock signal — no separate lock-tracking needed.

- **Output location:** default `C:\Users\<user>\Documents\lightnovels`,
  configurable.
  - **OneDrive caveat:** on Windows 11 `Documents` is often OneDrive-backed, so
    EPUBs there auto-upload. May be desirable (sync to devices) or not
    (bandwidth). Surface it in the generated config as a choice.
  - **Filename sanitization is mandatory:** strip Windows-illegal
    `<>:"/\|?*`, trailing dots/spaces, and reserved names (`CON`, `PRN`,
    `NUL`, …), or exports fail on real titles.
  - **Folder layout: `<library>/<author>/<novel>/<file>.epub`.** The library
    root defaults to `Documents\lightnovels`. Each author is a folder; each of
    their novels is a folder beneath it (an author with several novels gets
    sibling novel folders); the EPUB(s) live inside the novel folder. A missing
    author maps to an `Unknown Author` folder. Every path component is
    sanitized.
  - One EPUB per novel by default: `<novel>.epub` inside the novel folder.
  - **Optional volume-splitting** (`split_every_chapters`): a single
    2,500-chapter EPUB is large and can bog down e-readers. Splitting produces
    `<novel> - Vol 01.epub`, `Vol 02`, ... inside the same novel folder; on
    append only the last, in-progress volume is rewritten.

- **Metadata comes free from `og:novel:*` tags.** We extract title, author, and
  `og:image` (cover URL, stored on the novel) plus the status hint. Genre is not
  captured. **The cover URL is stored but not yet embedded in the EPUB** — no
  cover image is downloaded or added to the package (see Still open).

## Configuration

- **Global `config.ini`** (flat key=value; `rust-ini` or `configparser`)
  generated with defaults. **Per-novel overrides live in the DB** so one novel
  can differ without config sprawl.
- Settings actually generated (see `core::config`): `output_dir`,
  `request_delay_ms`, `poll_interval_minutes`, `retention_days`,
  `quiet_grace_days`, `auto_export`, `auto_append`, `split_every_chapters`.
- Deliberately *not* config keys: `keep_raw_for_ongoing` is implicit (retention
  only ever touches *Likely complete* novels, so ongoing novels always keep their
  working set); `user_agent` is a fixed browser UA in code; `log_path`/verbosity
  await a logging pass (see Observability / Still open).

## Observability

Because the sync runs unnoticed, it must be inspectable. **Partially implemented:**

- `subs` shows each novel's derived state, per-source last-seen chapter, and a
  pending-export flag. Sync/fetch print warnings and state transitions to stderr.
- **Not yet built:** a dedicated `status` command (last sync time, recent errors
  across the whole library) and a persistent log file at `log_path`. A windowless
  scheduled run discards its stderr, so a log file is the main gap for debugging
  background runs. Deferred (see Still open).

## Site profile: novgo.net

Verified by probing during design:

- **Cloudflare is CDN-only, no bot challenge.** Even curl's default User-Agent
  gets `200 OK` with the real page. Tier 1 (plain `reqwest`) is sufficient; no
  fingerprint impersonation or headless browser needed. Still send a normal
  browser UA and stay polite.
- **Fully server-rendered** — nothing loaded by JavaScript; fetch-and-parse
  works.
- **Table of contents is paginated** via `?page=N`
  (e.g. `/<novel>-novel.html?page=2`), ~50 chapters per page. Enumerate the
  whole novel by walking pages.
- **Chapter URLs:** `/<novel-slug>/chapter-<n>-<slug>.html` — the chapter number
  is embedded in the URL, useful for tracking last-seen.
- **Chapter content container:** `div#chapter-content.chapter-c`; strip the
  `div.ads*` blocks inside it.
- **Next-chapter link:** `a#next_chap` — enables walking the delta chain forward
  from the last-seen chapter.
- **Cheap delta check:** the novel's main page lists the latest chapters at the
  top, so "is there anything new?" is a single request comparing the top chapter
  number to last-seen.
- **Metadata via `og:novel:*` meta tags** plus `og:image` cover.
- **Status via `og:novel:status`** (`content="1"` seen for an Ongoing novel;
  confirm the completed value).

## Status of deferred items

### Resolved

- **Second source (freewebnovel).** Validated the `Source` trait as a
  hand-written adapter (AJAX ToC -> discovery generates sequential chapter URLs
  from `data-total-chapters`; title from the chapter page; word-form status) and
  the `Fetcher` trait as a second tier (see Fetching for the curl rationale).
  Shared extractors are reused, so the storage/sync/export pipeline was unchanged.
- **Third source (lightnovelworld).** A second hand-written adapter for a
  JS-rendered ToC. Metadata comes from page elements (`h1.novel-title`,
  `a.author-link`, `.status-badge`), not `og:novel:*`; discovery generates
  sequential `/novel/<slug>/chapter/<n>/` URLs from the count in `og:title`;
  content is `#chapterText`. Runs on Tier 1 (reqwest) — it doesn't
  fingerprint-block, unlike freewebnovel. Its `data-protected` flag is JS
  copy-blocking, not server-side obfuscation; prose is plain `<p>` text with no
  decoys observed (checked several chapters). Reuses `parse_chapter_body`,
  `parse_status_hint`, and `clean_chapter_title`.
- **Fourth source (royalroad).** A third hand-written adapter. Discovery parses
  the `window.chapters` JSON array embedded in the fiction page (serde_json —
  1-request, whole list), since chapter URLs use non-sequential DB ids. Metadata
  from `<title>`/`twitter:creator`/`og:image` and a status `span.label`. The
  notable bit is **decoy-paragraph filtering**: RoyalRoad marks a
  randomized-per-request class `display:none` in a `<style>` block and gives the
  decoy `<p>`s that class, so the adapter collects the hidden classes and skips
  matching paragraphs (unit-tested; live pages sometimes define the class without
  injecting a decoy). Tier 1 (reqwest) — it doesn't fingerprint-block.
- **Fifth source (scribblehub).** A fourth hand-written adapter and the hardest.
  Cloudflare 403s the curl tier unless it sends a *full* browser header set
  (`Sec-Fetch-*`, `sec-ch-ua*`, `Upgrade-Insecure-Requests`) **and** a `Referer`;
  both were added to `CurlFetcher::get` (the `Referer` fix was what finally let
  chapter pages through). The ToC is a WordPress `admin-ajax.php` **POST**
  (`action=wi_getreleases_pagination&pagenum=N&mypostid=<id>`), which forced a new
  `Fetcher::post` method — default-erroring on tiers that don't support it, real
  only on `CurlFetcher` (form-encoded `-X POST --data`). Pages are 15 chapters,
  newest-first (`a.toc_a`); an out-of-range page returns 403, so the adapter
  derives the page count from the total (`span.cnt_toc`) and stops paging at it
  rather than probing past the end. Chapter URLs use non-sequential ids and the
  "Chapter N" labels don't track the count (prologues/interludes/side stories), so
  chapters are numbered by **position**, oldest-first (the newest-first pages are
  reversed). Series page yields `#mypostid` + total; metadata from `og:title`,
  `a[href*="/profile/"]`, `span.rnd_stats`, `og:image` (minus `noimagefound`);
  content `#chp_raw`. This adapter is what exercised the POST path in the Fetcher
  abstraction.
- **External config-driven profiles.** `SiteProfile` holds owned strings and
  exposes `chapter_marker` + `page_param`; generic sites are added via `.ini`
  files in `<config_dir>/profiles/` (required: name, host, content_selector).
  `profiles::all()` merges built-ins with loaded files; bad files skipped with a
  warning; a `README.txt` self-documents; `vesper profiles` lists them.
- **Windowless scheduled task.** The task runs `wscript.exe sync-hidden.vbs`
  (`WScript.Shell.Run "<exe> sync", 0, False`), which hides the console. Keeps
  the no-password "only when logged on" task; uninstall removes the launcher.
- **Fallback content upgrade.** After gap-fill, `sync_novel` re-fetches any
  chapter held from a fallback that the primary now offers, replacing it with the
  primary's content and clearing its exported flag. Fires once after a lagging
  primary catches up; counted as `SyncReport.upgraded`.
- **Storage crate caveat.** Pinned `rusqlite = 0.31` (bundled SQLite) to dodge
  `libsqlite3-sys 0.38.1`'s unstable `cfg_select!` on Rust 1.92; the C toolchain
  itself was never the blocker. Revisit when the crate/toolchain catch up.
- **Status hints mapped**: novgo `og:novel:status` "1"=Ongoing/"2"=Completed and
  freewebnovel word forms; default `poll_interval_minutes` = 60.

### Resolved (wrap-up pass)

- **EPUB cover embedding.** `build_epub` embeds an optional cover; export
  downloads it from `cover_url` best-effort (`download_cover`) and adds it via
  `add_cover_image`. Missing/blocked covers are skipped. Verified live.
- **Observability.** A `status` command (DB/log paths, per-novel state, last
  completed sync, recent log tail) and a log file (config `log_path`) that the
  background sync appends to — its stderr is discarded, so the log is the trail.
  `util::format_unix_utc` gives readable timestamps with no date-crate dep.
- **Reduced poll cadence.** Config `likely_complete_recheck_days` (default 7);
  the all-subscriptions sync skips a LikelyComplete novel re-checked within that
  window. Activity reverts it to Live and resumes normal cadence.
- **auto_append.** Verified live: backfill -> Live (auto_export) -> delete a
  stored chapter -> re-sync fetches it and auto-appends, with no state change.
- **Linux/macOS `ServiceManager`.** systemd-user (`.service` + `.timer`) and
  launchd (plist + `StartInterval`) impls added, and **verified in GitHub Actions**
  (ubuntu + macos runners): they compile and type-check, and the actual
  `service install` / `status` / `uninstall` round-trip succeeds — systemd
  reports `timer is enabled`, launchd reports `loaded: com.vesper.sync`, with the
  unit/plist files written and removed. The unit/plist *content* is also
  unit-tested on every platform.
- **Genre metadata.** Captured from `og:novel:genre` (novgo, freewebnovel),
  stored on the novel, emitted as EPUB `dc:subject`. lightnovelworld leaves it
  `None` (genre is only in its JSON-LD, which we don't parse).

### Decided against (for now)

- **zstd compression of stored text.** A chapter is ~5–8 KB compressed and a
  whole library is a few GB; compression would add a dependency plus read/write
  and migration complexity for marginal gain. Revisit only if image support or
  very large libraries change the calculus.
- **Disambiguating same-site duplicate source names.** Two sources on one site
  show the same profile name in `subs`/progress, but the URL shown beside each
  already disambiguates them and the real use case is cross-site (distinct
  names). Not worth special-casing the contrived same-site scenario.

### Still open

- Tune the hardcoded-ish defaults (`quiet_grace_days`, `likely_complete_recheck_days`)
  if real-world usage suggests better values — all are config-exposed now.

Everything else on the original roadmap is implemented and verified (CI runs the
suite on ubuntu/macos/windows on every non-doc push; see `.github/workflows/`).
