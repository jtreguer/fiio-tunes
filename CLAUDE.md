# CLAUDE.md

fiio-tunes is a Linux tool (GTK4 window and CLI) that manages `.m3u8`
playlists for a local music library and syncs tracks and playlists one way,
library to device, to a FiiO X3 2nd gen (a FAT32 microSD of plain files). It
imports playlists from iprune (`../iprune`, the same tool for an iPod
Classic) and from iPods. It does not play music or edit tags.

Read `docs/design.md` (what and why) and `docs/technical.md` (how) before
changing behaviour. They are the spec. If code and docs disagree, ask which one
is right rather than silently picking one. Where they say "as in iprune", read
`../iprune/docs/technical.md`; the code there is the model, and the window and
its behaviour must stay identical to iprune's.

## Status

F0 to F4 done (workspace, forked crates, `init`, `scan`, `playlist`, `import`,
detection, `device info`, card reading, naming, `probe`, `adopt`, the planner,
`sync --plan-only`, `scope`; the probe not yet run on the X3, nor `adopt` on
its card). Milestones in
`docs/technical.md` section 16:

| # | Deliverable | Writes to the card |
|---|---|---|
| F0 | docs, workspace, forked crates, `init`, `scan`, `playlist` | no |
| F1 | `import` from iprune's folder, an iPod and an `iTunesDB` file | no |
| F2 | detection, `device info`, card reading, naming, the probe | probe files only |
| F3 | `adopt`, taking the `card_id` of a manifest already there | no |
| F4 | planner and `sync --plan-only` | no |
| F5 | executor for playlists only, backups, verify, restore, the card identity checks | yes |
| F6 | copy, replace, move, remove, checkpoints, `doctor`, signals | yes |
| F7 | the window | yes |
| F8 | desktop entry, notifications, `install.sh` | yes |

Do not start work that writes to the card before the earlier milestones are
done and tested.

## Decisions

- Target device: FiiO X3 2nd gen, USB `2972:0003`, one microSD card, FAT32.
- fiio-tunes has its own playlists folder. iPod playlists are imported (from
  iprune's folder, a connected iPod or an `iTunesDB` file), never shared.
- Forked from iprune; iprune is never modified from here.
- Card layout: `Music/<path relative to the library root>`, FAT-safe.
- No play statistics, ratings or cover art work.
- The library is the reference: tags live in the files, which are copied as
  they are.
- The interface is iprune's GTK4 window, not a terminal UI.

## Stack

As iprune: Rust stable, edition 2024, Cargo workspace. libgpod 0.8.3 via FFI
(`bindgen`, `glib-sys`), read only, for imports. `lofty`, `rusqlite`
(bundled), `walkdir` + `rayon`, `blake3`, `clap`, `gtk4-rs` (no libadwaita),
`serde` + `toml`, `directories`, `anyhow` in binaries, `thiserror` in
libraries. Eject through `udisksctl`.

## Layout

```
crates/gpod-sys/     raw bindgen output, links libgpod-1.0 via pkg-config
crates/gpod/         read-only safe wrapper; the only crate allowed to contain unsafe
crates/fiio-core/    config, store, scan, tags, playlist (.m3u8 format), playlists
                     (operations), mounts (mountinfo), lock, inhibit; the device,
                     card, import, adoption, planner and executor modules follow
crates/fiio-tunes/   binary `fiio-tunes`: CLI
```

## Commands

```
cargo build
cargo test                                  # needs libgpod and clang (bindgen), and ffmpeg
                                            # with libmp3lame, aac, alac, libvorbis, libopus
cargo clippy --all-targets -- -D warnings
cargo fmt
# cargo sets FIIO_TUNES_NO_DETECT (.cargo/config.toml): nothing it runs finds
# the real X3 or iPod by itself; set device.mountpoint for that
```

## Review and tests before every commit

No code is committed or pushed until both of these are done, for every
change, however small:

1. **Tests by Aldo.** Launch the `aldo-test-specialist` agent on the change. It
   writes or extends the tests, shows them failing against broken code, and
   passing against the change.
2. **Review by Vera.** Launch the `vera` agent on the diff (code and Aldo's
   tests). Fix what she finds, or record why a finding was rejected, then have
   her check the fixes.

`cargo test`, `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`
must pass at the end. Mention in the commit message that both steps ran.

## Rules that must hold

- **Never write to the real card** (`/run/media/julien/X3`) from code, tests or
  experiments unless the user explicitly asks in that moment. Tests use temp
  folders with `device.allow_folder = true`.
- **Never write to an iPod, `/mnt/seagate-1tb/iPod-backup-2026-10`, iprune's
  data folder (`~/.local/share/iprune`), its config, or
  `/mnt/seagate-1tb/Playlists`.** Import reads them; tests use copies.
- **Never modify files under `/mnt/seagate-1tb/Music`** during development.
- `plan.rs` and `naming.rs` are pure: no I/O. Same input, same output, same
  order.
- A card playlist is always written whole. No append.
- Only touch card files and playlists fiio-tunes manages. Unmanaged ones are
  reported and kept unless the user asks for removal.
- Back up the card's playlists, the manifest and the mappings before every
  write. Every copied file is read back and compared with the library file;
  after the sync the card is listed and compared with the plan. Any
  difference stops the sync and exits with code 3.
- `.m3u8` writes go to a temp file in the same folder, `fsync`, then rename.
  Unresolved entries are kept verbatim.
- Paths are stored exactly as on disk and looked up by their NFC form
  (`path_key`). At least one library file has an NFD name.
- A scan never marks tracks missing when a root is absent, a folder failed to
  read, or the walk found nothing: an unmounted disk is not an empty library.

## libgpod wrapper rules

- `Database` is neither `Send` nor `Sync`; all libgpod calls run on one thread.
- Nothing in `gpod` writes a database.
- Getters copy into `String` with lossy UTF-8. Every `GError` is converted to
  `gpod::Error` and freed.

## CLI exit codes

0 success, 1 error, 2 usage, 3 verification failed, 4 user declined the plan.
