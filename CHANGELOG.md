# Changelog

## 1.1.0

- Fix freewebnovel chapter titles being dropped. The page `<title>` separates
  the chapter name from "Chapter N" with the same `|` that precedes the site
  name, so splitting on the first `|` threw the name away and every chapter
  was stored as a bare "Chapter N" — which the EPUB then rendered as
  "Chapter 1: Chapter 1". The site name is now trimmed off the end instead.
  Chapters whose name happened to follow a space or dash were unaffected,
  which is why the occasional one looked right.
- `vesper refresh <novel> --titles` re-reads stored chapters' titles from the
  source and overwrites the ones that differ, to repair libraries filled
  before the fix. Sync inserts are `OR IGNORE`, so a re-sync can't do this.
- `vesper subs` and `vesper status` list novels in id order, so the printed
  `#N` labels run in sequence instead of being sorted by title.
- Novel ids are never reused. `novels.id` was a plain rowid, so while a
  mid-range unsubscribe left a permanent gap, deleting the *highest*-numbered
  novel handed that id to the next subscription — and ids are what you type
  (`vesper export 14`), so a recycled one silently retargets a command.
  `novels.id` is now `AUTOINCREMENT`; existing libraries are migrated in place
  on first run, preserving every id.

## 1.0.0

Initial release: subscriptions with ranked multi-source fallback, polite
tiered fetching, EPUB export with covers and volume splitting, scheduled
background sync on Windows/Linux/macOS, and five supported sites (novgo,
freewebnovel, lightnovelworld, royalroad, scribblehub).
