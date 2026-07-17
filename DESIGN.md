# Design

A Rust tool that crawls webnovel sites, downloads chapters, and packages them
into EPUB files. It has two faces sharing one codebase: a CLI for control and
export, and a lightweight background sync that keeps subscribed novels current.

This document records the decisions made and *why*, so they don't get
re-litigated later. CLAUDE.md holds the short rules-of-the-road and points here
for depth.

## Shape of the system

- **One binary, Cargo workspace.** A `core` library crate (fetch, extract,
  store, EPUB) plus a single binary exposing subcommands. The "service" is not a
  separate program — it is the same binary invoked in a sync mode by the OS
  scheduler. This keeps the CLI and the background worker sharing identical code
  instead of drifting apart, and it is one artifact to distribute.

- **The CLI owns:** subscription management (add/remove/list), EPUB export,
  service install/configure/uninstall, and a `status` command. No TUI — plain
  commands and arguments.

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
- **The data model keys by source.** A subscription is (source, novel identity);
  the same novel on two sites is two independent subscriptions. Chapter URLs are
  source-specific. URL-to-source resolution is by host.
- Cross-source features (a fallback/mirror source for the same novel) are out of
  scope for now, but the source-keyed model leaves room for them.

## Background sync: scheduled invocation, not a resident daemon

We run the sync as a **scheduled invocation**, not a long-lived process. The OS
scheduler launches the binary in sync mode every N minutes; it syncs, then
exits. When it is not actively syncing it uses **zero** memory, because nothing
is resident. This directly serves the "as little memory as possible, runs
unnoticed" goal.

A novel poller has no reason to hold state between runs — "last-seen chapter"
lives in the DB, not in RAM — so the main thing a resident daemon would buy
(in-memory state, sub-interval reactivity) is wasted here.

- **Windows (primary target):** Task Scheduler. The CLI's `install` registers a
  hidden task ("run every N minutes / at logon"); `uninstall` removes it.
  Driving this by shelling out to `schtasks.exe` is far less code than
  implementing a real Windows Service.
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

- **Right-sizing the storage fear:** a chapter is ~2,000–4,000 words ≈ 12–25 KB
  plaintext, ~5–8 KB compressed. A 2,500-chapter novel is ~15–25 MB compressed.
  A hundred such novels is a few GB. This is text, not video. The real
  ballooning risk is **images** (illustrated novels) — if image support is ever
  added, that is where a hard cap and aggressive retention earn their keep.

## Fetching and politeness

- **Tiered fetcher behind a trait.** Pick the cheapest tier that works per-site,
  configured per-site:
  1. Plain `reqwest` + a normal browser User-Agent (novgo needs only this).
  2. `rquest` with browser TLS/HTTP2 fingerprint impersonation (for sites that
     fingerprint-block plain clients).
  3. Headless browser (`chromiumoxide`) or FlareSolverr, used once to obtain a
     `cf_clearance` cookie that is then handed to a fast client — never run a
     browser per chapter.

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
  - One EPUB per novel, filename = sanitized title.
  - **Optional volume-splitting** (`split_every_chapters`): a single
    2,500-chapter EPUB is large and can bog down e-readers. Splitting produces
    `Title - Vol 01.epub`, etc.; on append only the last, in-progress volume is
    rewritten.

- **Metadata/cover come free from novgo:** `og:novel:author`,
  `og:novel:genre`, `og:novel:novel_name`, and `og:image` (cover thumbnail).

## Configuration

- **Global `config.ini`** (flat key=value; `rust-ini` or `configparser`)
  generated with defaults. **Per-novel overrides live in the DB** so one novel
  can differ without config sprawl.
- Settings to generate with defaults:
  - `output_dir` (default `Documents\lightnovels`)
  - `auto_export`, `auto_append` (separate toggles)
  - `retention_days` (0 = purge on export; N = grace period; never touches
    un-exported)
  - `keep_raw_for_ongoing` (default true — the working-set rule)
  - `split_every_chapters` (0 = single file)
  - `request_delay_ms` + backoff behavior, and `user_agent`
  - `poll_interval` (mirrors the scheduled-task cadence for reference)
  - `log_path` / verbosity — important precisely because it runs unnoticed; a
    silent daemon with no log is undebuggable

## Observability

Because the sync runs unnoticed, it must be inspectable:

- `status` command: last sync time, per-novel last-seen chapter and state,
  recent errors.
- A log file at `log_path`.

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

## Open items

- Map the `og:novel:status` values on novgo (which means completed) against a
  known finished novel — as a *hint* feeding completion detection, not a trigger.
- Decide the default `poll_interval`, and the reduced cadence + quiet grace
  window for *Likely complete* novels.
- Decide whether to zstd-compress stored chapter text from day one or later.
- Design the generic site-profile format (selectors, pagination pattern, tier,
  rate limits) and the second source to validate the abstraction against novgo.
