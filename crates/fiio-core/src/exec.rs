//! Applying a plan to the card (technical document, 11.2): removals, moves,
//! then copies and replaces with checkpoints, then the playlists, the check
//! of the card against the plan, and the record.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde::Serialize;

use crate::backup::{self, Backup, Saved};
use crate::card;
use crate::config::{Config, Paths};
use crate::device::Found;
use crate::error::IoContext;
use crate::manifest::{self, Manifest, ManifestTrack, hex};
use crate::paths::card_key;
use crate::plan::{CopyOp, Expected, ExpectedFile, MoveOp, PlaylistOp, RemoveOp, ReplaceOp};
use crate::playlist::{WriteMode, mtime_ns, write_atomic};
use crate::store::devices::{self, DeviceRow, PlaylistMapping, TrackMapping};
use crate::sync::{self, Prepared};
use crate::{Error, Result};

/// A checkpoint every this many copies, or this often, whichever comes
/// first.
const CHECKPOINT_FILES: usize = 500;
const CHECKPOINT_EVERY: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Apply a plan that removes more than a tenth of what is managed.
    pub allow_mass_removal: bool,
}

/// Where a sync is, for a progress line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Removing {
        done: usize,
        total: usize,
    },
    Moving {
        done: usize,
        total: usize,
    },
    /// Before copy `done` (0-based) of `total`, the file `name`.
    Copying {
        done: usize,
        total: usize,
        bytes: u64,
        bytes_total: u64,
        name: String,
    },
    Playlists,
    Verifying,
}

#[derive(Debug, Default)]
pub struct Done {
    /// `None` when the plan was empty: nothing was written.
    pub backup: Option<Backup>,
    /// Tracks copied to the card.
    pub copied: usize,
    /// Tracks copied again over their card file.
    pub recopied: usize,
    /// Tracks moved, a cycle's hop through a hidden name not counted.
    pub moved: usize,
    /// Audio files removed, managed and unmanaged.
    pub removed: usize,
    /// Bytes copied, copies again included.
    pub bytes: u64,
    /// Playlist files written, renamed, removed.
    pub written: usize,
    pub renamed: usize,
    pub deleted: usize,
    /// `(library file, why)` of tracks not copied: the file is gone,
    /// cannot be read or changed since the scan. A later sync copies them,
    /// and the playlists leave them out meanwhile.
    pub skipped: Vec<(PathBuf, String)>,
    /// Set when the sync was asked to stop during the copies: what was done
    /// is recorded and checked, and the rest, playlists included, is left
    /// for the next sync.
    pub stopped: Option<String>,
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

/// The track operations of a sync, in the order they are applied, saved
/// as `work.json` in its backup before any: `doctor` reads where a file
/// left under a hidden name was going.
#[derive(Debug, Serialize)]
pub struct Work<'a> {
    pub remove: &'a [RemoveOp],
    pub moves: &'a [MoveOp],
    pub copy: &'a [CopyOp],
    pub replace: &'a [ReplaceOp],
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

/// The refusals that need no look at the card: a plan that does not fit,
/// mass removals not allowed.
pub fn check(p: &Prepared, opts: Options) -> Result<()> {
    let plan = &p.plan;
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
    let listed: HashMap<String, &card::CardFile> =
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

/// The folder part of a card path, `""` at the root.
fn folder(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

/// Removes the card folder `dir` and then its parents while they are
/// empty, only below the music folder, which stays.
pub(crate) fn remove_empty_dirs(card: &Path, music_dir: &str, dir: &str) {
    let music = card_key(music_dir);
    let mut dir = dir;
    loop {
        let key = card_key(dir);
        let below = match music.as_str() {
            "" => !key.is_empty(),
            m => key.strip_prefix(m).is_some_and(|r| r.starts_with('/')),
        };
        if !below || std::fs::remove_dir(card.join(dir)).is_err() {
            return;
        }
        sync_dir(&card.join(folder(dir)));
        dir = folder(dir);
    }
}

/// Renames the card file `from` to `to`, creating its folder. Only the same
/// file under another case may be at `to`.
fn rename(card: &Path, from: &str, to: &str) -> Result<()> {
    let (a, b) = (card.join(from), card.join(to));
    if b.symlink_metadata().is_ok() && card_key(from) != card_key(to) {
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

/// Whether the card path is a cycle's hop through a hidden name.
fn is_hop(path: &str) -> bool {
    card::leftover(path.rsplit('/').next().unwrap_or(path)) == Some(card::Leftover::Hop)
}

/// A copy to make: a new one, or a copy again over the card file.
struct Job<'a> {
    track_id: i64,
    from: &'a Path,
    to: &'a str,
    size: u64,
    replace: bool,
}

/// Copies one library file to the card: to a temp file in the target's
/// folder, synced, checked for size, read back and compared with the
/// library file by BLAKE3, then renamed over the target. `Ok(Err(why))`
/// when the library file cannot be copied (gone, unreadable, or not what
/// the scan indexed, `want`): nothing is left on the card. `Err` is the
/// card's failure.
fn copy_one(
    card: &Path,
    job: &Job,
    want: &[u8; 32],
    n: &mut usize,
) -> Result<std::result::Result<[u8; 32], String>> {
    let target = card.join(job.to);
    let dir = target.parent().unwrap_or(card).to_owned();
    let mut src = match std::fs::File::open(job.from) {
        Ok(f) => f,
        Err(e) => return Ok(Err(e.to_string())),
    };
    std::fs::create_dir_all(&dir).at(&dir)?;
    let (part, mut out) = loop {
        *n += 1;
        let p = dir.join(card::part_name(*n));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
        {
            Ok(f) => break (p, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).at(&p),
        }
    };
    let gone = |why: String| {
        let _ = std::fs::remove_file(&part);
        Ok(Err(why))
    };
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let k = match src.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return gone(format!("cannot be read: {e}")),
        };
        hasher.update(&buf[..k]);
        if let Err(e) = out.write_all(&buf[..k]) {
            let _ = std::fs::remove_file(&part);
            return Err(e).at(&part);
        }
        total += k as u64;
    }
    let hash = *hasher.finalize().as_bytes();
    if total != job.size || hash != *want {
        return gone("changed since the scan".into());
    }
    let checked = (|| -> Result<()> {
        out.sync_all().at(&part)?;
        let len = out.metadata().at(&part)?.len();
        drop(out);
        if len != total {
            return Err(Error::VerifyFailed(format!(
                "{}: {len} bytes written, {total} expected",
                job.to
            )));
        }
        if card::read_back(&part).at(&part)? != hash {
            return Err(Error::VerifyFailed(format!("{} reads back wrong", job.to)));
        }
        // A new copy never lands on a file that appeared since the plan.
        if !job.replace && target.symlink_metadata().is_ok() {
            return Err(Error::WouldOverwrite(job.to.to_owned()));
        }
        std::fs::rename(&part, &target).at(&target)
    })();
    if let Err(e) = checked {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    sync_dir(&dir);
    Ok(Ok(hash))
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
/// lock from before `p` was prepared. `progress` is told where the sync
/// is; `cancel`, once set, stops the copies after the current file.
#[allow(clippy::too_many_arguments)]
pub fn run(
    conn: &Connection,
    cfg: &Config,
    paths: &Paths,
    found: &Found,
    p: &Prepared,
    opts: Options,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> std::result::Result<Done, Failed> {
    crate::device::check_writable(found, cfg.device.allow_folder)?;
    check(p, opts)?;
    if p.plan.is_empty() {
        return Ok(Done::default());
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
    let work = serde_json::to_vec_pretty(&Work {
        remove: &p.plan.remove,
        moves: &p.plan.moves,
        copy: &p.plan.copy,
        replace: &p.plan.replace,
    })
    .expect("plain data");
    let root = paths.data_dir.join("backups");
    let backup = backup::create(
        &root,
        key,
        card,
        &now_listed,
        &[
            ("mappings.json", &saved),
            ("plan.json", &plan_json),
            ("work.json", &work),
        ],
        now,
    )?;
    let failed = |error: Error| Failed {
        error,
        backup: Some(backup.clone()),
    };
    let mut done = Done {
        backup: Some(backup.clone()),
        first,
        ..Done::default()
    };
    let w = Writer {
        conn,
        cfg,
        card,
        key,
        device: &device,
        p,
        now,
        progress,
    };
    w.apply(cancel, &mut done).map_err(failed)?;
    // The sync is done and checked: a backup that cannot be pruned is a
    // warning, not a failure.
    done.pruned = match backup::prune(&root, key, cfg.sync.keep_backups as usize, &backup) {
        Ok(removed) => removed.len(),
        Err(e) => {
            done.warnings.push(format!("old backups not pruned: {e}"));
            0
        }
    };
    Ok(done)
}

/// What the writing part of [`run`] works with, after the backup.
struct Writer<'a> {
    conn: &'a Connection,
    cfg: &'a Config,
    card: &'a Path,
    key: &'a str,
    device: &'a DeviceRow,
    p: &'a Prepared,
    now: i64,
    progress: &'a dyn Fn(Progress),
}

impl Writer<'_> {
    /// The card path of the playlist file `name`.
    fn playlist_path(&self, name: &str) -> String {
        match self.cfg.card.playlists_dir.as_str() {
            "" => name.to_owned(),
            d => format!("{d}/{name}"),
        }
    }

    /// The managed playlist files as the card holds them before the
    /// playlists are written.
    fn playlists_before(&self) -> Vec<String> {
        self.p
            .playlists_map
            .iter()
            .map(|m| self.playlist_path(&m.card_name))
            .collect()
    }

    /// Records `state` as the device's track mappings, and the manifest
    /// that describes it on the card.
    fn checkpoint(&self, state: &BTreeMap<i64, TrackMapping>) -> Result<()> {
        let all: Vec<TrackMapping> = state.values().cloned().collect();
        devices::replace_track_mappings(self.conn, self.device.id, &all)?;
        let m = current_manifest(self.conn, self.key, self.now, self.playlists_before())?;
        manifest::write(self.card, &m)
    }

    fn apply(&self, cancel: &AtomicBool, done: &mut Done) -> Result<()> {
        let (conn, card, p) = (self.conn, self.card, self.p);
        // The card gets its id, recorded in the index before it is written.
        if done.first {
            let id = manifest::new_card_id()?;
            devices::set_card_id(conn, self.device.id, Some(&id))?;
            let written = current_manifest(conn, self.key, self.now, self.playlists_before())
                .and_then(|m| manifest::write(card, &m));
            if let Err(e) = written {
                // The card did not get it, or not right: it goes, and the index
                // forgets the id, so the card is still the one adopted for the
                // next try.
                let _ = manifest::remove(card);
                let _ = devices::set_card_id(conn, self.device.id, None);
                return Err(e);
            }
        }

        let undone = self.tracks(cancel, done)?;
        // The files the card holds now: a copy not made leaves out a new
        // track, and leaves a replaced file as it was.
        let files: Vec<ExpectedFile> = p
            .plan
            .expected
            .files
            .iter()
            .filter_map(|f| match undone.get(&f.track_id) {
                None => Some(f.clone()),
                Some(None) => None,
                Some(Some(old)) => Some(ExpectedFile {
                    size: *old,
                    ..f.clone()
                }),
            })
            .collect();
        if done.stopped.is_some() {
            // The playlists are left for the next sync: every playlist file
            // is as it was.
            let mut unmanaged: Vec<(String, u64)> = p
                .plan
                .expected
                .unmanaged
                .iter()
                .filter(|(path, _)| !card::is_playlist(path))
                .cloned()
                .collect();
            unmanaged.extend(
                p.contents
                    .playlists
                    .iter()
                    .map(|f| (f.path.clone(), f.size)),
            );
            return self.verify(&Expected {
                files,
                playlists: Vec::new(),
                unmanaged,
            });
        }

        // With a copy not made, the playlists are planned again from the
        // card as it is, so they leave that track out.
        let replanned;
        let (ops, contents, lists) = match undone.is_empty() {
            true => (&p.plan.playlists, &p.contents, &p.plan.expected.playlists),
            false => {
                replanned = sync::plan_card(
                    conn,
                    self.cfg,
                    card.to_owned(),
                    card::read(card)?,
                    self.device.clone(),
                    true,
                )?;
                (
                    &replanned.plan.playlists,
                    &replanned.contents,
                    &replanned.plan.expected.playlists,
                )
            }
        };
        (self.progress)(Progress::Playlists);
        write_playlists(card, ops, contents)?;
        for o in ops {
            match o {
                PlaylistOp::Write { .. } => done.written += 1,
                PlaylistOp::Rename { .. } => done.renamed += 1,
                PlaylistOp::Delete { .. } => done.deleted += 1,
            }
        }
        self.verify(&Expected {
            files,
            playlists: lists.clone(),
            unmanaged: p.plan.expected.unmanaged.clone(),
        })?;

        let mappings: Vec<PlaylistMapping> = lists
            .iter()
            .map(|e| PlaylistMapping {
                playlist_id: e.playlist_id,
                card_name: e.path.rsplit('/').next().unwrap_or(&e.path).to_owned(),
            })
            .collect();
        crate::store::savepoint(conn, || {
            devices::replace_playlist_mappings(conn, self.device.id, &mappings)?;
            devices::set_last_sync(conn, self.device.id, self.now)
        })?;
        // The card is written and checked, and the index records it: a manifest
        // that cannot be rewritten still names this card, so it is a warning.
        let lists = lists.iter().map(|e| e.path.clone()).collect();
        if let Err(e) = current_manifest(conn, self.key, self.now, lists)
            .and_then(|m| manifest::write(card, &m))
        {
            done.warnings
                .push(format!("the manifest's list of files was not updated: {e}"));
        }
        Ok(())
    }

    /// Lists the card and compares it with `expected`.
    fn verify(&self, expected: &Expected) -> Result<()> {
        (self.progress)(Progress::Verifying);
        let diffs = crate::verify::differences(self.card, expected)?;
        if diffs.is_empty() {
            return Ok(());
        }
        let mut lines: Vec<String> = diffs.iter().take(20).cloned().collect();
        if diffs.len() > 20 {
            lines.push(format!("and {} more", diffs.len() - 20));
        }
        Err(Error::VerifyFailed(lines.join("\n")))
    }

    /// Removals, moves, then copies and replaces. The mappings follow the
    /// card: they are recorded once the removals are done, after every move,
    /// at each checkpoint of the copies and at their end, and when the card
    /// fails. Returns the copies not made, by track: `None` for a new copy,
    /// `Some(old size)` for a copy again, whose old file stays.
    fn tracks(&self, cancel: &AtomicBool, done: &mut Done) -> Result<HashMap<i64, Option<u64>>> {
        let (conn, card, plan) = (self.conn, self.card, &self.p.plan);
        let mut undone = HashMap::new();
        if plan.copy.is_empty()
            && plan.replace.is_empty()
            && plan.moves.is_empty()
            && plan.remove.is_empty()
            && plan.stale_mappings.is_empty()
        {
            return Ok(undone);
        }
        let music = self.cfg.card.music_dir.as_str();
        // Mappings whose file is gone go: a track in scope is copied again.
        let stale: HashSet<i64> = plan.stale_mappings.iter().map(|m| m.track_id).collect();
        let mut state: BTreeMap<i64, TrackMapping> = self
            .p
            .tracks_map
            .iter()
            .filter(|m| !stale.contains(&m.track_id))
            .map(|m| (m.track_id, m.clone()))
            .collect();
        // On a failure of the card, what was done is recorded first.
        let fail = |state: &BTreeMap<i64, TrackMapping>, e: Error| -> Error {
            let _ = self.checkpoint(state);
            e
        };

        for (i, r) in plan.remove.iter().enumerate() {
            (self.progress)(Progress::Removing {
                done: i,
                total: plan.remove.len(),
            });
            let f = card.join(&r.path);
            match std::fs::remove_file(&f) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(fail(&state, Error::Io { path: f, source: e }));
                }
                _ => {}
            }
            if let Some(id) = r.track_id
                && state
                    .get(&id)
                    .is_some_and(|m| card_key(&m.card_path) == card_key(&r.path))
            {
                state.remove(&id);
            }
            done.removed += 1;
            sync_dir(&card.join(folder(&r.path)));
            remove_empty_dirs(card, music, folder(&r.path));
        }
        // Every name a mapping holds is now a file on the card: each move
        // below can be recorded on its own.
        self.checkpoint(&state).map_err(|e| fail(&state, e))?;

        for (i, m) in plan.moves.iter().enumerate() {
            (self.progress)(Progress::Moving {
                done: i,
                total: plan.moves.len(),
            });
            rename(card, &m.from, &m.to).map_err(|e| fail(&state, e))?;
            if let Some(s) = state.get_mut(&m.track_id) {
                // A hop's hidden name too, so the name it leaves is free for
                // the next move of the cycle.
                s.card_path = m.to.clone();
                devices::set_card_path(conn, self.device.id, m.track_id, &m.to)
                    .map_err(|e| fail(&state, e))?;
            }
            if !is_hop(&m.to) {
                done.moved += 1;
            }
            remove_empty_dirs(card, music, folder(&m.from));
        }

        let jobs: Vec<Job> = plan
            .copy
            .iter()
            .map(|c| Job {
                track_id: c.track_id,
                from: &c.from,
                to: &c.to,
                size: c.size,
                replace: false,
            })
            .chain(plan.replace.iter().map(|r| Job {
                track_id: r.track_id,
                from: &r.from,
                to: &r.to,
                size: r.size,
                replace: true,
            }))
            .collect();
        let old_size: HashMap<i64, u64> = plan
            .replace
            .iter()
            .map(|r| (r.track_id, r.old_size))
            .collect();
        let bytes_total: u64 = jobs.iter().map(|j| j.size).sum();
        let (mut since, mut last, mut n) = (0usize, Instant::now(), 0usize);
        for (i, job) in jobs.iter().enumerate() {
            if cancel.load(Ordering::SeqCst) {
                done.stopped = Some("asked to stop".into());
                for j in &jobs[i..] {
                    undone.insert(j.track_id, old_size.get(&j.track_id).copied());
                }
                break;
            }
            (self.progress)(Progress::Copying {
                done: i,
                total: jobs.len(),
                bytes: done.bytes,
                bytes_total,
                name: job.to.rsplit('/').next().unwrap_or(job.to).to_owned(),
            });
            let want = match self.p.library.get(&job.track_id) {
                Some(t) => t.hash,
                None => [0; 32],
            };
            match copy_one(card, job, &want, &mut n).map_err(|e| fail(&state, e))? {
                Ok(hash) => {
                    state.insert(
                        job.track_id,
                        TrackMapping {
                            track_id: job.track_id,
                            card_path: job.to.to_owned(),
                            synced_hash: hash,
                            synced_size: job.size,
                        },
                    );
                    match job.replace {
                        true => done.recopied += 1,
                        false => done.copied += 1,
                    }
                    done.bytes += job.size;
                }
                Err(why) => {
                    remove_empty_dirs(card, music, folder(job.to));
                    done.skipped.push((job.from.to_owned(), why));
                    undone.insert(job.track_id, old_size.get(&job.track_id).copied());
                }
            }
            since += 1;
            let more = i + 1 < jobs.len();
            if more && (since >= CHECKPOINT_FILES || last.elapsed() >= CHECKPOINT_EVERY) {
                self.checkpoint(&state).map_err(|e| fail(&state, e))?;
                since = 0;
                last = Instant::now();
            }
        }
        self.checkpoint(&state).map_err(|e| fail(&state, e))?;
        Ok(undone)
    }
}

/// Applies the playlist operations `ops` of a plan made from the card
/// listing `contents`: deletes, renames, then writes.
fn write_playlists(card: &Path, ops: &[PlaylistOp], contents: &card::Contents) -> Result<()> {
    // Paths the plan saw a file at: a write replaces only those.
    let mut seen: HashSet<String> = contents
        .playlists
        .iter()
        .map(|f| card_key(&f.path))
        .collect();
    for o in ops {
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
    for o in ops {
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
            PlaylistOp::Rename { from, to, .. } => rename(card, from, to)?,
            PlaylistOp::Write { path, text, .. } => {
                write_playlist(card, path, text, seen.contains(&card_key(path)))?
            }
        }
    }
    Ok(())
}
