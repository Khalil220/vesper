# Vesper

Vesper downloads webnovels and turns them into EPUBs. You give it a novel's
URL, it tracks the story, fetches chapters at a polite pace, and keeps an
EPUB in your library up to date — on demand, or on a schedule in the
background if you set it up that way.

It currently understands novgo.net, chikari.moe, freewebnovel.com,
royalroad.com and scribblehub.com. A nice side effect of supporting several
sites: if a novel exists on more than one of them, you can attach the extra
sites as fallbacks. Chapters that are missing or dead on the main site get
quietly filled in from the others, and you still end up with one EPUB, not
three copies of the same book.

Light Novel World's novel library has moved to chikari.moe, and that site is
shutting down. If you followed novels there, Vesper points those
subscriptions at chikari by itself the first time you run it after upgrading;
nothing is re-downloaded and no chapter is touched.

A few novels didn't make the move — Light Novel World's own notice says the
long tail of low-traffic titles was dropped rather than migrated. Vesper names
those instead of pretending they still work, and everything you already
downloaded of them stays in your library and exports as usual.

Everything you download is cached in a local SQLite database. Exporting,
re-exporting, splitting into volumes — none of that ever hits the network
again.

## Quick start

```
vesper subscribe https://www.royalroad.com/fiction/12345/some-novel
vesper fetch "Some Novel"
vesper export "Some Novel"
```

That's the whole loop. You'll find the result at
`Documents\lightnovels\<Author>\Some Novel\Some Novel.epub`.

If you'd rather not do this by hand every time a chapter drops, turn on
`auto_export` (and `auto_append`) in the config, then:

```
vesper service install
```

This registers a per-user scheduled task — Task Scheduler on Windows, a
systemd user timer on Linux, launchd on macOS — that runs `vesper sync`
every hour by default. There's no daemon sitting in the background eating
memory; between runs, nothing is running at all. On Windows the task is
windowless, so you won't get a console flashing at you every hour.

## Commands

A note before the list: wherever a command takes `<novel>`, you can pass
either the numeric id or the title. `vesper subs` shows both, so you never
have to guess.

### subscribe

```
vesper subscribe <url> [--force]
```

Start following a novel, with the page you gave as its main source. This
registers the novel and grabs its metadata (title, author, cover, status) —
it doesn't download any chapters yet; that's what `fetch` is for.

One thing to know: if the title matches a novel you already follow, Vesper
refuses and tells you to use `add-source` instead. The comparison is
deliberately loose — case, spacing and punctuation are ignored — because two
sites will happily format the same title three different ways, and the
whole point is to avoid ending up with duplicate novels. If it's genuinely
a different story that happens to share a name, `--force` gets you past the
check.

### add-source

```
vesper add-source <novel> <url>
```

Attach another site's copy of the novel as a fallback. Fallbacks are ranked
below the main source: they're only used to fill chapters the main site
doesn't have, and if the main site later provides a chapter that a fallback
filled in earlier, the main site's version wins. You'll get a warning if the
titles don't match, but it goes through anyway — you presumably know what
you're doing.

If the novel had permanently missing chapters (see `subs --gaps` below),
adding a source re-opens the backfill so the new site gets a chance to
provide them.

### set-primary

```
vesper set-primary <novel> <source>
```

Promote one of a novel's sources to be the main one, demoting the current
main source to a fallback. This is what you want when a site shuts down and
the novel has somewhere else to go.

Name the source however it appears in `vesper subs` — either its site name
(`freewebnovel`) or its URL. If you have the same site attached twice, the
name is ambiguous and you'll be asked for the URL instead.

Chapters you already downloaded are kept and re-attributed to the new main
source, so nothing is re-fetched. Vesper does not reorder sources on its own
when a site starts failing: a site being down for an hour looks exactly like
a site being gone, and guessing wrong would mean re-downloading a whole novel
twice. The one exception is the Light Novel World shutdown, where the site
told us outright which novels were dropped.

### subs

```
vesper subs [--gaps]
```

List everything you follow: each novel, its main source, its fallbacks, and
any chapters that are permanently missing. "Permanently missing" means the
main site returns a hard 404 for that chapter number and no fallback has it
either — some sites have holes in their chapter numbering where chapters were
deleted or merged, and there's simply nothing there to download. (chikari is
the exception: it publishes its real chapter list, so Vesper never asks it for
a chapter that isn't there.) `--gaps` filters the list down to novels
that have such holes, which is handy for deciding where an extra source
would actually help.

### fetch

```
vesper fetch <novel> [--limit N]
```

Download missing chapters into the database. It always resumes from
wherever it left off, so interrupting it costs you nothing. `--limit`
caps how many chapters this run downloads (`--limit 0` means everything
that's missing, which is also the default behavior).

Ctrl+C is handled gracefully: the chapter currently downloading is finished
and saved, then the command stops and tells you how to resume. If you press
Ctrl+C a second time it aborts immediately instead. Progress is shown as a
single updating `n/m` counter — only when you're actually at a terminal;
redirected output stays clean. If you want the counter anyway, set the
`VESPER_FORCE_PROGRESS` environment variable.

### export

```
vesper export <novel> [--out PATH]
```

Build the EPUB from cached chapters, cover image and genre included. The
write is atomic — the file is assembled elsewhere and swapped in — so a
crash mid-export can't leave you with a corrupt half-book. If the target
file is locked because something else has it open (Calibre, an e-reader
still plugged in, OneDrive doing its thing), the export is marked pending
and retried on the next sync instead of failing loudly.

If the novel has permanently missing chapters, the EPUB opens with a page
listing them, so future-you knows the gap was the site's fault and not a
broken download.

### sync

```
vesper sync [--limit N]
```

Update every subscription in one pass: check for new chapters, download
them, fill gaps from fallbacks, and run whatever automatic exports are due.
This is exactly what the scheduled task runs, but you can invoke it by hand
whenever. It takes a lock so overlapping runs skip instead of stepping on
each other — running it manually while the scheduled one is going is
harmless.

Novels that are fully caught up get a cheap freshness check rather than a
full crawl, so a sync where nothing changed is fast and light on the sites.

### refresh

```
vesper refresh <novel|all> [--titles]
```

Re-fetch a novel's metadata — author, cover, genre, status — from its main
source, leaving chapters alone. Useful when the author field was empty at
subscribe time (some sites are slow to fill these in) or a novel finally
flipped to "Completed". `all` does every subscription.

`--titles` additionally re-reads the *chapter* titles of everything already
downloaded and overwrites the ones that differ. A normal sync never revisits a
chapter it already has, so this is the way to repair titles that were saved
while a site's parsing was wrong. It costs one request per stored chapter, so
it's slow on a long novel — expect it to take a while and be gentle with it.
Re-export afterwards to get the corrected titles into the EPUB.

### unsubscribe

```
vesper unsubscribe <novel>
```

Stop following a novel. This also deletes its cached chapters, so if you
think you might come back to it, export first. Already-exported EPUBs are
never touched.

### prune

```
vesper prune [--retention-days N]
```

Reclaim disk space by deleting cached chapters — but only for novels that
are actually done: labeled complete by the site, no new chapters observed
for a good while, and already exported. The paranoia is intentional; site
status labels lie all the time ("completed" novels come back from the dead
constantly), so the label alone is never enough. Chapters that haven't been
exported yet are never deleted, period. And if a pruned novel does revive,
the next sync just re-downloads what it needs.

### service

```
vesper service install [--interval-minutes N]
vesper service uninstall
vesper service status
```

Manage the background sync job. Per-user, no admin rights needed, on all
three platforms. The interval defaults to whatever `poll_interval_minutes`
says in the config; note that it's baked in at install time, so if you
change the config value later, run `install` again to apply it.

### status, config, profiles, list

```
vesper status
vesper config
vesper profiles
vesper list <url>
```

The look-don't-touch commands. `status` shows where the database and log
live, each novel's sync state, when the last sync ran, and the tail of the
log — the first place to look when something seems off. `config` prints the
config file's path and current values. `profiles` lists the site profiles
Vesper knows about and where to put your own. `list` is a dry run: it walks
a novel's table of contents on the site and reports the chapter count and
the first/last entries, without saving anything — good for checking whether
Vesper can handle a page before you commit to subscribing.

## Configuration

Vesper writes a `config.ini` with commented defaults the first time it
runs; `vesper config` tells you where it landed. It's a flat file, no
sections to worry about. Here's each option, with defaults:

- `output_dir` (default `Documents/lightnovels`) — the root of your
  library. Books are laid out as `<output_dir>/<author>/<novel>/`, with the
  EPUB(s) inside.

- `request_delay_ms` (default `1500`) — how long to wait between requests
  to the same site, in milliseconds, with a bit of random jitter on top.
  Please use this one responsibly. If you don't care about the sites
  themselves, you should at least care about the fact that you'll most
  likely get your IP temporarily banned if you go overboard — these sites
  sit behind Cloudflare and they do notice. The default is deliberately
  conservative; a long backfill overnight beats a fast one that gets you
  blocked halfway through. Vesper also backs off on its own when a site
  starts returning 429s, but don't make it come to that.

- `poll_interval_minutes` (default `60`) — how often the background sync
  runs. Read at `service install` time, so changing it later means
  reinstalling the service. Hourly is plenty for novels that update once a
  day; there's little to gain from hammering a site every five minutes.

- `auto_export` (default `false`) — export the EPUB automatically once a
  novel's initial backfill finishes. Off by default so your first
  experience isn't Vesper silently writing files while you're still
  figuring out the tool, but for the hands-off workflow you want this on.

- `auto_append` (default `false`) — once a novel is caught up, re-export
  automatically whenever new chapters arrive. The companion to
  `auto_export`: that one handles the initial export, this one keeps the
  file current afterwards. With both on and the service installed, you
  never have to think about a novel again after subscribing.

- `split_every_chapters` (default `0`) — split exports into volumes of N
  chapters each, named `<novel> - Vol 01.epub` and so on. `0` means one
  single file. Mostly useful for the thousand-chapter monsters that some
  e-readers choke on as a single EPUB.

- `retention_days` (default `30`) — how long to keep a finished novel's
  cached chapters around after export before `prune` may delete them. This
  only bounds `prune`; nothing is deleted automatically.

- `quiet_grace_days` (default `30`) — how long a "completed" novel has to
  stay quiet — no new chapters — before Vesper actually believes the label.
  This is the safeguard behind `prune`: plenty of novels are marked
  complete and then get a sequel, a bonus arc, or just come back. Thirty
  days of silence on top of the label is when Vesper starts trusting it.

- `likely_complete_recheck_days` (default `7`) — once a novel is judged
  genuinely complete, how often to still check on it. Instead of polling
  it every sync like an active novel, Vesper looks in once a week — often
  enough to catch a revival, cheap enough not to matter.

- `log_path` (default `vesper.log` in the app-data directory) — where the
  background sync writes its log, since it has no console to print to.

## Where things live

- Your EPUBs: under `output_dir`, as described above. Filenames are
  sanitized automatically, so novels with `:` or `?` in the title won't
  blow up on Windows.
- The database (and the sync lock file): the local app-data directory —
  `%LOCALAPPDATA%\vesper\data\` on Windows, the XDG/Library equivalent
  elsewhere. Deliberately *local* app data, so the database doesn't get
  dragged through roaming profiles.
- Config and custom site profiles: the user config directory. Don't
  memorize any of these paths — `vesper status`, `vesper config` and
  `vesper profiles` print the real ones on your machine.

## Adding a site

If the site is plain server-rendered HTML — the kind where the chapter list
is real markup with page links — you don't need to touch the code. Drop an
`.ini` profile describing the site (selectors for content and metadata, how
the chapter list paginates) into the profiles folder and Vesper picks it up
on the next run. The folder contains a generated README documenting the
format, and novgo.net ships as a built-in profile you can crib from.

Sites that render their chapter list with JavaScript or need special
request handling are hand-written adapters in the source; the five built-in
sites cover a decent spread of examples if you want to write one.

## Building

```
cargo build --release
```

The binary lands at `target/release/vesper.exe` (or without `.exe`,
elsewhere). `cargo test` runs the test suite. If you're curious why things
are built the way they are, `DESIGN.md` goes into the reasoning.
