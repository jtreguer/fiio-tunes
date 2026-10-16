//! Importing playlists into fiio-tunes' folder: from iprune's playlists
//! folder, from a mounted iPod or from a copy of its `iTunesDB`. The sources
//! are only read; each imported playlist is written whole as a new file in
//! fiio-tunes' folder, or over one of the same name when asked.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::adopt::{self, LibTrack, NAME_PASSES, Reason};
use crate::config::{Config, Scope};
use crate::ipod;
use crate::iprune::{self, IpruneConfig};
use crate::playlist::{self, Entry, WriteMode};
use crate::playlists::{self, Item};
use crate::store::{self, TrackRow};
use crate::{Error, Result};

/// What to do when fiio-tunes already has a playlist of the name imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnConflict {
    /// Leave the existing playlist alone and import nothing.
    #[default]
    Skip,
    /// Write the imported playlist over the existing one.
    Replace,
    /// Import under `<name> (iPod)`, `<name> (iPod 2)`, ...
    Rename,
}

/// What became of one playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Written as a new playlist of the same name.
    Created,
    /// Written over the existing playlist of the same name.
    Replaced,
    /// Written as a new playlist under another name.
    Renamed,
    /// Not written: the name is taken.
    Skipped,
}

impl Outcome {
    pub fn written(&self) -> bool {
        !matches!(self, Outcome::Skipped)
    }
}

/// One playlist imported, or skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    /// Its name in the source.
    pub source: String,
    /// The iPod playlist's persistent id, for an import from an iPod.
    pub source_id: Option<u64>,
    /// Its name in fiio-tunes' folder: the one written, or the one that was
    /// taken when skipped.
    pub name: String,
    pub outcome: Outcome,
    /// Entries written, or that would have been.
    pub entries: usize,
}

/// Picks the sources named in `wanted`, in that order, repeats dropped; all
/// of them when `wanted` is empty. A name is looked for exactly, then
/// without case, then with the characters a file name cannot hold replaced
/// by `_`; a pass that finds several is ambiguous.
fn select<'a, T>(
    available: &'a [T],
    name_of: impl Fn(&T) -> &str,
    wanted: &[String],
) -> Result<Vec<&'a T>> {
    if wanted.is_empty() {
        return Ok(available.iter().collect());
    }
    let mut out: Vec<&T> = Vec::new();
    for w in wanted {
        let mut found = None;
        for key in NAME_PASSES {
            let k = key(w);
            let hits: Vec<&T> = available.iter().filter(|a| key(name_of(a)) == k).collect();
            match hits[..] {
                [] => continue,
                [one] => found = Some(one),
                _ => return Err(Error::AmbiguousName(w.clone())),
            }
            break;
        }
        let one = found.ok_or_else(|| Error::NoSuchPlaylist(w.clone()))?;
        if !out.iter().any(|o| std::ptr::eq(*o, one)) {
            out.push(one);
        }
    }
    Ok(out)
}

/// Where an imported playlist goes, given what is in fiio-tunes' folder.
enum Target {
    New(String, Outcome),
    Over(String, i64),
    Skip(String),
}

/// `written` holds the names this import already wrote: a second playlist
/// that comes to one of them (two iPod playlists of one name, or "A/B" and
/// "A_B") is renamed whatever `on_conflict` says, so an import never
/// replaces or hides a playlist it wrote itself.
fn target(
    cfg: &Config,
    name: &str,
    on_conflict: OnConflict,
    written: &HashSet<String>,
) -> Result<Target> {
    let file = playlists::file_for(cfg, name)?;
    if !file.exists() {
        return Ok(Target::New(name.to_owned(), Outcome::Created));
    }
    let on_conflict = match written.contains(name) {
        true => OnConflict::Rename,
        false => on_conflict,
    };
    match on_conflict {
        OnConflict::Skip => Ok(Target::Skip(name.to_owned())),
        OnConflict::Replace => Ok(Target::Over(name.to_owned(), playlist::mtime_ns(&file)?)),
        OnConflict::Rename => {
            for n in 1.. {
                let other = match n {
                    1 => format!("{name} (iPod)"),
                    n => format!("{name} (iPod {n})"),
                };
                if !playlists::file_for(cfg, &other)?.exists() {
                    return Ok(Target::New(other, Outcome::Renamed));
                }
            }
            unreachable!("some numbered name is free")
        }
    }
}

/// Writes `items` under `name` in fiio-tunes' folder, as `on_conflict` says,
/// and adds the name written to `written`.
#[allow(clippy::too_many_arguments)]
fn write(
    conn: &Connection,
    cfg: &Config,
    source: &str,
    name: &str,
    items: Vec<Item>,
    loose: &[String],
    on_conflict: OnConflict,
    written: &mut HashSet<String>,
) -> Result<Imported> {
    let entries = items.len();
    let (name, outcome, mode) = match target(cfg, name, on_conflict, written)? {
        Target::Skip(name) => {
            return Ok(Imported {
                source: source.to_owned(),
                source_id: None,
                name,
                outcome: Outcome::Skipped,
                entries,
            });
        }
        Target::New(name, outcome) => (name, outcome, WriteMode::CreateNew),
        Target::Over(name, mtime_ns) => (name, Outcome::Replaced, WriteMode::Replace { mtime_ns }),
    };
    playlists::write_imported(conn, cfg, &name, items, loose, mode)?;
    written.insert(name.clone());
    Ok(Imported {
        source: source.to_owned(),
        source_id: None,
        name,
        outcome,
        entries,
    })
}

// ---- iprune's playlists folder

/// iprune's playlists folder and the playlists in it.
#[derive(Debug, Clone)]
pub struct IpruneFolder {
    pub dir: PathBuf,
    /// `(name, file)`, sorted by name.
    pub playlists: Vec<(String, PathBuf)>,
    /// Entries of the folder that could not be read.
    pub problems: Vec<(PathBuf, String)>,
}

/// Reads iprune's config at `iprune_config` and lists its playlists folder.
pub fn iprune_folder(cfg: &Config, iprune_config: &Path) -> Result<IpruneFolder> {
    let ic = IpruneConfig::load(iprune_config)?;
    let dir = ic.playlists_dir(iprune_config)?.to_owned();
    let same = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    if same(&dir, &cfg.playlists.dir) {
        return Err(Error::Iprune(format!(
            "iprune's playlists folder {} is fiio-tunes' own; nothing to import",
            dir.display()
        )));
    }
    let listing = playlists::playlist_files(&dir)?;
    Ok(IpruneFolder {
        dir,
        playlists: listing.files,
        problems: listing.problems,
    })
}

#[derive(Debug, Default)]
pub struct IpruneReport {
    pub playlists: Vec<Imported>,
    /// `(playlist, entry)` for entries that match no library track, kept as
    /// they were.
    pub unresolved: Vec<(String, String)>,
    /// Files that could not be read, with the reason; nothing was imported
    /// from them.
    pub unreadable: Vec<(PathBuf, String)>,
    /// `(playlist, why)` for playlists that could not be written; the
    /// others were.
    pub failed: Vec<(String, String)>,
}

/// Imports the playlists named in `names` (all when empty) from iprune's
/// folder. Each entry naming a library file is written in fiio-tunes' path
/// style; the others, and the lines around them, are kept as they were.
pub fn import_iprune(
    conn: &Connection,
    cfg: &Config,
    src: &IpruneFolder,
    names: &[String],
    on_conflict: OnConflict,
) -> Result<IpruneReport> {
    let chosen = select(&src.playlists, |(n, _)| n, names)?;
    let mut report = IpruneReport::default();
    let mut written = HashSet::new();
    for (name, file) in chosen {
        let (entries, loose) = match playlist::read_all(file) {
            Ok(r) => r,
            Err(e) => {
                report.unreadable.push((file.clone(), e.to_string()));
                continue;
            }
        };
        let mut unresolved = Vec::new();
        let done = playlists::resolve_in(conn, &src.dir, entries).and_then(|items| {
            unresolved = items
                .iter()
                .filter(|i| i.track.is_none())
                .map(|i| i.entry.location.clone())
                .collect();
            write(
                conn,
                cfg,
                name,
                name,
                items,
                &loose,
                on_conflict,
                &mut written,
            )
        });
        let done = match done {
            Ok(d) => d,
            Err(e) => {
                report.failed.push((name.clone(), e.to_string()));
                continue;
            }
        };
        if done.outcome.written() {
            report
                .unresolved
                .extend(unresolved.into_iter().map(|e| (done.name.clone(), e)));
        }
        report.playlists.push(done);
    }
    Ok(report)
}

// ---- an iPod, or a copy of its database

/// Where the iPod database comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpodSource {
    /// A mounted iPod: its files can be compared with the library.
    Mounted(PathBuf),
    /// An `iTunesDB` file, such as a backup's.
    File(PathBuf),
}

pub fn open(source: &IpodSource) -> Result<gpod::Database> {
    Ok(match source {
        IpodSource::Mounted(m) => gpod::Database::open(m)?,
        IpodSource::File(f) => gpod::Database::open_file(f)?,
    })
}

/// How an iPod track was traced to its library file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum How {
    /// iprune's own record of the iPod.
    Iprune,
    /// The file on the iPod is identical to the library file.
    Identical,
    /// Same size, duration within a second, same title and artist.
    Tags,
    /// The matcher's proposal, for the reason given.
    Proposal(Reason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trace {
    pub track_id: i64,
    pub how: How,
}

/// An iPod read and traced to the library.
#[derive(Debug)]
pub struct Ipod {
    pub contents: ipod::Contents,
    /// The library file of each traced iPod track, by dbid.
    pub traced: HashMap<u64, Trace>,
    /// The iPod as iprune knows it, when its index has it.
    pub iprune_device: Option<iprune::Device>,
    /// The copy of iprune's index the tracing read, kept for its scope.
    pub iprune_index: Option<iprune::Index>,
    /// Set when iprune's index exists but could not be read: tracing used
    /// the matcher alone.
    pub iprune_error: Option<String>,
    /// iPod files read and compared with the library.
    pub compared: usize,
    /// `(dbid, why)` for iPod files that could not be read for the
    /// comparison.
    pub unreadable: Vec<(u64, String)>,
    /// Present library tracks, by id.
    pub library: HashMap<i64, TrackRow>,
}

/// One iPod playlist with its members traced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Traced<'a> {
    pub playlist: &'a ipod::Playlist,
    /// The library tracks, in playlist order.
    pub tracks: Vec<i64>,
    /// dbids of the members with no library file, in order, repeats kept.
    pub left_out: Vec<u64>,
}

impl Ipod {
    pub fn track(&self, dbid: u64) -> Option<&ipod::Track> {
        self.contents.tracks.iter().find(|t| t.adopt.key == dbid)
    }

    pub fn traced_playlist<'a>(&self, pl: &'a ipod::Playlist) -> Traced<'a> {
        let mut tracks = Vec::new();
        let mut left_out = Vec::new();
        for &dbid in &pl.members {
            match self.traced.get(&dbid) {
                Some(t) => tracks.push(t.track_id),
                None => left_out.push(dbid),
            }
        }
        Traced {
            playlist: pl,
            tracks,
            left_out,
        }
    }

    /// iPod tracks in at least one playlist, by how they were traced, `None`
    /// counting those with no library file.
    pub fn counts(&self) -> Vec<(Option<How>, usize)> {
        let members: HashSet<u64> = self
            .contents
            .playlists
            .iter()
            .flat_map(|p| p.members.iter().copied())
            .collect();
        let mut by: HashMap<Option<How>, usize> = HashMap::new();
        for dbid in members {
            *by.entry(self.traced.get(&dbid).map(|t| t.how)).or_default() += 1;
        }
        let mut out: Vec<_> = by.into_iter().collect();
        out.sort();
        out
    }
}

/// Hashes the iPod files of `dbids`, one at a time: the iPod is a single
/// disk. Reports `(done, total)` after each file.
fn hash_ipod_files(
    mountpoint: &Path,
    tracks: &mut [adopt::DeviceTrack],
    contents: &ipod::Contents,
    dbids: &[u64],
    progress: &dyn Fn(usize, usize),
) -> Vec<(u64, String)> {
    let paths: HashMap<u64, Option<&str>> = contents
        .tracks
        .iter()
        .map(|t| (t.adopt.key, t.ipod_path.as_deref()))
        .collect();
    let wanted: HashSet<u64> = dbids.iter().copied().collect();
    let mut unreadable = Vec::new();
    let mut done = 0;
    for t in tracks.iter_mut().filter(|t| wanted.contains(&t.key)) {
        let file = paths[&t.key].and_then(|p| ipod::file_path(mountpoint, p));
        match file.map(|f| crate::card::open_file(&f).and_then(crate::scan::hash_reader)) {
            Some(Ok(h)) => t.hash = Some(h),
            Some(Err(e)) => unreadable.push((t.key, e.to_string())),
            None => unreadable.push((t.key, "no usable path in the iPod database".into())),
        }
        done += 1;
        progress(done, dbids.len());
    }
    unreadable
}

/// Reads the iPod in `db` and traces each of its tracks to a library file:
/// through iprune's record of that iPod in `iprune_db` (its index file, read
/// from a copy) when there is one, then with the adoption matcher on size,
/// duration and tags. When `db` comes from a mounted iPod, the files the
/// tags cannot settle are read and compared with the library; `progress`
/// reports `(done, total)` files.
pub fn read_ipod(
    conn: &Connection,
    db: &gpod::Database,
    iprune_db: Option<&Path>,
    progress: &dyn Fn(usize, usize),
) -> Result<Ipod> {
    let library: HashMap<i64, TrackRow> = store::all_tracks(conn)?
        .into_iter()
        .filter(|t| t.missing_since.is_none())
        .map(|t| (t.id, t))
        .collect();
    if library.is_empty() {
        return Err(Error::EmptyIndex);
    }
    let contents = ipod::read(db);
    let mut traced: HashMap<u64, Trace> = HashMap::new();

    // iprune's record of the iPod, when there is one.
    let (iprune_index, iprune_device, iprune_error) =
        match iprune_db.map(|p| trace_with_iprune(conn, p, &contents)) {
            None => (None, None, None),
            Some(Ok(from)) => {
                traced.extend(from.traced);
                (from.index, from.device, None)
            }
            Some(Err(e)) => (None, None, Some(e.to_string())),
        };

    // The matcher, for the rest.
    let mut rest: Vec<adopt::DeviceTrack> = contents
        .tracks
        .iter()
        .filter(|t| !traced.contains_key(&t.adopt.key))
        .map(|t| t.adopt.clone())
        .collect();
    let mut lib: Vec<LibTrack> = library
        .values()
        .map(|t| LibTrack {
            id: t.id,
            size: t.size,
            duration_ms: t.info.duration_ms,
            // Without a title tag, iTunes used the file name, and that is
            // what the iPod got.
            title: Some(t.display_title().into_owned()),
            artist: t.tags.artist.clone(),
            album: t.tags.album.clone(),
            hash: t.hash,
        })
        .collect();
    lib.sort_by_key(|t| t.id);
    let mut adoption = adopt::adopt(&rest, &lib);
    let (mut compared, mut unreadable) = (0, Vec::new());
    if let Some(mountpoint) = db.mountpoint() {
        let dbids = adopt::to_compare(&adoption, &rest, &lib);
        if !dbids.is_empty() {
            unreadable = hash_ipod_files(&mountpoint, &mut rest, &contents, &dbids, progress);
            compared = dbids.len().saturating_sub(unreadable.len());
            adoption = adopt::adopt(&rest, &lib);
        }
    }
    let identical: HashSet<u64> = adoption.identical.iter().copied().collect();
    for &(dbid, track_id) in &adoption.accepted {
        let how = match identical.contains(&dbid) {
            true => How::Identical,
            false => How::Tags,
        };
        traced.insert(dbid, Trace { track_id, how });
    }
    // A playlist needs the song, not a file of its own: duplicates on the
    // iPod all go to the one library file, which the device mapping of an
    // adoption would not allow.
    for p in &adoption.proposals {
        let pick = match p.reason {
            Reason::SharedWithAnotherDeviceTrack => p.candidates.first().copied(),
            _ => p.default,
        };
        if let Some(track_id) = pick {
            traced.insert(
                p.key,
                Trace {
                    track_id,
                    how: How::Proposal(p.reason),
                },
            );
        }
    }
    Ok(Ipod {
        contents,
        traced,
        iprune_device,
        iprune_index,
        iprune_error,
        compared,
        unreadable,
        library,
    })
}

/// iprune's index, the iPod as it knows it, and the tracks traced through
/// its record.
#[derive(Default)]
struct FromIprune {
    index: Option<iprune::Index>,
    device: Option<iprune::Device>,
    traced: Vec<(u64, Trace)>,
}

/// The iPod tracks iprune mapped to a file that fiio-tunes' index has.
fn trace_with_iprune(
    conn: &Connection,
    database: &Path,
    contents: &ipod::Contents,
) -> Result<FromIprune> {
    let Some(index) = iprune::Index::open_copy(database)? else {
        return Ok(FromIprune::default());
    };
    let dbids: HashSet<u64> = contents.tracks.iter().map(|t| t.adopt.key).collect();
    let Some(device) = index.device_for(contents.firewire_guid.as_deref(), &dbids)? else {
        return Ok(FromIprune {
            index: Some(index),
            ..FromIprune::default()
        });
    };
    let mut out = Vec::new();
    for (dbid, path) in index.mappings(device.id)? {
        if !dbids.contains(&dbid) {
            continue;
        }
        let Some(key) = crate::paths::key(&path) else {
            continue;
        };
        if let Some(t) = store::present_track_by_key(conn, &key)? {
            out.push((
                dbid,
                Trace {
                    track_id: t.id,
                    how: How::Iprune,
                },
            ));
        }
    }
    Ok(FromIprune {
        index: Some(index),
        device: Some(device),
        traced: out,
    })
}

/// The name an iPod playlist gets in fiio-tunes' folder: the characters a
/// file name cannot hold replaced by `_`, as iprune's own files have them
/// ("Dance/Trance" is `Dance_Trance.m3u8`), and line breaks too; a leading
/// `.` made `_`, so the file is not hidden.
pub fn playlist_name(ipod_name: &str) -> String {
    let mut name = adopt::file_chars(ipod_name).replace(['\n', '\r', '\0'], "_");
    if name.starts_with('.') {
        name.replace_range(..1, "_");
    }
    if name.trim().is_empty() {
        name = "Untitled".into();
    }
    name
}

#[derive(Debug, Default)]
pub struct IpodReport {
    pub playlists: Vec<Imported>,
    /// For each playlist written, the members left out (no library file),
    /// by dbid, each once.
    pub left_out: Vec<(String, Vec<u64>)>,
    /// `(iPod playlist, why)` for playlists that could not be written; the
    /// others were.
    pub failed: Vec<(String, String)>,
}

/// Imports the iPod playlists named in `names` (all when empty).
pub fn import_ipod(
    conn: &Connection,
    cfg: &Config,
    ipod: &Ipod,
    names: &[String],
    on_conflict: OnConflict,
) -> Result<IpodReport> {
    let chosen = select(&ipod.contents.playlists, |p| &p.name, names)?;
    let mut report = IpodReport::default();
    let mut written = HashSet::new();
    for pl in chosen {
        let t = ipod.traced_playlist(pl);
        let items: Vec<Item> = t
            .tracks
            .iter()
            .map(|id| Item {
                entry: Entry::default(),
                track: Some(ipod.library[id].clone()),
            })
            .collect();
        let done = write(
            conn,
            cfg,
            &pl.name,
            &playlist_name(&pl.name),
            items,
            &[],
            on_conflict,
            &mut written,
        );
        let mut done = match done {
            Ok(d) => d,
            Err(e) => {
                report.failed.push((pl.name.clone(), e.to_string()));
                continue;
            }
        };
        done.source_id = Some(pl.id);
        if done.outcome.written() && !t.left_out.is_empty() {
            let mut seen = HashSet::new();
            let left: Vec<u64> = t.left_out.into_iter().filter(|d| seen.insert(*d)).collect();
            report.left_out.push((done.name.clone(), left));
        }
        report.playlists.push(done);
    }
    Ok(report)
}

/// The iPod's iprune scope, in fiio-tunes' playlist names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeCopy {
    /// Every playlist the scope names was imported.
    Ready {
        scope: Scope,
        playlists: Vec<String>,
    },
    /// These playlists of the scope were not imported.
    Missing(Vec<String>),
}

/// The scope iprune syncs the iPod with, its playlists named as imported in
/// `report`. iprune's names are traced to the iPod's playlists through its
/// record of which playlist file each iPod playlist is, then by name.
/// iprune's index is the copy [`read_ipod`] read.
pub fn iprune_scope(
    iprune_config: &IpruneConfig,
    ipod: &Ipod,
    report: &IpodReport,
) -> Result<ScopeCopy> {
    let device = ipod.iprune_device.as_ref().ok_or_else(|| {
        Error::Iprune("iprune has no record of this iPod, so it has no scope to copy".into())
    })?;
    let index = ipod
        .iprune_index
        .as_ref()
        .expect("an iPod iprune knows was read from its index");
    let ds = index.scope(device.id)?;
    let (scope, names) = match ds.scope {
        Some(s) => (s, ds.playlists),
        None => (
            iprune_config.sync.scope,
            iprune_config.sync.playlists.clone(),
        ),
    };
    if scope == Scope::All {
        return Ok(ScopeCopy::Ready {
            scope,
            playlists: Vec::new(),
        });
    }
    let ipod_ids = index.playlist_ipod_ids(device.id)?;
    // The fiio-tunes name of each iPod playlist imported.
    let imported: HashMap<u64, &str> = report
        .playlists
        .iter()
        .filter(|i| i.outcome.written())
        .filter_map(|i| Some((i.source_id?, i.name.as_str())))
        .collect();
    let mut playlists = Vec::new();
    let mut missing = Vec::new();
    for name in names {
        // An id iprune recorded for a playlist the iPod no longer has (an
        // older database) says nothing: the name decides.
        let on_ipod = |id: &u64| ipod.contents.playlists.iter().any(|p| p.id == *id);
        let id = ipod_ids.get(&name).copied().filter(on_ipod).or_else(|| {
            select(
                &ipod.contents.playlists,
                |p| &p.name,
                std::slice::from_ref(&name),
            )
            .ok()
            .map(|p| p[0].id)
        });
        match id.and_then(|id| imported.get(&id)) {
            Some(n) => playlists.push((*n).to_owned()),
            None => missing.push(name),
        }
    }
    if !missing.is_empty() {
        return Ok(ScopeCopy::Missing(missing));
    }
    playlists.sort();
    playlists.dedup();
    Ok(ScopeCopy::Ready { scope, playlists })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_names_are_file_safe() {
        assert_eq!(playlist_name("Dance/Trance"), "Dance_Trance");
        assert_eq!(playlist_name(".hidden"), "_hidden");
        assert_eq!(playlist_name("a\nb"), "a_b");
        assert_eq!(playlist_name("  "), "Untitled");
        assert_eq!(playlist_name("Café"), "Café");
    }

    #[test]
    fn select_goes_from_exact_to_loose_and_refuses_ambiguity() {
        let names = ["Jazz", "jazz", "Dance_Trance", "Rock"].map(String::from);
        let pick = |w: &[&str]| {
            let w: Vec<String> = w.iter().map(|s| s.to_string()).collect();
            select(&names, |n| n, &w).map(|v| v.into_iter().cloned().collect::<Vec<_>>())
        };
        assert_eq!(pick(&[]).unwrap().len(), 4);
        assert_eq!(pick(&["jazz"]).unwrap(), ["jazz"]);
        assert_eq!(pick(&["rock", "Rock"]).unwrap(), ["Rock"]);
        assert_eq!(pick(&["Dance/Trance"]).unwrap(), ["Dance_Trance"]);
        assert!(matches!(pick(&["JAZZ"]), Err(Error::AmbiguousName(_))));
        assert!(matches!(pick(&["Pop"]), Err(Error::NoSuchPlaylist(_))));
    }
}
