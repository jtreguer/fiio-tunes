# fiio-tunes: design document

Status: draft, 2026-10-16. Built after iprune (`../iprune`), whose window and
rules it follows.

## 1. Purpose

fiio-tunes is a small Linux tool that manages playlists for a local music
library and keeps a FiiO X3 2nd gen in sync with that library. It is iprune
for a player that has no database: deliberate, verifiable sync of tracks and
playlists, one way, from the computer to the player.

It does not play music, and it does not coordinate with music players.

## 2. Background

| Item | Value |
|---|---|
| Library root | `/mnt/seagate-1tb/Music`, about 81 GB |
| Track files | 8,133 mp3, 1,561 m4a, 91 flac, 79 wav, 1 aiff, 2 m4v (video) |
| Player | FiiO X3 2nd gen, USB `2972:0003`, one microSD slot, no internal storage |
| Card | 120 GB, FAT32, label `X3`, mounted at `/run/media/julien/X3` |
| Card today | 3.1 GB in 12 loose album folders, some byte-identical to library files |
| iPod playlists | 339 `.m3u8` files in `/mnt/seagate-1tb/Playlists`, managed by iprune |
| iPod backup | `/mnt/seagate-1tb/iPod-backup-2026-10` |

The X3 reads music from plain folders. It finds tags in the files
themselves, builds its own media library on request (Settings > Update media
library), and opens external playlists (`.m3u8` on the card) from Browse
Files. Its own playlists, play counts and favourites live in its internal
memory and cannot be read over USB.

## 3. Goals

1. Point at one or more local music folders and index them.
2. Create, edit, reorder, rename and delete playlists.
3. Import the playlists curated for the iPod: from iprune's folder, from the
   iPod, or from a copy of its database.
4. Sync tracks and playlists to the X3's card, one way, library to card.
5. Show exactly what a sync will do before it does it, and verify afterwards
   that the card matches what was planned.
6. Take over the card as it is, without copying again what is already there.
7. Look and behave like iprune: the same window, the same plan, the same
   safety rules, every action also a command.

## 4. Non-goals

- Playback, and any integration with music players.
- Play counts, skips, ratings: the X3 does not expose them.
- Cover art beyond what the audio files embed. fiio-tunes copies audio files
  only.
- Tag editing and transcoding.
- The X3's internal playlists ("Category > Playlists"), which FiiO says must
  not be mixed with external ones.
- Writing to the iPod, its backup or iprune's own files. Import only reads
  them.
- Automatic sync on plug-in.

## 5. Principles

The same as iprune's.

**The plan is the product.** Every sync computes a plan (files to copy,
replace, move and remove; playlist files to write, rename and delete), shows
it in full, and changes nothing until it is confirmed. The same plan is then
executed and checked.

**Files are the truth.** Tags live in the audio files; playlists live in the
`.m3u8` files of fiio-tunes' playlists folder. The card is a copy of both.

**Leave alone what you do not own.** fiio-tunes changes only the card files
and playlists it knows it manages. Anything else is reported and kept unless
the user asks for removal.

**Always reversible.** The card's playlists and fiio-tunes' record of the card
are backed up before every write. Audio files need no backup: the library has
them, and the next sync copies them again.

## 6. Concepts

**Library, playlist, sync scope.** As in iprune. Each device has its own sync
scope: the whole library, only the tracks of chosen playlists, or the whole
library except them.

**Device.** An X3, identified by its USB serial number. fiio-tunes keeps a
mapping between library tracks and card files for each device, and a
manifest on the card (`.fiio-tunes/manifest.json`) with a random card id, so
that another card in the same player, or a card changed elsewhere, is noticed
before anything is written.

**Card layout.** `Music/<path relative to the library root>`, so
`/mnt/seagate-1tb/Music/Jeff Buckley/Grace/02 Grace.mp3` becomes
`Music/Jeff Buckley/Grace/02 Grace.mp3`. Characters FAT32 refuses are
replaced with `_`, and names that differ only by case are told apart.
Playlists go where the X3 reads them (section 9).

**Adoption.** The one-time step that matches the audio files already on the
card to library files, so the first sync moves them into place instead of
copying them again.

**Import.** Copying playlists into fiio-tunes' folder from iprune's folder,
from a connected iPod, or from an `iTunesDB` file.

## 7. User workflows

### 7.1 First run

1. `fiio-tunes init --library /mnt/seagate-1tb/Music --playlists
   /mnt/seagate-1tb/Playlists-FiiO`.
2. `fiio-tunes scan` indexes the library.
3. `fiio-tunes import` (or Import… in the window) brings in the playlists
   wanted from the iPod side.
4. With the X3 connected, `fiio-tunes adopt` matches what is on the card and
   reports what it would leave unmanaged. Nothing on the card changes.
5. `fiio-tunes sync` shows the first plan: the library (or the chosen scope)
   to copy, adopted files to move into `Music/`, playlists to write.

### 7.2 Editing playlists

As in iprune: in the window or with `fiio-tunes playlist ...`. Each change is
written to the `.m3u8` file at once; changes made by other programs are picked
up.

### 7.3 Syncing

1. Connect the X3 in USB storage mode. fiio-tunes finds it by its USB vendor
   among the mounted FAT32 filesystems.
2. `fiio-tunes sync` (or Sync in the window) scans the library, reads the card
   and shows the plan:

   ```
   X3 (USB serial 323820474077)
     + 120 tracks to copy (1.2 GB)
     > 34 tracks to move into place
     ~ 3 tracks to copy again (changed in the library)
     - 1 track to remove (not in library any more)
     playlists: 2 new, 1 changed, 0 removed
     unmanaged on the card: 14 files, 0 playlists
   Apply? [y/N]
   ```

3. On confirmation: back up, remove, move, copy (each file checked after it is
   written), write the playlists, then read the card again and compare it with
   the plan.
4. `fiio-tunes eject` unmounts and powers off the card.
5. On the X3, Settings > Update media library shows the new tracks under
   Category; the playlists are under Browse Files.

### 7.4 Recovering

`fiio-tunes backups`, `fiio-tunes restore <stamp>` and `fiio-tunes doctor
[--fix]`, as in iprune, adapted to a card of plain files (technical document,
section 11).

## 8. Interface

The iprune window, unchanged in layout, colours and behaviour: a sidebar
(Library, Playlists with a filter, the device panel), a grid with sortable,
movable, resizable columns, search, drag and drop, the same dialogs and
keyboard shortcuts. The differences:

- The device panel names the X3 and its card.
- No Plays, Skips, Rating or Last Played columns.
- An Import… action next to the playlists, opening a dialog in the style of
  the Selection dialog.
- The plan's sections are copy, copy again, move, remove, playlists and
  unmanaged files; there is no statistics line.

## 9. Sync rules

### Tracks

| Library | Card | Action |
|---|---|---|
| In scope, not on the card | | copy |
| Changed (another hash) | managed | copy again over the same file |
| Moved or renamed in the library | managed | move on the card |
| | managed, no longer in scope or deleted | remove |
| | audio not managed by fiio-tunes | report, keep |

Files the X3 cannot play, files whose tags fiio-tunes cannot read yet (WMA,
DSD) and files of 4 GiB or more (the FAT32 limit) are skipped and reported.

### Playlists

- The card holds one `.m3u8` per library playlist in scope, with exactly the
  playlist's entries in order, written whole on every change.
- Entries outside the scope or missing from the library are left out and
  reported.
- Playlist files on the card that fiio-tunes did not write are reported and
  kept, unless the user asks for removal.

The exact form the X3 reads (folder, path style, encoding, line endings) is
settled on the device before the first sync writes a playlist (technical
document, section 9).

### Safety checks

Every copied file is compared with the library file after it is written. After
the sync, the card is listed again and compared with what the plan expects:
every managed file present with its size, every managed playlist byte for
byte, every unmanaged file unchanged. Any difference stops the sync and names
the backup.

## 10. Adoption

A card file is matched to a library file when it has the same size and the
same content (BLAKE3). Failing that, iprune's tag passes propose matches
(normalised title and artist with the duration within a second; then title,
artist and album within two seconds, any size); an accepted proposal is
copied again at the first sync, since its content differs from the library's.
Everything else stays unmanaged. Playlist files on the card are matched to
library playlists by name, as iprune matches iPod playlists.

## 11. Import

Three sources, read only:

1. **iprune's playlists folder.** Its `.m3u8` files name library files by
   absolute path; they are copied as they are, in fiio-tunes' path style.
2. **A connected iPod**, or 3. **an `iTunesDB` file** (the backup, or one of
   iprune's backups). Regular playlists and the current members of smart
   playlists are offered. Each iPod track is traced to its library file
   through iprune's own record of that iPod when there is one, then by the
   adoption matcher. Tracks with no library file are left out and listed.

The user picks the playlists; a name already taken can be skipped, replaced or
imported under a new name. The iPod's iprune sync scope can be copied to the
X3's when its playlists were imported.

## 12. Omarchy integration

As iprune: two binaries (`fiio-tunes`, `fiio-tunes-gui`), a desktop entry, a
notification when a sync ends, no automatic sync.

## 13. Decisions

Taken with the owner on 2026-10-16:

1. fiio-tunes has its own playlists folder; the iPod's playlists are
   imported, not shared.
2. The code is a fork of iprune's; iprune is not changed.
3. Card layout: `Music/` mirroring the library.
4. No play statistics anywhere in fiio-tunes.
