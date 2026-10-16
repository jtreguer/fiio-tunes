//! Adoption (technical document, section 10): matching the audio files
//! already on the card to library files, and the card's playlists to
//! library playlists, so the first sync moves and keeps them instead of
//! copying everything again. Reads the card, writes only the index.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use rusqlite::Connection;

use crate::adopt::{self, Adoption, DeviceTrack, LibTrack, NAME_PASSES, Proposal, Reason};
use crate::card::{self, CardFile, Contents};
use crate::config::Config;
use crate::device::Found;
use crate::manifest::{self, Identity};
use crate::paths::card_key;
use crate::store::devices::{self, PlaylistMapping, TrackMapping};
use crate::store::{self, TrackRow};
use crate::tags::{self, ReadError};
use crate::{Error, Result};

/// One audio file of the card, as adoption read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardTrack {
    pub file: CardFile,
    /// BLAKE3 of the file, when it was read: those a library file has the
    /// size of, and every file matched or proposed.
    pub hash: Option<[u8; 32]>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u32>,
}

/// A card playlist matched to a library playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistMatch {
    /// File name on the card, with its extension.
    pub card_name: String,
    pub playlist_id: i64,
    pub library_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Playlists {
    pub matched: Vec<PlaylistMatch>,
    /// Card playlists in the playlists folder with no library playlist:
    /// unmanaged.
    pub unmatched_card: Vec<String>,
    /// Library playlists with no card file: written by the next sync.
    pub unmatched_library: Vec<String>,
}

pub struct Run {
    pub key: String,
    pub contents: Contents,
    /// The card's audio files; a track's index is its key in `adoption`.
    pub tracks: Vec<CardTrack>,
    /// The identity in a manifest already on the card, of this device.
    pub manifest: Option<Identity>,
    pub adoption: Adoption,
    /// Final `(card track, library id)` pairs.
    pub mapping: Vec<(u64, i64)>,
    /// Proposals not resolved; their card files stay unmanaged.
    pub left: Vec<Proposal>,
    pub playlists: Playlists,
    /// Present library tracks, by id.
    pub library: HashMap<i64, TrackRow>,
    /// Card files read, for their content.
    pub hashed: usize,
    /// `(card path, why)` for files that could not be read.
    pub unreadable: Vec<(String, String)>,
}

/// The device key of the card `found`: `usb:<serial>` for an X3; for a
/// folder used as a card (`allow_folder`), the key in its manifest, or
/// `folder:<path>` for one that has none yet.
pub fn device_key(
    found: &Found,
    manifest: Option<&Identity>,
    allow_folder: bool,
) -> Result<String> {
    if let Some(k) = found.key() {
        return Ok(k);
    }
    if !allow_folder {
        return Err(Error::NotAnX3(found.mount.mountpoint.clone()));
    }
    Ok(match manifest {
        Some(m) => m.device_key.clone(),
        None => format!("folder:{}", found.mount.mountpoint.display()),
    })
}

/// BLAKE3 of a card file and the number of bytes hashed, which is the
/// size recorded with it: the two always agree, even if the file changed
/// since the card was listed.
fn hash_file(path: &Path) -> std::io::Result<([u8; 32], u64)> {
    struct Counted<R>(R, u64);
    impl<R: std::io::Read> std::io::Read for Counted<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.read(buf)?;
            self.1 += n as u64;
            Ok(n)
        }
    }
    let mut r = Counted(card::open_file(path)?, 0);
    let h = crate::scan::hash_reader(&mut r)?;
    Ok((h, r.1))
}

/// Adds a card file that could not be read to `unreadable`, once.
fn unreadable_once(unreadable: &mut Vec<(String, String)>, path: &str, why: String) {
    if !unreadable.iter().any(|(p, _)| p == path) {
        unreadable.push((path.to_owned(), why));
    }
}

/// A format the scan does not index (WMA, DSD, Opus...): no library file
/// can be it, so it is neither read nor matched.
fn unsupported(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, e)| {
        tags::UNSUPPORTED_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str())
    })
}

/// The folder part of a card path, `""` at the root.
fn folder(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

/// Matches the card at `card` (device `key`) with the index. Every card
/// file a library file has the size of is read and hashed; the tags of
/// those no library file is identical to are read too. `progress` reports
/// `(done, total)` files read. Nothing is written.
pub fn run(
    conn: &Connection,
    cfg: &Config,
    card: &Path,
    key: &str,
    accept_proposals: bool,
    progress: &dyn Fn(usize, usize),
) -> Result<Run> {
    let library: HashMap<i64, TrackRow> = store::all_tracks(conn)?
        .into_iter()
        .filter(|t| t.missing_since.is_none())
        .map(|t| (t.id, t))
        .collect();
    if library.is_empty() {
        return Err(Error::EmptyIndex);
    }
    let contents = card::read(card)?;
    let manifest = manifest::identity(card)?;
    if let Some(m) = &manifest
        && m.device_key != key
    {
        return Err(Error::ForeignManifest {
            found: m.device_key.clone(),
            key: key.to_owned(),
        });
    }

    let sizes: HashSet<u64> = library.values().map(|t| t.size).collect();
    let hashes: HashSet<[u8; 32]> = library.values().map(|t| t.hash).collect();
    let mut tracks: Vec<CardTrack> = contents
        .audio
        .iter()
        .map(|f| CardTrack {
            file: f.clone(),
            hash: None,
            title: None,
            artist: None,
            album: None,
            duration_ms: None,
        })
        .collect();
    let mut unreadable = Vec::new();
    let total = tracks.len();
    let mut hashed = 0;
    for (done, t) in tracks.iter_mut().enumerate() {
        if unsupported(&t.file.path) {
            progress(done + 1, total);
            continue;
        }
        let path = card.join(&t.file.path);
        if sizes.contains(&t.file.size) {
            match hash_file(&path) {
                Ok((h, n)) => {
                    t.hash = Some(h);
                    t.file.size = n;
                    hashed += 1;
                }
                Err(e) => unreadable_once(&mut unreadable, &t.file.path, e.to_string()),
            }
        }
        // A file identical to a library file needs no tags.
        if t.hash.is_none_or(|h| !hashes.contains(&h)) {
            match tags::read_with(&path, &card::open_file) {
                Ok(ft) => {
                    // Without a title tag, the file name, as for the library.
                    let stem = Path::new(&t.file.path)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned());
                    t.title = ft.tags.title.or(stem);
                    t.artist = ft.tags.artist;
                    t.album = ft.tags.album;
                    t.duration_ms = ft.info.duration_ms;
                }
                // Formats fiio-tunes does not copy stay unmanaged.
                Err(ReadError::Unsupported(_)) => {}
                Err(ReadError::Unreadable(e)) => unreadable_once(&mut unreadable, &t.file.path, e),
            }
        }
        progress(done + 1, total);
    }

    let device: Vec<DeviceTrack> = tracks
        .iter()
        .enumerate()
        .map(|(i, t)| DeviceTrack {
            key: i as u64,
            size: t.file.size,
            duration_ms: t.duration_ms.unwrap_or(0),
            title: t.title.clone(),
            artist: t.artist.clone(),
            album: t.album.clone(),
            playlists: 0,
            playcount: 0,
            hash: t.hash,
        })
        .collect();
    let mut lib: Vec<LibTrack> = library
        .values()
        .map(|t| LibTrack {
            id: t.id,
            size: t.size,
            duration_ms: t.info.duration_ms,
            title: Some(t.display_title().into_owned()),
            artist: t.tags.artist.clone(),
            album: t.tags.album.clone(),
            hash: t.hash,
        })
        .collect();
    lib.sort_by_key(|t| t.id);
    let adoption = adopt::adopt(&device, &lib);

    // A mapping records the card file's own hash: read the files matched or
    // proposed that were not read yet.
    let involved: Vec<u64> = adoption
        .accepted
        .iter()
        .map(|&(k, _)| k)
        .chain(adoption.proposals.iter().map(|p| p.key))
        .collect();
    for k in involved {
        let t = &mut tracks[k as usize];
        if t.hash.is_none() {
            match hash_file(&card.join(&t.file.path)) {
                Ok((h, n)) => {
                    t.hash = Some(h);
                    t.file.size = n;
                    hashed += 1;
                }
                Err(e) => unreadable_once(&mut unreadable, &t.file.path, e.to_string()),
            }
        }
    }

    let (mapping, left) = adopt::resolve(&adoption, accept_proposals);
    let left: Vec<Proposal> = left.into_iter().cloned().collect();
    let mapping = recordable(&tracks, mapping, &mut unreadable);
    let playlists = match_playlists(conn, cfg, &contents)?;
    Ok(Run {
        key: key.to_owned(),
        contents,
        tracks,
        manifest,
        adoption,
        mapping,
        left,
        playlists,
        library,
        hashed,
        unreadable,
    })
}

/// The pairs of `mapping` that can be recorded. A file that could not be
/// hashed cannot. Two files FAT would take for one (only a folder used as a
/// card can hold them) are one place on a card: the first is kept, and the
/// other noted in `unreadable`.
fn recordable(
    tracks: &[CardTrack],
    mapping: Vec<(u64, i64)>,
    unreadable: &mut Vec<(String, String)>,
) -> Vec<(u64, i64)> {
    let mut keys = HashSet::new();
    let mut mapping: Vec<(u64, i64)> = mapping
        .into_iter()
        .filter(|&(k, _)| tracks[k as usize].hash.is_some())
        .collect();
    mapping.retain(|&(k, _)| {
        let path = &tracks[k as usize].file.path;
        let first = keys.insert(card_key(path));
        if !first {
            unreadable_once(
                unreadable,
                path,
                "another card file has this name for FAT".into(),
            );
        }
        first
    });
    mapping
}

/// Replaces the run's mapping with the one from a decision per proposal
/// (see [`adopt::resolve_choices`]). A library track chosen twice is
/// refused and the run left as it was.
pub fn choose(r: &mut Run, choices: &HashMap<u64, Option<i64>>) -> Result<()> {
    let (mapping, left) = match adopt::resolve_choices(&r.adoption, choices) {
        Ok((m, l)) => (m, l.into_iter().cloned().collect::<Vec<Proposal>>()),
        Err(conflicts) => {
            return Err(Error::ChoiceConflicts(
                conflicts
                    .iter()
                    .map(|c| {
                        let path = r.library.get(&c.track_id).map_or_else(
                            || c.track_id.to_string(),
                            |t| t.path.display().to_string(),
                        );
                        let files: Vec<&str> = c
                            .keys
                            .iter()
                            .filter_map(|k| r.tracks.get(*k as usize))
                            .map(|t| t.file.path.as_str())
                            .collect();
                        format!("{path} is chosen for card files {}", files.join(", "))
                    })
                    .collect(),
            ));
        }
    };
    r.mapping = recordable(&r.tracks, mapping, &mut r.unreadable);
    r.left = left;
    Ok(())
}

/// Matches the `.m3u8` files of the card's playlists folder to library
/// playlists by name: exact, then without case, then with the characters a
/// file name cannot hold replaced by `_`. Each matches at most once.
fn match_playlists(conn: &Connection, cfg: &Config, contents: &Contents) -> Result<Playlists> {
    let dir = card_key(&cfg.card.playlists_dir);
    let mut card: Vec<(String, String)> = contents
        .playlists
        .iter()
        .filter(|f| card_key(folder(&f.path)) == dir)
        .map(|f| {
            let name = f.path.rsplit('/').next().unwrap_or(&f.path).to_owned();
            let stem = name[..name.len() - ".m3u8".len()].to_owned();
            (name, stem)
        })
        .collect();
    let mut library: Vec<(i64, String)> = store::playlists(conn)?
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();
    let mut out = Playlists::default();
    // Card names FAT would take for one (only a folder used as a card can
    // hold them): the first matched keeps the place, the others stay
    // unmanaged.
    let mut matched_keys = HashSet::new();
    for key in NAME_PASSES {
        let mut rest = Vec::new();
        for (name, stem) in card {
            let k = key(&stem);
            let free = !matched_keys.contains(&card_key(&name));
            match library
                .iter()
                .position(|(_, l)| key(l) == k)
                .filter(|_| free)
            {
                Some(i) => {
                    matched_keys.insert(card_key(&name));
                    let (playlist_id, library_name) = library.remove(i);
                    out.matched.push(PlaylistMatch {
                        card_name: name,
                        playlist_id,
                        library_name,
                    });
                }
                None => rest.push((name, stem)),
            }
        }
        card = rest;
    }
    out.unmatched_card = card.into_iter().map(|(n, _)| n).collect();
    out.unmatched_library = library.into_iter().map(|(_, n)| n).collect();
    out.matched.sort_by(|a, b| a.card_name.cmp(&b.card_name));
    out.unmatched_card.sort();
    out.unmatched_library.sort();
    Ok(out)
}

/// Saves the run's mappings for its device, with `name` and `model`. An
/// adoption replaces an earlier one only with `force`. The device takes the
/// `card_id` of a manifest of its own on the card, and has none otherwise.
pub fn save(
    conn: &Connection,
    r: &Run,
    name: Option<&str>,
    model: Option<&str>,
    force: bool,
) -> Result<()> {
    store::savepoint(conn, || {
        if let Some(d) = devices::device(conn, &r.key)?
            && !force
            && devices::mapped_count(conn, d.id)? > 0
        {
            return Err(Error::AlreadyAdopted(r.key.clone()));
        }
        let device = devices::upsert_device(conn, &r.key, name, model)?;
        devices::set_card_id(
            conn,
            device,
            r.manifest.as_ref().map(|m| m.card_id.as_str()),
        )?;
        let tracks: Vec<TrackMapping> = r
            .mapping
            .iter()
            .map(|&(k, track_id)| {
                let t = &r.tracks[k as usize];
                TrackMapping {
                    track_id,
                    card_path: t.file.path.clone(),
                    synced_hash: t.hash.expect("mapped files are hashed"),
                    synced_size: t.file.size,
                }
            })
            .collect();
        let playlists: Vec<PlaylistMapping> = r
            .playlists
            .matched
            .iter()
            .map(|m| PlaylistMapping {
                playlist_id: m.playlist_id,
                card_name: m.card_name.clone(),
            })
            .collect();
        devices::replace_mappings(conn, device, &tracks, &playlists)
    })
}

/// Why a proposal needs a decision, in a few words.
pub fn reason(r: Reason) -> &'static str {
    match r {
        Reason::WeakTags => "same size and duration, title or artist differ",
        Reason::SeveralCandidates => {
            "several library files match it equally (duplicates in the library)"
        }
        Reason::SharedWithAnotherDeviceTrack => {
            "another card file matched the same library file (a duplicate on the card)"
        }
        Reason::TagsOnly => "same artist, title and album, different size",
    }
}

/// A few lines for the terminal.
pub fn summary(r: &Run) -> String {
    let a = &r.adoption;
    let mut by_reason: HashMap<Reason, usize> = HashMap::new();
    for p in &a.proposals {
        *by_reason.entry(p.reason).or_default() += 1;
    }
    let mut reasons: Vec<_> = by_reason.into_iter().collect();
    reasons.sort();
    let mut s = format!(
        "card audio files: {}\n  matched: {} ({} by identical file)\n  proposals: {}",
        r.tracks.len(),
        a.accepted.len(),
        a.identical.len(),
        a.proposals.len()
    );
    for (why, n) in reasons {
        let _ = write!(s, "\n    {n}: {}", reason(why));
    }
    let _ = write!(
        s,
        "\n  no library file: {} (left on the card, unmanaged)\n\
         mapping after proposals: {} tracks, {} card files left unmanaged\n\
         playlists: {} matched, {} only on the card (unmanaged), {} only in the library\n\
         manifest: {}",
        a.unmatched_device.len(),
        r.mapping.len(),
        r.tracks.len() - r.mapping.len(),
        r.playlists.matched.len(),
        r.playlists.unmatched_card.len(),
        r.playlists.unmatched_library.len(),
        match &r.manifest {
            Some(m) => format!("present, card id {}", m.card_id),
            None => "none".into(),
        }
    );
    if !r.unreadable.is_empty() {
        let _ = write!(
            s,
            "\ncard files that could not be read: {}",
            r.unreadable.len()
        );
    }
    s
}

/// The full report, for `adopt-report.txt`.
pub fn report(r: &Run) -> String {
    let card_line = |k: u64| {
        let t = &r.tracks[k as usize];
        format!(
            "{} - {} [{}] {} bytes, {} ms, {}",
            t.artist.as_deref().unwrap_or("?"),
            t.title.as_deref().unwrap_or("?"),
            t.album.as_deref().unwrap_or("?"),
            t.file.size,
            t.duration_ms.map_or("?".into(), |d| d.to_string()),
            t.file.path
        )
    };
    let lib_line = |id: i64| {
        let t = &r.library[&id];
        format!(
            "{} - {} [{}] {} bytes, {} ms, {}",
            t.tags.artist.as_deref().unwrap_or("?"),
            t.display_title(),
            t.tags.album.as_deref().unwrap_or("?"),
            t.size,
            t.info.duration_ms.map_or("?".into(), |d| d.to_string()),
            t.path.display()
        )
    };
    let mapped: HashMap<u64, i64> = r.mapping.iter().copied().collect();
    let mut out = String::new();
    let w = &mut out;
    let _ = writeln!(w, "device: {}\n", r.key);
    let _ = writeln!(w, "{}", summary(r));
    let _ = writeln!(w, "\n== Proposals ({})", r.adoption.proposals.len());
    for p in &r.adoption.proposals {
        let outcome = match mapped.get(&p.key) {
            Some(id) => format!("mapped to {}", r.library[id].path.display()),
            None => "left unmanaged".to_owned(),
        };
        let _ = writeln!(
            w,
            "\n{}\n  why: {}\n  outcome: {outcome}",
            card_line(p.key),
            reason(p.reason)
        );
        for id in &p.candidates {
            let mark = if p.default == Some(*id) { "*" } else { " " };
            let _ = writeln!(w, "  {mark} {}", lib_line(*id));
        }
    }
    let _ = writeln!(
        w,
        "\n== Card files with no library file ({}), left on the card unmanaged",
        r.adoption.unmatched_device.len()
    );
    for k in &r.adoption.unmatched_device {
        let _ = writeln!(w, "  {}", card_line(*k));
    }
    if !r.unreadable.is_empty() {
        let _ = writeln!(
            w,
            "\n== Card files that could not be read ({})",
            r.unreadable.len()
        );
        for (p, why) in &r.unreadable {
            let _ = writeln!(w, "  {p}: {why}");
        }
    }
    if !r.contents.problems.is_empty() {
        let _ = writeln!(
            w,
            "\n== Card folders that could not be listed ({})",
            r.contents.problems.len()
        );
        for (p, why) in &r.contents.problems {
            let _ = writeln!(w, "  {p}: {why}");
        }
    }
    let pl = &r.playlists;
    let _ = writeln!(w, "\n== Playlists matched ({})", pl.matched.len());
    for m in &pl.matched {
        let _ = writeln!(w, "  {} = {}", m.card_name, m.library_name);
    }
    let _ = writeln!(
        w,
        "\n== Card playlists with no library playlist ({}), kept unmanaged",
        pl.unmatched_card.len()
    );
    for n in &pl.unmatched_card {
        let _ = writeln!(w, "  {n}");
    }
    let _ = writeln!(
        w,
        "\n== Library playlists not on the card ({}), written by the next sync",
        pl.unmatched_library.len()
    );
    for n in &pl.unmatched_library {
        let _ = writeln!(w, "  {n}");
    }
    out
}
