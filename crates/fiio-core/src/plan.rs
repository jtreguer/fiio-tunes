//! The sync planner (technical document, section 11.1): what to change on
//! the card so it holds the library, or the part of it in scope.
//!
//! Pure: plain data in, a [`Plan`] out, no I/O. The same input gives the
//! same plan in the same order.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::card::Contents;
use crate::naming;
use crate::paths::card_key;
use crate::playlist::{self, Entry};
use crate::store::TrackRow;
use crate::store::devices::{PlaylistMapping, TrackMapping};

/// A library playlist, its entries resolved to track ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibPlaylist {
    pub id: i64,
    pub name: String,
    /// `None` for an entry that matches no indexed file.
    pub members: Vec<Option<i64>>,
}

/// Which library tracks and playlists go to the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    All,
    /// These playlists and the tracks in them.
    Playlists(&'a [String]),
    /// Every playlist but these, and every track but those in them, even
    /// when another playlist holds it.
    Except(&'a [String]),
}

impl Scope<'_> {
    /// Whether the playlist `name` goes to the card.
    pub fn takes_playlist(&self, name: &str) -> bool {
        match self {
            Scope::All => true,
            Scope::Playlists(names) => names.iter().any(|n| n == name),
            Scope::Except(names) => !names.iter().any(|n| n == name),
        }
    }
}

/// The present library tracks that `scope` sends to the card.
pub fn in_scope(library: &[TrackRow], playlists: &[LibPlaylist], scope: Scope) -> BTreeSet<i64> {
    let present = library
        .iter()
        .filter(|t| t.missing_since.is_none())
        .map(|t| t.id);
    let members = |names: &[String]| -> HashSet<i64> {
        playlists
            .iter()
            .filter(|p| names.contains(&p.name))
            .flat_map(|p| p.members.iter().flatten().copied())
            .collect()
    };
    match scope {
        Scope::All => present.collect(),
        Scope::Playlists(names) => {
            let members = members(names);
            present.filter(|id| members.contains(id)).collect()
        }
        Scope::Except(names) => {
            let members = members(names);
            present.filter(|id| !members.contains(id)).collect()
        }
    }
}

/// The card's size and free space, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Space {
    pub capacity: u64,
    pub free: u64,
}

/// Where things go on the card.
#[derive(Debug, Clone, Copy)]
pub struct Layout<'a> {
    /// `[card] music_dir`.
    pub music_dir: &'a str,
    /// `[card] playlists_dir`, `""` for the card root.
    pub playlists_dir: &'a str,
    /// The library roots: a track's card path is its path relative to its
    /// root.
    pub roots: &'a [PathBuf],
}

pub struct PlanInput<'a> {
    /// Every indexed track, missing ones included.
    pub library: &'a [TrackRow],
    /// Playlists with a file, members resolved to track ids.
    pub playlists: &'a [LibPlaylist],
    /// `(id, name)` of deleted playlists still mapped to a card.
    pub removed_playlists: &'a [(i64, String)],
    /// The card as listed.
    pub card: &'a Contents,
    pub tracks_map: &'a [TrackMapping],
    pub playlists_map: &'a [PlaylistMapping],
    pub scope: Scope<'a>,
    pub remove_unmanaged: bool,
    /// `None` when unknown; the plan then cannot be refused for space.
    pub space: Option<Space>,
    pub layout: Layout<'a>,
    /// The bytes of the card's `.m3u8` files in the playlists folder, by
    /// card path.
    pub card_playlists: &'a HashMap<String, Vec<u8>>,
    /// Plan the playlists alone: every track stays as it is on the card,
    /// and the playlists name the files there now.
    pub playlists_only: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub copy: Vec<CopyOp>,
    pub replace: Vec<ReplaceOp>,
    pub moves: Vec<MoveOp>,
    pub remove: Vec<RemoveOp>,
    pub playlists: Vec<PlaylistOp>,
    pub unmanaged: Unmanaged,
    pub skipped: Vec<Skipped>,
    pub entries_skipped: Vec<EntrySkipped>,
    /// Mappings whose card file is gone. In scope, the track is copied
    /// again; otherwise the mapping is dropped.
    pub stale_mappings: Vec<StaleMapping>,
    pub space: SpaceCheck,
    pub expected: Expected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CopyOp {
    pub track_id: i64,
    /// The library file.
    pub from: PathBuf,
    /// The card path, relative to the card root.
    pub to: String,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplaceReason {
    /// The library file changed since it was copied.
    Changed,
    /// The card file is not what was copied (changed by another program).
    ChangedOnCard,
}

/// Copy the library file again over the card file at the same path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplaceOp {
    pub track_id: i64,
    pub from: PathBuf,
    pub to: String,
    pub size: u64,
    /// The card file's size now.
    pub old_size: u64,
    pub reason: ReplaceReason,
}

/// Rename a managed card file to the path the library now gives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MoveOp {
    pub track_id: i64,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoveReason {
    /// The file is gone from the library.
    MissingFromLibrary,
    /// The track is no longer in the sync scope.
    OutOfScope,
    /// The library file changed and goes to another card path: the old
    /// card file goes, the new one is copied.
    Replaced,
    /// Not managed by fiio-tunes, removed because `remove_unmanaged` is set.
    Unmanaged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoveOp {
    pub path: String,
    pub track_id: Option<i64>,
    pub size: u64,
    pub reason: RemoveReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaylistDeleteReason {
    /// The playlist file was deleted.
    FileDeleted,
    /// The playlist is no longer in the sync scope.
    OutOfScope,
    /// Not managed by fiio-tunes, removed because `remove_unmanaged` is set.
    Unmanaged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PlaylistOp {
    Delete {
        playlist_id: Option<i64>,
        /// Card path of the file.
        path: String,
        reason: PlaylistDeleteReason,
    },
    Rename {
        playlist_id: i64,
        from: String,
        to: String,
    },
    /// Write the file whole.
    Write {
        playlist_id: i64,
        path: String,
        text: String,
        entries: usize,
    },
}

impl PlaylistOp {
    /// Deletes first, then renames, then writes, each by name.
    fn order(&self) -> (u8, &str) {
        match self {
            PlaylistOp::Delete { path, .. } => (0, path),
            PlaylistOp::Rename { to, .. } => (1, to),
            PlaylistOp::Write { path, .. } => (2, path),
        }
    }
}

/// Card files fiio-tunes does not manage, kept as they are.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Unmanaged {
    /// Audio files, by card path.
    pub files: Vec<String>,
    /// `.m3u8` files, by card path.
    pub playlists: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// An iTunes Store purchase the X3 cannot play.
    FairPlay,
    /// 4 GiB or more, which FAT32 cannot hold.
    TooLarge,
    /// The file is under none of the library roots.
    NoCardPath,
    /// A card file fiio-tunes does not manage is where it would go; it is
    /// left alone, and a managed file stays where it is.
    Occupied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub track_id: i64,
    pub path: PathBuf,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryReason {
    /// The entry matches no indexed file.
    Unresolved,
    /// The file it points to is missing.
    Missing,
    /// The track is not on the card after the sync (out of scope or
    /// skipped).
    NotOnCard,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntrySkipped {
    pub playlist: String,
    /// 1-based, as `fiio-tunes playlist show` prints it.
    pub position: usize,
    pub reason: EntryReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StaleMapping {
    pub track_id: i64,
    pub card_path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SpaceCheck {
    /// Bytes copied minus bytes freed; negative when the plan frees space.
    pub needed: i64,
    /// The most the plan adds at any point: copies minus removals, since a
    /// replaced file goes only when its new copy is renamed over it.
    pub peak: i64,
    pub device: Option<Space>,
    /// False when `peak` does not fit in the free space with a margin of 1%
    /// of the capacity. The plan must then not be applied.
    pub fits: bool,
}

/// The card after the plan, for the check after writing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Expected {
    /// Every managed audio file: card path, size and track.
    pub files: Vec<ExpectedFile>,
    /// Every managed playlist file, byte for byte.
    pub playlists: Vec<ExpectedPlaylist>,
    /// Every unmanaged file kept, audio and playlists, with its size now.
    pub unmanaged: Vec<(String, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExpectedFile {
    pub path: String,
    pub size: u64,
    pub track_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExpectedPlaylist {
    pub playlist_id: i64,
    pub path: String,
    pub text: String,
}

impl Plan {
    /// True when the plan changes nothing on the card.
    pub fn is_empty(&self) -> bool {
        self.copy.is_empty()
            && self.replace.is_empty()
            && self.moves.is_empty()
            && self.remove.is_empty()
            && self.playlists.is_empty()
    }

    /// Managed tracks the plan takes off the card, a replace to another
    /// path not counted.
    pub fn removals(&self) -> usize {
        self.remove
            .iter()
            .filter(|r| {
                matches!(
                    r.reason,
                    RemoveReason::MissingFromLibrary | RemoveReason::OutOfScope
                )
            })
            .count()
    }

    /// Whether the plan removes more than five and more than a tenth of the
    /// `managed` tracks, which a sync applies only when asked to.
    pub fn mass_removal(&self, managed: usize) -> bool {
        let n = self.removals();
        n > 5 && n * 10 > managed
    }
}

/// The largest file FAT32 holds: 4 GiB less one byte.
const FAT32_MAX: u64 = 0xFFFF_FFFF;

/// The library root `path` is under: the longest one.
fn root_of<'r>(roots: &'r [PathBuf], path: &Path) -> Option<&'r Path> {
    roots
        .iter()
        .filter(|r| path.starts_with(r))
        .max_by_key(|r| r.components().count())
        .map(PathBuf::as_path)
}

/// The folder part of a card path, `""` at the root.
fn folder(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

/// `path` written relative to the folder `dir`, both card paths with `/`.
fn relative(path: &str, dir: &str) -> String {
    let rel = playlist::relative_to(&Path::new("/").join(path), &Path::new("/").join(dir));
    rel.to_str().expect("card paths are UTF-8").to_owned()
}

/// The card file name of a playlist: its name with the characters file
/// names cannot hold replaced by `_`, made acceptable to FAT, ` (n)` for the
/// `n`-th of names FAT takes for one, and `.m3u8`.
pub fn playlist_file_name(name: &str, n: usize) -> String {
    let mut base = naming::clean(&crate::adopt::file_chars(name));
    // A name starting with a dot would be hidden: the card listing skips
    // it, and the playlist would never be seen as written.
    if base.starts_with('.') {
        base.replace_range(..1, "_");
    }
    naming::numbered(&format!("{base}.m3u8"), n)
}

/// Renames `(id, from, to)` in an order that never renames onto a name
/// another rename still has to free: one whose target is the source of
/// another comes after it. A cycle (a swap) is broken by renaming one of
/// its files to `tmp(from, n)` first and to its target last. Ties go by
/// target, so the order is the same for the same input.
fn chain_order(
    mut pending: Vec<(i64, String, String)>,
    tmp: impl Fn(&str, usize) -> String,
) -> Vec<(i64, String, String)> {
    let mut out = Vec::new();
    let mut hops = 0;
    pending.sort_by(|a, b| a.2.cmp(&b.2).then(a.1.cmp(&b.1)));
    while !pending.is_empty() {
        // Free: no other pending rename still leaves its target.
        let free = pending.iter().position(|(_, from, to)| {
            !pending
                .iter()
                .any(|(_, f, _)| f != from && card_key(f) == card_key(to))
        });
        match free {
            Some(i) => out.push(pending.remove(i)),
            None => {
                // Every rename waits for another: a cycle. The one with the
                // smallest source goes aside first.
                let i = (0..pending.len())
                    .min_by(|&a, &b| pending[a].1.cmp(&pending[b].1))
                    .expect("pending is not empty");
                hops += 1;
                let (id, from, to) = pending.remove(i);
                let aside = tmp(&from, hops);
                out.push((id, from, aside.clone()));
                pending.push((id, aside, to));
                pending.sort_by(|a, b| a.2.cmp(&b.2).then(a.1.cmp(&b.1)));
            }
        }
    }
    out
}

/// A hidden name next to `path`, for a rename that breaks a cycle.
fn aside(path: &str, n: usize, ext: &str) -> String {
    let name = format!(".fiio-tunes-move-{n}.{ext}");
    match folder(path) {
        "" => name,
        d => format!("{d}/{name}"),
    }
}

pub fn plan(input: &PlanInput) -> Plan {
    let mut plan = Plan::default();
    let layout = input.layout;
    let present: BTreeMap<i64, &TrackRow> = input
        .library
        .iter()
        .filter(|t| t.missing_since.is_none())
        .map(|t| (t.id, t))
        .collect();
    let in_scope = in_scope(input.library, input.playlists, input.scope);

    // Where each present track goes, over the whole library so a scope
    // change moves nothing.
    let placed: Vec<(i64, &Path, &Path)> = present
        .values()
        .filter_map(|t| Some((t.id, root_of(layout.roots, &t.path)?, t.path.as_path())))
        .collect();
    let pairs: Vec<(&Path, &Path)> = placed.iter().map(|&(_, r, p)| (r, p)).collect();
    let target: HashMap<i64, String> = placed
        .iter()
        .zip(naming::card_paths(layout.music_dir, &pairs))
        .filter_map(|(&(id, _, _), p)| Some((id, p?)))
        .collect();

    // The card, by FAT key.
    let audio: HashMap<String, &crate::card::CardFile> = input
        .card
        .audio
        .iter()
        .map(|f| (card_key(&f.path), f))
        .collect();
    let by_track: HashMap<i64, &TrackMapping> =
        input.tracks_map.iter().map(|m| (m.track_id, m)).collect();
    let managed_keys: HashSet<String> = input
        .tracks_map
        .iter()
        .map(|m| card_key(&m.card_path))
        .collect();
    // Unmanaged audio stays in its place unless removed.
    let occupied: HashSet<String> = match input.remove_unmanaged {
        true => HashSet::new(),
        false => audio
            .keys()
            .filter(|k| !managed_keys.contains(*k))
            .cloned()
            .collect(),
    };
    let on_card = |m: &TrackMapping| audio.get(&card_key(&m.card_path)).copied();

    // Tracks. `sent` is where each track in scope is on the card after the
    // sync, `sizes` its size there.
    let mut sent: HashMap<i64, String> = HashMap::new();
    let mut sizes: HashMap<i64, u64> = HashMap::new();
    if input.playlists_only {
        // The tracks stay as they are: every managed file where it is, the
        // unmanaged ones too; playlists point at what is on the card now.
        let mut mappings: Vec<&TrackMapping> = input.tracks_map.iter().collect();
        mappings.sort_by(|a, b| a.card_path.cmp(&b.card_path));
        for m in mappings {
            let Some(f) = on_card(m) else {
                plan.stale_mappings.push(StaleMapping {
                    track_id: m.track_id,
                    card_path: m.card_path.clone(),
                });
                continue;
            };
            plan.expected.files.push(ExpectedFile {
                path: f.path.clone(),
                size: f.size,
                track_id: m.track_id,
            });
            if in_scope.contains(&m.track_id) {
                sent.insert(m.track_id, f.path.clone());
            }
        }
        for f in input
            .card
            .audio
            .iter()
            .filter(|f| !managed_keys.contains(&card_key(&f.path)))
        {
            plan.unmanaged.files.push(f.path.clone());
            plan.expected.unmanaged.push((f.path.clone(), f.size));
        }
        plan.unmanaged.files.sort();
    } else {
        // Managed files in scope that cannot go where the library puts them (no
        // card path, or the place is held) stay where they are, and hold their
        // place against every other copy and move. One that stays can block
        // another, so until nothing changes.
        let mut held: HashSet<String> = occupied.clone();
        let mut stays: HashSet<i64> = HashSet::new();
        loop {
            let mut more = false;
            for &id in &in_scope {
                if stays.contains(&id) {
                    continue;
                }
                let Some(f) = by_track.get(&id).and_then(|m| on_card(m)) else {
                    continue;
                };
                let here = card_key(&f.path);
                let stuck = match target.get(&id) {
                    None => true,
                    Some(to) => {
                        let to = card_key(to);
                        to != here && held.contains(&to)
                    }
                };
                if stuck {
                    stays.insert(id);
                    held.insert(here);
                    more = true;
                }
            }
            if !more {
                break;
            }
        }

        for &id in &in_scope {
            let t = present[&id];
            let mapping = by_track.get(&id).copied();
            let file = mapping.and_then(on_card);
            if let (Some(m), None) = (mapping, file) {
                plan.stale_mappings.push(StaleMapping {
                    track_id: id,
                    card_path: m.card_path.clone(),
                });
            }
            let skip = |plan: &mut Plan, reason| {
                plan.skipped.push(Skipped {
                    track_id: id,
                    path: t.path.clone(),
                    reason,
                })
            };
            let to = target.get(&id);
            let blocked = to.is_some_and(|p| held.contains(&card_key(p)));
            match (mapping, file) {
                (Some(m), Some(f)) => {
                    // A protected or oversized file already there stays as it is.
                    let keep = t.drm || t.size > FAT32_MAX;
                    let changed = !keep && t.hash != m.synced_hash;
                    let changed_on_card = !keep && f.size != m.synced_size;
                    // The name as it is on the card.
                    let here = &f.path;
                    let to = match to {
                        Some(to) if !stays.contains(&id) => to,
                        other => {
                            // Nowhere else to go: it stays where it is.
                            skip(
                                &mut plan,
                                match other {
                                    None => SkipReason::NoCardPath,
                                    Some(_) => SkipReason::Occupied,
                                },
                            );
                            here
                        }
                    };
                    if (changed || changed_on_card) && to == here {
                        plan.replace.push(ReplaceOp {
                            track_id: id,
                            from: t.path.clone(),
                            to: here.clone(),
                            size: t.size,
                            old_size: f.size,
                            reason: match changed {
                                true => ReplaceReason::Changed,
                                false => ReplaceReason::ChangedOnCard,
                            },
                        });
                        sizes.insert(id, t.size);
                    } else if changed || changed_on_card {
                        plan.remove.push(RemoveOp {
                            path: f.path.clone(),
                            track_id: Some(id),
                            size: f.size,
                            reason: RemoveReason::Replaced,
                        });
                        plan.copy.push(CopyOp {
                            track_id: id,
                            from: t.path.clone(),
                            to: to.clone(),
                            size: t.size,
                        });
                        sizes.insert(id, t.size);
                    } else {
                        if to != here {
                            plan.moves.push(MoveOp {
                                track_id: id,
                                from: here.clone(),
                                to: to.clone(),
                            });
                        }
                        sizes.insert(id, f.size);
                    }
                    sent.insert(id, to.clone());
                }
                _ => {
                    let reason = if t.drm {
                        Some(SkipReason::FairPlay)
                    } else if t.size > FAT32_MAX {
                        Some(SkipReason::TooLarge)
                    } else if to.is_none() {
                        Some(SkipReason::NoCardPath)
                    } else if blocked {
                        Some(SkipReason::Occupied)
                    } else {
                        None
                    };
                    match (reason, to) {
                        (None, Some(to)) => {
                            plan.copy.push(CopyOp {
                                track_id: id,
                                from: t.path.clone(),
                                to: to.clone(),
                                size: t.size,
                            });
                            sizes.insert(id, t.size);
                            sent.insert(id, to.clone());
                        }
                        (reason, _) => skip(&mut plan, reason.unwrap_or(SkipReason::NoCardPath)),
                    }
                }
            }
        }

        // Managed files of tracks not sent go.
        let mut mappings: Vec<&TrackMapping> = input.tracks_map.iter().collect();
        mappings.sort_by(|a, b| a.card_path.cmp(&b.card_path));
        for m in mappings {
            if sent.contains_key(&m.track_id) {
                continue;
            }
            let Some(f) = on_card(m) else {
                if !in_scope.contains(&m.track_id) {
                    plan.stale_mappings.push(StaleMapping {
                        track_id: m.track_id,
                        card_path: m.card_path.clone(),
                    });
                }
                continue;
            };
            // A track in scope that could not be sent anywhere stays put; one
            // that is out of scope, or gone from the library, goes.
            if in_scope.contains(&m.track_id) {
                continue;
            }
            plan.remove.push(RemoveOp {
                path: f.path.clone(),
                track_id: Some(m.track_id),
                size: f.size,
                reason: match present.contains_key(&m.track_id) {
                    true => RemoveReason::OutOfScope,
                    false => RemoveReason::MissingFromLibrary,
                },
            });
        }
        let mut unmanaged_audio: Vec<&crate::card::CardFile> = input
            .card
            .audio
            .iter()
            .filter(|f| !managed_keys.contains(&card_key(&f.path)))
            .collect();
        unmanaged_audio.sort_by(|a, b| a.path.cmp(&b.path));
        for f in unmanaged_audio {
            match input.remove_unmanaged {
                true => plan.remove.push(RemoveOp {
                    path: f.path.clone(),
                    track_id: None,
                    size: f.size,
                    reason: RemoveReason::Unmanaged,
                }),
                false => {
                    plan.unmanaged.files.push(f.path.clone());
                    plan.expected.unmanaged.push((f.path.clone(), f.size));
                }
            }
        }
    }

    // Playlists.
    let dir = layout.playlists_dir;
    let dir_key = card_key(dir);
    let in_dir = |p: &str| card_key(folder(p)) == dir_key;
    let card_lists: HashMap<String, &crate::card::CardFile> = input
        .card
        .playlists
        .iter()
        .filter(|f| in_dir(&f.path))
        .map(|f| (card_key(&f.path), f))
        .collect();
    let path_of = |name: &str| match dir {
        "" => name.to_owned(),
        d => format!("{d}/{name}"),
    };
    let mapped_lists: HashMap<i64, &PlaylistMapping> = input
        .playlists_map
        .iter()
        .map(|m| (m.playlist_id, m))
        .collect();
    let list_on_card =
        |m: &PlaylistMapping| card_lists.get(&card_key(&path_of(&m.card_name))).copied();
    let mut live: Vec<&LibPlaylist> = input.playlists.iter().collect();
    live.sort_by(|a, b| a.name.cmp(&b.name));
    let mut owned: HashSet<String> = HashSet::new();
    for p in &live {
        if let Some(f) = mapped_lists.get(&p.id).and_then(|m| list_on_card(m)) {
            owned.insert(card_key(&f.path));
            if !input.scope.takes_playlist(&p.name) {
                plan.playlists.push(PlaylistOp::Delete {
                    playlist_id: Some(p.id),
                    path: f.path.clone(),
                    reason: PlaylistDeleteReason::OutOfScope,
                });
            }
        }
    }
    for (id, _) in input.removed_playlists {
        if let Some(f) = mapped_lists.get(id).and_then(|m| list_on_card(m)) {
            owned.insert(card_key(&f.path));
            plan.playlists.push(PlaylistOp::Delete {
                playlist_id: Some(*id),
                path: f.path.clone(),
                reason: PlaylistDeleteReason::FileDeleted,
            });
        }
    }
    // Card playlists not mapped, anywhere on the card.
    let mut unmanaged_lists: Vec<&crate::card::CardFile> = input
        .card
        .playlists
        .iter()
        .filter(|f| !owned.contains(&card_key(&f.path)))
        .collect();
    unmanaged_lists.sort_by(|a, b| a.path.cmp(&b.path));
    // Names a playlist written cannot take: unmanaged files kept in the
    // playlists folder.
    let mut taken: HashSet<String> = HashSet::new();
    for f in unmanaged_lists {
        match input.remove_unmanaged {
            true => plan.playlists.push(PlaylistOp::Delete {
                playlist_id: None,
                path: f.path.clone(),
                reason: PlaylistDeleteReason::Unmanaged,
            }),
            false => {
                if in_dir(&f.path) {
                    taken.insert(card_key(&f.path));
                }
                plan.unmanaged.playlists.push(f.path.clone());
                plan.expected.unmanaged.push((f.path.clone(), f.size));
            }
        }
    }
    for p in live.iter().filter(|p| input.scope.takes_playlist(&p.name)) {
        let mut entries = Vec::new();
        for (i, member) in p.members.iter().enumerate() {
            let reason = match member {
                None => Some(EntryReason::Unresolved),
                Some(id) if !present.contains_key(id) => Some(EntryReason::Missing),
                Some(id) => match sent.get(id) {
                    None => Some(EntryReason::NotOnCard),
                    Some(path) => {
                        let t = present[id];
                        entries.push(Entry {
                            location: relative(path, dir),
                            extinf: Some(playlist::extinf(
                                t.info.duration_ms,
                                t.tags.artist.as_deref(),
                                &t.display_title(),
                            )),
                            ..Entry::default()
                        });
                        None
                    }
                },
            };
            if let Some(reason) = reason {
                plan.entries_skipped.push(EntrySkipped {
                    playlist: p.name.clone(),
                    position: i + 1,
                    reason,
                });
            }
        }
        let text = playlist::render(&entries);
        let name = (1..)
            .map(|n| playlist_file_name(&p.name, n))
            .find(|n| !taken.contains(&card_key(&path_of(n))))
            .expect("some numbered name is free");
        let path = path_of(&name);
        taken.insert(card_key(&path));
        let now = mapped_lists.get(&p.id).and_then(|m| list_on_card(m));
        let old_bytes = match now {
            Some(f) => {
                if f.path != path {
                    plan.playlists.push(PlaylistOp::Rename {
                        playlist_id: p.id,
                        from: f.path.clone(),
                        to: path.clone(),
                    });
                }
                input.card_playlists.get(&f.path)
            }
            None => None,
        };
        if old_bytes.map(Vec::as_slice) != Some(text.as_bytes()) {
            plan.playlists.push(PlaylistOp::Write {
                playlist_id: p.id,
                path: path.clone(),
                text: text.clone(),
                entries: entries.len(),
            });
        }
        plan.expected.playlists.push(ExpectedPlaylist {
            playlist_id: p.id,
            path,
            text,
        });
    }

    for (&id, path) in sent.iter().filter(|_| !input.playlists_only) {
        plan.expected.files.push(ExpectedFile {
            path: path.clone(),
            size: sizes[&id],
            track_id: id,
        });
    }
    plan.copy.sort_by(|a, b| a.to.cmp(&b.to));
    plan.replace.sort_by(|a, b| a.to.cmp(&b.to));
    let hops = chain_order(
        std::mem::take(&mut plan.moves)
            .into_iter()
            .map(|m| (m.track_id, m.from, m.to))
            .collect(),
        |from, n| aside(from, n, "move"),
    );
    plan.moves = hops
        .into_iter()
        .map(|(track_id, from, to)| MoveOp { track_id, from, to })
        .collect();
    plan.remove.sort_by(|a, b| a.path.cmp(&b.path));
    plan.skipped.sort_by(|a, b| a.path.cmp(&b.path));
    plan.stale_mappings
        .sort_by(|a, b| a.card_path.cmp(&b.card_path));
    plan.playlists.sort_by(|a, b| a.order().cmp(&b.order()));
    // Renames in an order that never renames onto a name still to be freed.
    let renames: Vec<(i64, String, String)> = plan
        .playlists
        .iter()
        .filter_map(|o| match o {
            PlaylistOp::Rename {
                playlist_id,
                from,
                to,
            } => Some((*playlist_id, from.clone(), to.clone())),
            _ => None,
        })
        .collect();
    let mut renames = chain_order(renames, |from, n| aside(from, n, "m3u8"))
        .into_iter()
        .map(|(playlist_id, from, to)| PlaylistOp::Rename {
            playlist_id,
            from,
            to,
        });
    for o in plan.playlists.iter_mut() {
        if matches!(o, PlaylistOp::Rename { .. }) {
            *o = renames.next().expect("as many renames as before");
        }
    }
    // A cycle adds a rename: the rest go just before the writes.
    let at = plan
        .playlists
        .iter()
        .position(|o| matches!(o, PlaylistOp::Write { .. }))
        .unwrap_or(plan.playlists.len());
    let rest: Vec<PlaylistOp> = renames.collect();
    plan.playlists.splice(at..at, rest);
    plan.expected.files.sort_by(|a, b| a.path.cmp(&b.path));
    plan.expected.playlists.sort_by(|a, b| a.path.cmp(&b.path));
    plan.expected.unmanaged.sort();

    let sum = |sizes: &mut dyn Iterator<Item = u64>| sizes.map(i128::from).sum::<i128>();
    let added =
        sum(&mut plan.copy.iter().map(|c| c.size)) + sum(&mut plan.replace.iter().map(|r| r.size));
    let removed = sum(&mut plan.remove.iter().map(|r| r.size));
    let replaced = sum(&mut plan.replace.iter().map(|r| r.old_size));
    let clamp = |n: i128| n.clamp(i64::MIN.into(), i64::MAX.into()) as i64;
    // Removals run before copies, but a replaced file goes only when its
    // new copy is renamed over it: both are on the card at the peak.
    let peak = added - removed;
    plan.space = SpaceCheck {
        needed: clamp(peak - replaced),
        peak: clamp(peak),
        device: input.space,
        fits: match input.space {
            None => true,
            Some(s) => peak <= 0 || peak + i128::from(s.capacity / 100) <= i128::from(s.free),
        },
    };
    plan
}
