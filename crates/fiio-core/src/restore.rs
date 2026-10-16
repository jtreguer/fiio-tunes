//! Putting a backup back (technical document, 11.4): the card's playlist
//! files and manifest as they were, and fiio-tunes' record of the card.
//! Audio files are not touched; those removed since are copied again by
//! the next sync.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::Connection;

use crate::backup::{self, Backup, Saved};
use crate::card;
use crate::config::{Config, Paths};
use crate::device::Found;
use crate::error::IoContext;
use crate::manifest;
use crate::paths::card_key;
use crate::playlist::{WriteMode, mtime_ns, write_atomic};
use crate::store::devices::{self, DeviceRow, PlaylistMapping};
use crate::store::{self};
use crate::{Error, Result};

#[derive(Debug)]
pub struct Restored {
    /// The backup taken of the card before the restore.
    pub before: Backup,
    /// Managed playlist files written back.
    pub written: usize,
    /// Managed playlist files the backup did not hold, deleted.
    pub deleted: usize,
    /// Library playlists gone since, kept as tombstones so the next sync
    /// deletes their card files.
    pub tombstones: usize,
    /// Mapped tracks whose library row is gone, dropped from the record.
    pub dropped: usize,
    /// What went wrong once the restore was done (old backups not pruned).
    pub warnings: Vec<String>,
}

/// Whether `path`, read from a backup, is one a restore may touch: a
/// relative card path of plain names ending in `.m3u8`, so nothing joined
/// onto the card can leave it.
fn safe_playlist_path(path: &str) -> bool {
    let p = Path::new(path);
    !path.is_empty()
        && p.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        && crate::card::is_playlist(path)
}

/// The playlist files fiio-tunes manages on the card now, by card path.
fn managed_playlists(conn: &Connection, cfg: &Config, device: &DeviceRow) -> Result<Vec<String>> {
    // Paths as `[card] playlists_dir` places them now.
    let dir = &cfg.card.playlists_dir;
    Ok(devices::playlist_mappings(conn, device.id)?
        .into_iter()
        .map(|m| match dir.as_str() {
            "" => m.card_name,
            d => format!("{d}/{}", m.card_name),
        })
        .collect())
}

/// Restores the backup `stamp` to the card `found`, whose device must be
/// the backup's. The card is backed up first; while the restore runs the
/// device is marked so no sync starts on a half-restored card. The caller
/// holds the card lock.
pub fn run(
    conn: &Connection,
    cfg: &Config,
    paths: &Paths,
    found: &Found,
    stamp: &str,
) -> Result<Restored> {
    crate::device::check_writable(found, cfg.device.allow_folder)?;
    let card = &found.mount.mountpoint;
    let manifest_now = manifest::identity(card)?;
    let key = crate::adoption::device_key(found, manifest_now.as_ref(), cfg.device.allow_folder)?;
    let device = devices::device(conn, &key)?.ok_or_else(|| Error::NotAdopted(key.clone()))?;
    let root = paths.data_dir.join("backups");
    let b = backup::find(&root, &key, stamp)?;
    let saved = backup::saved(&b)?;
    if saved.device_key != key {
        return Err(Error::BadBackup(format!(
            "{stamp} is a backup of {}, not of {key}",
            saved.device_key
        )));
    }
    // Only the playlists fiio-tunes managed then, in the folder they were
    // in then: the others are kept in the backup, but a restore leaves the
    // card's own as they are now.
    let dir_then = saved
        .playlists_dir
        .clone()
        .unwrap_or_else(|| cfg.card.playlists_dir.clone());
    let at = |name: &str| match dir_then.as_str() {
        "" => name.to_owned(),
        d => format!("{d}/{name}"),
    };
    let managed_then: HashSet<String> = saved.playlists.iter().map(|p| at(&p.card_name)).collect();
    if let Some(bad) = managed_then.iter().find(|p| !safe_playlist_path(p)) {
        return Err(Error::BadBackup(format!(
            "{bad}: not a playlist path on the card"
        )));
    }
    let all_then = backup::playlists(&b)?;
    if let Some((bad, _)) = all_then.iter().find(|(p, _)| !safe_playlist_path(p)) {
        return Err(Error::BadBackup(format!(
            "{bad}: not a playlist path on the card"
        )));
    }
    let existed_then: HashSet<String> = all_then.iter().map(|(p, _)| card_key(p)).collect();
    // Playlists the sync the backup was taken for deleted or wrote over,
    // the owner's own included (`remove_unmanaged`): put back as they were.
    let plan = backup::plan(&b)?;
    let touched: HashSet<String> = plan
        .iter()
        .flat_map(|plan| plan["playlists"].as_array().into_iter().flatten())
        .filter_map(|op| match op["op"].as_str() {
            Some("delete" | "write") => op["path"].as_str().map(card_key),
            _ => None,
        })
        .collect();
    let mut lists: Vec<(String, String)> = Vec::new();
    // Every file checked before anything on the card changes.
    for (path, bytes) in all_then {
        if managed_then.contains(&path) || touched.contains(&card_key(&path)) {
            let text = String::from_utf8(bytes)
                .map_err(|_| Error::BadBackup(format!("{path}: not UTF-8")))?;
            lists.push((path, text));
        }
    }
    let manifest_then = backup::manifest(&b)?;
    if let Some(bytes) = &manifest_then {
        std::str::from_utf8(bytes)
            .map_err(|_| Error::BadBackup("manifest.json: not UTF-8".into()))?;
    }
    // Playlists the sync the backup was taken for created (written or
    // renamed to, absent from the backup), left on the card if it failed
    // before recording them; and its hidden rename hops.
    let mut created: Vec<String> = Vec::new();
    if let Some(plan) = &plan {
        for op in plan["playlists"].as_array().into_iter().flatten() {
            let path = match op["op"].as_str() {
                Some("write") => op["path"].as_str(),
                Some("rename") => op["to"].as_str(),
                _ => None,
            };
            if let Some(p) = path
                && !existed_then.contains(&card_key(p))
            {
                // A hidden hop is a playlist path too; anything else in a
                // backup is refused.
                if !safe_playlist_path(p) {
                    return Err(Error::BadBackup(format!(
                        "{p}: not a playlist path on the card"
                    )));
                }
                created.push(p.to_owned());
            }
        }
    }

    // The card must be the one the backup or the index names: a second
    // card in the same player has the same key.
    let listed = card::read(card)?;
    let id_now = manifest_now.as_ref().map(|m| m.card_id.as_str());
    let ours = match id_now {
        Some(id) => Some(id) == device.card_id.as_deref() || Some(id) == saved.card_id.as_deref(),
        // No manifest: a card adopted and never synced, a backup taken
        // before the first sync, or one whose playlists are still there.
        None => {
            let on_card: HashSet<String> =
                listed.playlists.iter().map(|f| card_key(&f.path)).collect();
            device.card_id.is_none()
                || saved.card_id.is_none()
                || managed_then.iter().any(|p| on_card.contains(&card_key(p)))
        }
    };
    if !ours {
        return Err(Error::WrongCard(key));
    }

    let now = crate::now_unix();
    let mut record = Saved::of(conn, &device)?;
    record.playlists_dir = Some(cfg.card.playlists_dir.clone());
    let current = serde_json::to_vec_pretty(&record).expect("plain data");
    let before = backup::create(
        &root,
        &key,
        card,
        &listed,
        &[("mappings.json", &current), ("work.json", b"[]")],
        now,
    )?;
    devices::set_restoring(conn, device.id, Some(stamp))?;

    // Playlists: the backup's back; the managed ones it did not hold, and
    // those a failed sync created, gone. Compared by exact name: on FAT,
    // removing one spelling and writing the other leaves the backup's file;
    // on a folder, both would stay.
    let keep: HashSet<&str> = lists.iter().map(|(p, _)| p.as_str()).collect();
    let mut deleted = 0;
    let mut gone: Vec<String> = managed_playlists(conn, cfg, &device)?;
    gone.extend(created);
    gone.sort();
    gone.dedup();
    for path in gone {
        if keep.contains(path.as_str()) {
            continue;
        }
        let p = card.join(&path);
        match std::fs::remove_file(&p) {
            Ok(()) => deleted += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).at(&p),
        }
    }
    for (path, text) in &lists {
        let p = card.join(path);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).at(d)?;
        }
        let mode = match mtime_ns(&p) {
            Ok(mtime_ns) => WriteMode::Replace { mtime_ns },
            Err(_) => WriteMode::CreateNew,
        };
        write_atomic(&p, text, mode)?;
        if card::read_back(&p).at(&p)? != *blake3::hash(text.as_bytes()).as_bytes() {
            return Err(Error::VerifyFailed(format!("{path} reads back wrong")));
        }
    }
    match &manifest_then {
        Some(bytes) => manifest::write_raw(card, bytes)?,
        None => manifest::remove(card)?,
    }

    // The record: the backup's mappings, for library rows that still exist.
    let (tracks, dropped) = {
        let ids: HashSet<i64> = store::all_tracks(conn)?.iter().map(|t| t.id).collect();
        let all = saved.track_mappings()?;
        let n = all.len();
        let kept: Vec<_> = all
            .into_iter()
            .filter(|m| ids.contains(&m.track_id))
            .collect();
        let dropped = n - kept.len();
        (kept, dropped)
    };
    let mut tombstones = 0;
    let live: HashMap<i64, String> = store::playlists(conn)?
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();
    let removed: HashSet<i64> = store::removed_playlists(conn)?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    store::savepoint(conn, || {
        let mut playlists = Vec::new();
        for p in &saved.playlists {
            let id = if live.contains_key(&p.playlist_id) || removed.contains(&p.playlist_id) {
                p.playlist_id
            } else {
                // The library playlist is gone: a tombstone stands for it,
                // so the next sync deletes the card file again.
                tombstones += 1;
                store::insert_tombstone(conn, &p.name)?
            };
            playlists.push(PlaylistMapping {
                playlist_id: id,
                card_name: p.card_name.clone(),
            });
        }
        devices::replace_mappings(conn, device.id, &tracks, &playlists)?;
        devices::set_card_id(conn, device.id, saved.card_id.as_deref())?;
        devices::set_restoring(conn, device.id, None)
    })?;
    let mut warnings = Vec::new();
    if let Err(e) = backup::prune(&root, &key, cfg.sync.keep_backups as usize, &before) {
        warnings.push(format!("old backups not pruned: {e}"));
    }
    Ok(Restored {
        before,
        written: lists.len(),
        deleted,
        tombstones,
        dropped,
        warnings,
    })
}

/// The device a backup listing is for: the connected card's, or `key`.
pub fn device_key(cfg: &Config, found: Option<&Found>, key: Option<&str>) -> Result<String> {
    match (key, found) {
        (Some(k), _) => Ok(k.to_owned()),
        (None, Some(f)) => {
            let m = manifest::identity(&f.mount.mountpoint)?;
            crate::adoption::device_key(f, m.as_ref(), cfg.device.allow_folder)
        }
        (None, None) => Err(Error::NoCard),
    }
}
