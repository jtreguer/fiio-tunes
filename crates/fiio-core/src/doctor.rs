//! `fiio-tunes doctor`: what a sync cut short, a restore or another program
//! can leave on the card or in the index, found and, when asked, repaired
//! (technical document, 11.4).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use serde::Deserialize;

use crate::backup::{self, Backup, Saved};
use crate::card::{self, Contents};
use crate::config::{Config, Paths};
use crate::device::Found;
use crate::error::IoContext;
use crate::manifest;
use crate::paths::card_key;
use crate::store::devices::{self, DeviceRow, TrackMapping};
use crate::sync;
use crate::{Error, Result};

/// A library track missing for longer than this, and on no device, is
/// dropped from the index by `--fix`.
pub const FORGET_AFTER_SECS: i64 = 30 * 24 * 3600;

/// Where orphan files go, on the card itself: a rename, no copy. Nothing
/// reads it; deleting it frees the space.
pub const ORPHANS: &str = ".fiio-tunes/orphans";

/// A file a sync stopped between the two renames of a cycle left under a
/// hidden name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop {
    pub path: String,
    /// Where it goes back: the move's target, or its source, whichever the
    /// record of the card and the free names allow; `None` when neither is
    /// free or no backup knows the move. It is then only reported.
    pub to: Option<String>,
    /// The track whose mapping names the hidden file, which follows it.
    pub track_id: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Temp files of copies not finished (deleted).
    pub parts: Vec<String>,
    pub hops: Vec<Hop>,
    /// Files a sync left where the library puts them without recording
    /// them (mapped again).
    pub taken_back: Vec<TrackMapping>,
    /// `(track id, card path)` of mappings whose file is gone (dropped).
    pub stale: Vec<(i64, String)>,
    /// Audio files under the music folder that no mapping names but that
    /// fiio-tunes managed or wrote, by its backups (moved to [`ORPHANS`]).
    /// Other unmanaged files are the owner's, and only a sync's report
    /// names them.
    pub orphans: Vec<String>,
    /// `(track id, path)` of library tracks missing for over 30 days and on
    /// no device (dropped from the index).
    pub forgotten: Vec<(i64, PathBuf)>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.parts.is_empty()
            && self.hops.is_empty()
            && self.taken_back.is_empty()
            && self.stale.is_empty()
            && self.orphans.is_empty()
            && self.forgotten.is_empty()
    }
}

/// The card examined: where, whose, and what was found.
pub struct Examined {
    pub card: PathBuf,
    pub device: DeviceRow,
    pub contents: Contents,
    pub report: Report,
}

#[derive(Deserialize)]
struct Move {
    from: String,
    to: String,
}

#[derive(Deserialize)]
struct WorkMoves {
    moves: Vec<Move>,
}

/// Whether `path` is a plain relative path on the card: no `..`, no root,
/// no empty part. Paths read from a backup are checked before use.
fn safe_card_path(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

#[derive(Deserialize)]
struct Target {
    to: String,
}

/// What a sync's `work.json` put on the card.
#[derive(Deserialize)]
struct WorkTargets {
    moves: Vec<Move>,
    copy: Vec<Target>,
    replace: Vec<Target>,
}

/// The card paths, by FAT key, that fiio-tunes managed or wrote according
/// to the backups: the mappings each recorded, and the targets of the copies
/// and moves of the sync each was taken for.
fn written(backups: &[Backup]) -> HashSet<String> {
    let mut out = HashSet::new();
    for b in backups {
        if let Ok(saved) = backup::saved(b) {
            out.extend(saved.tracks.iter().map(|t| card_key(&t.card_path)));
        }
        if let Some(w) = std::fs::read(b.dir.join("work.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<WorkTargets>(&bytes).ok())
        {
            let targets = w.moves.iter().map(|m| &m.to);
            let copies = w.copy.iter().chain(&w.replace).map(|t| &t.to);
            out.extend(targets.chain(copies).map(|p| card_key(p)));
        }
    }
    out
}

/// `(source, target)` of the move through the hidden name `hop`, from the
/// newest backup that holds one: track moves from its `work.json`,
/// playlist renames from its `plan.json`.
fn hop_ends(backups: &[Backup], hop: &str) -> Option<(String, String)> {
    let k = card_key(hop);
    for b in backups.iter().rev() {
        let moves: Vec<Move> = if card::is_playlist(hop) {
            let Ok(Some(plan)) = backup::plan(b) else {
                continue;
            };
            plan["playlists"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|o| o["op"] == "rename")
                .filter_map(|o| {
                    Some(Move {
                        from: o["from"].as_str()?.to_owned(),
                        to: o["to"].as_str()?.to_owned(),
                    })
                })
                .collect()
        } else {
            match std::fs::read(b.dir.join("work.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<WorkMoves>(&bytes).ok())
            {
                Some(w) => w.moves,
                None => continue,
            }
        };
        let moves: Vec<Move> = moves
            .into_iter()
            .filter(|m| safe_card_path(&m.from) && safe_card_path(&m.to))
            .collect();
        let source = moves.iter().find(|m| card_key(&m.to) == k);
        let target = moves.iter().find(|m| card_key(&m.from) == k);
        if let (Some(s), Some(t)) = (source, target) {
            return Some((s.from.clone(), t.to.clone()));
        }
    }
    None
}

/// Whether the folder part of `path` is below the music folder.
fn under(path: &str, music_dir: &str) -> bool {
    let (k, m) = (card_key(path), card_key(music_dir));
    m.is_empty() || k.strip_prefix(&m).is_some_and(|r| r.starts_with('/'))
}

/// Reads the card `found` and the index and reports what needs repair.
/// Writes nothing. The caller holds the card lock.
pub fn examine(conn: &Connection, cfg: &Config, paths: &Paths, found: &Found) -> Result<Examined> {
    let card = found.mount.mountpoint.clone();
    let contents = card::read(&card)?;
    // An unreadable folder would make its files look gone.
    if !contents.problems.is_empty() {
        let list: Vec<String> = contents
            .problems
            .iter()
            .take(5)
            .map(|(p, why)| format!("{p}: {why}"))
            .collect();
        return Err(Error::CardIncomplete(list.join("; ")));
    }
    let identity = manifest::identity(&card)?;
    let key = crate::adoption::device_key(found, identity.as_ref(), cfg.device.allow_folder)?;
    let device = devices::device(conn, &key)?.ok_or_else(|| Error::NotAdopted(key.clone()))?;
    sync::check_card(&device, identity.as_ref())?;
    if let Some(stamp) = devices::restoring(conn, device.id)? {
        return Err(Error::RestoreUnfinished(stamp));
    }
    let mappings = devices::track_mappings(conn, device.id)?;
    let by_path: HashMap<String, i64> = mappings
        .iter()
        .map(|m| (card_key(&m.card_path), m.track_id))
        .collect();
    let backups = backup::list(&paths.data_dir.join("backups"), &key)?;
    let free = |p: &str| card.join(p).symlink_metadata().is_err();

    let mut report = Report {
        parts: contents.parts.clone(),
        ..Report::default()
    };
    for h in &contents.hops {
        let track_id = by_path.get(&card_key(h)).copied();
        // A recorded hop goes on to its target; otherwise the record still
        // names its source, where it goes back.
        let to = hop_ends(&backups, h).and_then(|(source, target)| {
            let order = match track_id {
                Some(_) => [target, source],
                None => [source, target],
            };
            order.into_iter().find(|p| free(p))
        });
        report.hops.push(Hop {
            path: h.clone(),
            to,
            track_id,
        });
    }
    // A hop's track follows it, and a mapping naming where a hop goes back
    // is not stale.
    let hop_tracks: HashSet<i64> = report.hops.iter().filter_map(|h| h.track_id).collect();
    let mut listed: HashSet<String> = contents.audio.iter().map(|f| card_key(&f.path)).collect();
    listed.extend(
        report
            .hops
            .iter()
            .filter_map(|h| h.to.as_deref())
            .map(card_key),
    );
    report.stale = mappings
        .iter()
        .filter(|m| !listed.contains(&card_key(&m.card_path)) && !hop_tracks.contains(&m.track_id))
        .map(|m| (m.track_id, m.card_path.clone()))
        .collect();
    report.taken_back = sync::takeable(conn, cfg, &device, &card, &contents)?;
    let taken: HashSet<String> = report
        .taken_back
        .iter()
        .map(|m| card_key(&m.card_path))
        .collect();
    let ours = written(&backups);
    report.orphans = contents
        .audio
        .iter()
        .filter(|f| under(&f.path, &cfg.card.music_dir) && ours.contains(&card_key(&f.path)))
        .filter(|f| {
            !by_path.contains_key(&card_key(&f.path)) && !taken.contains(&card_key(&f.path))
        })
        .map(|f| f.path.clone())
        .collect();
    report.forgotten = forgettable(conn, crate::now_unix())?;
    Ok(Examined {
        card,
        device,
        contents,
        report,
    })
}

/// Library tracks missing since before `now` less 30 days, on no device,
/// whose file is not back.
fn forgettable(conn: &Connection, now: i64) -> Result<Vec<(i64, PathBuf)>> {
    let mut stmt = conn.prepare(
        "SELECT id, path FROM tracks WHERE missing_since IS NOT NULL AND missing_since < ?1 \
         AND id NOT IN (SELECT track_id FROM device_tracks) ORDER BY path",
    )?;
    let rows: Vec<(i64, PathBuf)> = stmt
        .query_map(params![now - FORGET_AFTER_SECS], |r| {
            Ok((r.get(0)?, PathBuf::from(r.get::<_, String>(1)?)))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows
        .into_iter()
        .filter(|(_, p)| p.symlink_metadata().is_err())
        .collect())
}

#[derive(Debug, Default)]
pub struct Fixed {
    /// `None` when nothing on the card needed a change.
    pub backup: Option<Backup>,
    pub parts: usize,
    pub hops: usize,
    pub taken_back: usize,
    pub stale: usize,
    pub orphans: usize,
    /// Where the orphans went, relative to the card.
    pub orphans_dir: Option<String>,
    pub forgotten: usize,
    pub warnings: Vec<String>,
}

fn sync_dir(dir: &Path) {
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Renames the card file `from` to `to` when nothing is there, creating
/// its folder.
fn rename_free(card: &Path, from: &str, to: &str) -> Result<()> {
    let (a, b) = (card.join(from), card.join(to));
    if b.symlink_metadata().is_ok() {
        return Err(Error::WouldOverwrite(to.to_owned()));
    }
    if let Some(d) = b.parent() {
        std::fs::create_dir_all(d).at(d)?;
    }
    std::fs::rename(&a, &b).at(&b)?;
    for d in [a.parent(), b.parent()].into_iter().flatten() {
        sync_dir(d);
    }
    Ok(())
}

/// Repairs what `examined` reports, after backing up the card's playlists,
/// manifest and the record of it: places the hidden files of a cycle, deletes
/// unfinished copies, maps again the files a sync left in place, drops
/// mappings whose file is gone, moves orphans aside, forgets long-missing
/// tracks, and rewrites the manifest. The caller holds the card lock.
pub fn fix(
    conn: &Connection,
    cfg: &Config,
    paths: &Paths,
    found: &Found,
    examined: &Examined,
) -> Result<Fixed> {
    crate::device::check_writable(found, cfg.device.allow_folder)?;
    let (card, device, report) = (&examined.card, &examined.device, &examined.report);
    // The card may have been swapped while the prompt waited.
    let now_device = devices::device(conn, &device.device_key)?
        .ok_or_else(|| Error::NotAdopted(device.device_key.clone()))?;
    sync::check_card(&now_device, manifest::identity(card)?.as_ref())?;
    let mut fixed = Fixed::default();
    let now = crate::now_unix();
    let touches_card = !report.parts.is_empty()
        || report.hops.iter().any(|h| h.to.is_some())
        || !report.taken_back.is_empty()
        || !report.stale.is_empty()
        || !report.orphans.is_empty();
    if touches_card {
        let mut record = Saved::of(conn, device)?;
        record.playlists_dir = Some(cfg.card.playlists_dir.clone());
        let saved = serde_json::to_vec_pretty(&record).expect("plain data");
        let root = paths.data_dir.join("backups");
        let b = backup::create(
            &root,
            &device.device_key,
            card,
            &card::read(card)?,
            &[("mappings.json", &saved), ("work.json", b"[]")],
            now,
        )?;
        fixed.backup = Some(b.clone());
        if let Err(e) = backup::prune(
            &root,
            &device.device_key,
            cfg.sync.keep_backups as usize,
            &b,
        ) {
            fixed.warnings.push(format!("old backups not pruned: {e}"));
        }
    }
    let music = cfg.card.music_dir.as_str();
    for p in &report.parts {
        let f = card.join(p);
        match std::fs::remove_file(&f) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).at(&f),
            _ => {}
        }
        fixed.parts += 1;
        crate::exec::remove_empty_dirs(card, music, p.rsplit_once('/').map_or("", |(d, _)| d));
    }
    // The hops are placed on the card first; the mappings then change in
    // one write, so no step holds a name twice (a mapping may still name
    // the place a hop goes to, its own file being elsewhere).
    let mut placed: Vec<(i64, &str)> = Vec::new();
    let mut failure = None;
    for h in &report.hops {
        let Some(to) = &h.to else { continue };
        if let Err(e) = rename_free(card, &h.path, to) {
            failure = Some(e);
            break;
        }
        if let Some(id) = h.track_id {
            placed.push((id, to));
        }
        fixed.hops += 1;
    }
    let before = devices::track_mappings(conn, device.id)?;
    let moved: Vec<TrackMapping> = placed
        .iter()
        .filter_map(|(id, to)| {
            let m = before.iter().find(|m| m.track_id == *id)?;
            Some(TrackMapping {
                card_path: (*to).to_owned(),
                ..m.clone()
            })
        })
        .collect();
    let held: HashSet<String> = moved.iter().map(|m| card_key(&m.card_path)).collect();
    let mut all: Vec<TrackMapping> = before
        .into_iter()
        .filter(|m| {
            !placed.iter().any(|(id, _)| *id == m.track_id)
                && !held.contains(&card_key(&m.card_path))
        })
        .chain(moved)
        .collect();
    if let Some(e) = failure {
        // What was placed is recorded; the rest is left for the next run.
        devices::replace_track_mappings(conn, device.id, &all)?;
        return Err(e);
    }
    let stale: HashSet<i64> = report.stale.iter().map(|(id, _)| *id).collect();
    let taken: HashSet<i64> = report.taken_back.iter().map(|t| t.track_id).collect();
    all.retain(|m| !stale.contains(&m.track_id) && !taken.contains(&m.track_id));
    all.extend(report.taken_back.iter().cloned());
    fixed.stale = stale.len();
    fixed.taken_back = report.taken_back.len();
    devices::replace_track_mappings(conn, device.id, &all)?;
    if !report.orphans.is_empty() {
        let dir = format!("{ORPHANS}/{}", backup::stamp(now));
        for o in &report.orphans {
            rename_free(card, o, &format!("{dir}/{o}"))?;
            fixed.orphans += 1;
            crate::exec::remove_empty_dirs(card, music, o.rsplit_once('/').map_or("", |(d, _)| d));
        }
        fixed.orphans_dir = Some(dir);
    }
    fixed.forgotten = forget(conn, &report.forgotten)?;
    // The manifest describes the card as recorded now; a card not synced
    // yet gets its first one from the first sync.
    if touches_card && let Some(card_id) = &now_device.card_id {
        let tracks = devices::track_mappings(conn, device.id)?
            .into_iter()
            .map(|m| manifest::ManifestTrack {
                path: m.card_path,
                size: m.synced_size,
                hash: manifest::hex(&m.synced_hash),
            })
            .collect();
        let playlists = devices::playlist_mappings(conn, device.id)?
            .into_iter()
            .map(|m| match cfg.card.playlists_dir.as_str() {
                "" => m.card_name,
                d => format!("{d}/{}", m.card_name),
            })
            .collect();
        let m = manifest::Manifest {
            version: 1,
            device_key: device.device_key.clone(),
            card_id: card_id.clone(),
            written_at: now,
            tracks,
            playlists,
        };
        if let Err(e) = manifest::write(card, &m) {
            fixed
                .warnings
                .push(format!("the manifest's list of files was not updated: {e}"));
        }
    }
    Ok(fixed)
}

/// Drops the tracks `forgotten` from the index, those still missing and on
/// no device: their playlist entries stay, unresolved. Returns how many.
fn forget(conn: &Connection, forgotten: &[(i64, PathBuf)]) -> Result<usize> {
    crate::store::savepoint(conn, || {
        let mut n = 0;
        for (id, _) in forgotten {
            // A scan since the examination may have found it again.
            let still: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM tracks WHERE id = ?1 AND missing_since IS NOT NULL \
                 AND id NOT IN (SELECT track_id FROM device_tracks))",
                [id],
                |r| r.get(0),
            )?;
            if !still {
                continue;
            }
            conn.execute(
                "UPDATE playlist_items SET track_id = NULL WHERE track_id = ?1",
                [id],
            )?;
            conn.execute("DELETE FROM tracks WHERE id = ?1", [id])?;
            n += 1;
        }
        Ok(n)
    })
}
