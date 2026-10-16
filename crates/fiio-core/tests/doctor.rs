//! `doctor::examine` and `doctor::fix` (technical document, 11.4, decisions
//! taken in F6) on cards that are temp folders (`device.allow_folder`). The
//! states a stopped sync leaves are built by hand from a real sync's card
//! and backup. Nothing here can reach the real X3 or the real home.

mod common;
mod rig;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;

use fiio_core::doctor::{self, Examined, Fixed, Hop};
use fiio_core::plan::PlaylistOp;
use fiio_core::store::devices::{self, TrackMapping};
use fiio_core::{Error, Result, backup};
use rig::Rig;

fn examine(r: &Rig) -> Result<Examined> {
    doctor::examine(r.conn(), &r.env.cfg, &r.paths(), &r.found())
}

fn fix(r: &Rig, e: &Examined) -> Result<Fixed> {
    doctor::fix(r.conn(), &r.env.cfg, &r.paths(), &r.found(), e)
}

fn lib(r: &Rig, rel: &str) -> Vec<u8> {
    fs::read(r.env.music.join(rel)).unwrap()
}

fn tid(r: &Rig, rel: &str) -> i64 {
    r.env.track(&r.env.music.join(rel)).id
}

fn paths(m: &[TrackMapping]) -> BTreeSet<String> {
    m.iter().map(|m| m.card_path.clone()).collect()
}

fn mappings(r: &Rig) -> Vec<TrackMapping> {
    devices::track_mappings(r.conn(), r.device().id).unwrap()
}

fn manifest_tracks(r: &Rig) -> BTreeSet<String> {
    r.card_manifest()["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["path"].as_str().unwrap().to_owned())
        .collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// Three tracks synced onto the card with a playlist: the card holds
/// `Music/t1..t3.mp3`, a manifest and `Rock.m3u8`.
fn synced() -> Rig {
    let r = Rig::new(3);
    r.adopt_empty();
    r.playlist("Rock", &["t1.mp3", "t2.mp3", "t3.mp3"]);
    r.full_sync();
    r
}

// ------------------------------------------------------------------- clean

// Prevents: doctor reporting problems on a card that has none, writing
// anything when asked to fix nothing (no backup, no card change), or
// `examine` itself writing: a synced card is clean, and the card and the
// index are the same after `examine` and after `fix`.
#[test]
fn a_clean_card_reports_nothing_and_fix_writes_nothing() {
    let r = synced();
    let before = r.snapshot();
    let maps = mappings(&r);
    let backups = r.backups().len();
    let e = examine(&r).unwrap();
    assert!(e.report.is_clean(), "{:?}", e.report);
    assert_eq!(r.snapshot(), before);
    let f = fix(&r, &e).unwrap();
    assert!(f.backup.is_none());
    assert_eq!(
        (
            f.parts,
            f.hops,
            f.taken_back,
            f.stale,
            f.orphans,
            f.forgotten
        ),
        (0, 0, 0, 0, 0, 0)
    );
    assert_eq!(r.snapshot(), before);
    assert_eq!(mappings(&r), maps);
    assert_eq!(r.backups().len(), backups);
}

// ------------------------------------------------------------------- parts

// Prevents: unfinished copies staying on the card (they take room and
// confuse every listing), `examine` deleting them before it is asked, or
// a fix without a backup: both part files (one nested in a folder that
// held nothing else) are reported, kept by `examine`, deleted by `fix`
// with the emptied folder, after a backup that holds the mappings as they
// were; managed files are untouched.
#[test]
fn part_files_are_reported_then_deleted_with_their_empty_folder() {
    let r = synced();
    r.put("Music/.fiio-tunes-1.part", b"half");
    r.put("Music/sub/.fiio-tunes-7.part", b"half too");
    r.put("Music/.fiio-tunes-x.part", b"not a part file name");
    let before = r.snapshot();
    let e = examine(&r).unwrap();
    assert_eq!(
        e.report.parts,
        ["Music/.fiio-tunes-1.part", "Music/sub/.fiio-tunes-7.part"]
    );
    assert!(e.report.hops.is_empty() && e.report.orphans.is_empty());
    assert_eq!(r.snapshot(), before);

    let f = fix(&r, &e).unwrap();
    assert_eq!(f.parts, 2);
    let b = f.backup.expect("a backup before the card changes");
    assert_eq!(backup::saved(&b).unwrap().tracks.len(), 3);
    assert!(!r.exists("Music/.fiio-tunes-1.part"));
    assert!(!r.exists("Music/sub"), "emptied folder kept");
    assert_eq!(
        r.read("Music/.fiio-tunes-x.part"),
        b"not a part file name",
        "a file that is not a part file was deleted"
    );
    for rel in ["t1.mp3", "t2.mp3", "t3.mp3"] {
        assert!(r.read(&format!("Music/{rel}")) == lib(&r, rel));
    }
    assert_eq!(r.backups().len(), 2);
}

// -------------------------------------------------------------------- hops

/// A swap done for real, then cut back to the state a sync killed part way
/// would leave: the card and the index as they were after the move named.
struct Swap {
    r: Rig,
    /// The hidden file, and the two card paths. `x` went to the hop first;
    /// its target is `py`, `y`'s source.
    hop: String,
    px: String,
    py: String,
    x: i64,
    y: i64,
}

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    /// After the first move, recorded: the hop is `x`'s file and its
    /// mapping names it; its target is still held by `y`.
    FirstRecorded,
    /// The same, killed before the record of that move.
    FirstUnrecorded,
    /// After the second move: the hop's target is free.
    Second,
    /// The second move done on the card but not recorded: the index has
    /// `x` at the hop and `y` still at the hop's target.
    SecondUnrecorded,
}

fn stopped_swap(stage: Stage) -> Swap {
    let r = Rig::new(2);
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t2.mp3");
    r.adopt_file(dev, "t2.mp3", "Music/t1.mp3");
    let p = r.prepare(false).unwrap();
    let m = p.plan.moves.clone();
    assert_eq!(m.len(), 3, "{m:?}");
    assert_eq!((&m[0].to, &m[1].to), (&m[2].from, &m[0].from));
    assert_eq!(m[1].from, m[2].to);
    r.run(&p, Default::default()).unwrap();
    let s = Swap {
        hop: m[0].to.clone(),
        px: m[0].from.clone(),
        py: m[2].to.clone(),
        x: m[0].track_id,
        y: m[1].track_id,
        r,
    };
    // Final: x at py, y at px. Cut back.
    let mv = |a: &str, b: &str| fs::rename(s.r.card.join(a), s.r.card.join(b)).unwrap();
    mv(&s.py, &s.hop);
    let set_path = |id: i64, to: &str| devices::set_card_path(s.r.conn(), dev, id, to).unwrap();
    match stage {
        Stage::Second => {
            set_path(s.x, &s.hop);
        }
        Stage::SecondUnrecorded => {
            set_path(s.x, &s.hop);
            set_path(s.y, &s.py);
        }
        Stage::FirstRecorded | Stage::FirstUnrecorded => {
            mv(&s.px, &s.py);
            set_path(s.x, &s.hop);
            set_path(s.y, &s.py);
            if stage == Stage::FirstUnrecorded {
                set_path(s.x, &s.px);
            }
        }
    }
    s
}

fn lib_of(r: &Rig, id: i64) -> Vec<u8> {
    let t = r.env.tracks().into_iter().find(|t| t.id == id).unwrap();
    fs::read(t.path).unwrap()
}

// Prevents: a hop whose first move was recorded going forward to a target
// that is still taken (the other file would be overwritten), or its
// mapping staying on the hidden name: it goes back to its source, the
// mapping follows, and the card is as it was before the swap began.
#[test]
fn a_recorded_hop_whose_target_is_taken_goes_back_and_its_mapping_follows() {
    let s = stopped_swap(Stage::FirstRecorded);
    let e = examine(&s.r).unwrap();
    assert_eq!(
        e.report.hops,
        [Hop {
            path: s.hop.clone(),
            to: Some(s.px.clone()),
            track_id: Some(s.x),
        }]
    );
    assert!(e.report.stale.is_empty(), "{:?}", e.report.stale);
    assert!(e.report.orphans.is_empty() && e.report.taken_back.is_empty());
    let f = fix(&s.r, &e).unwrap();
    assert_eq!((f.hops, f.stale, f.orphans), (1, 0, 0));
    assert!(s.r.read(&s.px) == lib_of(&s.r, s.x));
    assert!(s.r.read(&s.py) == lib_of(&s.r, s.y));
    assert!(!s.r.exists(&s.hop));
    let by: BTreeMap<i64, String> = mappings(&s.r)
        .into_iter()
        .map(|m| (m.track_id, m.card_path))
        .collect();
    assert_eq!((&by[&s.x], &by[&s.y]), (&s.px, &s.py));
    assert_eq!(manifest_tracks(&s.r), set(&[&s.px, &s.py]));
    assert!(f.backup.is_some());
}

// Prevents: a hop whose second move was done going back instead of
// forward (the swap would be undone half way and the freed name wasted),
// or its mapping staying on the hidden name: it goes to its target, the
// mapping follows, and the card ends as the swap meant it.
#[test]
fn a_recorded_hop_whose_target_is_free_goes_forward_and_its_mapping_follows() {
    let s = stopped_swap(Stage::Second);
    let e = examine(&s.r).unwrap();
    assert_eq!(
        e.report.hops,
        [Hop {
            path: s.hop.clone(),
            to: Some(s.py.clone()),
            track_id: Some(s.x),
        }]
    );
    assert!(e.report.stale.is_empty(), "{:?}", e.report.stale);
    let f = fix(&s.r, &e).unwrap();
    assert_eq!(f.hops, 1);
    assert!(s.r.read(&s.py) == lib_of(&s.r, s.x));
    assert!(s.r.read(&s.px) == lib_of(&s.r, s.y));
    assert!(!s.r.exists(&s.hop));
    let by: BTreeMap<i64, String> = mappings(&s.r)
        .into_iter()
        .map(|m| (m.track_id, m.card_path))
        .collect();
    assert_eq!((&by[&s.x], &by[&s.y]), (&s.py, &s.px));
    assert_eq!(manifest_tracks(&s.r), set(&[&s.px, &s.py]));
}

// Prevents: a hop whose target the index still gives to the other track
// (the second move was done on the card, not recorded) failing on the
// index's uniqueness when the hop's mapping follows it, or leaving two
// mappings on one path: the hop goes forward, its track follows, the file
// at its place is taken back, and nothing errors.
#[test]
fn a_hop_whose_target_the_index_still_gives_to_the_other_track_is_placed() {
    let s = stopped_swap(Stage::SecondUnrecorded);
    let e = examine(&s.r).unwrap();
    assert_eq!(
        e.report.hops,
        [Hop {
            path: s.hop.clone(),
            to: Some(s.py.clone()),
            track_id: Some(s.x),
        }]
    );
    assert_eq!(e.report.taken_back.len(), 1);
    assert_eq!(e.report.taken_back[0].track_id, s.y);
    let f = fix(&s.r, &e).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!((f.hops, f.taken_back), (1, 1));
    assert!(s.r.read(&s.py) == lib_of(&s.r, s.x));
    assert!(s.r.read(&s.px) == lib_of(&s.r, s.y));
    let by: BTreeMap<i64, String> = mappings(&s.r)
        .into_iter()
        .map(|m| (m.track_id, m.card_path))
        .collect();
    assert_eq!((&by[&s.x], &by[&s.y]), (&s.py, &s.px));
    assert_eq!(manifest_tracks(&s.r), set(&[&s.px, &s.py]));
    assert!(s.r.prepare(false).unwrap().plan.is_empty());
}

// Prevents: a backup's work.json steering a rename outside the card: a hop
// whose recorded move has a `..` or absolute path is left unplaced, and
// nothing outside the card is created or touched.
#[test]
fn a_backup_naming_paths_outside_the_card_places_no_hop() {
    for evil in ["../escaped.mp3", "/tmp/fiio-tunes-escaped.mp3"] {
        let s = stopped_swap(Stage::FirstRecorded);
        let b = s.r.backups().pop().unwrap();
        let file = b.dir.join("work.json");
        let mut w: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        // The hop's source becomes the evil path.
        w["moves"][1]["to"] = evil.into();
        w["moves"][0]["from"] = evil.into();
        fs::write(&file, w.to_string()).unwrap();
        let outside = s.r.card.parent().unwrap().join("escaped.mp3");
        let abs = std::path::Path::new("/tmp/fiio-tunes-escaped.mp3");
        let abs_before = abs.exists();
        let before = s.r.snapshot();
        let e = examine(&s.r).unwrap();
        assert_eq!(e.report.hops.len(), 1, "{evil}");
        assert_eq!(e.report.hops[0].to, None, "{evil}");
        let f = fix(&s.r, &e).unwrap();
        assert_eq!(f.hops, 0, "{evil}");
        assert!(s.r.exists(&s.hop), "{evil}");
        assert!(!outside.exists(), "{evil}");
        assert_eq!(abs.exists(), abs_before, "{evil}");
        let after = s.r.snapshot();
        assert_eq!(after[&s.hop], before[&s.hop], "{evil}");
    }
}

// Prevents: a hop whose move was not recorded (killed between the rename
// and the record) being sent forward on a guess: no mapping names it, so
// it goes back to its source, which the mapping still names, and that
// mapping is not reported stale meanwhile.
#[test]
fn an_unrecorded_hop_goes_back_to_its_source() {
    let s = stopped_swap(Stage::FirstUnrecorded);
    let e = examine(&s.r).unwrap();
    assert_eq!(
        e.report.hops,
        [Hop {
            path: s.hop.clone(),
            to: Some(s.px.clone()),
            track_id: None,
        }]
    );
    assert!(e.report.stale.is_empty(), "{:?}", e.report.stale);
    let maps = mappings(&s.r);
    let f = fix(&s.r, &e).unwrap();
    assert_eq!((f.hops, f.stale), (1, 0));
    assert!(s.r.read(&s.px) == lib_of(&s.r, s.x));
    assert!(!s.r.exists(&s.hop));
    assert_eq!(mappings(&s.r), maps);
}

// Prevents: the choice between target and source depending on what the
// free names happen to be: when both are free, a hop whose mapping names it
// (the move was recorded) goes forward to its target, and one no mapping
// names goes back to its source.
#[test]
fn with_both_names_free_a_recorded_hop_goes_forward_and_an_unrecorded_one_back() {
    let s = stopped_swap(Stage::FirstRecorded);
    fs::remove_file(s.r.card.join(&s.py)).unwrap();
    let e = examine(&s.r).unwrap();
    assert_eq!(e.report.hops.len(), 1);
    assert_eq!(e.report.hops[0].to.as_deref(), Some(s.py.as_str()));
    assert_eq!(e.report.hops[0].track_id, Some(s.x));

    let s = stopped_swap(Stage::FirstUnrecorded);
    fs::remove_file(s.r.card.join(&s.py)).unwrap();
    let e = examine(&s.r).unwrap();
    assert_eq!(e.report.hops.len(), 1);
    assert_eq!(e.report.hops[0].to.as_deref(), Some(s.px.as_str()));
    assert_eq!(e.report.hops[0].track_id, None);
}

// Prevents: a hop with nowhere to go being moved onto something (data
// lost) or silently dropped: with its source and target both taken, or
// with no backup that knows the move, it is reported with no place,
// `fix` leaves it where it is and counts none.
#[test]
fn a_hop_with_no_free_place_or_no_known_move_is_reported_and_left() {
    // Both names taken: the source by a stray file.
    let s = stopped_swap(Stage::FirstRecorded);
    s.r.put(&s.px, b"a stray audio file");
    let e = examine(&s.r).unwrap();
    assert_eq!(e.report.hops.len(), 1);
    assert_eq!(e.report.hops[0].to, None);
    assert_eq!(e.report.hops[0].track_id, Some(s.x));
    let f = fix(&s.r, &e).unwrap();
    assert_eq!(f.hops, 0);
    assert!(s.r.exists(&s.hop));
    assert!(s.r.read(&s.hop) == lib_of(&s.r, s.x));
    assert!(s.r.read(&s.py) == lib_of(&s.r, s.y));

    // Names free, but no backup says where the file was going.
    let s = stopped_swap(Stage::FirstRecorded);
    fs::remove_dir_all(s.r.data.join("backups")).unwrap();
    let e = examine(&s.r).unwrap();
    assert_eq!(e.report.hops[0].to, None);
    let f = fix(&s.r, &e).unwrap();
    assert_eq!(f.hops, 0);
    assert!(s.r.exists(&s.hop));
}

// Prevents: a playlist hop being sent forward like a track's: with no
// mapping to follow it goes back to its source when that is free.
#[test]
fn a_playlist_hop_goes_back_to_its_source() {
    let r = Rig::new(2);
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
    r.adopt_file(dev, "t2.mp3", "Music/t2.mp3");
    r.playlist("Alpha", &["t1.mp3"]);
    r.playlist("Beta", &["t2.mp3"]);
    r.sync();
    let (a, b) = (r.read("Alpha.m3u8"), r.read("Beta.m3u8"));
    let (ida, idb) = (
        r.env.playlist_id("Alpha").unwrap(),
        r.env.playlist_id("Beta").unwrap(),
    );
    let map = |id, name: &str| fiio_core::store::devices::PlaylistMapping {
        playlist_id: id,
        card_name: name.into(),
    };
    devices::replace_playlist_mappings(
        r.conn(),
        dev,
        &[map(ida, "Beta.m3u8"), map(idb, "Alpha.m3u8")],
    )
    .unwrap();
    r.put("Beta.m3u8", &a);
    r.put("Alpha.m3u8", &b);
    let p = r.prepare(true).unwrap();
    let renames: Vec<(String, String)> = p
        .plan
        .playlists
        .iter()
        .filter_map(|o| match o {
            PlaylistOp::Rename { from, to, .. } => Some((from.clone(), to.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(renames.len(), 3, "{renames:?}");
    let (f1, hop) = renames[0].clone();
    let f2 = renames[2].1.clone();
    assert_eq!(renames[0].1, renames[2].0);
    assert_eq!((&renames[1].0, &renames[1].1), (&f2, &f1));
    r.run(&p, Default::default()).unwrap();
    let before_content = r.read(&f2);
    // Cut back to after the first rename: the hop holds what was at f1, f2
    // keeps its own, f1 is free.
    fs::rename(r.card.join(&f2), r.card.join(&hop)).unwrap();
    fs::rename(r.card.join(&f1), r.card.join(&f2)).unwrap();
    let e = examine(&r).unwrap();
    assert_eq!(
        e.report.hops,
        [Hop {
            path: hop.clone(),
            to: Some(f1.clone()),
            track_id: None
        }]
    );
    let f = fix(&r, &e).unwrap();
    assert_eq!(f.hops, 1);
    assert!(!r.exists(&hop));
    assert_eq!(r.read(&f1), before_content);
}

// ------------------------------------------------------------- take-back

// Prevents: doctor leaving a file a cut short sync put in place unrecorded:
// it is reported (and not as an orphan), `examine` records nothing, `fix`
// maps it with its size and hash, rewrites the manifest and moves nothing.
#[test]
fn doctor_takes_back_a_file_in_place_with_no_mapping() {
    let r = synced();
    let all = mappings(&r);
    let t2 = tid(&r, "t2.mp3");
    let rest: Vec<TrackMapping> = all.iter().filter(|m| m.track_id != t2).cloned().collect();
    devices::replace_track_mappings(r.conn(), r.device().id, &rest).unwrap();
    let before = r.snapshot();
    let e = examine(&r).unwrap();
    assert_eq!(e.report.taken_back.len(), 1);
    assert_eq!(
        e.report.taken_back[0],
        *all.iter().find(|m| m.track_id == t2).unwrap()
    );
    assert!(e.report.orphans.is_empty(), "{:?}", e.report.orphans);
    assert_eq!(mappings(&r).len(), 2, "examine recorded");
    assert_eq!(r.snapshot(), before);

    let f = fix(&r, &e).unwrap();
    assert_eq!((f.taken_back, f.orphans), (1, 0));
    let mut now = mappings(&r);
    let mut all = all;
    now.sort_by_key(|m| m.track_id);
    all.sort_by_key(|m| m.track_id);
    assert_eq!(now, all);
    assert_eq!(manifest_tracks(&r).len(), 3);
    assert!(r.exists("Music/t2.mp3"));
    assert!(!r.exists(".fiio-tunes/orphans"));
}

// ------------------------------------------------------------------- stale

// Prevents: a mapping whose file is gone staying in the index (the next
// manifest would claim a file the card lacks) or one that still has its
// file being dropped: the backup is taken before, holds the old mappings,
// and the manifest is rewritten without the dropped one. The track, still
// in scope, is copied again by the next sync.
#[test]
fn a_mapping_whose_file_is_gone_is_dropped_and_the_manifest_rewritten() {
    let r = synced();
    fs::remove_file(r.card.join("Music/t2.mp3")).unwrap();
    let t2 = tid(&r, "t2.mp3");
    let e = examine(&r).unwrap();
    assert_eq!(e.report.stale, [(t2, "Music/t2.mp3".to_owned())]);
    assert!(e.report.orphans.is_empty() && e.report.hops.is_empty());
    assert_eq!(mappings(&r).len(), 3, "examine dropped it");

    let f = fix(&r, &e).unwrap();
    assert_eq!(f.stale, 1);
    let b = f.backup.unwrap();
    assert_eq!(backup::saved(&b).unwrap().tracks.len(), 3);
    assert_eq!(paths(&mappings(&r)), set(&["Music/t1.mp3", "Music/t3.mp3"]));
    assert_eq!(manifest_tracks(&r), set(&["Music/t1.mp3", "Music/t3.mp3"]));
    let p = r.prepare(false).unwrap();
    assert_eq!(p.plan.copy.len(), 1);
    assert_eq!(p.plan.copy[0].to, "Music/t2.mp3");
}

// ----------------------------------------------------------------- orphans

// Prevents: orphans deleted instead of set aside, or the net cast too wide.
// A file fiio-tunes wrote under Music (a backup's work.json names it) whose
// mapping is lost, and whose bytes are no longer the track's, moves (a
// rename, same bytes) to `.fiio-tunes/orphans/<stamp>/<card path>`. Audio
// the owner put there (no backup knows it), audio outside Music, an
// unmanaged playlist, other files and managed files stay put.
#[test]
fn orphans_are_the_files_fiio_tunes_wrote_and_the_owners_are_left_alone() {
    let r = synced();
    r.put("Music/junk/j.mp3", b"junk");
    r.put("Music/zz.flac", b"zz");
    r.put("Stray/s.flac", b"stray");
    r.put("Other.m3u8", b"#EXTM3U\nmine\n");
    r.put("Music/notes.txt", b"not audio");
    let mut wrong = lib(&r, "t2.mp3");
    wrong.pop();
    r.put("Music/t2.mp3", &wrong);
    // t2's mapping is lost.
    let rest: Vec<TrackMapping> = mappings(&r)
        .into_iter()
        .filter(|m| m.card_path != "Music/t2.mp3")
        .collect();
    devices::replace_track_mappings(r.conn(), r.device().id, &rest).unwrap();

    let e = examine(&r).unwrap();
    assert_eq!(e.report.orphans, ["Music/t2.mp3"]);
    assert!(e.report.taken_back.is_empty());
    let f = fix(&r, &e).unwrap();
    assert_eq!(f.orphans, 1);
    let dir = f.orphans_dir.clone().expect("where they went");
    assert!(dir.starts_with(".fiio-tunes/orphans/"), "{dir}");
    let stamp = dir.rsplit('/').next().unwrap();
    assert!(stamp.len() == 15 && stamp.as_bytes()[8] == b'-', "{stamp}");
    assert_eq!(r.read(&format!("{dir}/Music/t2.mp3")), wrong);
    assert!(!r.exists("Music/t2.mp3"));
    assert_eq!(r.read("Music/junk/j.mp3"), b"junk");
    assert_eq!(r.read("Music/zz.flac"), b"zz");
    assert_eq!(r.read("Stray/s.flac"), b"stray");
    assert_eq!(r.read("Other.m3u8"), b"#EXTM3U\nmine\n");
    assert_eq!(r.read("Music/notes.txt"), b"not audio");
    assert!(r.exists("Music/t1.mp3") && r.exists("Music/t3.mp3"));

    // The moved file is on no listing: the next sync copies t2 again, and
    // the owner's files are reported unmanaged, not removed.
    let p = r.prepare(false).unwrap();
    assert_eq!(p.plan.copy.len(), 1);
    assert_eq!(p.plan.copy[0].to, "Music/t2.mp3");
    assert!(p.plan.remove.is_empty());
    let mut un = p.plan.unmanaged.files.clone();
    un.sort();
    assert_eq!(un, ["Music/junk/j.mp3", "Music/zz.flac", "Stray/s.flac"]);
}

// Prevents: orphans being looked for on the whole card when the music
// folder is a subfolder of it: with `music_dir` set, a file fiio-tunes
// wrote there and lost is an orphan; the owner's audio inside it, and
// anything beside it, is not.
#[test]
fn orphans_follow_the_configured_music_folder() {
    let mut r = Rig::new(1);
    r.env.cfg.card.music_dir = "Songs".into();
    r.adopt_empty();
    r.full_sync();
    assert!(r.exists("Songs/t1.mp3"));
    r.put("Music/m.mp3", b"beside");
    r.put("Songs/s.mp3", b"inside, the owner's");
    let mut wrong = lib(&r, "t1.mp3");
    wrong.pop();
    r.put("Songs/t1.mp3", &wrong);
    devices::replace_track_mappings(r.conn(), r.device().id, &[]).unwrap();
    let e = examine(&r).unwrap();
    assert_eq!(e.report.orphans, ["Songs/t1.mp3"]);
    let f = fix(&r, &e).unwrap();
    assert_eq!(f.orphans, 1);
    assert_eq!(r.read("Music/m.mp3"), b"beside");
    assert_eq!(r.read("Songs/s.mp3"), b"inside, the owner's");
    assert!(!r.exists("Songs/t1.mp3"));
}

// Prevents: a file adopted (so never copied by a sync, and in no work.json)
// not counting as fiio-tunes' own: a backup's mappings.json lists its path,
// which is enough for it to be an orphan once its mapping is lost.
#[test]
fn a_path_only_a_backups_mappings_name_marks_an_orphan() {
    let r = Rig::new(1);
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
    r.playlist("Rock", &["t1.mp3"]);
    r.sync();
    devices::replace_track_mappings(r.conn(), dev, &[]).unwrap();
    let mut wrong = lib(&r, "t1.mp3");
    wrong.pop();
    r.put("Music/t1.mp3", &wrong);
    let e = examine(&r).unwrap();
    assert_eq!(e.report.orphans, ["Music/t1.mp3"]);
}

// --------------------------------------------------------------- forgotten

// Prevents: the library index forgetting tracks it should keep, or never
// forgetting: only a track missing for over 30 days, on no device, whose
// file is not back is dropped; its playlist entry stays, unresolved. The
// card is not written (no backup) when nothing on it changes.
#[test]
fn tracks_missing_for_over_30_days_on_no_device_are_forgotten() {
    let r = Rig::new(5);
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t2.mp3", "Music/t2.mp3");
    r.playlist("Rock", &["t1.mp3", "t5.mp3"]);
    let ids: Vec<i64> = (1..=5).map(|i| tid(&r, &format!("t{i}.mp3"))).collect();
    let now = fiio_core::now_unix();
    let day = 24 * 3600;
    let age = |id: i64, days: i64| {
        r.conn()
            .execute(
                "UPDATE tracks SET missing_since = ?2 WHERE id = ?1",
                rusqlite::params![id, now - days * day],
            )
            .unwrap();
    };
    for rel in ["t1.mp3", "t2.mp3", "t3.mp3"] {
        fs::remove_file(r.env.music.join(rel)).unwrap();
    }
    age(ids[0], 31); // gone, long ago, on no device: forgotten
    age(ids[1], 31); // on the card: kept
    age(ids[2], 29); // not long enough: kept
    age(ids[3], 31); // file still there: kept
    let before = r.snapshot();
    let e = examine(&r).unwrap();
    assert_eq!(e.report.forgotten, [(ids[0], r.env.music.join("t1.mp3"))]);
    assert_eq!(r.env.tracks().len(), 5, "examine forgot");

    let f = fix(&r, &e).unwrap();
    assert_eq!(f.forgotten, 1);
    assert!(f.backup.is_none());
    assert_eq!(r.snapshot(), before);
    let left: BTreeSet<i64> = r.env.tracks().into_iter().map(|t| t.id).collect();
    assert_eq!(left, [ids[1], ids[2], ids[3], ids[4]].into_iter().collect());
    let items = r.env.indexed_items("Rock");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].1, None);
    assert!(items[0].2.contains("t1.mp3"), "{items:?}");
    assert_eq!(items[1].1, Some(ids[4]));
}

// ---------------------------------------------------------------- refusals

// Prevents: doctor working on a card it must not: one not adopted, one
// whose restore is unfinished, one that is not the adopted card, one with
// an unreadable folder (its files would look gone and be dropped). Each
// refusal is the specific error and nothing is changed.
#[test]
fn examine_refuses_the_cards_it_must_not_work_on() {
    // Not adopted.
    let r = Rig::new(1);
    assert!(matches!(examine(&r), Err(Error::NotAdopted(_))));

    // Restore unfinished.
    let r = synced();
    devices::set_restoring(r.conn(), r.device().id, Some("20260101-000000")).unwrap();
    match examine(&r) {
        Err(Error::RestoreUnfinished(s)) => assert_eq!(s, "20260101-000000"),
        other => panic!("{:?}", other.map(|e| e.report)),
    }

    // Another card in the same player.
    let r = synced();
    let mut m = r.card_manifest();
    m["card_id"] = "0123456789abcdef0123456789abcdef".into();
    r.put(".fiio-tunes/manifest.json", m.to_string().as_bytes());
    assert!(matches!(examine(&r), Err(Error::WrongCard(_))));

    // An unreadable folder.
    let r = synced();
    r.put("Music/locked/x.mp3", b"x");
    let dir = r.card.join("Music/locked");
    if common::chmod_000_blocks_reads(&dir) {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
        let res = examine(&r);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(res, Err(Error::CardIncomplete(_))),
            "{:?}",
            res.map(|e| e.report)
        );
    } else {
        eprintln!("SKIPPED the unreadable folder case: permissions do not stop this user");
    }
}

// Prevents: a fix that writes to a card it may not write to, or to a card
// swapped while the prompt waited: neither deletes the part file nor makes
// a backup.
#[test]
fn fix_refuses_a_card_not_allowed_or_swapped_since_the_examination() {
    let r = synced();
    r.put("Music/.fiio-tunes-1.part", b"half");
    let e = examine(&r).unwrap();
    let before = r.snapshot();
    let backups = r.backups().len();

    let mut cfg = r.env.cfg.clone();
    cfg.device.allow_folder = false;
    let res = doctor::fix(r.conn(), &cfg, &r.paths(), &r.found(), &e);
    assert!(matches!(res, Err(Error::NotWritable(_))), "{res:?}");
    assert_eq!(r.snapshot(), before);

    let mut m = r.card_manifest();
    m["card_id"] = "0123456789abcdef0123456789abcdef".into();
    let swapped = serde_json::to_vec(&m).unwrap();
    let original = r.read(".fiio-tunes/manifest.json");
    r.put(".fiio-tunes/manifest.json", &swapped);
    let res = fix(&r, &e);
    assert!(matches!(res, Err(Error::WrongCard(_))), "{res:?}");
    assert!(r.exists("Music/.fiio-tunes-1.part"));
    assert_eq!(r.backups().len(), backups);
    r.put(".fiio-tunes/manifest.json", &original);
    assert!(fix(&r, &e).is_ok());
    assert!(!r.exists("Music/.fiio-tunes-1.part"));
}
