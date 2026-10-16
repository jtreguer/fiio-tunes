# fiio-tunes: technical document

Status: draft, 2026-10-16. Companion to [design.md](design.md). Where this
document says "as in iprune", the rule is the one in
`../iprune/docs/technical.md`, and the code is a copy of iprune's.

## 1. Stack

As iprune: Rust stable, edition 2024, Cargo workspace; `lofty`, `rusqlite`
(bundled), `walkdir`, `rayon`, `blake3`, `clap`, `gtk4-rs` (plain GTK4),
`serde`, `toml`, `directories`, `anyhow`, `thiserror`; `udisksctl` for eject.
libgpod 0.8.3 through FFI, read only, for importing from an iPod.

## 2. Repository layout

```
crates/gpod-sys/        bindgen output, links libgpod-1.0 (copied from iprune)
crates/gpod/            read-only safe wrapper; the only crate with unsafe code
crates/fiio-core/       config, store, scan, tags, playlist (.m3u8), playlists,
                        mounts, lock, inhibit; later device, card, naming,
                        adopt, import, plan, sync, exec, verify, backup, doctor,
                        notify
crates/fiio-tunes/      binary `fiio-tunes`: the CLI
crates/fiio-tunes-gui/  binary `fiio-tunes-gui`: the GTK4 window (F7)
packaging/              desktop entry and install.sh (F8)
```

`plan.rs` stays pure: no I/O. `naming.rs` (library path to card path) is pure
too.

## 3. libgpod

`gpod-sys` is iprune's, unchanged. `gpod` keeps iprune's reading API only:
`Database::open`, `open_file`, `tracks`, `playlists`, `track_by_dbid`,
`master_playlist`, `mountpoint`, `device_info`, `firewire_guid`, and the
`TrackRef` and `PlaylistRef` getters. There is no write, no playlist or track
change, no `itdb_write`. The rules of iprune's wrapper hold: `Database` is
neither `Send` nor `Sync`, strings are copied with lossy UTF-8, every `GError`
is freed.

## 4. Configuration

`~/.config/fiio-tunes/config.toml`:

```toml
[library]
roots = ["/mnt/seagate-1tb/Music"]
exclude_extensions = ["m4v", "mp4", "mov"]

[playlists]
dir = "/mnt/seagate-1tb/Playlists-FiiO"
path_style = "absolute"          # or "relative" to the playlists dir

[sync]                           # for a device with no selection of its own
scope = "all"                    # or "playlists", or "except"
playlists = []
remove_unmanaged = false
keep_backups = 20

[device]
mountpoint = ""                  # empty means detect
# allow_folder = true            # tests only: write to a plain folder

[card]                           # added in F2 (not accepted before), set from the probe (section 9.4)
music_dir = "Music"
playlists_dir = ""               # "" is the card root
```

## 5. State and files

| Path | Content |
|---|---|
| `~/.local/share/fiio-tunes/fiio-tunes.sqlite` | library index, mappings |
| `~/.local/share/fiio-tunes/backups/<key>/<stamp>/` | the card's playlists, manifest, `mappings.json`, `plan.json`, `work.json`, `listing.txt` |
| `~/.local/share/fiio-tunes/card.lock` | advisory lock, one fiio-tunes at a time with the card |
| `~/.local/state/fiio-tunes/ui.toml` | the window's columns and background |
| `<card>/.fiio-tunes/manifest.json` | device key, mappings, playlist names, stamp |
| `<card>/Music/...` | the managed audio files |

## 6. Database schema

One migration, `store/migrations/0001_init.sql`: iprune's `tracks`,
`playlists` (with tombstones) and `playlist_items` after all its migrations,
then:

- `devices (id, device_key UNIQUE, card_id, name, model, last_sync_at,
  restoring, scope)`, the key being `usb:<serial>` (for a folder used as a
  card in tests, the key in its manifest), `card_id` the id in the manifest of
  the card adopted (9.1);
- `device_scope_playlists (device_id, name)`, as in iprune;
- `device_tracks (device_id, track_id, card_path, card_key, synced_hash,
  synced_size)`, `card_path` relative to the card root with `/`, exactly as
  on the card, and unique per device by `card_key`;
- `device_playlists (device_id, playlist_id, card_name, card_key)`, unique
  per device by `card_key`.

`card_key` (`paths::card_key`) stands for the form FAT32 compares names in:
NFC, then each character upper-cased when its upper case is one character,
which is what FAT's upcase table does for ordinary letters. The kernel's own
table may differ for rare characters (the dotless `ı`); the probe (9.4)
checks a pair of accented names on the real card. SQLite's `NOCASE` folds ASCII only, so it is not used.

No statistics tables.

## 7. Library scan

As in iprune, with two changes:

- The formats read are those the X3 plays and lofty reads: MP3, AAC and ALAC
  (m4a), WAV, AIFF, FLAC, Ogg Vorbis, APE. Opus, WMA, Musepack, WavPack, DSF
  and DFF are reported as unsupported.
- A folder under a root holding a player (`.fiio-tunes/manifest.json`, or an
  iPod's `iPod_Control/iTunes/iTunesDB`) is skipped and reported.

A file moved and retagged in one go gets a new row hash, so its card copy no
longer matches and is copied again; nothing else marks it.

## 8. Playlists

As in iprune (section 8 there): UTF-8 `.m3u8`, unknown lines kept, atomic
writes with an mtime check, no-clobber create and rename, the folder watched by
the window.

## 9. The card

### 9.1 Detection and identity

- Detect: vfat mounts in `/proc/self/mountinfo` whose block device has a USB
  ancestor with `idVendor` `2972` (FiiO). `device.mountpoint` overrides.
  `FIIO_TUNES_NO_DETECT` (set by `.cargo/config.toml` for everything cargo
  runs) turns detection off.
- Key: `usb:<serial>` from the same USB device. A folder (tests, with
  `allow_folder`) uses the key in its manifest.
- The card's identity is `card_id`, a random id kept in
  `.fiio-tunes/manifest.json` on the card and in `devices.card_id`.
  Adoption itself writes nothing; when the card already holds a manifest
  with the same device key (the index was lost and rebuilt, or a first sync
  crashed after writing it), adoption takes its `card_id` into `devices`.
- The first sync after an adoption that found no manifest checks, before
  writing anything, that the card still holds what adoption saw: every
  adopted file present with its adopted size and the same BLAKE3 (read again,
  adopted files only), and still no manifest. It then commits a new random
  `card_id` to `devices`, and only after that writes the manifest.
- Every other sync is refused when the card has no manifest, another key or
  another `card_id`: "this card is not the one adopted; adopt it again". So a
  second card in the same player, which has the same USB serial, is never
  taken for the first; adopting it again is always the way out. A card cloned
  file for file, manifest included, passes as the same card.
- Writes need the root of a vfat mount of a `/dev` device, not read-only, as
  in iprune (`check_writable`), and that device must be FiiO's (vendor
  `2972`), also when `device.mountpoint` names it: a mistyped mountpoint
  (`/boot` is vfat too) is refused. `allow_folder` lifts both checks, not
  the read-only one. Read-only is read from the mount's options and from
  the filesystem's, where a card the kernel remounted read-only after an
  error says so. A card mounted at two places is one card.
- Every file written to the card is synced, its pages dropped from the
  page cache (`posix_fadvise(DONTNEED)`), then read back and compared by
  BLAKE3 (`card::read_back`), so the bytes compared come from the card.

### 9.2 Reading the card

`card::read` walks the card into its audio files (relative path, size, mtime)
and its `.m3u8` files, skipping `System Volume Information`, `.Trash-*`,
`.fiio-tunes` and any other dot entry. Cover images, `Thumbs.db` and other
non-audio files are neither listed nor touched. Audio means the extensions
the scan indexes and those it reports as unsupported. Folders are opened
with `O_NOATIME` and files only stat'ed, so reading the card does not
rewrite its access dates.

### 9.3 Naming

`naming::card_path(root, path)`: the path relative to its library root, under
`music_dir`. Each component has `" * : < > ? \ |` and control characters
replaced with `_`, trailing dots and spaces removed from the end of the name
(a stem keeps them before its extension: `Track .mp3` stays), and is cut to
255 UTF-16 units, keeping the extension; names are written in NFC. A folder
spelled two ways that FAT takes for one gets the first spelling, in library
path order, for every file under it. Two library files
whose card paths have the same `card_key` (section 6: Unicode case and
normalisation folded, as FAT compares them) get ` (2)`, ` (3)` before the
extension, in library path order. Pure and deterministic.

### 9.4 Playlist format: the probe

FiiO documents only that the X3 2nd gen reads `.m3u8` files at the card root
with relative paths and no drive letter, from Browse Files. The rest is
settled on the device, once, before the first playlist is written:
`fiio-tunes probe` (run only when the owner says so, with the X3 at hand)
writes `zz-probe-NN.m3u8` files in these variants. The files they point at
are copies of one library MP3 (present, of one to ten minutes, the
smallest), written to `Music/zz-probe/` under the names the variants need
(`01 Probe.mp3`, accents, CJK, `04 é.mp3` and `04 É.mp3`, a path over 200
characters), each read back and compared. The probe refuses to start when
any of its files is already there, and remembers in
`~/.local/share/fiio-tunes/probe.toml` the folders it created (`Music`,
`Playlists`) so that `--clean` removes those only if they are empty, and
`Music/zz-probe` always. One probe at a time: a probe of another card that
still holds its files is refused; a record whose card no longer holds them
(cleaned by hand, or the card mounted elsewhere now) is replaced, and with
it the list of folders the probe made there: an empty `Music` or `Playlists`
it left on that card stays. `--clean` also removes temp files a playlist write
cut short left in the two folders it writes playlists to:

| # | Variant |
|---|---|
| 1 | root, `Music/...` with `/`, UTF-8, LF, `#EXTM3U` and `#EXTINF` |
| 2 | as 1, with `\` |
| 3 | as 1, with a UTF-8 BOM |
| 4 | as 1, CRLF |
| 5 | as 1, no `#EXTM3U` and no `#EXTINF` |
| 6 | as 1, a leading `/` |
| 7 | as 1, `TF1:/Music/...` |
| 8 | in `Playlists/`, `../Music/...` |
| 9 | in `Playlists/`, `Music/...` (relative to the root) |
| 10 | as 1, a path with accents and one with CJK characters; also two files whose names differ only by the case of an accented letter, to see whether the card keeps both |
| 11 | as 1, a path over 200 characters |
| 12 | as 1, 2,000 entries |

The owner reports which play. `fiio-tunes probe --clean` removes them. The
result fixes `[card] playlists_dir`, the separator, the BOM and the line
endings, and is recorded here. Until then the defaults are variant 1.

Results: not run yet.

### 9.5 Card playlists

A card playlist is rendered with `playlist::render` from entries whose
locations are card paths relative to the playlist's folder, each with its
`#EXTINF`. Its file name is the playlist's name with file-name characters
replaced by `_` (iprune's third name pass), ` (2)` when its `card_key`
collides with another's, and `.m3u8`.

## 10. Adoption

`adoption::run` (read only; the matcher itself is `adopt.rs`, shared with
import) over the card's audio files and the index:

1. Same size and same BLAKE3 as exactly one library file: accepted. As
   several (duplicates in the library): a duplicate proposal, the first as
   default, the next free copy for the next card file.
2. Otherwise iprune's passes on tags, read from the card file: normalised
   title and artist, duration within 1 s (score 2 accepted only when the size
   is equal too, else proposed); then title, artist and album within 2 s, any
   size, proposed.
3. One library file claimed by two card files: both are proposals.
4. Everything else is unmanaged.

Accepted matches go into `device_tracks` with the card file's own hash, so the
first sync copies again those whose content differs and moves the others into
`Music/`. Card `.m3u8` files are matched to library playlists by iprune's three
name passes. `adopt-report.txt`, `--accept-proposals`, `--force`, and an empty
card adopting with nothing to match, as in iprune. Adoption writes nothing to
the card.

How it reads the card: every card file a library file has the size of is
hashed; tags are read (lofty, as the scan) only from the files no library
file is identical to, and a file with no title tag is titled by its file
name, as the library is. Every file matched or proposed is hashed, since its
mapping records the card file's hash. Files are opened with `O_NOATIME`.
Formats the scan does not index (by extension: WMA, DSD, Opus...) are
neither read nor matched and stay unmanaged; files that cannot be read are
listed in the report, once. The size recorded with a file is the number of
bytes its hash was taken over. The card lock is held while the card is read.

Only the `.m3u8` files of `[card] playlists_dir` (the card root by default)
are matched to library playlists; others are unmanaged.

Identity: the device key is `usb:<serial>`; a folder used as a card
(`allow_folder`) takes the key in its manifest, or `folder:<path>` when it
has none. A manifest of the same key gives its `card_id` to `devices`; with
no manifest `card_id` is cleared, so the first sync makes a new one (9.1).
A manifest of another key, or one that cannot be parsed, refuses the
adoption: the owner moves it aside. Two card files or playlists FAT would take
for one (only a folder can hold them) are mapped once, the first. A
re-adoption through a folder keeps the device's name and model.

## 11. Sync

### 11.1 Planner

`PlanInput`: library rows, library playlists, tombstones, card contents, track
and playlist mappings, scope, `remove_unmanaged`, space, and the card's
rendered playlist bytes. `Plan`:

- `copy`: in scope with no mapping, or whose mapped file is gone (a stale
  mapping);
- `replace`: mapped, and the library hash differs from `synced_hash`;
- `moves`: mapped, and `naming` gives another card path (also adopted files
  outside `Music/`); a replace whose path changes is a remove plus a copy;
- `remove`: mapped, and out of scope or missing from the library;
- `playlists`: `Delete`, `Rename`, `Write` (whenever the rendered bytes differ
  from the card file's), deletes first, then renames, then writes, by name;
- `unmanaged`: audio and `.m3u8` files on the card not mapped, kept, or
  removed with `remove_unmanaged`;
- `skipped`, `entries_skipped`, `stale_mappings`, `space` (removals first; a
  replace holds both files until its rename; 1% of capacity as margin),
  `expected`.

Scope rules, `prepare`'s refusals and the mass-removal guards (more than five
and more than a tenth) are iprune's.

Decisions taken in F4:

- Card paths are computed by `naming::card_paths` over every present
  library track, not only those in scope, so a change of scope moves
  nothing. A track's root is the longest library root holding it.
- A mapped card file whose size is not its `synced_size` was changed on
  the card: it is copied again (`replace`, reason `changed_on_card`). A
  FairPlay or 4 GiB file already on the card stays as it is; one not there
  yet is skipped. The X3 has no sample-rate limit to skip for.
- A target path held by an unmanaged card file (FAT key) is never written:
  a track to copy is skipped (`occupied`) and a managed file to move stays
  where it is. With `remove_unmanaged` the unmanaged file goes first. A
  mapped track under no library root (`no_card_path`) also stays in place.
  A managed file that stays holds its path in turn: no other copy or move
  goes there (computed until nothing changes, since one file staying can
  keep another in place).
- Moves come in an order the executor follows as given: a move whose
  target another move still has to free comes after it. A cycle (a swap)
  is broken by a first move to a hidden name next to the source
  (`.fiio-tunes-move-<n>.move`) and a last one from it. Playlist renames are
  ordered the same way (`.fiio-tunes-move-<n>.m3u8`). Paths of files on the
  card are taken from the card listing, as they are written there.
- Playlists: the file name of 9.5 also avoids the names of unmanaged
  `.m3u8` files kept in the playlists folder: fiio-tunes never takes over a
  card playlist it did not write. `.m3u8` files elsewhere on the card are
  unmanaged. A renamed playlist gets a `Rename` and, if its bytes differ, a
  `Write` under its new name. A playlist name starting with `.` gets `_`
  instead, since the card listing skips hidden files. Changing
  `[card] playlists_dir` leaves the playlists written in the old folder:
  they are unmanaged from then on (kept, or removed with
  `remove_unmanaged`), and new ones are written in the new folder.
- `prepare` refuses, besides iprune's cases: a card listing with problems
  (an unreadable folder would look like missing files), and a card whose
  manifest does not agree with `devices.card_id` (the read-only part of
  9.1), checked before anything else about the device. An adopted card
  with no mapping but with audio files is refused, as in iprune.
- The mass-removal guard counts managed tracks removed because they left
  the scope or the library; the old file of a replace that changes path is
  not counted.
- Until F5, `sync` without `--plan-only` is refused. `scope` sets the
  connected player's own scope (`--device KEY` for another), as in iprune.

### 11.2 Executor

1. Refuse: not writable (9.1), wrong manifest, a restore unfinished, a plan
   that does not fit, mass removals without `--allow-mass-removal`.
2. Back up the card's playlist files, the manifest, `mappings.json`,
   `plan.json`, `work.json` and `listing.txt` (path and size of every card
   file) to `backups/<key>/<stamp>/`, filled under a hidden name and renamed
   when complete. Keep `keep_backups`.
3. Removals, then moves (`rename`, creating folders; folders under `Music/`
   left empty are removed), then copies and replaces. A copy goes to
   `<dir>/.fiio-tunes-<n>.part`, is `fsync`ed, checked for size, read back and
   compared by BLAKE3 with the library file, then renamed over its target. A
   library file gone or unreadable is skipped and reported; a card error
   stops the sync.
4. Checkpoints every 500 files or 10 minutes: mappings committed and the
   manifest rewritten. An interrupted sync loses at most the file in flight,
   whose `.part` file `doctor` removes.
5. Playlists: deletes, renames, then writes with `write_atomic`.
6. Verify (11.3).
7. Record mappings, playlist mappings and `last_sync_at`, drop the tombstones
   whose card file was deleted, write the manifest, prune backups, notify.

Signals, Stop, the logind inhibitor, the lock, exit codes (0 success, 1 error,
2 usage, 3 verification failed, 4 declined) and notifications follow iprune.

Decisions taken in F5, which applies playlists only:

- `sync --playlists-only` plans with every track left as it is (the
  planner's `playlists_only`): managed files stay where they are, the
  playlists name the files on the card now, and entries whose track is not
  there yet are left out and reported. It stays as an option once tracks
  are synced. Until F6, `sync` without it or `--plan-only` is refused, and
  the executor refuses a plan with track changes.
- Refused before asking: track changes (F5), a plan that does not fit,
  mass removals of tracks, or of playlist files (more than five and more
  than a tenth of the managed playlists; unmanaged ones `remove_unmanaged`
  deletes count too, since no restore puts them back), without
  `--allow-mass-removal`. An empty plan writes nothing, not even a backup.
- After the prompt, the card is checked again: its manifest against
  `devices.card_id` (9.1), and for a first sync every adopted file by size
  and BLAKE3, read again, with still no manifest.
- The backup holds every `.m3u8` on the card under `playlists/<card path>`,
  `manifest.json` when there is one, `mappings.json` (device key, card id,
  track mappings with hashes, playlist mappings with the library names),
  `plan.json`, `work.json` (`[]` until F6 logs copies) and `listing.txt`
  (path and size of each audio and `.m3u8` file). The folder is
  `backups/<key>/`, the key with characters other than letters, digits,
  `.`, `-` and `_` made `_`.
- A first sync records a new `card_id` (32 hex digits from the kernel's
  random source) in the index, then writes the manifest, before any
  playlist. If that manifest write fails, the index forgets the id again,
  so the card is still the one adopted for the next try. The manifest (version 1) holds the device key, the card id, the
  time, every managed track (path, size, BLAKE3) and the managed playlist
  paths. It is written with `write_atomic` and read back.
- A rename never lands on an existing file other than the same file under
  another case (`WouldOverwrite`). A write replaces a file only where the
  plan saw one (listed when it was made, or put there by a rename of the
  same plan); elsewhere it creates the file and fails if one appeared
  since the plan. Each playlist written is read back
  through `card::read_back`. Verification compares the card with
  `expected`, and any audio or `.m3u8` file the plan does not account for
  is a difference too.
- A failure after the backup names it, with the restore command to run
  before syncing again. Once the card is written, checked and recorded, a
  manifest that cannot be rewritten or old backups that cannot be pruned
  are warnings, not failures.
- `mappings.json` also records `[card] playlists_dir`, so a restore finds
  the playlists where they were.

`restore <stamp>` refuses a card that is not the one the backup or the
index names: its manifest's card id must be the index's or the backup's;
with no manifest, the index or the backup must have no card id, or one of
the backup's managed playlists must be on the card (a second card in the
same player has the same key). It checks every file of the backup before
changing anything, backs the card up, and marks the device as restoring
until it is done, so no sync runs on a half-restored card; a failure part
way says so. It puts back only the playlists fiio-tunes managed at the time
of the backup, in the folder they were in then,
deletes the managed ones it did not hold and those the sync the backup
was taken for created (from the backup's `plan.json`: files written or
renamed to that were not on the card, left there when that sync failed
before recording them), reads each file back from the card, writes back
the manifest (or removes it, and its folder when empty, when the card had
none) and replaces the record with the
backup's: track mappings of library rows that still exist, playlist
mappings, a tombstone for a library playlist gone since, and the card id.
Old backups are then pruned. Unmanaged playlists that `remove_unmanaged`
deleted are in the backup's `playlists/` folder; a restore does not write
unmanaged files, so they are copied back by hand. `backups` lists a card's
backups.

### 11.3 Verification

List the card again and compare with `expected`: every managed path present
with its size, no managed path left that was removed, every managed playlist
byte for byte, every unmanaged file as before. Any difference is printed (up to
20 lines) and exits 3, naming the backup.

### 11.4 Restore and doctor

`restore <stamp>` puts back the backup's playlist files and manifest and
replaces the mappings with the backup's; audio files removed since are
copied again by the next sync. `doctor [--fix]` reports and repairs: `.part`
files (deleted), `.fiio-tunes-move-<n>` files a sync stopped between the
hops of a move cycle left (renamed to the path the work log gives them),
card audio no mapping names (moved to
`.fiio-tunes/orphans/<stamp>/`), mappings whose file is gone (dropped), library
rows missing for 30 days and on no device (dropped).

## 12. Import

`import.rs`, CLI `fiio-tunes import`, and the window's Import… dialog.

- **iprune folder.** Read `[playlists] dir` from
  `~/.config/iprune/config.toml`. Each chosen file is read with
  `playlist::read_all`, its resolved entries rewritten in fiio-tunes'
  `path_style`, unresolved lines kept verbatim, and written as a new file.
- **iPod or `iTunesDB` file.** Read with `gpod`. Offered: regular playlists
  and smart playlists (their stored members), not the master nor Podcasts.
  Each member is traced to a library path: first through a copy of
  `~/.local/share/iprune/iprune.sqlite` (with its `-wal`) opened read-only in
  a temp folder, `devices.firewire_guid` to `device_tracks.dbid` to
  `tracks.path`, looked up in fiio-tunes' index by `path_key`; then by the
  adoption matcher on size, duration and tags, with the content check when
  the iPod is mounted. Members with no library file are left out and listed.
- **Names.** A name already in fiio-tunes' folder is skipped (the default),
  replaced (an ordinary write, with the mtime check) or imported as
  `<name> (iPod)`, then `<name> (iPod 2)` and so on. A name the same import
  already wrote (two iPod playlists of one name, or "A/B" and "A_B") is
  always renamed, so an import never replaces a playlist it wrote. A
  playlist that cannot be written is reported and the others are still
  imported; the command then exits 1. An iPod playlist's name
  has the characters iprune's file names cannot hold (`/ \ : * ? " < > |`)
  and line breaks replaced by `_`, as iprune's own files have them, and a
  leading `.` made `_`. Names given on the command line are found by
  iprune's three name passes; two playlists equal under the pass that finds
  them are refused as ambiguous.
- **Scope.** Optionally, the iPod's iprune scope (`devices.scope` and
  `device_scope_playlists`, or iprune's `[sync]` when the scope is NULL)
  becomes the X3's when every playlist it names was imported. Until F3 there
  is no X3 row in `devices`, so it is written to `[sync]` in the config, the
  scope of a device with no selection of its own. iprune's names are traced
  to the iPod's playlists through `device_playlists.ipod_id`, then by the
  name passes.

The `adopt.rs` matcher is iprune's, with the device track named by a `u64`
key. For an import it is used without the one-to-one rule of an adoption:
a playlist needs the song, so every accepted match and every proposal's
default is taken, and duplicates on the iPod (two iPod tracks claiming one
library file) all go to that file. The iPod's record in iprune's index is
found by its FireWire GUID; for an `iTunesDB` file, whose GUID is usually
unknown, it is the iPod whose mappings name the most of the file's dbids.
An index that cannot be read leaves the tracing to the matcher, and says so.

Import never writes to the iPod, the backup, iprune's index or iprune's
playlists folder.

## 13. CLI

```
fiio-tunes init --library DIR... --playlists DIR [--path-style S] [--force]
fiio-tunes scan
fiio-tunes playlist list|show|new|rm|rename|add|remove|move ...
fiio-tunes import iprune [NAME...] [--on-conflict skip|replace|rename] [--list]
fiio-tunes import ipod [--file ITUNESDB | --mountpoint DIR] [NAME...] [--on-conflict ...] [--scope | --list]
fiio-tunes device info
fiio-tunes probe [--clean] [--yes]
fiio-tunes adopt [--accept-proposals] [--force]
fiio-tunes sync [--plan-only | --playlists-only] [--verbose] [--json] [--yes] [--allow-mass-removal]
fiio-tunes scope [all | only PLAYLIST... | except PLAYLIST... | default]
fiio-tunes backups [--device KEY]
fiio-tunes restore <stamp> [--yes]
fiio-tunes doctor [--fix] [--yes]
fiio-tunes eject
```

`import` with no NAME takes every playlist; `--list` shows them, with the
names already taken and, for an iPod, how many members have a library file,
and writes nothing. Without `--file` or `--mountpoint`, `import ipod` uses
the one mounted filesystem holding `iPod_Control/iTunes/iTunesDB`;
`FIIO_TUNES_NO_DETECT` turns that off. A scope that cannot be copied is an
error (exit 1) after the playlists are imported.

## 14. Desktop UI

iprune's section 14, with application id `dev.fiiotunes.FiioTunes`, state in
`~/.local/state/fiio-tunes/ui.toml`, the statistics columns removed, the
device panel and texts naming the X3, and the Import… dialog. The code of
`style.rs`, the CSS files, `view_model.rs`, `selection_dialog.rs` and the
widgets of `main.rs` is iprune's.

## 15. Testing

| Level | What | How |
|---|---|---|
| Unit | naming, planner, card playlist rendering, adoption scoring, import tracing | pure functions, table driven |
| Integration | full sync on a fake card | a temp folder with `allow_folder`, ffmpeg fixtures |
| Regression | sync twice | the second plan is empty and the card listing unchanged; also a proptest with random edits |
| Import | fake iPods | `make_ipod.c` (libgpod) builds the `iTunesDB`; a fake `iprune.sqlite` maps it |
| Real data, read only | import from the iPod backup | ignored test on `/mnt/seagate-1tb/iPod-backup-2026-10/iTunes/iTunesDB` |
| Device | probe, adopt, playlists only, one small scope, the library | by hand, each with the owner's go |

## 16. Milestones

| # | Deliverable | Writes to the card |
|---|---|---|
| F0 | docs, workspace, forked crates, `init`, `scan`, `playlist` | no |
| F1 | `import` from iprune's folder, an iPod and an `iTunesDB` file | no |
| F2 | detection, `device info`, card reading, naming, the probe | probe files only |
| F3 | `adopt`, taking the `card_id` of a manifest already there | no |
| F4 | planner and `sync --plan-only` | no |
| F5 | executor for playlists only, backups, verify, restore, the card identity checks of 9.1 | yes |
| F6 | copy, replace, move, remove, checkpoints, `doctor`, signals | yes |
| F7 | the window | yes |
| F8 | desktop entry, notifications, `install.sh` | yes |
