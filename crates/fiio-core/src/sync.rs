//! Preparing a sync: gather what the planner needs from the index and the
//! card, run it, and describe the plan. Writing to the card comes later
//! (F5, F6).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::card::{self, Contents};
use crate::config::{self, Config};
use crate::device::Found;
use crate::manifest::{self, Identity};
use crate::paths::card_key;
use crate::plan::{
    self, EntryReason, Layout, LibPlaylist, Plan, PlanInput, PlaylistDeleteReason, PlaylistOp,
    RemoveReason, ReplaceReason, Scope, SkipReason, Space,
};
use crate::store::devices::{self, DeviceRow, DeviceScope};
use crate::store::{self, TrackRow};
use crate::{Error, Result};

pub struct Prepared {
    pub plan: Plan,
    pub device: DeviceRow,
    pub card: PathBuf,
    pub contents: Contents,
    /// The device's mappings the plan was made from.
    pub tracks_map: Vec<devices::TrackMapping>,
    pub playlists_map: Vec<devices::PlaylistMapping>,
    /// Every indexed track, by id, for describing the plan.
    pub library: HashMap<i64, TrackRow>,
    /// The scope used, and whether it is the device's own.
    pub scope: (DeviceScope, bool),
}

/// Refuses a card that is not the one adopted for `device` (section 9.1):
/// a manifest of another key or another card id, a manifest where the
/// device has none recorded, or none where it has one. The check of a card
/// adopted without a manifest against what adoption saw comes with the
/// first write.
pub fn check_card(device: &DeviceRow, manifest: Option<&Identity>) -> Result<()> {
    let ok = match (&device.card_id, manifest) {
        (None, None) => true,
        (Some(id), Some(m)) => m.card_id == *id && m.device_key == device.device_key,
        _ => false,
    };
    match ok {
        true => Ok(()),
        false => Err(Error::WrongCard(device.device_key.clone())),
    }
}

/// What the device syncs: its own scope, else `[sync]` in the config. The
/// flag is true for the device's own.
pub fn device_scope(conn: &Connection, cfg: &Config, device: i64) -> Result<(DeviceScope, bool)> {
    Ok(match devices::scope(conn, device)? {
        Some(s) => (s, true),
        None => (
            DeviceScope {
                kind: cfg.sync.scope,
                playlists: cfg.sync.playlists.clone(),
            },
            false,
        ),
    })
}

impl DeviceScope {
    /// The planner's form of this scope.
    pub fn as_plan(&self) -> Scope<'_> {
        match self.kind {
            config::Scope::All => Scope::All,
            config::Scope::Playlists => Scope::Playlists(&self.playlists),
            config::Scope::Except => Scope::Except(&self.playlists),
        }
    }

    /// The scope in words: "the whole library", "only 2 playlists: A, B",
    /// "the whole library except 1 playlist: C".
    pub fn describe(&self) -> String {
        let list = |n: &[String]| {
            let noun = if n.len() == 1 {
                "playlist"
            } else {
                "playlists"
            };
            format!("{} {noun}: {}", n.len(), n.join(", "))
        };
        match self.kind {
            config::Scope::All => "the whole library".into(),
            config::Scope::Playlists => format!("only {}", list(&self.playlists)),
            config::Scope::Except if self.playlists.is_empty() => "the whole library".into(),
            config::Scope::Except => format!("the whole library except {}", list(&self.playlists)),
        }
    }

    /// How many present tracks this scope sends, and their size.
    pub fn size(&self, library: &[TrackRow], playlists: &[LibPlaylist]) -> (usize, u64) {
        let ids = plan::in_scope(library, playlists, self.as_plan());
        let bytes = library
            .iter()
            .filter(|t| ids.contains(&t.id))
            .map(|t| t.size)
            .sum();
        (ids.len(), bytes)
    }
}

/// Why a scope cannot be synced, if it cannot: it names a playlist that
/// does not exist, or selects no present track, which would plan the
/// removal of every managed track. `own` is true for the device's own
/// scope, false for the config's.
pub fn scope_problem(
    s: &DeviceScope,
    own: bool,
    library: &[TrackRow],
    playlists: &[LibPlaylist],
) -> Option<String> {
    if s.kind == config::Scope::All {
        return None;
    }
    if s.kind == config::Scope::Playlists && s.playlists.is_empty() {
        return Some(if own {
            "this player's scope keeps only some playlists but names none".into()
        } else {
            "sync.scope is \"playlists\" but sync.playlists is empty".into()
        });
    }
    let unknown: Vec<&str> = s
        .playlists
        .iter()
        .filter(|n| !playlists.iter().any(|p| &p.name == *n))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        let unknown = unknown.join(", ");
        return Some(if own {
            format!("this player's scope names playlists that do not exist: {unknown}")
        } else {
            format!("sync.playlists names playlists that do not exist: {unknown}")
        });
    }
    if !plan::in_scope(library, playlists, s.as_plan()).is_empty() {
        return None;
    }
    Some(match (s.kind, own) {
        (config::Scope::Except, true) => {
            "this player's scope leaves out every library track".into()
        }
        (config::Scope::Except, false) => {
            "the playlists in sync.playlists hold every library track, which \"except\" leaves out"
                .into()
        }
        (_, true) => "the playlists this player keeps hold no library track".into(),
        (_, false) => "the playlists in sync.playlists hold no library track".into(),
    })
}

/// The bytes of the `.m3u8` files of the card's playlists folder, by card
/// path.
fn playlist_bytes(card: &Path, contents: &Contents, dir: &str) -> Result<HashMap<String, Vec<u8>>> {
    let dir = card_key(dir);
    let mut out = HashMap::new();
    for f in &contents.playlists {
        let folder = f.path.rsplit_once('/').map_or("", |(d, _)| d);
        if card_key(folder) != dir {
            continue;
        }
        let p = card.join(&f.path);
        let mut bytes = Vec::new();
        card::open_file(&p)
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .map_err(|source| Error::Io {
                path: p.clone(),
                source,
            })?;
        out.insert(f.path.clone(), bytes);
    }
    Ok(out)
}

/// Plans a sync of the card `found`, which must have been adopted. Reads
/// the card; writes nothing.
pub fn prepare(conn: &Connection, cfg: &Config, found: &Found) -> Result<Prepared> {
    prepare_with(conn, cfg, found, false)
}

/// [`prepare`]; with `playlists_only`, the plan leaves every track as it is
/// and writes the playlists for the files on the card now.
pub fn prepare_with(
    conn: &Connection,
    cfg: &Config,
    found: &Found,
    playlists_only: bool,
) -> Result<Prepared> {
    let card = found.mount.mountpoint.clone();
    let contents = card::read(&card)?;
    if !contents.problems.is_empty() {
        let list: Vec<String> = contents
            .problems
            .iter()
            .take(5)
            .map(|(p, why)| format!("{p}: {why}"))
            .collect();
        return Err(Error::CardIncomplete(list.join("; ")));
    }
    let manifest = manifest::identity(&card)?;
    let key = crate::adoption::device_key(found, manifest.as_ref(), cfg.device.allow_folder)?;
    let device = devices::device(conn, &key)?.ok_or_else(|| Error::NotAdopted(key.clone()))?;
    check_card(&device, manifest.as_ref())?;
    // Without a mapping every song already on the card would be copied a
    // second time; an empty card has nothing to copy twice.
    if devices::mapped_count(conn, device.id)? == 0 && !contents.audio.is_empty() {
        return Err(Error::NothingMatched {
            key,
            files: contents.audio.len(),
        });
    }
    if let Some(stamp) = devices::restoring(conn, device.id)? {
        return Err(Error::RestoreUnfinished(stamp));
    }
    let library = store::all_tracks(conn)?;
    let playlists = devices::library_playlists(conn)?;
    let removed = store::removed_playlists(conn)?;
    let tracks_map = devices::track_mappings(conn, device.id)?;
    let playlists_map = devices::playlist_mappings(conn, device.id)?;
    let (selection, own) = device_scope(conn, cfg, device.id)?;
    if let Some(why) = scope_problem(&selection, own, &library, &playlists) {
        return Err(Error::BadScope(if own {
            format!("{why}; change it with `fiio-tunes scope`")
        } else {
            why
        }));
    }
    let (capacity, free) = crate::device::space(&card)?;
    let card_playlists = playlist_bytes(&card, &contents, &cfg.card.playlists_dir)?;
    let plan = plan::plan(&PlanInput {
        library: &library,
        playlists: &playlists,
        removed_playlists: &removed,
        card: &contents,
        tracks_map: &tracks_map,
        playlists_map: &playlists_map,
        scope: selection.as_plan(),
        remove_unmanaged: cfg.sync.remove_unmanaged,
        space: Some(Space { capacity, free }),
        layout: Layout {
            music_dir: &cfg.card.music_dir,
            playlists_dir: &cfg.card.playlists_dir,
            roots: &cfg.library.roots,
        },
        card_playlists: &card_playlists,
        playlists_only,
    });
    Ok(Prepared {
        plan,
        device,
        card,
        contents,
        tracks_map,
        playlists_map,
        library: library.into_iter().map(|t| (t.id, t)).collect(),
        scope: (selection, own),
    })
}

/// Bytes in decimal units, as file managers show them.
pub fn bytes(n: u64) -> String {
    if n < 1_000 {
        return format!("{n} B");
    }
    // The unit is chosen after rounding: 999,999 bytes is "1.0 MB".
    let mut v = n as f64 / 1e3;
    for unit in ["kB", "MB", "GB"] {
        if (v * 10.0).round() < 10_000.0 || unit == "GB" {
            return format!("{v:.1} {unit}");
        }
        v /= 1e3;
    }
    unreachable!("GB returns")
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The plan as text: a summary, and with `verbose` every change.
pub fn describe(p: &Prepared, verbose: bool) -> String {
    let plan = &p.plan;
    let path = |id: i64| match p.library.get(&id) {
        Some(t) => t.path.display().to_string(),
        None => format!("track {id}"),
    };
    let mut s = String::new();
    let w = &mut s;
    let _ = writeln!(
        w,
        "{} ({})",
        p.device.name.as_deref().unwrap_or("X3"),
        p.device.device_key
    );
    let copy_bytes: u64 = plan.copy.iter().map(|c| c.size).sum();
    let _ = writeln!(
        w,
        "  + {} to copy ({})",
        plural(plan.copy.len(), "track", "tracks"),
        bytes(copy_bytes)
    );
    let _ = writeln!(
        w,
        "  > {} to move into place",
        // A swap's hop through a hidden name is one track moving, not two.
        plural(
            plan.moves
                .iter()
                .map(|m| m.track_id)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            "track",
            "tracks"
        )
    );
    let _ = writeln!(
        w,
        "  ~ {} to copy again (changed in the library or on the card)",
        plural(plan.replace.len(), "track", "tracks")
    );
    let _ = writeln!(
        w,
        "  - {} to remove",
        plural(plan.removals(), "track", "tracks")
    );
    let count = |f: fn(&PlaylistOp) -> bool| plan.playlists.iter().filter(|o| f(o)).count();
    let written = count(|o| matches!(o, PlaylistOp::Write { .. }));
    let renamed = count(|o| matches!(o, PlaylistOp::Rename { .. }));
    let deleted = count(|o| matches!(o, PlaylistOp::Delete { .. }));
    let new = plan
        .playlists
        .iter()
        .filter(|o| match o {
            PlaylistOp::Write { playlist_id, .. } => !p
                .playlists_map
                .iter()
                .any(|m| m.playlist_id == *playlist_id),
            _ => false,
        })
        .count();
    let _ = writeln!(
        w,
        "  playlists: {new} new, {} changed, {renamed} renamed, {deleted} removed",
        written - new
    );
    let unmanaged_removed = plan
        .remove
        .iter()
        .filter(|r| r.reason == RemoveReason::Unmanaged)
        .count();
    if unmanaged_removed > 0 {
        let _ = writeln!(
            w,
            "  unmanaged files to remove (remove_unmanaged): {unmanaged_removed}"
        );
    }
    let _ = writeln!(
        w,
        "  unmanaged on the card, kept: {}, {}",
        plural(plan.unmanaged.files.len(), "file", "files"),
        plural(plan.unmanaged.playlists.len(), "playlist", "playlists")
    );
    if !plan.skipped.is_empty() || !plan.entries_skipped.is_empty() {
        let _ = writeln!(
            w,
            "  skipped: {}, {}",
            plural(plan.skipped.len(), "track", "tracks"),
            plural(
                plan.entries_skipped.len(),
                "playlist entry",
                "playlist entries"
            )
        );
    }
    if !plan.stale_mappings.is_empty() {
        let _ = writeln!(
            w,
            "  mapped files no longer on the card: {}",
            plan.stale_mappings.len()
        );
    }
    let signed = |n: i64| match n {
        n if n < 0 => format!("frees {}", bytes(n.unsigned_abs())),
        n => format!("needs {}", bytes(n as u64)),
    };
    let need = match (plan.space.peak, plan.space.needed) {
        (peak, net) if peak == net => signed(net),
        (peak, net) => format!("{} at the peak, {} once done", signed(peak), signed(net)),
    };
    let _ = match plan.space.device {
        Some(d) => writeln!(w, "  space: {need}, {} free", bytes(d.free)),
        None => writeln!(w, "  space: {need}"),
    };
    if !plan.space.fits {
        let _ = writeln!(w, "  THE PLAN DOES NOT FIT ON THE CARD");
    }
    if plan.mass_removal(p.tracks_map.len()) {
        let _ = writeln!(
            w,
            "  removes more than a tenth of the managed tracks: a sync needs --allow-mass-removal"
        );
    }
    if !verbose {
        return s;
    }

    let section = |w: &mut String, title: &str, lines: Vec<String>| {
        if !lines.is_empty() {
            let _ = writeln!(w, "\n== {title} ({})", lines.len());
            for l in lines {
                let _ = writeln!(w, "  {l}");
            }
        }
    };
    section(
        w,
        "Copy",
        plan.copy
            .iter()
            .map(|c| format!("{} -> {}", c.from.display(), c.to))
            .collect(),
    );
    section(
        w,
        "Move",
        plan.moves
            .iter()
            .map(|m| format!("{} -> {}", m.from, m.to))
            .collect(),
    );
    section(
        w,
        "Copy again",
        plan.replace
            .iter()
            .map(|r| {
                let why = match r.reason {
                    ReplaceReason::Changed => "changed in the library",
                    ReplaceReason::ChangedOnCard => "changed on the card",
                };
                format!("{} ({why})", r.to)
            })
            .collect(),
    );
    section(
        w,
        "Remove",
        plan.remove
            .iter()
            .map(|r| {
                let why = match r.reason {
                    RemoveReason::MissingFromLibrary => "file gone from the library",
                    RemoveReason::OutOfScope => "no longer in the sync scope",
                    RemoveReason::Replaced => "changed, copied again under its new name",
                    RemoveReason::Unmanaged => "not managed by fiio-tunes",
                };
                format!("{} ({why})", r.path)
            })
            .collect(),
    );
    section(
        w,
        "Playlists",
        plan.playlists
            .iter()
            .map(|o| match o {
                PlaylistOp::Write { path, entries, .. } => {
                    format!("write {path}, {}", plural(*entries, "entry", "entries"))
                }
                PlaylistOp::Rename { from, to, .. } => format!("rename {from} to {to}"),
                PlaylistOp::Delete { path, reason, .. } => {
                    let why = match reason {
                        PlaylistDeleteReason::FileDeleted => "playlist deleted",
                        PlaylistDeleteReason::OutOfScope => "no longer in the sync scope",
                        PlaylistDeleteReason::Unmanaged => "not managed by fiio-tunes",
                    };
                    format!("remove {path} ({why})")
                }
            })
            .collect(),
    );
    section(
        w,
        "Unmanaged files kept on the card",
        plan.unmanaged.files.clone(),
    );
    section(
        w,
        "Unmanaged playlists kept on the card",
        plan.unmanaged.playlists.clone(),
    );
    section(
        w,
        "Skipped tracks",
        plan.skipped
            .iter()
            .map(|t| {
                let why = match t.reason {
                    SkipReason::FairPlay => "FairPlay protected",
                    SkipReason::TooLarge => "4 GiB or more, which FAT32 cannot hold",
                    SkipReason::NoCardPath => "under none of the library folders",
                    SkipReason::Occupied => "a file fiio-tunes does not manage is in its place",
                };
                format!("{} ({why})", t.path.display())
            })
            .collect(),
    );
    section(
        w,
        "Skipped playlist entries",
        plan.entries_skipped
            .iter()
            .map(|e| {
                let why = match e.reason {
                    EntryReason::Unresolved => "matches no library file",
                    EntryReason::Missing => "file missing",
                    EntryReason::NotOnCard => "track not on the card",
                };
                format!("{} #{} ({why})", e.playlist, e.position)
            })
            .collect(),
    );
    section(
        w,
        "Mapped files no longer on the card",
        plan.stale_mappings
            .iter()
            .map(|m| format!("{} ({})", m.card_path, path(m.track_id)))
            .collect(),
    );
    s
}
