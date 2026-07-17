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
- **Tiered fetcher behind a trait.** novgo needs only Tier 1 (plain `reqwest` +
  browser UA). Escalate to `rquest` fingerprinting or a headless browser only
  per-site as needed.
- **Adaptive, per-host politeness:** modest delay + jitter, one request in
  flight per host, back off on 429/503, honor `Retry-After`, resume-on-disk.
  Parallelism only across distinct hosts. **No proxy-rotation / ban-evasion.**
- **Completion detection:** prefer the site's status field (novgo:
  `og:novel:status`); fall back to a staleness heuristic. Hiatus != completed.
- **Retention resolves delete-vs-append:** ongoing novels keep chapters in the
  DB (so append = regenerate-from-DB); only completed/dormant novels get purged,
  after final export. Retention **never** deletes an un-exported chapter.
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

TBD once scaffolded.

## Module map

TBD once scaffolded.
