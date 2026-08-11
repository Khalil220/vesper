# Changelog

## 1.2.0

- Light Novel World's novel library has moved to chikari.moe, and Vesper has
  moved with it. The novels you already follow are pointed at the new site the
  first time Vesper runs after the upgrade. Nothing is re-downloaded and no
  chapter is touched — the chapters already in your library stay exactly as
  they are.

### Upgrading

Nothing to do. The first command you run after upgrading moves your Light
Novel World subscriptions across and tells you what it changed.

Light Novel World is shutting down, so a novel that didn't make the move is a
dead end rather than a fallback. Vesper names those in the report; export what
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
