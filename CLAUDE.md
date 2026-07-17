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
  recompile. Hand-written adapters only for weird sites. Data model keys by
  (source, novel); URL-to-source resolves by host. novgo is the first source,
  not the only one.
- **Tiered fetcher behind a trait.** novgo needs only Tier 1 (plain `reqwest` +
  browser UA). Escalate to `rquest` fingerprinting or a headless browser only
  per-site as needed.
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
- Config: global flat `config.ini` (defaults) + per-novel overrides in the DB.

## novgo.net quick reference

- Cloudflare CDN-only, no challenge. Server-rendered.
- ToC paginated `?page=N` (~50/page). Chapter URLs
  `/<slug>/chapter-<n>-<slug>.html`.
- Content: `div#chapter-content.chapter-c` (strip `div.ads*`). Next chapter:
  `a#next_chap`. Metadata/cover: `og:novel:*` + `og:image`.

## Build / Test / Run

From the repo root:

- Build: `cargo build`
- Test: `cargo test` (unit tests live inline in `core`'s modules)
- Run: `cargo run -p crawler -- export <novel-url> [--limit N] [--out PATH] [--delay-ms MS]`
  (`--limit 0` = all chapters; default delay 1500ms)
- Built binary: `target/debug/crawler.exe`
- Live smoke test: export a few chapters from a novgo novel and validate the
  EPUB (unzip; check `mimetype` == `application/epub+zip`, `content.opf`, and
  that chapter XHTML holds real prose). `cargo test` does NOT rebuild the
  binary — run `cargo build` before invoking `target/debug/crawler.exe`.

## Module map

Cargo workspace, two crates under `crates/`:

- `core` (lib `crawler-core`):
  - `fetch` — `Fetcher` trait + Tier-1 `ReqwestFetcher` (fixed politeness delay).
  - `source` — `Source` trait, declarative `SiteProfile`, `GenericSource`
    adapter, and the HTML extraction (novel metadata, chapter links, chapter
    body). Keep parsing synchronous so the non-`Send` `scraper::Html` never
    crosses an `.await`.
  - `profiles` — built-in `SiteProfile`s (novgo).
  - `model` — domain types (`NovelMeta`, `ChapterRef`, `Chapter`, `NovelStatus`).
  - `epub` — EPUB packaging (reconstructed XHTML; atomic temp-file+rename).
  - `util` — filename sanitization, chapter number/title parsing.
- `cli` (bin `crawler`): clap subcommands. Currently just `export`, which drives
  fetch -> discover -> extract -> package end to end.

Not yet built (see DESIGN.md): SQLite persistence, subscriptions, the sync
command + Task Scheduler install, retention, and the auto-export state machine.
