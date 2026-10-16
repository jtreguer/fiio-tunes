//! The sync planner (technical document, section 11.1) on hand-built input.
//! Nothing here touches a disk or a database: `TrackRow`, `Contents`,
//! mappings and `LibPlaylist` values go in, a `Plan` comes out.

use std::collections::HashMap;
use std::path::PathBuf;

use fiio_core::card::{CardFile, Contents};
use fiio_core::paths::card_key;
use fiio_core::plan::{
    self, CopyOp, EntryReason, EntrySkipped, Layout, LibPlaylist, MoveOp, Plan, PlanInput,
    PlaylistDeleteReason, PlaylistOp, RemoveOp, RemoveReason, ReplaceOp, ReplaceReason, Scope,
    SkipReason, Skipped, Space,
};
use fiio_core::store::TrackRow;
use fiio_core::store::devices::{PlaylistMapping, TrackMapping};
use fiio_core::tags::{AudioInfo, FileKind, Tags};

const SIZE: u64 = 4_000;
const MS: u32 = 200_000;
/// The first size FAT32 cannot hold.
const FOUR_GIB: u64 = 1 << 32;

fn hash_of(id: i64) -> [u8; 32] {
    [id as u8; 32]
}

#[derive(Clone, Debug)]
enum Sel {
    All,
    Only(Vec<String>),
    Except(Vec<String>),
}

/// Everything a plan is made from, owned, with helpers to fill it in.
#[derive(Clone, Debug)]
struct World {
    lib: Vec<TrackRow>,
    lists: Vec<LibPlaylist>,
    removed: Vec<(i64, String)>,
    audio: Vec<CardFile>,
    card_lists: Vec<CardFile>,
    bytes: HashMap<String, Vec<u8>>,
    tmap: Vec<TrackMapping>,
    pmap: Vec<PlaylistMapping>,
    sel: Sel,
    remove_unmanaged: bool,
    space: Option<Space>,
    music_dir: String,
    playlists_dir: String,
    roots: Vec<PathBuf>,
}

fn names(n: &[&str]) -> Vec<String> {
    n.iter().map(|s| (*s).to_owned()).collect()
}

impl World {
    fn new() -> Self {
        World {
            lib: vec![],
            lists: vec![],
            removed: vec![],
            audio: vec![],
            card_lists: vec![],
            bytes: HashMap::new(),
            tmap: vec![],
            pmap: vec![],
            sel: Sel::All,
            remove_unmanaged: false,
            space: None,
            music_dir: "Music".into(),
            playlists_dir: String::new(),
            roots: vec![PathBuf::from("/music")],
        }
    }

    /// A present library track `/music/<rel>`; the hash is `[id; 32]`.
    fn track(&mut self, id: i64, rel: &str, size: u64) {
        let tags = Tags {
            title: Some(format!("Title{id}")),
            artist: Some(format!("Artist{id}")),
            ..Tags::default()
        };
        let path = PathBuf::from(format!("/music/{rel}"));
        self.lib.push(TrackRow {
            id,
            path_key: path.to_string_lossy().into_owned(),
            path,
            size,
            mtime_ns: 0,
            hash: hash_of(id),
            info: AudioInfo {
                kind: FileKind::Mp3,
                duration_ms: Some(MS),
                bitrate: None,
                samplerate: None,
            },
            tags_hash: tags.hash(),
            tags,
            added_at: 0,
            missing_since: None,
            tag_error: None,
            drm: false,
        });
    }

    fn row(&mut self, id: i64) -> &mut TrackRow {
        self.lib.iter_mut().find(|t| t.id == id).unwrap()
    }

    /// A track already synced: `Music/<rel>` on the card, and mapped with
    /// the library hash and size.
    fn synced(&mut self, id: i64, rel: &str, size: u64) {
        self.track(id, rel, size);
        self.map_file(id, &format!("Music/{rel}"), size);
    }

    /// A card file at `path` and its mapping to `id`, hash as the library's.
    fn map_file(&mut self, id: i64, path: &str, size: u64) {
        self.card_audio(path, size);
        self.map_only(id, path, size);
    }

    fn map_only(&mut self, id: i64, path: &str, synced_size: u64) {
        self.tmap.push(TrackMapping {
            track_id: id,
            card_path: path.to_owned(),
            synced_hash: hash_of(id),
            synced_size,
        });
    }

    fn card_audio(&mut self, path: &str, size: u64) {
        self.audio.push(CardFile {
            path: path.to_owned(),
            size,
            mtime_ns: 0,
        });
    }

    fn card_list(&mut self, path: &str, bytes: &[u8]) {
        self.card_lists.push(CardFile {
            path: path.to_owned(),
            size: bytes.len() as u64,
            mtime_ns: 0,
        });
        self.bytes.insert(path.to_owned(), bytes.to_vec());
    }

    fn list(&mut self, id: i64, name: &str, members: &[Option<i64>]) {
        self.lists.push(LibPlaylist {
            id,
            name: name.to_owned(),
            members: members.to_vec(),
        });
    }

    fn map_list(&mut self, id: i64, card_name: &str) {
        self.pmap.push(PlaylistMapping {
            playlist_id: id,
            card_name: card_name.to_owned(),
        });
    }

    fn plan(&self) -> Plan {
        let scope = match &self.sel {
            Sel::All => Scope::All,
            Sel::Only(n) => Scope::Playlists(n),
            Sel::Except(n) => Scope::Except(n),
        };
        let card = Contents {
            audio: self.audio.clone(),
            playlists: self.card_lists.clone(),
            problems: vec![],
            manifest: false,
        };
        plan::plan(&PlanInput {
            library: &self.lib,
            playlists: &self.lists,
            removed_playlists: &self.removed,
            card: &card,
            tracks_map: &self.tmap,
            playlists_map: &self.pmap,
            scope,
            remove_unmanaged: self.remove_unmanaged,
            space: self.space,
            layout: Layout {
                music_dir: &self.music_dir,
                playlists_dir: &self.playlists_dir,
                roots: &self.roots,
            },
            card_playlists: &self.bytes,
        })
    }
}

fn some(ids: &[i64]) -> Vec<Option<i64>> {
    ids.iter().map(|&i| Some(i)).collect()
}

/// The playlist operations of a plan, as `kind path`.
fn ops(p: &Plan) -> Vec<String> {
    p.playlists
        .iter()
        .map(|o| match o {
            PlaylistOp::Delete { path, .. } => format!("delete {path}"),
            PlaylistOp::Rename { from, to, .. } => format!("rename {from} {to}"),
            PlaylistOp::Write { path, .. } => format!("write {path}"),
        })
        .collect()
}

fn write_text<'a>(p: &'a Plan, path: &str) -> &'a str {
    p.playlists
        .iter()
        .find_map(|o| match o {
            PlaylistOp::Write { path: q, text, .. } if q == path => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no write of {path} in {:?}", ops(p)))
}

// ---------------------------------------------------------------- tracks

// Prevents: an unmapped track in scope not being copied, or the copy
// carrying the wrong source, card path or size, or touching anything else.
#[test]
fn an_unmapped_track_is_copied_to_music_under_its_relative_path() {
    let mut w = World::new();
    w.track(1, "Artist/Album/one.mp3", SIZE);
    w.track(2, "two.mp3", 7);
    let p = w.plan();
    assert_eq!(
        p.copy,
        [
            CopyOp {
                track_id: 1,
                from: "/music/Artist/Album/one.mp3".into(),
                to: "Music/Artist/Album/one.mp3".into(),
                size: SIZE
            },
            CopyOp {
                track_id: 2,
                from: "/music/two.mp3".into(),
                to: "Music/two.mp3".into(),
                size: 7
            },
        ]
    );
    assert!(p.replace.is_empty() && p.moves.is_empty() && p.remove.is_empty());
    assert!(p.skipped.is_empty() && p.stale_mappings.is_empty());
    let files: Vec<_> = p
        .expected
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.size, f.track_id))
        .collect();
    assert_eq!(
        files,
        [
            ("Music/Artist/Album/one.mp3", SIZE, 1),
            ("Music/two.mp3", 7, 2)
        ]
    );
}

// Prevents: a synced track being copied, replaced or moved again.
#[test]
fn a_synced_track_in_its_place_gives_an_empty_plan() {
    let mut w = World::new();
    w.synced(1, "a.mp3", SIZE);
    let p = w.plan();
    assert!(p.is_empty(), "{p:?}");
    assert_eq!(p.expected.files.len(), 1);
    assert_eq!(p.expected.files[0].path, "Music/a.mp3");
}

// Prevents: a mapping whose card file is gone being trusted (the track would
// never be copied again), or a stale mapping out of scope being copied or
// removed.
#[test]
fn a_stale_mapping_is_copied_again_in_scope_and_only_reported_out_of_scope() {
    let mut w = World::new();
    w.track(1, "a.mp3", SIZE);
    w.map_only(1, "Music/a.mp3", SIZE);
    let p = w.plan();
    assert_eq!(p.copy.len(), 1);
    assert_eq!(p.copy[0].to, "Music/a.mp3");
    assert_eq!(
        p.stale_mappings,
        [plan::StaleMapping {
            track_id: 1,
            card_path: "Music/a.mp3".into()
        }]
    );
    assert!(p.remove.is_empty() && p.replace.is_empty());

    // The same track out of scope: reported, neither copied nor removed.
    w.list(10, "Other", &[]);
    w.sel = Sel::Only(names(&["Other"]));
    let p = w.plan();
    assert!(p.copy.is_empty() && p.remove.is_empty(), "{p:?}");
    assert_eq!(p.stale_mappings.len(), 1);
    assert!(p.expected.files.is_empty());
}

// Prevents: a changed library file not being copied again over the card
// file, or the replace losing the card file's size.
#[test]
fn a_library_hash_that_differs_replaces_in_place() {
    let mut w = World::new();
    w.synced(1, "a.mp3", SIZE);
    w.row(1).hash = [0xAA; 32];
    w.row(1).size = 5_000;
    let p = w.plan();
    assert_eq!(
        p.replace,
        [ReplaceOp {
            track_id: 1,
            from: "/music/a.mp3".into(),
            to: "Music/a.mp3".into(),
            size: 5_000,
            old_size: SIZE,
            reason: ReplaceReason::Changed
        }]
    );
    assert!(p.copy.is_empty() && p.remove.is_empty() && p.moves.is_empty());
    assert_eq!(p.expected.files[0].size, 5_000);
}

// Prevents: a card file edited by another program being left as it is: its
// size is not the one recorded when it was copied.
#[test]
fn a_card_size_that_differs_from_the_synced_size_is_changed_on_card() {
    let mut w = World::new();
    w.track(1, "a.mp3", SIZE);
    w.map_only(1, "Music/a.mp3", SIZE);
    w.card_audio("Music/a.mp3", SIZE + 1);
    let p = w.plan();
    assert_eq!(p.replace.len(), 1);
    assert_eq!(p.replace[0].reason, ReplaceReason::ChangedOnCard);
    assert_eq!(p.replace[0].old_size, SIZE + 1);
    assert_eq!(p.replace[0].size, SIZE);
    assert!(p.stale_mappings.is_empty() && p.copy.is_empty());
}

// Prevents: the reason reporting the card when the library changed too.
#[test]
fn a_changed_library_file_wins_over_a_changed_card_file_as_the_reason() {
    let mut w = World::new();
    w.track(1, "a.mp3", SIZE);
    w.map_only(1, "Music/a.mp3", SIZE);
    w.card_audio("Music/a.mp3", SIZE + 1);
    w.row(1).hash = [9; 32];
    assert_eq!(w.plan().replace[0].reason, ReplaceReason::Changed);
}

// Prevents: a changed file whose card path also changes being replaced at
// its old path (or copied twice): it is the old file removed (not counted
// as a removal) plus a copy to the new path, and no move.
#[test]
fn a_replace_to_a_new_path_is_a_replaced_remove_and_a_copy() {
    let mut w = World::new();
    w.track(1, "new.mp3", 5_000);
    w.map_file(1, "Music/old.mp3", SIZE);
    w.row(1).hash = [7; 32];
    let p = w.plan();
    assert_eq!(
        p.remove,
        [RemoveOp {
            path: "Music/old.mp3".into(),
            track_id: Some(1),
            size: SIZE,
            reason: RemoveReason::Replaced
        }]
    );
    assert_eq!(p.copy.len(), 1);
    assert_eq!(p.copy[0].to, "Music/new.mp3");
    assert_eq!(p.copy[0].size, 5_000);
    assert!(p.moves.is_empty() && p.replace.is_empty());
    assert_eq!(
        p.removals(),
        0,
        "the old file of a replace is not a removal"
    );
    let paths: Vec<_> = p.expected.files.iter().map(|f| &f.path[..]).collect();
    assert_eq!(paths, ["Music/new.mp3"]);
}

// Prevents: an adopted file outside Music/ staying where it is, a move
// copying instead of renaming, and a changed name (case only) being ignored.
#[test]
fn a_mapped_file_at_another_path_is_moved_not_copied() {
    let mut w = World::new();
    w.track(1, "Album/a.mp3", SIZE);
    w.map_file(1, "Old layout/a.mp3", SIZE);
    w.track(2, "B.mp3", SIZE);
    w.map_file(2, "Music/b.mp3", SIZE);
    let p = w.plan();
    assert_eq!(
        p.moves,
        [
            MoveOp {
                track_id: 1,
                from: "Old layout/a.mp3".into(),
                to: "Music/Album/a.mp3".into()
            },
            MoveOp {
                track_id: 2,
                from: "Music/b.mp3".into(),
                to: "Music/B.mp3".into()
            },
        ]
    );
    assert!(p.copy.is_empty() && p.remove.is_empty() && p.replace.is_empty());
    let paths: Vec<_> = p.expected.files.iter().map(|f| &f.path[..]).collect();
    assert_eq!(paths, ["Music/Album/a.mp3", "Music/B.mp3"]);
    assert!(p.expected.files.iter().all(|f| f.size == SIZE));
}

// Prevents: managed files of tracks that left the scope or the library
// staying on the card, or the reason being wrong.
#[test]
fn files_out_of_scope_or_gone_from_the_library_are_removed_with_their_reason() {
    let mut w = World::new();
    w.synced(1, "keep.mp3", SIZE);
    w.synced(2, "out.mp3", SIZE);
    w.synced(3, "gone.mp3", SIZE);
    w.synced(4, "deleted-row.mp3", SIZE);
    w.row(3).missing_since = Some(5);
    w.lib.retain(|t| t.id != 4);
    w.list(10, "Keep", &some(&[1]));
    w.sel = Sel::Only(names(&["Keep"]));
    let p = w.plan();
    let got: Vec<_> = p
        .remove
        .iter()
        .map(|r| (r.path.as_str(), r.track_id, r.reason))
        .collect();
    assert_eq!(
        got,
        [
            (
                "Music/deleted-row.mp3",
                Some(4),
                RemoveReason::MissingFromLibrary
            ),
            ("Music/gone.mp3", Some(3), RemoveReason::MissingFromLibrary),
            ("Music/out.mp3", Some(2), RemoveReason::OutOfScope),
        ]
    );
    assert!(p.copy.is_empty() && p.replace.is_empty() && p.moves.is_empty());
    assert_eq!(p.removals(), 3);
    let kept: Vec<_> = p.expected.files.iter().map(|f| f.track_id).collect();
    assert_eq!(kept, [1]);
}

// Prevents: a missing track in scope All staying on the card (a missing
// file is not an in-scope file).
#[test]
fn a_missing_track_is_removed_even_in_scope_all() {
    let mut w = World::new();
    w.synced(1, "a.mp3", SIZE);
    w.row(1).missing_since = Some(1);
    let p = w.plan();
    assert_eq!(p.remove.len(), 1);
    assert_eq!(p.remove[0].reason, RemoveReason::MissingFromLibrary);
    assert!(p.copy.is_empty());
}

// Prevents: unmanaged audio being touched by default, or surviving
// remove_unmanaged; and a managed file counting as unmanaged.
#[test]
fn unmanaged_audio_is_kept_and_listed_or_removed_when_asked() {
    let mut w = World::new();
    w.synced(1, "a.mp3", SIZE);
    w.card_audio("Zed/z.mp3", 11);
    w.card_audio("Other/o.flac", 22);
    let p = w.plan();
    assert!(p.is_empty());
    assert_eq!(p.unmanaged.files, ["Other/o.flac", "Zed/z.mp3"]);
    assert_eq!(
        p.expected.unmanaged,
        [
            ("Other/o.flac".to_owned(), 22),
            ("Zed/z.mp3".to_owned(), 11)
        ]
    );

    w.remove_unmanaged = true;
    let p = w.plan();
    assert_eq!(
        p.remove,
        [
            RemoveOp {
                path: "Other/o.flac".into(),
                track_id: None,
                size: 22,
                reason: RemoveReason::Unmanaged
            },
            RemoveOp {
                path: "Zed/z.mp3".into(),
                track_id: None,
                size: 11,
                reason: RemoveReason::Unmanaged
            },
        ]
    );
    assert!(p.unmanaged.files.is_empty() && p.expected.unmanaged.is_empty());
    assert_eq!(p.removals(), 0, "unmanaged removals are not mass removals");
    assert_eq!(p.expected.files.len(), 1);
}

// Prevents: a protected or oversized file being copied, and the boundary
// moving: 4 GiB less one byte is the largest file FAT32 holds.
#[test]
fn fairplay_and_four_gib_files_are_skipped_and_the_largest_fat_file_is_copied() {
    let mut w = World::new();
    w.track(1, "drm.m4a", SIZE);
    w.row(1).drm = true;
    w.track(2, "huge.wav", FOUR_GIB);
    w.track(3, "largest.wav", FOUR_GIB - 1);
    let p = w.plan();
    assert_eq!(
        p.skipped,
        [
            Skipped {
                track_id: 1,
                path: "/music/drm.m4a".into(),
                reason: SkipReason::FairPlay
            },
            Skipped {
                track_id: 2,
                path: "/music/huge.wav".into(),
                reason: SkipReason::TooLarge
            },
        ]
    );
    assert_eq!(p.copy.len(), 1);
    assert_eq!(p.copy[0].track_id, 3);
    assert_eq!(p.expected.files.len(), 1);
}

// Prevents: a track under none of the library roots being copied to a
// made-up path, or the longest root not being the one used.
#[test]
fn a_track_under_no_root_is_skipped_and_the_longest_root_is_used() {
    let mut w = World::new();
    w.roots = vec![PathBuf::from("/music"), PathBuf::from("/music/deep")];
    w.track(1, "deep/x.mp3", SIZE);
    w.track(2, "y.mp3", SIZE);
    w.track(3, "z.mp3", SIZE);
    w.row(3).path = PathBuf::from("/elsewhere/z.mp3");
    let p = w.plan();
    let to: Vec<_> = p.copy.iter().map(|c| &c.to[..]).collect();
    assert_eq!(to, ["Music/x.mp3", "Music/y.mp3"]);
    assert_eq!(
        p.skipped,
        [Skipped {
            track_id: 3,
            path: "/elsewhere/z.mp3".into(),
            reason: SkipReason::NoCardPath
        }]
    );
}

// Prevents: a mapped track under no root being removed, or its playlist
// entry dropped: it stays where it is.
#[test]
fn a_mapped_track_under_no_root_stays_in_place() {
    let mut w = World::new();
    w.track(1, "z.mp3", SIZE);
    w.row(1).path = PathBuf::from("/elsewhere/z.mp3");
    w.map_file(1, "Old/z.mp3", SIZE);
    w.list(10, "P", &some(&[1]));
    let p = w.plan();
    assert_eq!(p.skipped.len(), 1);
    assert_eq!(p.skipped[0].reason, SkipReason::NoCardPath);
    assert!(p.copy.is_empty() && p.remove.is_empty() && p.moves.is_empty());
    assert_eq!(p.expected.files[0].path, "Old/z.mp3");
    assert!(write_text(&p, "P.m3u8").contains("\nOld/z.mp3\n"));
    assert!(p.entries_skipped.is_empty());
}

// Prevents: an unmanaged card file being overwritten: the target is held
// (by FAT key, so another case counts), the track is skipped, and its
// playlist entry is dropped with NotOnCard.
#[test]
fn a_target_held_by_an_unmanaged_file_is_never_written() {
    let mut w = World::new();
    w.track(1, "a.mp3", SIZE);
    w.card_audio("MUSIC/A.MP3", 99);
    w.list(10, "P", &some(&[1]));
    let p = w.plan();
    assert!(p.copy.is_empty() && p.remove.is_empty() && p.replace.is_empty());
    assert_eq!(
        p.skipped,
        [Skipped {
            track_id: 1,
            path: "/music/a.mp3".into(),
            reason: SkipReason::Occupied
        }]
    );
    assert_eq!(p.unmanaged.files, ["MUSIC/A.MP3"]);
    assert_eq!(
        p.entries_skipped,
        [EntrySkipped {
            playlist: "P".into(),
            position: 1,
            reason: EntryReason::NotOnCard
        }]
    );
}

// Prevents: a managed file that cannot move being moved over an unmanaged
// one, or being removed/copied instead of staying put; and the unmanaged
// file not going first with remove_unmanaged.
#[test]
fn a_managed_file_that_cannot_move_stays_put_unless_the_unmanaged_file_goes() {
    let mut w = World::new();
    w.track(1, "a.mp3", SIZE);
    w.map_file(1, "Old/a.mp3", SIZE);
    w.card_audio("Music/a.mp3", 99);
    let p = w.plan();
    assert!(p.moves.is_empty() && p.remove.is_empty() && p.copy.is_empty());
    assert_eq!(p.skipped.len(), 1);
    assert_eq!(p.skipped[0].reason, SkipReason::Occupied);
    assert_eq!(p.expected.files[0].path, "Old/a.mp3");
    assert_eq!(p.unmanaged.files, ["Music/a.mp3"]);

    // A changed file that cannot move is replaced where it is.
    w.row(1).hash = [3; 32];
    let p = w.plan();
    assert_eq!(p.replace.len(), 1);
    assert_eq!(p.replace[0].to, "Old/a.mp3");
    assert!(p.copy.is_empty() && p.remove.is_empty());

    w.row(1).hash = hash_of(1);
    w.remove_unmanaged = true;
    let p = w.plan();
    assert_eq!(
        p.moves,
        [MoveOp {
            track_id: 1,
            from: "Old/a.mp3".into(),
            to: "Music/a.mp3".into()
        }]
    );
    assert_eq!(p.remove.len(), 1);
    assert_eq!(p.remove[0].path, "Music/a.mp3");
    assert_eq!(p.remove[0].reason, RemoveReason::Unmanaged);
    assert!(p.skipped.is_empty());
}

// Prevents: FairPlay or 4 GiB files already on the card being replaced,
// skipped or removed: they stay as they are, even if card and library
// disagree about them.
#[test]
fn drm_and_large_files_already_on_the_card_are_kept_untouched() {
    let mut w = World::new();
    w.synced(1, "drm.m4a", SIZE);
    w.row(1).drm = true;
    w.row(1).hash = [0xEE; 32];
    w.card_audio("Music/big.wav", FOUR_GIB + 5);
    w.track(2, "big.wav", FOUR_GIB + 5);
    w.map_only(2, "Music/big.wav", 123);
    w.row(2).hash = [0xEE; 32];
    let p = w.plan();
    assert!(p.is_empty(), "{p:?}");
    assert!(p.skipped.is_empty());
    let kept: Vec<_> = p
        .expected
        .files
        .iter()
        .map(|f| (f.track_id, f.size))
        .collect();
    assert_eq!(kept, [(2, FOUR_GIB + 5), (1, SIZE)]);
}

// Prevents: a stale mapping of a protected file being copied again.
#[test]
fn a_stale_fairplay_mapping_is_skipped_not_copied() {
    let mut w = World::new();
    w.track(1, "drm.m4a", SIZE);
    w.row(1).drm = true;
    w.map_only(1, "Music/drm.m4a", SIZE);
    let p = w.plan();
    assert!(p.copy.is_empty());
    assert_eq!(p.skipped[0].reason, SkipReason::FairPlay);
    assert_eq!(p.stale_mappings.len(), 1);
    assert!(p.remove.is_empty());
}

// ----------------------------------------------------------------- scope

fn scope_world() -> World {
    // 1 only in A, 2 in A and B, 3 only in B, 4 in none.
    let mut w = World::new();
    for id in 1..=4 {
        w.track(id, &format!("t{id}.mp3"), SIZE);
    }
    w.list(10, "A", &some(&[1, 2]));
    w.list(11, "B", &some(&[2, 3]));
    w
}

fn copied(p: &Plan) -> Vec<i64> {
    let mut v: Vec<i64> = p.copy.iter().map(|c| c.track_id).collect();
    v.sort();
    v
}

// Prevents: each scope sending the wrong tracks, in particular Except
// keeping a track of an excluded playlist because another playlist holds
// it too.
#[test]
fn the_scope_chooses_the_tracks() {
    let mut w = scope_world();
    assert_eq!(copied(&w.plan()), [1, 2, 3, 4]);

    w.sel = Sel::Only(names(&["A"]));
    assert_eq!(copied(&w.plan()), [1, 2]);
    w.sel = Sel::Only(names(&["A", "B"]));
    assert_eq!(copied(&w.plan()), [1, 2, 3]);

    w.sel = Sel::Except(names(&["B"]));
    assert_eq!(copied(&w.plan()), [1, 4], "2 is in B, which is excluded");
    w.sel = Sel::Except(names(&[]));
    assert_eq!(copied(&w.plan()), [1, 2, 3, 4]);
    w.sel = Sel::Only(names(&[]));
    assert!(w.plan().copy.is_empty());
}

// Prevents: playlist selection and track selection disagreeing.
#[test]
fn the_scope_chooses_the_playlists() {
    let mut w = scope_world();
    w.sel = Sel::Only(names(&["A"]));
    assert_eq!(ops(&w.plan()), ["write A.m3u8"]);
    w.sel = Sel::Except(names(&["A"]));
    assert_eq!(ops(&w.plan()), ["write B.m3u8"]);
    assert!(Scope::All.takes_playlist("x"));
    let n = names(&["x"]);
    assert!(Scope::Playlists(&n).takes_playlist("x"));
    assert!(!Scope::Playlists(&n).takes_playlist("X"));
    assert!(!Scope::Except(&n).takes_playlist("x"));
    assert!(Scope::Except(&n).takes_playlist("y"));
}

// Prevents: a track disappearing silently from a playlist that stays when
// another playlist leaves the scope (Except): the entry is dropped with
// NotOnCard.
#[test]
fn an_entry_of_a_track_excluded_by_another_playlist_is_reported() {
    let mut w = scope_world();
    w.sel = Sel::Except(names(&["B"]));
    let p = w.plan();
    assert_eq!(
        write_text(&p, "A.m3u8"),
        "#EXTM3U\n#EXTINF:200,Artist1 - Title1\nMusic/t1.mp3\n"
    );
    assert_eq!(
        p.entries_skipped,
        [EntrySkipped {
            playlist: "A".into(),
            position: 2,
            reason: EntryReason::NotOnCard
        }]
    );
}

// Prevents: a scope change moving files: card paths are computed over the
// whole library, so a name collision is settled the same way whatever is
// in scope.
#[test]
fn card_paths_do_not_change_with_the_scope() {
    let mut w = World::new();
    // On FAT these are one name; "A.mp3" sorts first and keeps it.
    w.track(1, "a.mp3", SIZE);
    w.track(2, "A.mp3", SIZE);
    let all = w.plan();
    let path = |p: &Plan, id: i64| p.copy.iter().find(|c| c.track_id == id).unwrap().to.clone();
    assert_eq!(path(&all, 2), "Music/A.mp3");
    assert_eq!(path(&all, 1), "Music/a (2).mp3");

    w.list(10, "Only1", &some(&[1]));
    w.sel = Sel::Only(names(&["Only1"]));
    let p = w.plan();
    assert_eq!(copied(&p), [1]);
    assert_eq!(path(&p, 1), "Music/a (2).mp3");
}

// ------------------------------------------------------------- playlists

// Prevents: the rendered text drifting: header, EXTINF with rounded
// duration, artist, title, LF endings, entries in order, the same track
// twice, a track with no tags.
#[test]
fn a_playlist_is_rendered_byte_for_byte() {
    let mut w = World::new();
    w.track(1, "1.mp3", SIZE);
    w.track(2, "d/2.mp3", SIZE);
    w.track(3, "3.mp3", SIZE);
    w.row(2).info.duration_ms = Some(200_500);
    w.row(3).info.duration_ms = None;
    w.row(3).tags.artist = None;
    w.row(3).tags.title = None;
    w.list(10, "Rock", &some(&[2, 1, 2, 3]));
    let p = w.plan();
    assert_eq!(
        write_text(&p, "Rock.m3u8"),
        "#EXTM3U\n\
         #EXTINF:201,Artist2 - Title2\nMusic/d/2.mp3\n\
         #EXTINF:200,Artist1 - Title1\nMusic/1.mp3\n\
         #EXTINF:201,Artist2 - Title2\nMusic/d/2.mp3\n\
         #EXTINF:-1,3\nMusic/3.mp3\n"
    );
    match &p.playlists[0] {
        PlaylistOp::Write {
            playlist_id,
            entries,
            ..
        } => assert_eq!((*playlist_id, *entries), (10, 4)),
        other => panic!("{other:?}"),
    }
    assert_eq!(p.expected.playlists.len(), 1);
    assert_eq!(p.expected.playlists[0].path, "Rock.m3u8");
    assert_eq!(p.expected.playlists[0].playlist_id, 10);
    assert_eq!(p.expected.playlists[0].text, write_text(&p, "Rock.m3u8"));
}

// Prevents: an empty playlist not being written, or written without header.
#[test]
fn an_empty_playlist_is_written_as_a_header() {
    let mut w = World::new();
    w.list(10, "Empty", &[]);
    let p = w.plan();
    assert_eq!(write_text(&p, "Empty.m3u8"), "#EXTM3U\n");
}

// Prevents: playlists being rewritten on every sync (equal bytes), and a
// changed byte not being rewritten, however small.
#[test]
fn a_playlist_is_written_only_when_its_bytes_differ() {
    let mut w = World::new();
    w.synced(1, "a.mp3", SIZE);
    w.list(10, "Rock", &some(&[1]));
    let text = write_text(&w.plan(), "Rock.m3u8").to_owned();
    w.map_list(10, "Rock.m3u8");

    w.card_list("Rock.m3u8", text.as_bytes());
    let p = w.plan();
    assert!(p.is_empty(), "{:?}", ops(&p));
    assert_eq!(p.expected.playlists[0].text, text);

    for other in [
        text.replace('\n', "\r\n"),
        text.trim_end().to_owned(),
        format!("{text}\n"),
        String::new(),
    ] {
        w.bytes.insert("Rock.m3u8".into(), other.into_bytes());
        let p = w.plan();
        assert_eq!(ops(&p), ["write Rock.m3u8"]);
        assert_eq!(write_text(&p, "Rock.m3u8"), text);
    }
    // The mapped file gone: written again.
    w.card_lists.clear();
    assert_eq!(ops(&w.plan()), ["write Rock.m3u8"]);
}

// Prevents: a renamed playlist leaving its old file behind, or the rename
// being skipped when the bytes are the same; a case-only rename being taken
// for no change (FAT compares names without case, the planner must not).
#[test]
fn a_renamed_playlist_is_renamed_and_then_written_only_if_its_bytes_differ() {
    let mut w = World::new();
    w.synced(1, "a.mp3", SIZE);
    w.list(10, "New", &some(&[1]));
    let text = write_text(&w.plan(), "New.m3u8").to_owned();
    w.map_list(10, "Old.m3u8");
    w.card_list("Old.m3u8", b"#EXTM3U\n");
    let p = w.plan();
    assert_eq!(ops(&p), ["rename Old.m3u8 New.m3u8", "write New.m3u8"]);
    assert!(matches!(
        p.playlists[0],
        PlaylistOp::Rename {
            playlist_id: 10,
            ..
        }
    ));

    w.bytes.insert("Old.m3u8".into(), text.into_bytes());
    assert_eq!(ops(&w.plan()), ["rename Old.m3u8 New.m3u8"]);

    // Case only.
    let mut c = World::new();
    c.synced(1, "a.mp3", SIZE);
    c.list(10, "rock", &some(&[1]));
    let text = write_text(&c.plan(), "rock.m3u8").to_owned();
    c.map_list(10, "Rock.m3u8");
    c.card_list("Rock.m3u8", text.as_bytes());
    assert_eq!(ops(&c.plan()), ["rename Rock.m3u8 rock.m3u8"]);
}

// Prevents: the file name rules: characters a file name cannot hold, and
// playlists FAT takes for one getting the same file.
#[test]
fn playlist_file_names_are_cleaned_and_made_distinct_for_fat() {
    let mut w = World::new();
    w.list(1, "A/B", &[]);
    w.list(2, "Rock", &[]);
    w.list(3, "rock", &[]);
    w.list(4, "ROCK", &[]);
    let p = w.plan();
    assert_eq!(
        ops(&p),
        [
            "write A_B.m3u8",
            "write ROCK.m3u8",
            "write Rock (2).m3u8",
            "write rock (3).m3u8"
        ]
    );
    assert_eq!(plan::playlist_file_name("a", 1), "a.m3u8");
    assert_eq!(plan::playlist_file_name("a", 2), "a (2).m3u8");
}

// Prevents: a playlist out of scope staying on the card; one never sent
// being "deleted"; and a tombstone not deleting its file.
#[test]
fn playlists_out_of_scope_and_tombstones_are_deleted_from_the_card() {
    let mut w = World::new();
    w.list(1, "Out", &[]);
    w.list(2, "NeverSent", &[]);
    w.list(3, "In", &[]);
    w.removed = vec![(4, "Dead".into()), (5, "AlreadyGone".into())];
    w.map_list(1, "Out.m3u8");
    w.card_list("Out.m3u8", b"#EXTM3U\n");
    w.map_list(4, "Dead.m3u8");
    w.card_list("Dead.m3u8", b"#EXTM3U\n");
    w.map_list(5, "AlreadyGone.m3u8");
    w.sel = Sel::Only(names(&["In"]));
    let p = w.plan();
    assert_eq!(
        p.playlists,
        [
            PlaylistOp::Delete {
                playlist_id: Some(4),
                path: "Dead.m3u8".into(),
                reason: PlaylistDeleteReason::FileDeleted
            },
            PlaylistOp::Delete {
                playlist_id: Some(1),
                path: "Out.m3u8".into(),
                reason: PlaylistDeleteReason::OutOfScope
            },
            PlaylistOp::Write {
                playlist_id: 3,
                path: "In.m3u8".into(),
                text: "#EXTM3U\n".into(),
                entries: 0
            },
        ]
    );
    assert!(
        p.unmanaged.playlists.is_empty(),
        "deleted files are not unmanaged"
    );
    let kept: Vec<_> = p.expected.playlists.iter().map(|e| &e.path[..]).collect();
    assert_eq!(kept, ["In.m3u8"]);
}

// Prevents: a tombstone deleting a file of an unmapped, same-named card
// playlist (not ours).
#[test]
fn a_tombstone_never_deletes_an_unmapped_playlist_of_the_same_name() {
    let mut w = World::new();
    w.removed = vec![(4, "Dead".into())];
    w.card_list("Dead.m3u8", b"#EXTM3U\n");
    let p = w.plan();
    assert!(p.is_empty(), "{:?}", ops(&p));
    assert_eq!(p.unmanaged.playlists, ["Dead.m3u8"]);
}

// Prevents: fiio-tunes taking over a card playlist it did not write: its
// name is avoided, and it is reported and kept. One elsewhere on the card
// does not block the name.
#[test]
fn an_unmanaged_playlist_is_kept_and_its_name_avoided() {
    let mut w = World::new();
    w.list(1, "Rock", &[]);
    w.card_list("Rock.m3u8", b"mine\n");
    w.card_list("Other/Pop.m3u8", b"theirs\n");
    w.list(2, "Pop", &[]);
    let p = w.plan();
    assert_eq!(ops(&p), ["write Pop.m3u8", "write Rock (2).m3u8"]);
    assert_eq!(p.unmanaged.playlists, ["Other/Pop.m3u8", "Rock.m3u8"]);
    assert_eq!(
        p.expected.unmanaged,
        [
            ("Other/Pop.m3u8".to_owned(), 7),
            ("Rock.m3u8".to_owned(), 5)
        ]
    );
    // Avoided whatever its case.
    let mut w = World::new();
    w.list(1, "Rock", &[]);
    w.card_list("ROCK.M3U8", b"mine\n");
    assert_eq!(ops(&w.plan()), ["write Rock (2).m3u8"]);
}

// Prevents: remove_unmanaged leaving .m3u8 files, or not freeing the name:
// the delete comes before the write that takes the name.
#[test]
fn remove_unmanaged_deletes_unmanaged_playlists_before_writing() {
    let mut w = World::new();
    w.list(1, "Rock", &[]);
    w.card_list("Rock.m3u8", b"mine\n");
    w.card_list("Other/Pop.m3u8", b"theirs\n");
    w.remove_unmanaged = true;
    let p = w.plan();
    assert_eq!(
        p.playlists,
        [
            PlaylistOp::Delete {
                playlist_id: None,
                path: "Other/Pop.m3u8".into(),
                reason: PlaylistDeleteReason::Unmanaged
            },
            PlaylistOp::Delete {
                playlist_id: None,
                path: "Rock.m3u8".into(),
                reason: PlaylistDeleteReason::Unmanaged
            },
            PlaylistOp::Write {
                playlist_id: 1,
                path: "Rock.m3u8".into(),
                text: "#EXTM3U\n".into(),
                entries: 0
            },
        ]
    );
    assert!(p.unmanaged.playlists.is_empty() && p.expected.unmanaged.is_empty());
}

// Prevents: the playlists folder being ignored: files go into it, locations
// become relative to it, and only .m3u8 files in it block names.
#[test]
fn a_playlists_folder_changes_paths_and_locations() {
    let mut w = World::new();
    w.playlists_dir = "Playlists".into();
    w.track(1, "d/a.mp3", SIZE);
    w.list(10, "Rock", &some(&[1]));
    let p = w.plan();
    assert_eq!(ops(&p), ["write Playlists/Rock.m3u8"]);
    assert_eq!(
        write_text(&p, "Playlists/Rock.m3u8"),
        "#EXTM3U\n#EXTINF:200,Artist1 - Title1\n../Music/d/a.mp3\n"
    );
    // The root has a Rock.m3u8 that is not in the playlists folder.
    w.card_list("Rock.m3u8", b"x");
    assert_eq!(ops(&w.plan()), ["write Playlists/Rock.m3u8"]);
    // One in the folder blocks the name, whatever its case.
    w.card_list("playlists/rock.m3u8", b"x");
    assert_eq!(ops(&w.plan()), ["write Playlists/Rock (2).m3u8"]);
}

// Prevents: a managed playlist in a playlists folder not being found on the
// card (it would be written again each time).
#[test]
fn a_playlist_in_the_playlists_folder_is_matched_by_its_mapped_name() {
    let mut w = World::new();
    w.playlists_dir = "Playlists".into();
    w.synced(1, "a.mp3", SIZE);
    w.list(10, "Rock", &some(&[1]));
    let text = write_text(&w.plan(), "Playlists/Rock.m3u8").to_owned();
    w.map_list(10, "Rock.m3u8");
    w.card_list("Playlists/Rock.m3u8", text.as_bytes());
    let p = w.plan();
    assert!(p.is_empty(), "{:?}", ops(&p));
    assert!(p.unmanaged.playlists.is_empty());
}

// Prevents: entries dropped silently or with the wrong position or reason,
// and a dropped entry leaving a hole in the file.
#[test]
fn entries_that_cannot_be_written_are_left_out_and_reported_with_positions() {
    let mut w = World::new();
    w.track(1, "ok.mp3", SIZE);
    w.track(2, "miss.mp3", SIZE);
    w.row(2).missing_since = Some(3);
    w.track(3, "drm.m4a", SIZE);
    w.row(3).drm = true;
    w.list(
        10,
        "P",
        &[None, Some(1), Some(2), Some(3), Some(99), Some(1)],
    );
    w.list(11, "Q", &some(&[1]));
    w.sel = Sel::Only(names(&["P"]));
    let p = w.plan();
    let skips = |reason, position| EntrySkipped {
        playlist: "P".into(),
        position,
        reason,
    };
    assert_eq!(
        p.entries_skipped,
        [
            skips(EntryReason::Unresolved, 1),
            skips(EntryReason::Missing, 3),
            skips(EntryReason::NotOnCard, 4),
            skips(EntryReason::Missing, 5),
        ]
    );
    assert_eq!(
        write_text(&p, "P.m3u8"),
        "#EXTM3U\n#EXTINF:200,Artist1 - Title1\nMusic/ok.mp3\n\
         #EXTINF:200,Artist1 - Title1\nMusic/ok.mp3\n"
    );
    // Q is out of scope: nothing of it is reported.
    assert!(p.entries_skipped.iter().all(|e| e.playlist == "P"));
}

// Prevents: operations in an order the executor cannot rely on: deletes
// first (they free names), then renames, then writes, each by name.
#[test]
fn playlist_operations_run_deletes_then_renames_then_writes_each_by_name() {
    let mut w = World::new();
    w.list(1, "Zeta", &[]);
    w.list(2, "Alpha", &[]);
    w.list(3, "Mid", &[]);
    w.list(4, "Gone", &[]);
    w.list(5, "Beta", &[]);
    w.removed = vec![(9, "Dead".into())];
    w.map_list(3, "Old mid.m3u8");
    w.card_list("Old mid.m3u8", b"x");
    w.map_list(5, "Old beta.m3u8");
    w.card_list("Old beta.m3u8", b"x");
    w.map_list(4, "Gone.m3u8");
    w.card_list("Gone.m3u8", b"x");
    w.map_list(9, "Dead.m3u8");
    w.card_list("Dead.m3u8", b"x");
    w.card_list("Stray/zz.m3u8", b"x");
    w.remove_unmanaged = true;
    w.sel = Sel::Except(names(&["Gone"]));
    assert_eq!(
        ops(&w.plan()),
        [
            "delete Dead.m3u8",
            "delete Gone.m3u8",
            "delete Stray/zz.m3u8",
            "rename Old beta.m3u8 Beta.m3u8",
            "rename Old mid.m3u8 Mid.m3u8",
            "write Alpha.m3u8",
            "write Beta.m3u8",
            "write Mid.m3u8",
            "write Zeta.m3u8",
        ]
    );
}

// ----------------------------------------------------------------- space

fn sp(capacity: u64, free: u64) -> Option<Space> {
    Some(Space { capacity, free })
}

// Prevents: the 1% margin being dropped or off by one: peak plus 1% of the
// capacity must fit in the free space.
#[test]
fn the_space_check_keeps_a_one_percent_margin() {
    let mut w = World::new();
    w.track(1, "a.mp3", 1_000);
    w.space = sp(100_000, 2_000);
    let p = w.plan();
    assert_eq!((p.space.peak, p.space.needed), (1_000, 1_000));
    assert!(p.space.fits);
    assert_eq!(p.space.device, sp(100_000, 2_000));
    w.space = sp(100_000, 1_999);
    assert!(!w.plan().space.fits);
    w.space = sp(100_000, 999);
    assert!(!w.plan().space.fits);
}

// Prevents: an unknown size refusing the plan, and a plan that adds
// nothing being refused on a full card.
#[test]
fn unknown_space_and_plans_that_add_nothing_fit() {
    let mut w = World::new();
    w.track(1, "a.mp3", 1_000);
    w.space = None;
    let p = w.plan();
    assert!(p.space.fits);
    assert_eq!(p.space.device, None);

    let mut w = World::new();
    w.synced(1, "a.mp3", 1_000);
    w.space = sp(100_000, 0);
    assert!(w.plan().space.fits, "a full card needs nothing");
    // A plan that frees space: negative, and fits.
    w.row(1).missing_since = Some(1);
    let p = w.plan();
    assert_eq!((p.space.peak, p.space.needed), (-1_000, -1_000));
    assert!(p.space.fits);
}

// Prevents: a replace being counted as net-only at the peak: the new file
// and the old one are both on the card until the rename; removals first.
#[test]
fn a_replace_holds_both_files_at_the_peak() {
    let mut w = World::new();
    w.synced(1, "a.mp3", 3_000);
    w.row(1).hash = [8; 32];
    w.row(1).size = 5_000;
    w.space = sp(100_000, 5_000 + 1_000);
    let p = w.plan();
    assert_eq!(p.space.peak, 5_000);
    assert_eq!(p.space.needed, 2_000);
    assert!(p.space.fits);
    w.space = sp(100_000, 5_999);
    assert!(!w.plan().space.fits, "the peak is 5000, not the net 2000");

    // With a removal: removals run first, a replaced file does not free.
    w.synced(2, "b.mp3", 4_000);
    w.row(2).missing_since = Some(1);
    let p = w.plan();
    assert_eq!(p.space.peak, 1_000);
    assert_eq!(p.space.needed, -2_000);
}

// Prevents: a replace to a new path counting its old file twice, or a
// copy's size being taken from the card.
#[test]
fn a_replaced_remove_frees_its_old_size_at_the_peak() {
    let mut w = World::new();
    w.track(1, "new.mp3", 5_000);
    w.map_file(1, "Music/old.mp3", 3_000);
    w.row(1).hash = [8; 32];
    let p = w.plan();
    assert_eq!((p.space.peak, p.space.needed), (2_000, 2_000));
}

// ------------------------------------------------------- guards and misc

fn removal(reason: RemoveReason) -> RemoveOp {
    RemoveOp {
        path: "x".into(),
        track_id: Some(1),
        size: 1,
        reason,
    }
}

// Prevents: the mass-removal guard counting the wrong removals.
#[test]
fn removals_count_only_tracks_that_left_the_scope_or_the_library() {
    let p = Plan {
        remove: vec![
            removal(RemoveReason::MissingFromLibrary),
            removal(RemoveReason::OutOfScope),
            removal(RemoveReason::Replaced),
            removal(RemoveReason::Unmanaged),
        ],
        ..Plan::default()
    };
    assert_eq!(p.removals(), 2);
}

// Prevents: the thresholds moving: more than five AND more than a tenth.
#[test]
fn mass_removal_needs_more_than_five_and_more_than_a_tenth() {
    let with = |n: usize| Plan {
        remove: (0..n).map(|_| removal(RemoveReason::OutOfScope)).collect(),
        ..Plan::default()
    };
    assert!(!with(5).mass_removal(10), "five is not more than five");
    assert!(with(6).mass_removal(10));
    assert!(!with(6).mass_removal(60), "a tenth exactly is not more");
    assert!(with(6).mass_removal(59));
    assert!(!with(6).mass_removal(1_000));
    assert!(with(101).mass_removal(1_000));
    assert!(!with(100).mass_removal(1_000));
    assert!(!with(0).mass_removal(0));
}

// Prevents: is_empty ignoring one kind of change, or counting what changes
// nothing (skips, unmanaged files, stale mappings).
#[test]
fn is_empty_looks_at_every_kind_of_change() {
    assert!(Plan::default().is_empty());
    let one = |f: fn(&mut Plan)| {
        let mut p = Plan::default();
        f(&mut p);
        assert!(!p.is_empty());
    };
    one(|p| {
        p.copy.push(CopyOp {
            track_id: 1,
            from: "a".into(),
            to: "b".into(),
            size: 1,
        })
    });
    one(|p| {
        p.replace.push(ReplaceOp {
            track_id: 1,
            from: "a".into(),
            to: "b".into(),
            size: 1,
            old_size: 1,
            reason: ReplaceReason::Changed,
        })
    });
    one(|p| {
        p.moves.push(MoveOp {
            track_id: 1,
            from: "a".into(),
            to: "b".into(),
        })
    });
    one(|p| p.remove.push(removal(RemoveReason::OutOfScope)));
    one(|p| {
        p.playlists.push(PlaylistOp::Delete {
            playlist_id: None,
            path: "x".into(),
            reason: PlaylistDeleteReason::Unmanaged,
        })
    });
    let mut p = Plan::default();
    p.skipped.push(Skipped {
        track_id: 1,
        path: "a".into(),
        reason: SkipReason::FairPlay,
    });
    p.unmanaged.files.push("u".into());
    p.stale_mappings.push(plan::StaleMapping {
        track_id: 1,
        card_path: "a".into(),
    });
    assert!(p.is_empty());
}

// ------------------------------------------------- determinism, idempotence

/// The card and the mappings as the plan expects them once it is applied.
fn apply(w: &mut World, p: &Plan) {
    let dir = w.playlists_dir.clone();
    w.audio.clear();
    w.card_lists.clear();
    w.bytes.clear();
    w.tmap.clear();
    w.pmap.clear();
    w.removed.clear();
    for f in &p.expected.files {
        let hash = w.lib.iter().find(|t| t.id == f.track_id).unwrap().hash;
        w.audio.push(CardFile {
            path: f.path.clone(),
            size: f.size,
            mtime_ns: 0,
        });
        w.tmap.push(TrackMapping {
            track_id: f.track_id,
            card_path: f.path.clone(),
            synced_hash: hash,
            synced_size: f.size,
        });
    }
    for (path, size) in &p.expected.unmanaged {
        if path.ends_with(".m3u8") {
            w.card_lists.push(CardFile {
                path: path.clone(),
                size: *size,
                mtime_ns: 0,
            });
            w.bytes.insert(path.clone(), vec![b'?'; *size as usize]);
        } else {
            w.audio.push(CardFile {
                path: path.clone(),
                size: *size,
                mtime_ns: 0,
            });
        }
    }
    for e in &p.expected.playlists {
        w.card_lists.push(CardFile {
            path: e.path.clone(),
            size: e.text.len() as u64,
            mtime_ns: 0,
        });
        w.bytes.insert(e.path.clone(), e.text.clone().into_bytes());
        let name = match dir.as_str() {
            "" => e.path.clone(),
            d => e.path.strip_prefix(&format!("{d}/")).unwrap().to_owned(),
        };
        w.pmap.push(PlaylistMapping {
            playlist_id: e.playlist_id,
            card_name: name,
        });
    }
    w.audio.sort_by(|a, b| a.path.cmp(&b.path));
    w.card_lists.sort_by(|a, b| a.path.cmp(&b.path));
}

// Prevents: a plan that does not converge: after the card and mappings are
// brought to what the plan expects, the next plan has nothing to do.
#[test]
fn a_second_plan_after_applying_the_first_is_empty() {
    let mut w = World::new();
    w.playlists_dir = "Playlists".into();
    w.track(1, "new.mp3", SIZE);
    w.synced(2, "changed.mp3", SIZE);
    w.row(2).hash = [0xD2; 32];
    w.row(2).size = 6_000;
    w.track(3, "oncard.mp3", SIZE);
    w.map_only(3, "Music/oncard.mp3", SIZE);
    w.card_audio("Music/oncard.mp3", SIZE + 9);
    w.track(4, "moved.mp3", SIZE);
    w.map_file(4, "Elsewhere/moved.mp3", SIZE);
    w.track(5, "renamed.mp3", SIZE);
    w.map_file(5, "Music/before.mp3", SIZE);
    w.row(5).hash = [0xC5; 32];
    w.synced(6, "gone.mp3", SIZE);
    w.row(6).missing_since = Some(1);
    w.synced(7, "out.mp3", SIZE);
    w.track(8, "stale.mp3", SIZE);
    w.map_only(8, "Music/stale.mp3", SIZE);
    w.track(9, "drm.m4a", SIZE);
    w.row(9).drm = true;
    w.card_audio("Unmanaged/u.mp3", 12);
    w.card_list("Rock.m3u8", b"theirs");
    w.list(
        10,
        "Rock",
        &[Some(1), Some(2), None, Some(9), Some(3), Some(4), Some(5)],
    );
    w.list(11, "Old", &some(&[7]));
    w.map_list(11, "Old.m3u8");
    w.card_list("Playlists/Old.m3u8", b"#EXTM3U\n");
    w.removed = vec![(12, "Dead".into())];
    w.map_list(12, "Dead.m3u8");
    w.card_list("Playlists/Dead.m3u8", b"#EXTM3U\n");
    w.sel = Sel::Except(names(&["Old"]));

    let p1 = w.plan();
    assert!(!p1.is_empty());
    assert_eq!(copied(&p1), [1, 5, 8]);
    assert_eq!(p1.replace.len(), 2);
    assert_eq!(p1.moves.len(), 1);
    apply(&mut w, &p1);
    let p2 = w.plan();
    assert!(
        p2.is_empty(),
        "{:?} {:?} {:?} {:?} {:?}",
        p2.copy,
        p2.replace,
        p2.moves,
        p2.remove,
        p2.playlists
    );
    assert_eq!(p2.expected, p1.expected);
    assert_eq!(p2.unmanaged, p1.unmanaged);
}

fn reversed<T: Clone>(v: &[T]) -> Vec<T> {
    v.iter().rev().cloned().collect()
}

fn rotated<T: Clone>(v: &[T], by: usize) -> Vec<T> {
    let mut v = v.to_vec();
    if !v.is_empty() {
        let n = by % v.len();
        v.rotate_left(n);
    }
    v
}

fn shuffled_world(w: &World, by: usize) -> World {
    let mut s = w.clone();
    s.lib = rotated(&reversed(&w.lib), by);
    s.lists = rotated(&reversed(&w.lists), by + 1);
    s.removed = rotated(&reversed(&w.removed), by);
    s.audio = rotated(&reversed(&w.audio), by + 2);
    s.card_lists = rotated(&reversed(&w.card_lists), by);
    s.tmap = rotated(&reversed(&w.tmap), by + 1);
    s.pmap = rotated(&reversed(&w.pmap), by);
    s
}

fn busy() -> World {
    let mut w = World::new();
    w.track(1, "a.mp3", SIZE);
    w.track(2, "A.mp3", SIZE);
    w.track(3, "d/c.mp3", SIZE);
    w.track(4, "D/C.mp3", SIZE);
    w.synced(5, "five.mp3", SIZE);
    w.row(5).hash = [0x55; 32];
    w.track(6, "six.mp3", SIZE);
    w.map_file(6, "Old/six.mp3", SIZE);
    w.synced(7, "seven.mp3", SIZE);
    w.row(7).missing_since = Some(1);
    w.card_audio("Zed/z.mp3", 3);
    w.card_audio("Aaa/a.mp3", 3);
    w.card_list("Rock.m3u8", b"x");
    w.card_list("Misc/other.m3u8", b"x");
    w.list(10, "Rock", &[Some(1), Some(2), Some(5), None]);
    w.list(11, "rock", &[Some(3), Some(4)]);
    w.list(12, "Pop", &[Some(6)]);
    w.removed = vec![(13, "Dead".into()), (14, "Dead2".into())];
    w.map_list(13, "Dead.m3u8");
    w.card_list("Dead.m3u8", b"x");
    w.map_list(12, "Pop old.m3u8");
    w.card_list("Pop old.m3u8", b"x");
    w
}

// Prevents: the plan depending on the order rows come in (hash maps, sorted
// by nothing): the same plan, field for field, for any order of library
// rows, playlists, mappings and card listing.
#[test]
fn the_plan_does_not_depend_on_the_order_of_the_input() {
    let w = busy();
    let want = w.plan();
    assert!(!want.is_empty());
    assert!(!want.copy.is_empty() && !want.remove.is_empty() && !want.moves.is_empty());
    for by in 0..7 {
        assert_eq!(shuffled_world(&w, by).plan(), want, "rotation {by}");
    }
}

mod random {
    use super::*;
    use proptest::prelude::*;

    const RELS: &[&str] = &[
        "a.mp3",
        "A.mp3",
        "b.mp3",
        "d/c.mp3",
        "D/C.mp3",
        "e\u{301}.mp3",
        "\u{e9}.mp3",
        "x/y/z.mp3",
    ];
    const LISTS: &[&str] = &["Rock", "rock", "A/B", "Pop", "ROCK"];

    #[derive(Debug, Clone)]
    struct TrackSpec {
        rel: usize,
        size: u8,
        drm: bool,
        missing: bool,
        state: u8,
    }

    #[derive(Debug, Clone)]
    struct ListSpec {
        name: usize,
        members: Vec<u8>,
        mapped: u8,
    }

    struct Shape {
        tracks: Vec<TrackSpec>,
        lists: Vec<ListSpec>,
        unmanaged: Vec<u8>,
        tombstones: u8,
        scope: u8,
        remove_unmanaged: bool,
        folder: bool,
    }

    fn world(s: &Shape) -> World {
        let mut w = World::new();
        if s.folder {
            w.playlists_dir = "Pl".into();
        }
        let in_dir = |file: &str| match s.folder {
            true => format!("Pl/{file}"),
            false => file.to_owned(),
        };
        let mut used = std::collections::HashSet::new();
        let mut keys = std::collections::HashSet::new();
        let mut id = 0;
        for t in &s.tracks {
            let rel = RELS[t.rel % RELS.len()];
            if !used.insert(rel) {
                continue;
            }
            id += 1;
            let size = [100u64, 5_000, FOUR_GIB + 1, 0][t.size as usize % 4];
            w.track(id, rel, size);
            w.row(id).drm = t.drm;
            w.row(id).missing_since = t.missing.then_some(1);
            let natural = format!("Music/{rel}");
            let (path, card_size) = match t.state % 6 {
                0 => continue,
                1 => (natural, size),
                2 => (format!("Old/{id}.mp3"), size),
                3 => {
                    if keys.insert(card_key(&natural)) {
                        w.map_only(id, &natural, size);
                    }
                    continue;
                }
                4 => {
                    w.row(id).hash = [0xF0; 32];
                    (natural, size)
                }
                _ => (natural, size + 7),
            };
            if keys.insert(card_key(&path)) {
                w.card_audio(&path, card_size);
                w.map_only(id, &path, size);
            }
        }
        for u in &s.unmanaged {
            let path = format!("Music/{}", RELS[*u as usize % RELS.len()]);
            if keys.insert(card_key(&path)) {
                w.card_audio(&path, 55);
            }
        }
        let count = id;
        let mut taken = std::collections::HashSet::new();
        let mut file_keys = std::collections::HashSet::new();
        for (i, l) in s.lists.iter().enumerate() {
            let name = LISTS[l.name % LISTS.len()];
            if !taken.insert(name) {
                continue;
            }
            let lid = 100 + i as i64;
            let members: Vec<Option<i64>> = l
                .members
                .iter()
                .map(|m| match i64::from(*m) % (count + 3) {
                    k if k < count => Some(k + 1),
                    k if k == count => None,
                    _ => Some(999),
                })
                .collect();
            w.list(lid, name, &members);
            if l.mapped % 3 != 0 {
                let file = plan::playlist_file_name(name, 1);
                if file_keys.insert(card_key(&file)) {
                    w.map_list(lid, &file);
                    let bytes: &[u8] = if l.mapped % 3 == 1 { b"stale" } else { b"" };
                    w.card_list(&in_dir(&file), bytes);
                }
            }
        }
        for t in 0..s.tombstones % 3 {
            let name = format!("Dead{t}.m3u8");
            w.removed.push((900 + i64::from(t), format!("Dead{t}")));
            w.map_list(900 + i64::from(t), &name);
            w.card_list(&in_dir(&name), b"x");
        }
        let chosen = names(&["Rock", "Pop"]);
        w.sel = match s.scope % 3 {
            0 => Sel::All,
            1 => Sel::Only(chosen),
            _ => Sel::Except(chosen),
        };
        w.remove_unmanaged = s.remove_unmanaged;
        w
    }

    fn arb() -> impl Strategy<Value = World> {
        let track = (0usize..8, 0u8..4, any::<bool>(), any::<bool>(), 0u8..6).prop_map(
            |(rel, size, drm, missing, state)| TrackSpec {
                rel,
                size,
                drm,
                missing,
                state,
            },
        );
        let list = (0usize..5, proptest::collection::vec(0u8..12, 0..6), 0u8..3).prop_map(
            |(name, members, mapped)| ListSpec {
                name,
                members,
                mapped,
            },
        );
        (
            proptest::collection::vec(track, 0..8),
            proptest::collection::vec(list, 0..4),
            proptest::collection::vec(0u8..8, 0..3),
            0u8..3,
            0u8..3,
            (any::<bool>(), any::<bool>()),
        )
            .prop_map(
                |(tracks, lists, unmanaged, tombstones, scope, (remove_unmanaged, folder))| {
                    world(&Shape {
                        tracks,
                        lists,
                        unmanaged,
                        tombstones,
                        scope,
                        remove_unmanaged,
                        folder,
                    })
                },
            )
    }

    proptest! {
        // Prevents: nondeterminism and non-convergence over many shapes of
        // library and card.
        #[test]
        fn plans_ignore_input_order_and_converge(w in arb(), by in 0usize..9) {
            let p = w.plan();
            prop_assert_eq!(&shuffled_world(&w, by).plan(), &p);
            let mut applied = w.clone();
            apply(&mut applied, &p);
            let again = applied.plan();
            prop_assert!(
                again.is_empty(),
                "copy {:?} replace {:?} moves {:?} remove {:?} playlists {:?}",
                again.copy, again.replace, again.moves, again.remove, again.playlists
            );
            prop_assert_eq!(&again.expected, &p.expected);
        }
    }
}

// ------------------------------------------------- F4 follow-up: moves

use std::collections::BTreeMap;

/// Applies the plan's moves in order to the card files `start` (track id to
/// path). Panics when a move starts from a file that is not there or lands
/// on another file; returns where every track ends.
fn run_moves(start: &BTreeMap<i64, String>, moves: &[MoveOp]) -> BTreeMap<i64, String> {
    let mut at = start.clone();
    for m in moves {
        assert_eq!(
            at.get(&m.track_id).map(|p| card_key(p)),
            Some(card_key(&m.from)),
            "move {m:?} does not start where the file is ({at:?})"
        );
        for (id, p) in &at {
            assert!(
                *id == m.track_id || card_key(p) != card_key(&m.to),
                "move {m:?} lands on the file of track {id} ({at:?})"
            );
        }
        at.insert(m.track_id, m.to.clone());
    }
    at
}

/// The same for playlist renames: a set of names.
fn run_renames(start: &[String], plan: &Plan) -> Vec<String> {
    let mut names = start.to_vec();
    for o in &plan.playlists {
        if let PlaylistOp::Rename { from, to, .. } = o {
            let i = names
                .iter()
                .position(|n| card_key(n) == card_key(from))
                .unwrap_or_else(|| panic!("rename of {from}, not there: {names:?}"));
            assert!(
                !names
                    .iter()
                    .enumerate()
                    .any(|(j, n)| j != i && card_key(n) == card_key(to)),
                "rename {from} -> {to} lands on a file ({names:?})"
            );
            names[i] = to.clone();
        }
    }
    names.sort();
    names
}

fn mapped_at(w: &World) -> BTreeMap<i64, String> {
    w.tmap
        .iter()
        .map(|m| (m.track_id, m.card_path.clone()))
        .collect()
}

fn expected_at(p: &Plan) -> BTreeMap<i64, String> {
    p.expected
        .files
        .iter()
        .map(|f| (f.track_id, f.path.clone()))
        .collect()
}

// Prevents: a chain of moves (a new library track takes the first name, the
// others shift down) landing on a file that has not moved yet.
#[test]
fn a_chain_of_moves_frees_each_name_before_the_next_move_takes_it() {
    let mut w = World::new();
    // Names FAT takes for one: SONG keeps it, Song is "Song (2)", song is
    // "song (3)". 2 and 3 were synced before track 1 existed.
    w.track(1, "SONG.mp3", SIZE);
    w.track(2, "Song.mp3", SIZE);
    w.track(3, "song.mp3", SIZE);
    w.map_file(2, "Music/SONG.mp3", SIZE);
    w.map_file(3, "Music/Song (2).mp3", SIZE);
    let p = w.plan();
    assert_eq!(
        p.moves,
        [
            MoveOp {
                track_id: 3,
                from: "Music/Song (2).mp3".into(),
                to: "Music/song (3).mp3".into()
            },
            MoveOp {
                track_id: 2,
                from: "Music/SONG.mp3".into(),
                to: "Music/Song (2).mp3".into()
            },
        ]
    );
    assert_eq!(p.copy.len(), 1);
    assert_eq!(p.copy[0].to, "Music/SONG.mp3");
    let end = run_moves(&mapped_at(&w), &p.moves);
    let want: BTreeMap<_, _> = expected_at(&p)
        .into_iter()
        .filter(|(i, _)| *i != 1)
        .collect();
    assert_eq!(end, want);
}

fn swap_world(names: &[&str]) -> World {
    // Track i (rel names[i]) is on the card at the next track's name.
    let mut w = World::new();
    for (i, n) in names.iter().enumerate() {
        w.track(i as i64 + 1, &format!("{n}.mp3"), SIZE);
    }
    for i in 0..names.len() {
        let at = names[(i + 1) % names.len()];
        w.map_file(i as i64 + 1, &format!("Music/{at}.mp3"), SIZE);
    }
    w
}

// Prevents: a swap or a cycle having no valid order: it is broken through a
// hidden name next to the source, first hop out, last hop in, and nothing
// is ever overwritten.
#[test]
fn a_swap_goes_through_a_hidden_name() {
    let w = swap_world(&["a", "b"]);
    let p = w.plan();
    let hop = "Music/.fiio-tunes-move-1.move";
    // Track 2 is on a.mp3 and goes to b.mp3; track 1 is on b.mp3.
    assert_eq!(
        p.moves,
        [
            MoveOp {
                track_id: 2,
                from: "Music/a.mp3".into(),
                to: hop.into()
            },
            MoveOp {
                track_id: 1,
                from: "Music/b.mp3".into(),
                to: "Music/a.mp3".into()
            },
            MoveOp {
                track_id: 2,
                from: hop.into(),
                to: "Music/b.mp3".into()
            },
        ]
    );
    assert_eq!(run_moves(&mapped_at(&w), &p.moves), expected_at(&p));
    assert!(p.copy.is_empty() && p.remove.is_empty());
}

// Prevents: a 3-cycle (and one in a subfolder) being ordered wrongly or the
// hidden name leaving the source's folder.
#[test]
fn a_three_cycle_is_broken_once_and_stays_in_the_folder() {
    let mut w = World::new();
    for (i, n) in ["d/a", "d/b", "d/c"].iter().enumerate() {
        w.track(i as i64 + 1, &format!("{n}.mp3"), SIZE);
    }
    for (i, at) in ["d/b", "d/c", "d/a"].iter().enumerate() {
        w.map_file(i as i64 + 1, &format!("Music/{at}.mp3"), SIZE);
    }
    let p = w.plan();
    assert_eq!(p.moves.len(), 4, "{:?}", p.moves);
    let hidden: Vec<_> = p
        .moves
        .iter()
        .filter(|m| m.to.contains(".fiio-tunes-move-") || m.from.contains(".fiio-tunes-move-"))
        .collect();
    assert_eq!(hidden.len(), 2);
    assert_eq!(
        hidden[0].to, "Music/d/.fiio-tunes-move-1.move",
        "first hop out"
    );
    assert_eq!(hidden[1].from, hidden[0].to, "last hop in");
    assert_eq!(p.moves.last().unwrap(), hidden[1]);
    assert_eq!(run_moves(&mapped_at(&w), &p.moves), expected_at(&p));
}

// Prevents: a case-only move waiting on itself (it would be taken for a
// cycle and sent through a hidden name), and a chain with one in it.
#[test]
fn a_case_only_move_is_one_move() {
    let mut w = World::new();
    w.track(1, "B.mp3", SIZE);
    w.map_file(1, "Music/b.mp3", SIZE);
    let p = w.plan();
    assert_eq!(
        p.moves,
        [MoveOp {
            track_id: 1,
            from: "Music/b.mp3".into(),
            to: "Music/B.mp3".into()
        }]
    );
    // With a chain beside it: 2 goes onto the name 3 leaves.
    w.track(2, "y.mp3", SIZE);
    w.map_file(2, "Music/x.mp3", SIZE);
    w.track(3, "z.mp3", SIZE);
    w.map_file(3, "Music/y.mp3", SIZE);
    let p = w.plan();
    assert_eq!(p.moves.len(), 3, "{:?}", p.moves);
    assert!(p.moves.iter().all(|m| !m.to.contains(".fiio-tunes-move-")));
    assert_eq!(run_moves(&mapped_at(&w), &p.moves), expected_at(&p));
}

// Prevents: playlist renames landing on a file another rename still has to
// free, and the extra rename of a cycle landing after the writes.
#[test]
fn playlist_renames_are_ordered_like_moves_and_before_the_writes() {
    let mut w = World::new();
    // Chain: Mid takes the name Old leaves, Old goes to a new name.
    w.list(1, "B", &[]);
    w.list(2, "C", &[]);
    w.map_list(1, "A.m3u8");
    w.map_list(2, "B.m3u8");
    for n in ["A.m3u8", "B.m3u8"] {
        w.card_list(n, b"old");
    }
    let p = w.plan();
    assert_eq!(
        ops(&p),
        [
            "rename B.m3u8 C.m3u8",
            "rename A.m3u8 B.m3u8",
            "write B.m3u8",
            "write C.m3u8"
        ]
    );
    assert_eq!(
        run_renames(&["A.m3u8".into(), "B.m3u8".into()], &p),
        ["B.m3u8", "C.m3u8"]
    );

    // Swap.
    let mut w = World::new();
    w.list(1, "X", &[]);
    w.list(2, "Y", &[]);
    w.map_list(1, "Y.m3u8");
    w.map_list(2, "X.m3u8");
    w.card_list("X.m3u8", b"old");
    w.card_list("Y.m3u8", b"old");
    let p = w.plan();
    let o = ops(&p);
    assert_eq!(o.len(), 5, "{o:?}");
    assert_eq!(o[0], "rename X.m3u8 .fiio-tunes-move-1.m3u8");
    assert_eq!(o[1], "rename Y.m3u8 X.m3u8");
    assert_eq!(
        o[2], "rename .fiio-tunes-move-1.m3u8 Y.m3u8",
        "the cycle's last hop is before the writes"
    );
    assert_eq!(&o[3..], ["write X.m3u8", "write Y.m3u8"]);
    assert_eq!(
        run_renames(&["X.m3u8".into(), "Y.m3u8".into()], &p),
        ["X.m3u8", "Y.m3u8"]
    );
}

// Prevents: the order depending on the input order (ties go by target).
#[test]
fn the_order_of_a_cycle_does_not_depend_on_the_input_order() {
    let mut w = swap_world(&["a", "b", "c", "d"]);
    w.list(1, "X", &[]);
    w.list(2, "Y", &[]);
    w.map_list(1, "Y.m3u8");
    w.map_list(2, "X.m3u8");
    w.card_list("X.m3u8", b"old");
    w.card_list("Y.m3u8", b"old");
    let want = w.plan();
    assert!(want.moves.len() > 4);
    for by in 0..7 {
        assert_eq!(shuffled_world(&w, by).plan(), want, "rotation {by}");
    }
}

mod permutations {
    use super::*;
    use proptest::prelude::*;

    fn arb() -> impl Strategy<Value = (Vec<u32>, Vec<bool>)> {
        (2usize..8).prop_flat_map(|n| {
            (
                proptest::collection::vec(any::<u32>(), n),
                proptest::collection::vec(any::<bool>(), n),
            )
        })
    }

    proptest! {
        // Prevents: any permutation of card names (cycles, fixed points,
        // case-only moves included) producing moves or renames that
        // overwrite a file, or files ending elsewhere than `expected` says.
        #[test]
        fn any_permutation_of_names_is_applied_without_overwriting((keys, flip) in arb()) {
            let n = keys.len();
            let mut perm: Vec<usize> = (0..n).collect();
            perm.sort_by_key(|&i| keys[i]);
            let name = |j: usize, upper: bool| match upper {
                true => format!("F{j}"),
                false => format!("f{j}"),
            };
            let mut w = World::new();
            for i in 0..n {
                w.track(i as i64 + 1, &format!("f{i}.mp3"), SIZE);
                w.map_file(i as i64 + 1, &format!("Music/{}.mp3", name(perm[i], flip[perm[i]])), SIZE);
                w.list(100 + i as i64, &format!("p{i}"), &[]);
                w.map_list(100 + i as i64, &format!("{}.m3u8", name(perm[i], flip[perm[i]]).replace('f', "p").replace('F', "P")));
                w.card_list(&format!("{}.m3u8", name(perm[i], flip[perm[i]]).replace('f', "p").replace('F', "P")), b"old");
            }
            let p = w.plan();
            prop_assert!(p.copy.is_empty() && p.remove.is_empty());
            let end = run_moves(&mapped_at(&w), &p.moves);
            prop_assert_eq!(end, expected_at(&p));
            let start: Vec<String> = w.card_lists.iter().map(|f| f.path.clone()).collect();
            let mut want: Vec<String> = p.expected.playlists.iter().map(|e| e.path.clone()).collect();
            want.sort();
            prop_assert_eq!(run_renames(&start, &p), want);
            // Renames all come before the writes.
            let first_write = p.playlists.iter().position(|o| matches!(o, PlaylistOp::Write { .. })).unwrap_or(p.playlists.len());
            let only_writes = p.playlists[first_write..].iter().all(|o| matches!(o, PlaylistOp::Write { .. }));
            prop_assert!(only_writes, "a rename after a write");
            // And it is the same whatever the input order.
            prop_assert_eq!(shuffled_world(&w, n).plan(), p);
        }
    }
}

// ------------------------------------------------- F4 follow-up: stays

// Prevents: a managed file that cannot move being copied or moved over:
// its path is held, and a chain of files held back by each other resolves.
#[test]
fn a_managed_file_that_stays_holds_its_path() {
    let mut w = World::new();
    // 5 should be Music/a.mp3, which an unmanaged file holds: stays on b.
    // 3 should be b: held by 5's stay, so stays on c. 1 should be c: stays
    // on e. 6 is new and should be e. The ids run against the chain, so one
    // pass over the tracks is not enough.
    w.track(5, "a.mp3", SIZE);
    w.map_file(5, "Music/b.mp3", SIZE);
    w.card_audio("Music/a.mp3", 99);
    w.track(3, "b.mp3", SIZE);
    w.map_file(3, "Music/c.mp3", SIZE);
    w.track(1, "c.mp3", SIZE);
    w.map_file(1, "Music/e.mp3", SIZE);
    w.track(6, "e.mp3", SIZE);
    let p = w.plan();
    assert!(p.moves.is_empty(), "{:?}", p.moves);
    assert!(
        p.copy.is_empty(),
        "copied over a file that stays: {:?}",
        p.copy
    );
    assert!(p.remove.is_empty() && p.replace.is_empty());
    let skipped: Vec<_> = p.skipped.iter().map(|s| (s.track_id, s.reason)).collect();
    assert_eq!(
        skipped,
        [
            (5, SkipReason::Occupied),
            (3, SkipReason::Occupied),
            (1, SkipReason::Occupied),
            (6, SkipReason::Occupied)
        ]
    );
    let at = expected_at(&p);
    assert_eq!(at[&5], "Music/b.mp3");
    assert_eq!(at[&3], "Music/c.mp3");
    assert_eq!(at[&1], "Music/e.mp3");
    assert!(!at.contains_key(&6));
    // Converges.
    let mut applied = w.clone();
    apply(&mut applied, &p);
    assert!(applied.plan().is_empty());
}

// Prevents: a file with no card path (under no root) letting a new track
// copy over its place.
#[test]
fn a_file_with_no_card_path_holds_its_place_too() {
    let mut w = World::new();
    w.track(1, "z.mp3", SIZE);
    w.row(1).path = PathBuf::from("/elsewhere/z.mp3");
    w.map_file(1, "Music/b.mp3", SIZE);
    w.track(2, "b.mp3", SIZE);
    let p = w.plan();
    assert!(p.copy.is_empty(), "{:?}", p.copy);
    let skipped: Vec<_> = p.skipped.iter().map(|s| (s.track_id, s.reason)).collect();
    assert_eq!(
        skipped,
        [(1, SkipReason::NoCardPath), (2, SkipReason::Occupied)]
    );
    assert_eq!(expected_at(&p)[&1], "Music/b.mp3");
}

// Prevents: a held file in the way only for files that are in scope.
#[test]
fn a_file_that_can_move_does_not_hold_its_old_path() {
    let mut w = World::new();
    w.track(1, "a.mp3", SIZE);
    w.map_file(1, "Music/b.mp3", SIZE);
    w.track(2, "b.mp3", SIZE);
    let p = w.plan();
    assert!(p.skipped.is_empty());
    assert_eq!(p.moves.len(), 1);
    assert_eq!(p.copy.len(), 1);
    assert_eq!(p.copy[0].to, "Music/b.mp3");
}

// -------------------------------------- F4 follow-up: names and listing

// Prevents: a hidden playlist file (the card listing skips dot entries, so
// it would be written again every sync).
#[test]
fn a_playlist_named_with_a_leading_dot_gets_an_underscore_and_converges() {
    assert_eq!(plan::playlist_file_name(".mix", 1), "_mix.m3u8");
    assert_eq!(plan::playlist_file_name("..mix", 1), "_.mix.m3u8");
    assert_eq!(plan::playlist_file_name("a.mix", 1), "a.mix.m3u8");
    let mut w = World::new();
    w.list(1, ".mix", &[]);
    let p = w.plan();
    assert_eq!(ops(&p), ["write _mix.m3u8"]);
    apply(&mut w, &p);
    assert_eq!(w.pmap[0].card_name, "_mix.m3u8");
    assert!(w.plan().is_empty());
}

// Prevents: card paths taken from the mapping when the card spells the name
// differently (FAT keeps the case it was written with): the move, replace
// and expected state must name the file as listed.
#[test]
fn paths_of_card_files_come_from_the_listing_not_the_mapping_for_a_move() {
    // Move.
    let mut w = World::new();
    w.track(1, "b.mp3", SIZE);
    w.map_only(1, "Music/a.mp3", SIZE);
    w.card_audio("Music/A.mp3", SIZE);
    let p = w.plan();
    assert_eq!(
        p.moves,
        [MoveOp {
            track_id: 1,
            from: "Music/A.mp3".into(),
            to: "Music/b.mp3".into()
        }]
    );
}

// Prevents: the same for a replace to another spelling.
#[test]
fn paths_of_card_files_come_from_the_listing_not_the_mapping_for_a_replace() {
    let mut w = World::new();
    w.track(1, "a.mp3", 5_000);
    w.map_only(1, "Music/a.mp3", SIZE);
    w.card_audio("Music/A.mp3", SIZE);
    w.row(1).hash = [9; 32];
    let p = w.plan();
    // The old file is the card's spelling; the copy goes to the library's.
    assert_eq!(
        p.remove,
        [RemoveOp {
            path: "Music/A.mp3".into(),
            track_id: Some(1),
            size: SIZE,
            reason: RemoveReason::Replaced
        }]
    );
    assert_eq!(p.copy.len(), 1);
    assert_eq!(p.copy[0].to, "Music/a.mp3");
    assert!(p.replace.is_empty() && p.moves.is_empty());
}

// Prevents: the same for a file only spelled differently.
#[test]
fn paths_of_card_files_come_from_the_listing_not_the_mapping_for_a_respelled_file() {
    let mut w = World::new();
    w.track(1, "b.mp3", SIZE);
    w.map_only(1, "Music/b.mp3", SIZE);
    w.card_audio("Music/B.MP3", SIZE);
    let p = w.plan();
    assert_eq!(
        p.moves,
        [MoveOp {
            track_id: 1,
            from: "Music/B.MP3".into(),
            to: "Music/b.mp3".into()
        }]
    );
    assert_eq!(p.expected.files.len(), 1);
    assert_eq!(p.expected.files[0].path, "Music/b.mp3");
}
