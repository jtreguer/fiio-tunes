# fiio-tunes

fiio-tunes manages the playlists of a local music library and syncs the
library and its playlists to a FiiO X3 2nd gen on Linux. The X3 has no
database to talk to: its microSD card is a FAT32 folder of plain files.
fiio-tunes keeps that card a faithful copy of the library, with a sync
you can read before it happens and that is checked after it is written.

It is the sister of [iprune](https://github.com/jtreguer/iprune), which
does the same for an iPod Classic, and shares its window, its rules and
most of its code. It can import the playlists curated for the iPod.

It has two parts:

- `fiio-tunes-gui`, iprune's GTK4 window: a sidebar of playlists, a
  sortable grid of tracks, drag and drop, and a Sync button.
- `fiio-tunes`, a command line tool that does everything the window does.

fiio-tunes does not play music, edit tags or convert files. Your music
folder and its `.m3u8` playlist files are the reference. The card is a
copy of them, kept up to date one way, from the library to the player.

## How a sync works

Every sync starts with a plan: tracks to copy, copy again, move and
remove, and playlists to write, rename and delete, with the space it
needs. Nothing touches the card until you accept it. Every copied file is
then read back and compared with the library file, and at the end the
card is listed and compared with the plan. Any difference stops the sync.

## How playlists are managed

There are three places a playlist can live, and fiio-tunes keeps them
apart.

**fiio-tunes' own folder.** Your playlists are `.m3u8` files in one
folder (`[playlists] dir` in the config), holding absolute paths to the
library files, or paths relative to the folder if you prefer. You create,
rename, delete and reorder them in the window or with `fiio-tunes
playlist`, and drag tracks onto them. Every change is written at once,
atomically, and changes made by other programs are picked up. An entry
fiio-tunes cannot find in the library is kept as it is, never dropped.

**iprune's folder, an iPod, or an `iTunesDB` file.** These are sources to
import from, never shared. `fiio-tunes import iprune` (or Import… in the
window) copies the playlists you choose into fiio-tunes' folder; `import
ipod` reads them from a connected iPod or a copy of its database, tracing
each track back to its library file. A name already taken is skipped by
default, replaced with `--on-conflict replace`, or imported as
`<name> (iPod)` with `--on-conflict rename`. After an import the two
copies are independent: editing a playlist in fiio-tunes never changes
iprune's file, and a change made in iprune reaches the X3 only when you
import that playlist again with `replace`. fiio-tunes never writes to an
iPod, its backups, iprune's index or iprune's playlists folder.

**The card.** At each sync, every playlist in the X3's selection is
written whole to the card root as one `.m3u8` file, in the form FiiO
documents. The file is UTF-8, and its paths are relative to the root
(`Music/Artist/Album/01 Song.mp3`), with no drive letter. An entry
whose track is not on the card (out of the selection, protected, or not
found in the library) is left out of the card's copy only. A playlist renamed or
deleted in the library is renamed or deleted on the card at the next
sync. Playlist files on the card that fiio-tunes did not write are
reported and left alone.

**On the X3** these playlists are found under **Browse Files**, not in
the Playlists menu. That menu shows only playlists made on the player,
which live in its internal memory and cannot be read or written over USB;
FiiO keeps the two apart on purpose. The tracks themselves appear under
the categories after Settings > Update media library.

## Features

- **Library index.** Scans one or more music folders (MP3, AAC, ALAC,
  FLAC, WAV, AIFF, Ogg Vorbis, APE) into a SQLite index. Files moved or
  renamed keep their identity, by content hash. An unmounted disk is
  never taken for an empty library. Formats the X3 cannot play, or whose
  tags are not read (WMA, DSD, Opus), are reported, and FairPlay
  protected files are indexed but never copied.
- **Card layout.** The card mirrors the library under `Music/`, with the
  characters FAT32 refuses replaced and names that differ only by case
  told apart. Tags and embedded cover art travel inside the files, which
  are copied as they are.
- **The window.** Sortable and resizable columns, a search field, a
  filter for the playlists, drag and drop into and within playlists,
  Add to Playlist and Show in Playlist in the right-click menu, Import…,
  and the connected X3's state with its Sync, Selection, Adopt and Eject
  buttons.
- **Adoption.** Takes over a card as it is: audio files already there
  are matched to library files by identical content, or by tags and
  duration when you accept the proposal, then moved into place by the
  first sync instead of being copied again.
- **Per-card selection.** Each X3 syncs the whole library, only some
  playlists and their tracks, or the whole library except some
  playlists (`fiio-tunes scope`, or the Selection button).
- **Card identity.** A random id kept on the card and in the index, so a
  second card in the same player, or a card changed elsewhere, is
  noticed before anything is written.
- **Safety.** Before every write, the card's playlists, its manifest and
  fiio-tunes' record of the card are backed up. A sync interrupted by a
  crash, a signal or a pulled cable loses at most the file it was
  copying: the next sync takes back what was already copied, and
  `doctor` removes the partial file. Files on the card that fiio-tunes
  does not manage are reported and kept. A plan that would remove more
  than five tracks and more than a tenth of the managed ones, or the
  same share of the playlists, needs an explicit `--allow-mass-removal`.
- **Repair.** `fiio-tunes backups` and `fiio-tunes restore` put earlier
  playlists and records back. `fiio-tunes doctor` finds what an
  interrupted sync or another program left on the card or in the index,
  and `doctor --fix` repairs it.
- **Desktop.** A desktop entry for the app launcher, a notification when
  a sync ends, and `fiio-tunes eject`, which unmounts the card and powers
  the player off.

## Supported device

fiio-tunes has been used with one FiiO X3 2nd gen (USB `2972:0003`),
firmware 2.0, its last, with a 128 GB microSD card formatted FAT32. A
first full sync wrote about 9,900 tracks and 340 playlists.

fiio-tunes finds the player by FiiO's USB vendor id. Other FiiO players
that read `.m3u8` playlists from Browse Files may work but have not been
tried.

`fiio-tunes probe` writes a set of test playlists in different forms, to
see on the player which ones it plays.

## Requirements

- Linux, with `udisksctl` for ejecting.
- libgpod 0.8.3 (`pacman -S libgpod` on Arch), found through
  `pkg-config libgpod-1.0`, and clang for `bindgen`. It is used only to
  read iPod databases for imports.
- GTK 4.12 or later.
- Rust stable, 1.92 or later.
- For the tests: `ffmpeg` with libmp3lame, aac, alac, libvorbis and
  libopus, and `gtk4-broadwayd` for the window tests.

It was developed on Arch Linux with Omarchy (Hyprland), and follows the
GTK light or dark theme.

## Install

```sh
packaging/install.sh            # installs fiio-tunes, fiio-tunes-gui and the desktop entry
packaging/install.sh --uninstall
```

## Getting started

```sh
fiio-tunes init --library ~/Music --playlists ~/Playlists-FiiO
fiio-tunes scan
fiio-tunes import iprune        # optional: copy in iprune's playlists
fiio-tunes adopt                # once per card, with the X3 connected in USB storage mode
fiio-tunes sync --plan-only     # see what a sync would do
fiio-tunes sync                 # do it, after confirmation
fiio-tunes eject
```

Or open fiio-tunes from the app launcher and use the Import…, Adopt and
Sync buttons. A first sync of a large library takes a while: the X3's
USB storage mode writes at about 3 MB/s, and every file is read back.

The configuration lives in `~/.config/fiio-tunes/config.toml`, the index
and backups in `~/.local/share/fiio-tunes/`, the window's layout in
`~/.local/state/fiio-tunes/ui.toml`.

## Documentation

- [`docs/design.md`](docs/design.md): what fiio-tunes does and why.
- [`docs/technical.md`](docs/technical.md): how it does it.

## Built with AI

fiio-tunes was written with Claude Code, Anthropic's AI coding assistant,
working from a specification and under the direction of its author.
Every change was tested and reviewed before being committed, and the
tool has been used on a real X3, but it has not had the scrutiny of a
large user base.

## Use at your own risk

fiio-tunes writes to your player's memory card: it copies, moves and
removes audio files there and rewrites playlist files. It backs up the
card's playlists before every write and checks the result, but it does
not back up the audio files on the card, which it expects your library
to hold, and it comes with no warranty of any kind. Keep your own copy
of the card and of your playlists before you try it.

## License

MIT; see [LICENSE](LICENSE). You are free to use, modify and share it.
