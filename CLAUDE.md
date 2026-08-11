# CLAUDE.md

**Vesper** — a Rust tool that crawls webnovel sites, downloads chapters, and
packages them into EPUBs. One binary: a CLI plus the same binary run as a
scheduled background sync. `DESIGN.md` has the architecture and rationale;
`README.md` has the user-facing command reference. This file is the short
rules-of-the-road.

## Load-bearing constraints (don't re-litigate — see DESIGN.md for why)

- **One binary, Cargo workspace:** a `core` lib crate plus a single binary with
  subcommands. The background "service" is the same binary in a sync mode, not a
  separate program.
- **Background sync = scheduled invocation, not a resident daemon.** Zero
  memory when idle. Service management sits behind a per-OS trait (Task
  Scheduler / systemd user timer / launchd, all per-user, no elevation). A
  process lock per run prevents double-fire.
- **SQLite (WAL) is the single source of truth**, in `%LOCALAPPDATA%` (via
  `directories::data_local_dir`) — **never roaming `%APPDATA%`**. Store cleaned
  text/XHTML, not raw HTML.
- **Multi-source by design.** A `Source` trait abstracts each site; a generic
  config-driven adapter handles the common server-rendered + CSS + `?page=N`
  case, so such sites (novgo included) are declarative profiles, no recompile.
  Hand-written adapters only for weird sites. URL-to-source resolves by host.
- **One logical novel, multiple ranked sources** (not per-source
  subscriptions). A novel has a primary source plus optional fallbacks — one
  novel => one EPUB. `subscribe` blocks when the title matches an existing
  novel under a *normalized* comparison (case/spacing/punctuation-insensitive)
  and points at `add-source`; `--force` overrides. **Active fallback:** sync
  gap-fills chapters the primary lacks from fallbacks by priority; primary is
  authoritative for content. Schema: `novels` / `sources` / `chapters`.
- **Novel ids are permanent handles, never reused.** They're what the user
  types (`vesper export 14`) and what the sync log records, so a recycled id
  would silently retarget a command at a different novel. `novels.id` is
  `AUTOINCREMENT` for exactly that reason — a plain rowid is `max(id)+1` over
  *surviving* rows, which hands the highest id to the next subscription once
  it's deleted. Unsubscribing leaves a permanent hole in the numbering; that's
  intended, not something to compact.
- **Tiered fetcher behind a trait.** Tier 1 = `ReqwestFetcher` (browser UA +
  headers). Tier 2 = `CurlFetcher` (shells out to system `curl`; GET and form
  POST), for hosts where Tier 1 gets challenged (freewebnovel, scribblehub).
  `build_source` picks tier + adapter per host. Escalate further only if a
  site needs it.
- **Adaptive, per-host politeness:** modest delay + jitter, one request in
  flight per host, back off on 429/503, honor `Retry-After`, resume-on-disk.
  Parallelism only across distinct hosts. **No proxy-rotation / ban-evasion.**
- **Completion detection — don't trust the label.** The site status field is a
  *hint*; observed activity is authoritative. New chapters observed => Ongoing,
  overriding any "completed" label. The label only lowers poll cadence; never
  stop polling until unsubscribed. Hiatus != completed.
- **404 gaps don't wedge completion.** A `chapter_gaps` row means the
  **primary** source returned a permanent 404 (`fetch::NotFound`, distinct from
  transient 5xx/timeout) for that number. It counts as accounted for (`target ⊆
  have ∪ gaps`) so backfill can finish instead of hammering a dead URL forever.
  The gap is recorded **even when a fallback fills the chapter** — that's what
  stops the content-upgrade pass from re-fetching the dead primary URL every
  sync. User-facing surfaces show only **unfilled** gaps
  (`store::unfilled_gaps`): `subs`/`status` list them, and the EPUB gets a
  front "Missing Chapters" page. Re-probing a known gap is silent. Only
  *URL-generating* adapters (freewebnovel) produce these: an adapter that reads
  the site's real chapter list (chikari, royalroad, scribblehub) never asks for
  a number the site doesn't have, so it yields no gaps at all. Prefer reading a
  list over generating a range whenever the site offers one.
- **A stored chapter is never revisited, so a bad one needs an explicit
  repair.** `insert_chapter_if_absent` is `OR IGNORE`: once a chapter is
  stored, no amount of syncing will correct it. Hence
  `store::update_chapter_title` for titles and `core::repair` (`vesper repair`)
  for bodies. The case that motivates the latter: a gated site serves a short
  "log in to keep reading" placeholder with an ordinary 200, so the fetch
  succeeds and the placeholder becomes the chapter's text. **Detect those on
  the placeholder's wording, never on length alone** — a real between-arcs
  author's note runs a few hundred characters, and length-based detection would
  silently destroy it (there is a test pinning exactly that). Validate the
  replacement too: refuse incoming text that is itself a placeholder, or that
  is no longer than what is stored, so repairing against a still-gated source —
  or re-running it while a site is having a bad day — can't overwrite good
  prose.
- **Promotion re-attributes, and is never automatic on failure.** Making a
  fallback primary (`store::promote_source`, `vesper set-primary`) renumbers
  priorities *and* re-attributes the old primary's chapters to the promoted
  source. Skipping that re-attribution makes every stored chapter an upgrade
  candidate (`chapters_from_other_sources`), so the next sync re-downloads the
  whole back catalogue — the same storm the in-place site move avoids. Sync
  does **not** promote on its own: at one pass a transient outage is
  indistinguishable from a site being dropped, and flipping content authority
  and back costs a full re-download each way. A dead primary already costs
  nothing but a warning, because `sync::discover` skips a failed source and the
  fallbacks still fetch. The migration is the one place promotion happens
  automatically, because the site *said* which novels it dropped.
- **Sites move; subscriptions follow them in place.** When a site relocates
  (lightnovelworld -> chikari.moe), the fix is a one-shot migration that
  **repoints the existing `sources` row** (`store::repoint_source`), not a new
  source row. Keeping the same `sources.id` keeps every stored chapter
  attributed to it, so nothing is re-downloaded and priorities are untouched;
  adding a new primary instead makes the content-upgrade pass re-fetch the
  whole back catalogue for identical prose. A move is only applied once the new
  site *confirms* the novel (slug, else exact normalized-title search) — a slug
  that resolves to a different novel is refused, and an unreachable site is
  never read as "not there": the marker stays unset and it retries next launch.
  Migrations live in `core::migrate`, are guarded by a key in the `meta` table,
  and run from `main()` before the command — but only for commands that touch
  the library. **Repointing is only safe because the two sites share a chapter
  number-space**; verify that with `examples/live_migration` before assuming it
  for the next move.
- **Retention resolves delete-vs-append:** ongoing novels keep chapters in the
  DB (append = regenerate-from-DB); only *Likely complete* novels (labeled
  complete AND quiet for the grace window AND exported) get purged. Never on
  the label alone; never an un-exported chapter. Revival re-hydrates.
- **Auto-export via a Backfilling -> Live state machine**, not a chapter-count
  heuristic. `auto_export` and `auto_append` are separate toggles.
- **EPUB writes are atomic** (temp file + rename). A sharing-violation on
  replace means the file is locked -> mark export `pending`, retry next cycle.
- **Filename sanitization is mandatory** on Windows (strip `<>:"/\|?*`,
  trailing dots/spaces, reserved names).
- **Output layout: `<library>/<author>/<novel>/<novel>.epub`** (library
  defaults to `Documents/lightnovels`); volumes are `<novel> - Vol NN.epub` in
  the novel folder. See `core::paths`.
- Config: global flat `config.ini` (defaults) + per-novel overrides in the DB.

## Site quick reference

- **novgo.net** (generic profile): Cloudflare CDN-only, no challenge, Tier 1.
  Server-rendered; ToC paginated `?page=N` (~50/page); chapter URLs
  `/<slug>/chapter-<n>-<slug>.html`; content `div#chapter-content.chapter-c`
  (strip `div.ads*`); metadata/cover `og:novel:*` + `og:image`; status "1"/"2".
- **freewebnovel.com** (hand-written adapter, Tier 2 curl): AJAX/JS ToC (no
  scrapable pagination), so discovery reads `data-total-chapters` and generates
  sequential `/novel/<slug>/chapter-<n>` URLs from one request; chapter title
  from the chapter page `<title>`, which reads
  "Novel - Chapter N | Name | Free Web Novel" — the chapter name is usually
  pipe-separated, the **same** character as the site-name suffix, so strip the
  branding off the *end*; splitting on the first `|` eats the name and leaves a
  bare "Chapter N". Content `.txt`; metadata `og:novel:*`; status word form
  ("Completed"/"Ongoing").
- **chikari.moe** (hand-written adapter, Tier 1): where lightnovelworld's novel
  library moved. A SvelteKit app whose chapter pages are **client-rendered**
  (the HTML for `/novels/<slug>/<n>` is an empty app shell), so there is nothing
  to scrape — but it publishes the JSON API its own front end calls, described
  at `/api/openapi.json`. Read that, not the HTML. `GET /api/novels/<slug>` =
  metadata (`title`, `authors[]` — prefer `role == "author"`, `cover_url`,
  `status`, `genres[]`); `GET /api/novels/<slug>/chapters?order=asc&limit=500
  &offset=N` = the real ToC (`limit` is **server-clamped to 500**, so page it);
  `GET /api/novels/<slug>/chapters/<n>/read` = one chapter, `body` being plain
  text with newline-separated paragraphs. **Discovery reads the list, never
  generates `1..=N`** — the numbering has holes (`latest_number` runs ahead of
  `stored_chapter_count` for about half the catalogue) that 404 on read, but the
  listing omits them, so this source yields **no 404 gaps at all**. Bodies carry
  literal inline markup (`<em>`, `<br>`, ...) that must be stripped, since EPUB
  paragraphs are XML-escaped; a stray `<` in dialogue is prose and must survive.
  Status words: `releasing` / `completed` / `hiatus` / `cancelled` / `dropped`.
  A `locked` chapter is early access — a *retryable* error, not a 404 gap.
  Chapter `number` is typed as a float; a non-integral one is skipped, never
  rounded into a neighbour's slot. Note a chapter's *displayed* label often
  differs from its canonical number ("Chapter 1176" at number 1200) — that also
  held on lightnovelworld, which is part of why the number-spaces line up.
- **lightnovelworld.org** (hand-written adapter, Tier 1) — **superseded by
  chikari.moe and shutting down**. It now 302s to `/site-notice/` (a merge
  announcement), so the adapter fails: Tier 1 follows the redirect and reports
  "could not read the chapter count" because the notice has no `og:title`
  count; the curl tier, which doesn't follow redirects, fails on the 302
  itself. Don't try to fix that — the site is frozen (its notice says updates
  are paused and it stays up only days), so chasing the redirect buys nothing.
  The adapter stays only so a lingering subscription resolves to something
  that fails with a real message. **It is not a usable fallback**, and per the
  notice the long tail of low-traffic novels was dropped rather than migrated,
  so those are not coming to chikari either.
  Its historical shape, kept because the migration reasons about it: JS-rendered
  ToC, so
  discovery reads the total from `og:title` ("… - N Chapters") and generates
  sequential `/novel/<slug>/chapter/<n>/` URLs. Metadata from page elements
  (`h1.novel-title`, `p.novel-author` — text after the "Author:" label, so
  authors without a profile link still parse, `.status-badge`) + `og:image` —
  NOT `og:novel:*`. Content `#chapterText`; `data-protected` is JS
  copy-blocking only (prose is plain `<p>` text, no decoys observed). **Its
  `/chapter/N/` id sequence has 404 holes** (deleted/merged chapters), so the
  generated 1..N range hits dead URLs — handled by the 404-gap logic above,
  not a bug.
- **royalroad.com** (hand-written adapter, Tier 1): whole chapter list is a
  `window.chapters = [...]` JSON array in the fiction page (serde_json) —
  1-request discovery, but chapter URLs use non-sequential DB ids so the list
  must be read, not generated. Metadata: `<title>` (minus "| Royal Road"),
  `twitter:creator`, `og:image`, status from a `span.label`. Content
  `.chapter-inner`; **decoy paragraphs** are filtered — a `<style>` marks a
  randomized class `display:none` and decoy `<p>`s use it, so collect those
  classes and skip matching paragraphs.
- **scribblehub.com** (hand-written adapter, Tier 2 curl): the hardest source.
  403s without a full browser header set *and* a `Referer` (the curl tier
  sends both). ToC is a WordPress `admin-ajax.php` **POST**
  (`action=wi_getreleases_pagination&pagenum=N&mypostid=<id>`), 15/page,
  newest-first, `a.toc_a` links — an out-of-range page returns 403, so page
  count is derived from the total (`span.cnt_toc`) and paging stops at it.
  Chapter URLs use non-sequential ids and the "Chapter N" labels don't match
  the count, so chapters are numbered by **position**, oldest-first. Series
  page gives `#mypostid` + total; metadata `og:title` /
  `a[href*="/profile/"]` / `span.rnd_stats` + `og:image` (minus
  `noimagefound`); content `#chp_raw`.

## Build / Test / Run

- Build: `cargo build`. Test: `cargo test` (unit tests live inline in `core`'s
  modules). `cargo test` does NOT rebuild the binary — run `cargo build` before
  invoking `target/debug/vesper.exe`.
- Full command reference: `README.md`. `<novel>` args accept an id or a title.
- DB + `sync.lock` live at `%LOCALAPPDATA%/vesper/data/`.
- Live smoke test: export a few chapters from a novgo novel and validate the
  EPUB (unzip; check `mimetype` == `application/epub+zip`, `content.opf`, and
  that chapter XHTML holds real prose).
- Adapter parsing is only really provable against the live site — unit tests
  fix in whatever shape the fixture was written with, which is exactly how a
  wrong title parse survives. `cargo run -p vesper-core --example live_titles
  -- <novel-url> <n>...` fetches those chapters through the real adapter and
  fetch tier and prints the parsed titles. Network-bound, so it stays out of
  `cargo test`. Sample chapters from across a novel's range: freewebnovel
  formats its `<title>` inconsistently, so checking only chapter 1 proves
  little.
- Site *moves* need the same treatment on a real library: `cargo run -p
  vesper-core --example live_migration -- <path-to-library.db> [--apply]`
  resolves each stale subscription on the new site and compares stored chapter
  text against the same numbers there, so a numbering mismatch surfaces before
  anything is written. Preview-only without `--apply`, and it refuses to apply
  when a sample disagrees. **Point it at a copy of a library, never the live
  one** — `%LOCALAPPDATA%/vesper/data/library.db`.

## Releasing

- **Versioning is minor-only:** 1.0.0 -> 1.1.0 -> 1.2.0. Never bump the patch
  component; it stays 0.
- The version lives once, in `[workspace.package]` in the root `Cargo.toml`
  (both crates inherit it). To release: bump it, run `cargo update -w` to
  refresh the lockfile, add a `CHANGELOG.md` entry, commit, then tag `vX.Y.0`
  and push the tag — `.github/workflows/release.yml` builds Windows/Linux/
  macOS binaries and attaches them to the GitHub release.
- Release notes are the `CHANGELOG.md` section under the `## X.Y.0` heading
  matching the tag (no `v` prefix, heading exact — the workflow extracts it
  verbatim). A tag without a matching entry fails the release job on purpose.

## Module map

Cargo workspace, two crates under `crates/`:

- `core` (lib `vesper-core`):
  - `fetch` — `Fetcher` trait; Tier-1 `ReqwestFetcher` (adaptive per-host
    backoff, jitter, bounded retries) and Tier-2 `CurlFetcher` (GET with full
    browser headers + `Referer`; form-encoded POST). Both return a typed
    `NotFound` on 404/410 (`is_not_found` is chain-aware) so sync can tell a
    permanent hole from a transient failure.
  - `source` — `Source` trait, declarative `SiteProfile`, `GenericSource`
    adapter, shared HTML extraction reused by hand-written adapters. Keep
    parsing synchronous so the non-`Send` `scraper::Html` never crosses an
    `.await`.
  - `chikari` / `freewebnovel` / `lightnovelworld` / `royalroad` /
    `scribblehub` — hand-written adapters (site details above). `chikari` reads
    a JSON API rather than HTML, so it uses `serde_json` and no `scraper`.
  - `migrate` — one-shot library migrations, guarded by a `meta` key.
    Currently `migrate_lightnovelworld` (see the site-move invariant above).
    Generic over `Fetcher`, which is the seam the tests feed canned JSON
    through; `resolve_on_chikari` is public so `examples/live_migration` can
    preview a real library without writing to it.
  - `repair` — re-fetch chapters stored as a site's gating placeholder (see the
    invariant above). `looks_like_gate_stub` and the replacement check are pure
    and unit-tested, including the short-author's-note case that length-based
    detection would eat; `repair_novel` drives them over a novel's sources.
  - `profiles` — built-in profiles plus user `.ini` files from
    `<config_dir>/profiles/` (bad files skipped with a warning).
    `crate::build_source` (lib.rs) resolves a URL to adapter + fetch tier.
  - `model` — domain types (`NovelMeta`, `ChapterRef`, `Chapter`,
    `NovelStatus`).
  - `epub` — EPUB packaging: reconstructed XHTML, atomic temp-file+rename,
    cover + `dc:subject` genre, "Missing Chapters" page for 404 gaps.
  - `paths` — library layout (`epub_path`, `novel_dir`).
  - `store` — SQLite persistence: novels/sources/chapters/chapter_gaps/meta
    schema, WAL, resume-aware insert, gap record/clear/list, normalized-title
    lookup, subscriptions listed in id order. `update_chapter_title` is the
    repair hatch for chapters stored while an adapter parsed titles wrong —
    inserts are `OR IGNORE`, so a re-sync alone can never fix them;
    `update_chapter_content` is the body equivalent, used by both the
    content-upgrade pass and `repair`. `repoint_source` (site moved) and
    `promote_source` (fallback takes over) both keep stored chapters
    attributed to a live source — see their invariants above.
    `chapters_shorter_than` narrows the placeholder scan. `meta` is the
    key/value table one-shot migrations mark themselves done in. `migrate()` is
    additive and idempotent; the one destructive step is the `novels` rebuild
    that adds `AUTOINCREMENT`, which runs with `foreign_keys=OFF` (`DROP
    TABLE` otherwise cascades every chapter away), refuses to drop the
    original unless the copied row count matches, and checks
    `foreign_key_check` before committing. rusqlite pinned to 0.31
    (cfg_select workaround). Connection is not `Send`, so the CLI uses a
    current-thread runtime.
  - `config` — flat self-generating `config.ini`; tolerant, escape-disabled
    reads so Windows `C:\` paths round-trip.
  - `sync` — `sync_novel`, the shared multi-source engine behind both `fetch`
    and `sync`. Backfilling walks the full ToC; a Live novel does a cheap delta
    check with full-walk fallback. Gap-fills from fallbacks, upgrades
    fallback-sourced chapters once the primary catches up, drives
    Backfilling->Live, returns a `SyncReport` (`.gaps` = unfilled only).
    Progress goes through a `SyncProgress` callback returning `ControlFlow`;
    `Break` stops cleanly after the current committed chapter and sets
    `SyncReport.interrupted` (how Ctrl+C becomes a resumable pause; an
    interrupted backfill does not transition to Live).
  - `util` — filename sanitization, chapter number/title parsing, `now_unix`.
- `cli` (bin `vesper`): clap subcommands on a current-thread Tokio runtime.
  `sync` takes a single-instance advisory file lock (`fs2`) and appends to a
  log file. The in-place `n/m` progress line draws only when stderr is a
  terminal (`VESPER_FORCE_PROGRESS` forces it). `fetch` watches
  `tokio::signal::ctrl_c` and flips the flag that makes the progress callback
  return `Break`.
  - `cli::service` — `ServiceManager` trait, one impl per OS (`schtasks` /
    `systemctl --user` / `launchctl`). Generated unit/plist content is pure and
    unit-tested everywhere; only the scheduler glue is OS-gated. On Windows the
    task runs through a generated `wscript.exe` VBS wrapper so it's windowless;
    the VBS is removed on uninstall.
