//! Applying a plan to the card (technical document, 11.2). This version
//! applies the playlist operations only; a plan that changes tracks is
//! refused until the copy, move and remove steps exist (F6).

use std::path::Path;

use rusqlite::Connection;

use crate::backup::{self, Backup, Saved};
use crate::card;
use crate::config::{Config, Paths};
use crate::device::Found;
use crate::error::IoContext;
use crate::manifest::{self, Manifest, ManifestTrack, hex};
use crate::paths::card_key;
use crate::plan::PlaylistOp;
use crate::playlist::{WriteMode, mtime_ns, write_atomic};
use crate::store::devices::{self, PlaylistMapping};
use crate::sync::{self, Prepared};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Apply a plan that removes more than a tenth of what is managed.
    pub allow_mass_removal: bool,
}

#[derive(Debug)]
pub struct Done {
    /// `None` when the plan was empty: nothing was written.
    pub backup: Option<Backup>,
    pub written: usize,
    pub renamed: usize,
    pub deleted: usize,
    /// The first sync after an adoption that found no manifest: the card
    /// got its id.
    pub first: bool,
    /// Old backups deleted.
    pub pruned: usize,
    /// What went wrong after the card was written and checked, which does
    /// not undo the sync (a backup that could not be pruned).
    pub warnings: Vec<String>,
}

/// A sync that failed; `backup` is set once the card was backed up, and
/// the card may then be partly written.
#[derive(Debug)]
pub struct Failed {
    pub error: Error,
    pub backup: Option<Backup>,
}

impl From<Error> for Failed {
    fn from(error: Error) -> Self {
        Failed {
            error,
            backup: None,
        }
    }
}

/// Playlist files the plan deletes: managed ones that left the scope or
/// the library, and unmanaged ones `remove_unmanaged` takes, which no
/// restore puts back.
fn playlist_removals(p: &Prepared) -> usize {
    p.plan
        .playlists
        .iter()
        .filter(|o| matches!(o, PlaylistOp::Delete { .. }))
        .count()
}

/// The refusals that need no look at the card: track changes (not yet
/// done), a plan that does not fit, mass removals not allowed.
pub fn check(p: &Prepared, opts: Options) -> Result<()> {
    let plan = &p.plan;
    let tracks = plan.copy.len() + plan.replace.len() + plan.moves.len() + plan.remove.len();
    if tracks > 0 {
        return Err(Error::TracksLater(tracks));
    }
    if !plan.space.fits {
        return Err(Error::NoSpace {
            needed: plan.space.peak,
            free: plan.space.device.map_or(0, |d| d.free),
        });
    }
    if !opts.allow_mass_removal {
        if plan.mass_removal(p.tracks_map.len()) {
            return Err(Error::MassRemoval(format!(
                "{} of the {} managed tracks",
                plan.removals(),
                p.tracks_map.len()
            )));
        }
        let n = playlist_removals(p);
        if n > 5 && n * 10 > p.playlists_map.len() {
            return Err(Error::MassRemoval(format!(
                "{n} of the {} managed playlists",
                p.playlists_map.len()
            )));
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> std::io::Result<[u8; 32]> {
    crate::scan::hash_reader(card::open_file(path)?)
}

/// The first sync after an adoption that found no manifest: the card must
/// still hold what adoption saw, every adopted file with its size and
/// content, and still no manifest (section 9.1).
fn check_adopted(p: &Prepared, now: &card::Contents) -> Result<()> {
    if now.manifest {
        return Err(Error::CardChanged("a manifest appeared".into()));
    }
    let listed: std::collections::HashMap<String, &card::CardFile> =
        now.audio.iter().map(|f| (card_key(&f.path), f)).collect();
    for m in &p.tracks_map {
        let ok = match listed.get(&card_key(&m.card_path)) {
            Some(f) if f.size == m.synced_size => hash_file(&p.card.join(&f.path))
                .map(|h| h == m.synced_hash)
                .at(p.card.join(&f.path))?,
            _ => false,
        };
        if !ok {
            return Err(Error::CardChanged(format!(
                "{} is not as adopted",
                m.card_path
            )));
        }
    }
    Ok(())
}

fn sync_dir(dir: &Path) {
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Writes the playlist `text` at `path` on the card, whole, and reads it
/// back.
/// `replaces` says whether the plan saw a file there (listed when it was
/// made, or put there by a rename of the same plan): only then is one
/// replaced; otherwise a file that appeared since is never written over.
fn write_playlist(card: &Path, path: &str, text: &str, replaces: bool) -> Result<()> {
    let p = card.join(path);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).at(dir)?;
    }
    let mode = match replaces {
        true => WriteMode::Replace {
            mtime_ns: mtime_ns(&p)?,
        },
        false => WriteMode::CreateNew,
    };
    write_atomic(&p, text, mode)?;
    if card::read_back(&p).at(&p)? != *blake3::hash(text.as_bytes()).as_bytes() {
        return Err(Error::VerifyFailed(format!("{path} reads back wrong")));
    }
    Ok(())
}

/// The manifest describing the card as the index records it now.
fn current_manifest(
    conn: &Connection,
    key: &str,
    now: i64,
    playlists: Vec<String>,
) -> Result<Manifest> {
    let device = devices::device(conn, key)?.ok_or_else(|| Error::NotAdopted(key.to_owned()))?;
    let tracks = devices::track_mappings(conn, device.id)?
        .into_iter()
        .map(|m| ManifestTrack {
            path: m.card_path,
            size: m.synced_size,
            hash: hex(&m.synced_hash),
        })
        .collect();
    Ok(Manifest {
        version: 1,
        device_key: device.device_key,
        card_id: device
            .card_id
            .ok_or_else(|| Error::CardChanged("no card id recorded".into()))?,
        written_at: now,
        tracks,
        playlists,
    })
}

/// Applies the plan `p` to the card `found`. The caller holds the card
/// lock from before `p` was prepared.
pub fn run(
    conn: &Connection,
    cfg: &Config,
    paths: &Paths,
    found: &Found,
    p: &Prepared,
    opts: Options,
) -> std::result::Result<Done, Failed> {
    crate::device::check_writable(found, cfg.device.allow_folder)?;
    check(p, opts)?;
    if p.plan.is_empty() {
        return Ok(Done {
            backup: None,
            written: 0,
            renamed: 0,
            deleted: 0,
            first: false,
            pruned: 0,
            warnings: Vec::new(),
        });
    }
    let card = &p.card;
    let key = p.device.device_key.as_str();
    // The card now, not as it was when the plan was made: it must still be
    // the card adopted.
    let device = devices::device(conn, key)?.ok_or_else(|| Error::NotAdopted(key.to_owned()))?;
    sync::check_card(&device, manifest::identity(card)?.as_ref())?;
    let now_listed = card::read(card)?;
    let first = device.card_id.is_none();
    if first {
        check_adopted(p, &now_listed)?;
    }

    let now = crate::now_unix();
    let mut record = Saved::of(conn, &device)?;
    record.playlists_dir = Some(cfg.card.playlists_dir.clone());
    let saved = serde_json::to_vec_pretty(&record).expect("plain data");
    let plan_json = serde_json::to_vec_pretty(&p.plan).expect("plain data");
    let root = paths.data_dir.join("backups");
    let backup = backup::create(
        &root,
        key,
        card,
        &now_listed,
        &[
            ("mappings.json", &saved),
            ("plan.json", &plan_json),
            ("work.json", b"[]"),
        ],
        now,
    )?;
    let failed = |error: Error| Failed {
        error,
        backup: Some(backup.clone()),
    };
    let mut warnings = Vec::new();
    write(conn, cfg, card, key, &device, first, p, now, &mut warnings).map_err(failed)?;
    let (mut written, mut renamed, mut deleted) = (0, 0, 0);
    for o in &p.plan.playlists {
        match o {
            PlaylistOp::Write { .. } => written += 1,
            PlaylistOp::Rename { .. } => renamed += 1,
            PlaylistOp::Delete { .. } => deleted += 1,
        }
    }
    // The sync is done and checked: a backup that cannot be pruned is a
    // warning, not a failure.
    let pruned = match backup::prune(&root, key, cfg.sync.keep_backups as usize, &backup) {
        Ok(removed) => removed.len(),
        Err(e) => {
            warnings.push(format!("old backups not pruned: {e}"));
            0
        }
    };
    Ok(Done {
        backup: Some(backup),
        written,
        renamed,
        deleted,
        first,
        pruned,
        warnings,
    })
}

/// The writing part of [`run`], after the backup.
#[allow(clippy::too_many_arguments)]
fn write(
    conn: &Connection,
    cfg: &Config,
    card: &Path,
    key: &str,
    device: &devices::DeviceRow,
    first: bool,
    p: &Prepared,
    now: i64,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let path_of = |name: &str| match cfg.card.playlists_dir.as_str() {
        "" => name.to_owned(),
        d => format!("{d}/{name}"),
    };
    // The card gets its id, recorded in the index before it is written.
    if first {
        let id = manifest::new_card_id()?;
        devices::set_card_id(conn, device.id, Some(&id))?;
        let lists = p
            .playlists_map
            .iter()
            .map(|m| path_of(&m.card_name))
            .collect();
        let written =
            current_manifest(conn, key, now, lists).and_then(|m| manifest::write(card, &m));
        if let Err(e) = written {
            // The card did not get it, or not right: it goes, and the index
            // forgets the id, so the card is still the one adopted for the
            // next try.
            let _ = manifest::remove(card);
            let _ = devices::set_card_id(conn, device.id, None);
            return Err(e);
        }
    }
    // Paths the plan saw a file at: a write replaces only those.
    let mut seen: std::collections::HashSet<String> = p
        .contents
        .playlists
        .iter()
        .map(|f| card_key(&f.path))
        .collect();
    for o in &p.plan.playlists {
        match o {
            PlaylistOp::Rename { to, .. } => {
                seen.insert(card_key(to));
            }
            PlaylistOp::Delete { path, .. } => {
                seen.remove(&card_key(path));
            }
            PlaylistOp::Write { .. } => {}
        }
    }
    for o in &p.plan.playlists {
        match o {
            PlaylistOp::Delete { path, .. } => {
                let f = card.join(path);
                match std::fs::remove_file(&f) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).at(&f),
                    _ => {}
                }
                if let Some(d) = f.parent() {
                    sync_dir(d);
                }
            }
            PlaylistOp::Rename { from, to, .. } => {
                let (a, b) = (card.join(from), card.join(to));
                // Only the same file under another case may be there.
                if b.symlink_metadata().is_ok() && card_key(from) != card_key(to) {
                    return Err(Error::WouldOverwrite(to.clone()));
                }
                if let Some(d) = b.parent() {
                    std::fs::create_dir_all(d).at(d)?;
                }
                std::fs::rename(&a, &b).at(&b)?;
                for d in [a.parent(), b.parent()].into_iter().flatten() {
                    sync_dir(d);
                }
            }
            PlaylistOp::Write { path, text, .. } => {
                write_playlist(card, path, text, seen.contains(&card_key(path)))?
            }
        }
    }

    let diffs = crate::verify::differences(card, &p.plan.expected)?;
    if !diffs.is_empty() {
        let mut lines: Vec<String> = diffs.iter().take(20).cloned().collect();
        if diffs.len() > 20 {
            lines.push(format!("and {} more", diffs.len() - 20));
        }
        return Err(Error::VerifyFailed(lines.join("\n")));
    }

    let mappings: Vec<PlaylistMapping> = p
        .plan
        .expected
        .playlists
        .iter()
        .map(|e| PlaylistMapping {
            playlist_id: e.playlist_id,
            card_name: e.path.rsplit('/').next().unwrap_or(&e.path).to_owned(),
        })
        .collect();
    crate::store::savepoint(conn, || {
        devices::replace_playlist_mappings(conn, device.id, &mappings)?;
        devices::set_last_sync(conn, device.id, now)
    })?;
    // The card is written and checked, and the index records it: a manifest
    // that cannot be rewritten still names this card, so it is a warning.
    let lists = p
        .plan
        .expected
        .playlists
        .iter()
        .map(|e| e.path.clone())
        .collect();
    if let Err(e) = current_manifest(conn, key, now, lists).and_then(|m| manifest::write(card, &m))
    {
        warnings.push(format!("the manifest's list of files was not updated: {e}"));
    }
    Ok(())
}
