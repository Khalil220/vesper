# Changelog

## 1.2.0

- Support chikari.moe, where Light Novel World's novel library has moved.
- Move existing lightnovelworld subscriptions to chikari automatically, the
  first time Vesper runs after the upgrade. Nothing is re-downloaded and no
  chapter is touched: the subscription is repointed at the new site, and the
  chapters already in your library stay exactly as they are.
- Stop missing chapters happening on this source at all. Vesper now reads
  chikari's real chapter list instead of guessing that a novel runs from 1 to
  N, so it never asks for a chapter number the site doesn't have — the holes
  that used to show up as permanently missing chapters on lightnovelworld.
- Record genres for novels from chikari, so exported books carry them.

### Upgrading

Nothing to do. The first command you run after upgrading moves your Light
Novel World subscriptions across and tells you what it changed.

A novel chikari doesn't carry is left on lightnovelworld, which still works,
and named in that report. If it appears on chikari later:

```
vesper add-source <novel> <chikari url>
```

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
