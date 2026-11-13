# Changelog

## 1.5.0

- Add `vesper update`, which downloads the latest release and replaces the
  copy you ran with it.
- Sync checks once a day for a newer version, and `vesper status` says when
  one is out. `check_for_updates = false` in the config turns this off.
- Add `vesper unsub`, short for `vesper unsubscribe`.
- Strip other sites' adverts too, not only freewebnovel's, including the ones
  in chikari.moe's chapters and a leak group's Telegram lines. Chapters from
  chikari.moe are cleaned as they download.
- `vesper service install` and `vesper service status` warn on Windows when
  SmartScreen will block the background sync, and say how to unblock
  `vesper.exe`.
- Fix Royal Road failing with "403 Forbidden".
- Fix chikari.moe chapters showing escape codes such as `2/5 -\u003e 3/5`.
- Fix words running together where a site put a line break inside a
  paragraph, as in Royal Road's "Chapter 001Good Morning Brother".
- Fix a finished novel being downloaded again in full each time it was
  rechecked after `prune` deleted its chapters.
- Fix a pruned novel's EPUB being overwritten by a partial copy when
  downloading its chapters again stopped early.
- `vesper refresh` picks up a title the site changed, and moves the EPUB
  when the title or author changes.
- Help and output say each thing once. `sync` prints only the novels that
  had something happen, plus a one-line summary. `status` shows what needs
  attention and how the last sync went instead of listing every novel again.
  Output uses readable wording instead of technical labels.
- Remove the `help` subcommand in favor of `-h`.

### Upgrading

Download this release by hand; `vesper update` handles the ones after it.

To clear other sites' adverts out of chapters you already have:

```
vesper scrub all
```

To fix chikari.moe chapters that show escape codes:

```
vesper refetch <novel>
```

## 1.4.0

- Chapters from freewebnovel no longer carry the adverts the site drops into
  the prose, lines like "Enjoy more content from freewebnovel". Run
  `vesper scrub all` to clear them out of chapters you already downloaded.
- `vesper fetch` now takes a URL. A novel you don't follow is downloaded and
  saved as an EPUB without joining your library, so a finished novel no longer
  needs subscribing to and unsubscribing from to get one file.
- `vesper sub` is short for `vesper subscribe`.
- `vesper service` no longer lists a help subcommand beside its own -h and
  --help.

## 1.3.0

- Add `vesper refetch <novel|all>`, which re-downloads chapters you already
  have and replaces their text. A normal sync never revisits a stored chapter,
  so this is the way to pick up a correction the site made after you
  downloaded it. `--chapters 152-154` limits it to a range.
- Add `vesper repair <novel|all>`, which re-downloads chapters that were saved
  as a site's "log in to read" notice instead of the chapter. Those download
  successfully, so nothing catches them at the time.
- Fix chapters from chikari.moe running words together where the original had
  italics, such as "have a[+1]next to it". Run `vesper refetch` on a novel to
  correct chapters you already have.
- Stop a sync from undoing a repair. A chapter repaired from a fallback was
  re-downloaded from the main site on the next sync, which returned the same
  placeholder and overwrote the real text.

## 1.2.0

- Light Novel World's novel library has moved to chikari.moe, and Vesper has
  moved with it. The novels you already follow are pointed at the new site the
  first time Vesper runs after the upgrade. Nothing is re-downloaded and no
  chapter is touched — the chapters already in your library stay exactly as
  they are.
- Add `vesper set-primary <novel> <source>`, which promotes one of a novel's
  sources to be its main one. Useful when a site shuts down and the novel has
  a fallback that can take over. Chapters you already have are kept and
  nothing is re-fetched.

### Upgrading

Nothing to do. The first command you run after upgrading moves your Light
Novel World subscriptions across and tells you what it changed.

Light Novel World is shutting down, so a novel that didn't make the move is a
dead end. If such a novel has another site attached, Vesper hands it over to
that one automatically and tells you so — again without re-downloading
anything. If it has nowhere to go, Vesper names it, and you should export what
you already have while you can:

```
vesper export <novel>
```

Its own notice says the long tail of low-traffic titles was dropped
deliberately and won't be appearing on chikari later.

If the site can't be reached when you upgrade, nothing is changed and Vesper
tries again the next time it runs.

## 1.1.0

- Fix most freewebnovel chapters losing their title. Affected chapters were
  saved as a bare "Chapter N" and exported as "Chapter 1: Chapter 1"; a
  minority, whose titles the site formats differently, came through correctly
  all along.
- Add `refresh <novel> --titles`, which re-reads the titles of chapters
  already downloaded and corrects the ones that are wrong. Costs one request
  per stored chapter, so it is opt-in.
- List novels by id in `subs` and `status`, so the `#N` labels run in
  sequence rather than in title order.
- Stop reusing novel ids. Unsubscribing the highest-numbered novel used to
  hand its id to the next subscription, silently repointing any command that
  named it. Ids are now permanent, and unsubscribing leaves a gap in the
  numbering.

### Upgrading

Existing libraries are migrated automatically on first run; no action needed
and no chapters are touched.

Chapter titles already stored are not corrected by a normal sync, which never
revisits a chapter it has. To repair a novel downloaded from freewebnovel,
re-read its titles and rebuild the book:

```
vesper refresh <novel> --titles
vesper export <novel>
```

## 1.0.0

Initial release: subscriptions with ranked multi-source fallback, polite
tiered fetching, EPUB export with covers and volume splitting, scheduled
background sync on Windows/Linux/macOS, and five supported sites (novgo,
freewebnovel, lightnovelworld, royalroad, scribblehub).
