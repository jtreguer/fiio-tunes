//! Backups taken before every write to a card (technical document, 11.2):
//! the card's playlist files, its manifest, fiio-tunes' record of the card
//! (`mappings.json`), the plan, the work done and a listing, in
//! `backups/<key>/<stamp>/` of the data folder. Audio files are not backed
//! up: the library has them.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::card::{self, Contents};
use crate::error::IoContext;
use crate::manifest::{hex, unhex};
use crate::store::devices::{self, DeviceRow, PlaylistMapping, TrackMapping};
use crate::{Error, Result};

/// Where a backup keeps the card's playlist files.
const PLAYLISTS: &str = "playlists";
const MAPPINGS: &str = "mappings.json";
const MANIFEST: &str = "manifest.json";
const LISTING: &str = "listing.txt";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backup {
    pub dir: PathBuf,
    /// `YYYYMMDD-HHMMSS` in UTC, possibly with a `-N` suffix.
    pub stamp: String,
}

/// fiio-tunes' record of a card at the time of a backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub device_key: String,
    pub card_id: Option<String>,
    /// `[card] playlists_dir` at the time, where the playlists were.
    #[serde(default)]
    pub playlists_dir: Option<String>,
    pub tracks: Vec<SavedTrack>,
    pub playlists: Vec<SavedPlaylist>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTrack {
    pub track_id: i64,
    pub card_path: String,
    /// BLAKE3, hex.
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedPlaylist {
    pub playlist_id: i64,
    pub card_name: String,
    /// The library playlist's name then, to stand for it if it is gone.
    pub name: String,
}

impl Saved {
    /// The device's record in the index now.
    pub fn of(conn: &Connection, device: &DeviceRow) -> Result<Self> {
        let names: std::collections::HashMap<i64, String> = conn
            .prepare("SELECT id, name FROM playlists")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Saved {
            device_key: device.device_key.clone(),
            card_id: device.card_id.clone(),
            playlists_dir: None,
            tracks: devices::track_mappings(conn, device.id)?
                .into_iter()
                .map(|m| SavedTrack {
                    track_id: m.track_id,
                    card_path: m.card_path,
                    hash: hex(&m.synced_hash),
                    size: m.synced_size,
                })
                .collect(),
            playlists: devices::playlist_mappings(conn, device.id)?
                .into_iter()
                .map(|m| SavedPlaylist {
                    name: names.get(&m.playlist_id).cloned().unwrap_or_default(),
                    playlist_id: m.playlist_id,
                    card_name: m.card_name,
                })
                .collect(),
        })
    }

    pub fn track_mappings(&self) -> Result<Vec<TrackMapping>> {
        self.tracks
            .iter()
            .map(|t| {
                Ok(TrackMapping {
                    track_id: t.track_id,
                    card_path: t.card_path.clone(),
                    synced_hash: unhex(&t.hash)
                        .ok_or_else(|| Error::BadBackup(format!("bad hash for {}", t.card_path)))?,
                    synced_size: t.size,
                })
            })
            .collect()
    }
}

/// UTC date and time of a Unix timestamp as `YYYYMMDD-HHMMSS`.
pub fn stamp(unix: i64) -> String {
    let (days, secs) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    // Days to civil date, from Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// Whether `name` is a backup stamp, `YYYYMMDD-HHMMSS` with an optional
/// `-N`.
fn is_stamp(name: &str) -> bool {
    let b = name.as_bytes();
    let digits =
        |r: std::ops::Range<usize>| b.get(r).is_some_and(|d| d.iter().all(u8::is_ascii_digit));
    b.len() >= 15
        && digits(0..8)
        && b[8] == b'-'
        && digits(9..15)
        && (b.len() == 15
            || (b[15] == b'-'
                && b.len() > 16
                && digits(16..b.len())
                && name[16..].parse::<u32>().is_ok()))
}

/// How stamps sort: by time, then by the `-N` suffix taken as a number.
fn stamp_key(s: &str) -> (String, u32) {
    match s.get(15..).and_then(|r| r.strip_prefix('-')) {
        Some(n) => (s[..15].to_owned(), n.parse::<u32>().unwrap_or(u32::MAX)),
        None => (s.to_owned(), 1),
    }
}

/// The folder of a device's backups: its key with every character other
/// than letters, digits, `.`, `-` and `_` made `_`.
pub fn folder(root: &Path, key: &str) -> PathBuf {
    let name: String = key
        .chars()
        .map(|c| match c {
            c if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') => c,
            _ => '_',
        })
        .collect();
    // Never the backups folder itself, nor a hidden name.
    match name.trim_start_matches('.') {
        "" => root.join("_"),
        n => root.join(n),
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).at(dir)?;
    }
    let mut f = std::fs::File::create(path).at(path)?;
    f.write_all(bytes).at(path)?;
    f.sync_all().at(path)
}

/// Backs up the card at `card`, listed as `contents`, for the device `key`,
/// with `extra` files (`mappings.json`, `plan.json`, `work.json`). The
/// backup is filled under a hidden name and renamed when complete.
pub fn create(
    root: &Path,
    key: &str,
    card: &Path,
    contents: &Contents,
    extra: &[(&str, &[u8])],
    now: i64,
) -> Result<Backup> {
    let base = folder(root, key);
    std::fs::create_dir_all(&base).at(&base)?;
    // Folders left by a backup interrupted part way.
    for e in std::fs::read_dir(&base).at(&base)?.flatten() {
        if e.file_name().as_encoded_bytes().starts_with(b".partial-") {
            std::fs::remove_dir_all(e.path()).at(e.path())?;
        }
    }
    let first = stamp(now);
    // The new backup sorts last, even when the clock is behind earlier ones.
    let latest = std::fs::read_dir(&base)
        .at(&base)?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| is_stamp(n))
        .map(|n| stamp_key(&n))
        .max();
    let stamp = match latest {
        Some((b, n)) if b >= first => format!("{b}-{}", n.saturating_add(1)),
        _ => first,
    };
    let dir = base.join(&stamp);
    let partial = base.join(format!(".partial-{stamp}"));
    std::fs::create_dir(&partial).at(&partial)?;
    for f in &contents.playlists {
        let from = card.join(&f.path);
        let mut bytes = Vec::new();
        card::open_file(&from)
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .at(&from)?;
        write_synced(&partial.join(PLAYLISTS).join(&f.path), &bytes)?;
    }
    if let Some(bytes) = crate::manifest::raw(card)? {
        write_synced(&partial.join(MANIFEST), &bytes)?;
    }
    let mut listing = String::new();
    for f in contents.audio.iter().chain(&contents.playlists) {
        listing.push_str(&format!("{}\t{}\n", f.path, f.size));
    }
    write_synced(&partial.join(LISTING), listing.as_bytes())?;
    for (name, bytes) in extra {
        write_synced(&partial.join(name), bytes)?;
    }
    std::fs::rename(&partial, &dir).at(&dir)?;
    if let Ok(d) = std::fs::File::open(&base) {
        let _ = d.sync_all();
    }
    Ok(Backup { dir, stamp })
}

/// The backups of one device, oldest first.
pub fn list(root: &Path, key: &str) -> Result<Vec<Backup>> {
    let base = folder(root, key);
    if !base.is_dir() {
        return Ok(Vec::new());
    }
    let mut out: Vec<Backup> = std::fs::read_dir(&base)
        .at(&base)?
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(is_stamp))
        .filter(|e| e.path().join(MAPPINGS).is_file())
        .filter_map(|e| {
            Some(Backup {
                stamp: e.file_name().into_string().ok()?,
                dir: e.path(),
            })
        })
        .collect();
    out.sort_by_key(|b| stamp_key(&b.stamp));
    Ok(out)
}

/// The backup `stamp` of the device `key`.
pub fn find(root: &Path, key: &str, stamp: &str) -> Result<Backup> {
    list(root, key)?
        .into_iter()
        .find(|b| b.stamp == stamp)
        .ok_or_else(|| Error::NoSuchBackup(stamp.to_owned()))
}

/// Deletes the oldest backups so at most `keep` remain, never `new`.
/// Returns the removed folders.
pub fn prune(root: &Path, key: &str, keep: usize, new: &Backup) -> Result<Vec<PathBuf>> {
    let all: Vec<Backup> = list(root, key)?
        .into_iter()
        .filter(|b| b.dir != new.dir)
        .collect();
    let excess = (all.len() + 1).saturating_sub(keep.max(1));
    let mut removed = Vec::new();
    for b in all.into_iter().take(excess) {
        std::fs::remove_dir_all(&b.dir).at(&b.dir)?;
        removed.push(b.dir);
    }
    Ok(removed)
}

/// The record of the card in a backup.
pub fn saved(b: &Backup) -> Result<Saved> {
    let p = b.dir.join(MAPPINGS);
    let text = std::fs::read(&p).at(&p)?;
    serde_json::from_slice(&text).map_err(|e| Error::BadBackup(format!("{}: {e}", p.display())))
}

/// The card's manifest as the backup holds it; `None` when the card had
/// none.
pub fn manifest(b: &Backup) -> Result<Option<Vec<u8>>> {
    let p = b.dir.join(MANIFEST);
    match std::fs::read(&p) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).at(&p),
    }
}

/// The plan of the sync the backup was taken for, when there was one.
pub fn plan(b: &Backup) -> Result<Option<serde_json::Value>> {
    let p = b.dir.join("plan.json");
    match std::fs::read(&p) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::BadBackup(format!("{}: {e}", p.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).at(&p),
    }
}

/// The card's playlist files as the backup holds them: card path and
/// bytes, sorted by path.
pub fn playlists(b: &Backup) -> Result<Vec<(String, Vec<u8>)>> {
    let base = b.dir.join(PLAYLISTS);
    let mut out = Vec::new();
    if !base.is_dir() {
        return Ok(out);
    }
    for e in walkdir::WalkDir::new(&base) {
        let e = e.map_err(|e| Error::BadBackup(e.to_string()))?;
        if !e.file_type().is_file() {
            continue;
        }
        let rel = e
            .path()
            .strip_prefix(&base)
            .expect("under the base")
            .to_str()
            .ok_or_else(|| Error::BadBackup(format!("{}: not UTF-8", e.path().display())))?
            .to_owned();
        out.push((rel, std::fs::read(e.path()).at(e.path())?));
    }
    out.sort();
    Ok(out)
}

/// Mapping rows of a saved record, for [`devices::replace_mappings`].
pub fn playlist_mappings(s: &Saved) -> Vec<PlaylistMapping> {
    s.playlists
        .iter()
        .map(|p| PlaylistMapping {
            playlist_id: p.playlist_id,
            card_name: p.card_name.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_are_utc_and_sort() {
        assert_eq!(stamp(0), "19700101-000000");
        assert_eq!(stamp(1_792_143_045), "20261016-093045");
        assert!(is_stamp("20261016-094405") && is_stamp("20261016-094405-2"));
        assert!(!is_stamp("20261016-094405-") && !is_stamp(".partial-20261016-094405"));
        assert!(stamp_key("20261016-094405-10") > stamp_key("20261016-094405-2"));
    }

    #[test]
    fn folder_names_are_safe() {
        let r = Path::new("/b");
        assert_eq!(
            folder(r, "usb:323820474077"),
            Path::new("/b/usb_323820474077")
        );
        assert_eq!(
            folder(r, "folder:/tmp/x y"),
            Path::new("/b/folder__tmp_x_y")
        );
    }
}
