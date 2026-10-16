//! `exec::run` with track changes (technical document, 11.2, decisions
//! taken in F6): copies, replaces, moves and removals, the mappings kept
//! as the card changes, a stop, a card failure, the take-back of files a
//! cut short sync left, and `prepare` refusing a hidden hop file. Cards are
//! temp folders (`device.allow_folder`), the data folder is a temp folder
//! too; nothing here can reach the real X3 or the real home.

mod common;
mod rig;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, Ordering};

use common::{Kind, fixture};
use fiio_core::config::Scope;
use fiio_core::exec::{Done, Failed, Options, Progress};
use fiio_core::plan::RemoveReason;
use fiio_core::store::devices::{self, DeviceScope, PlaylistMapping, TrackMapping};
use fiio_core::{Error, backup, card};
use rig::{Rig, readonly_dir_blocks_writes};

fn lib(r: &Rig, rel: &str) -> Vec<u8> {
    fs::read(r.env.music.join(rel)).unwrap()
}

fn locations(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// Every hidden file a sync leaves behind: `.part` and hop files.
fn leftovers(r: &Rig) -> Vec<String> {
    r.snapshot()
        .into_keys()
        .filter(|k| k.rsplit('/').next().unwrap().starts_with(".fiio-tunes-"))
        .collect()
}

fn failed(res: Result<Done, Failed>) -> Failed {
    match res {
        Ok(d) => panic!("expected a failure, got {d:?}"),
        Err(f) => f,
    }
}

fn work(r: &Rig, b: &backup::Backup) -> serde_json::Value {
    let _ = r;
    serde_json::from_slice(&fs::read(b.dir.join("work.json")).unwrap()).unwrap()
}

fn track_id(r: &Rig, rel: &str) -> i64 {
    r.env.track(&r.env.music.join(rel)).id
}

fn mapping(r: &Rig, rel: &str) -> Option<TrackMapping> {
    let id = track_id(r, rel);
    devices::track_mappings(r.conn(), r.device().id)
        .unwrap()
        .into_iter()
        .find(|m| m.track_id == id)
}

/// Paths of the tracks the card manifest lists.
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

/// Two tracks adopted on the card under each other's names: the next sync
/// swaps them, through a hidden hop.
fn swap_world() -> Rig {
    let r = Rig::new(2);
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t2.mp3");
    r.adopt_file(dev, "t2.mp3", "Music/t1.mp3");
    r
}

// ------------------------------------------------------------ first sync

// Prevents: a first full sync that leaves anything to the next one: every
// track copied byte for byte into the folder layout, mapped with the hash
// and size of the library file, listed in the card's manifest, the playlist
// written with the card paths, nothing but those on the card, and an empty
// plan for the second sync.
#[test]
fn a_first_sync_onto_an_empty_card_copies_everything_and_the_next_plan_is_empty() {
    let mut r = Rig::new(3);
    r.env.add("sub/t4.mp3", Kind::Mp3, 4);
    r.env.scan();
    r.adopt_empty();
    r.playlist("Rock", &["t1.mp3", "sub/t4.mp3"]);
    let p = r.prepare(false).unwrap();
    assert_eq!(p.plan.copy.len(), 4);
    let done = r.run(&p, Options::default()).unwrap();
    assert!(done.first);
    assert_eq!(
        (done.copied, done.recopied, done.moved, done.removed),
        (4, 0, 0, 0)
    );
    assert!(done.skipped.is_empty() && done.stopped.is_none());

    let rels = ["t1.mp3", "t2.mp3", "t3.mp3", "sub/t4.mp3"];
    let mut total = 0;
    for rel in rels {
        let bytes = lib(&r, rel);
        assert!(
            r.read(&format!("Music/{rel}")) == bytes,
            "{rel}: card bytes differ"
        );
        let m = mapping(&r, rel).unwrap_or_else(|| panic!("{rel} not mapped"));
        assert_eq!(m.card_path, format!("Music/{rel}"));
        assert_eq!(m.synced_size, bytes.len() as u64);
        assert_eq!(m.synced_hash, *blake3::hash(&bytes).as_bytes());
        total += bytes.len() as u64;
    }
    assert_eq!(done.bytes, total);

    assert_eq!(
        manifest_tracks(&r),
        set(&[
            "Music/t1.mp3",
            "Music/t2.mp3",
            "Music/t3.mp3",
            "Music/sub/t4.mp3"
        ])
    );
    assert_eq!(r.card_manifest()["card_id"], r.device().card_id.unwrap());
    assert_eq!(
        locations(&r.read("Rock.m3u8")),
        ["Music/t1.mp3", "Music/sub/t4.mp3"]
    );
    let files: BTreeSet<String> = r.snapshot().into_keys().collect();
    assert_eq!(
        files,
        set(&[
            ".fiio-tunes/manifest.json",
            "Rock.m3u8",
            "Music/t1.mp3",
            "Music/t2.mp3",
            "Music/t3.mp3",
            "Music/sub/t4.mp3"
        ])
    );

    let w = work(&r, done.backup.as_ref().unwrap());
    let to: Vec<&str> = w["copy"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["to"].as_str().unwrap())
        .collect();
    assert_eq!(to.len(), 4);
    assert!(to.contains(&"Music/sub/t4.mp3"), "{to:?}");

    let again = r.prepare(false).unwrap();
    assert!(again.plan.is_empty(), "{:?}", again.plan);
    let done = r.run(&again, Options::default()).unwrap();
    assert!(done.backup.is_none());
}

// ----------------------------------------------------------------- replace

// Prevents: a changed library file not reaching the card, or reaching it
// under another name: it is copied again over the same path, the mapping
// takes the new hash and size, the neighbours are left alone.
#[test]
fn a_changed_library_file_is_copied_again_over_its_card_file() {
    let mut r = Rig::new(2);
    r.adopt_empty();
    r.playlist("Rock", &["t1.mp3", "t2.mp3"]);
    r.full_sync();
    let t2_before = r.read("Music/t2.mp3");
    let t2_ino = fs::metadata(r.card.join("Music/t2.mp3")).unwrap().ino();

    let new = fs::read(fixture(Kind::Mp3, 9)).unwrap();
    assert_ne!(new, lib(&r, "t1.mp3"));
    fs::write(r.env.music.join("t1.mp3"), &new).unwrap();
    r.env.scan();
    let p = r.prepare(false).unwrap();
    assert_eq!(
        (
            p.plan.replace.len(),
            p.plan.copy.len(),
            p.plan.moves.len(),
            p.plan.remove.len()
        ),
        (1, 0, 0, 0)
    );
    let done = r.run(&p, Options::default()).unwrap();
    assert_eq!((done.recopied, done.copied, done.moved), (1, 0, 0));
    assert_eq!(done.bytes, new.len() as u64);
    assert!(r.read("Music/t1.mp3") == new, "t1 not replaced");
    assert_eq!(r.read("Music/t2.mp3"), t2_before);
    assert_eq!(
        fs::metadata(r.card.join("Music/t2.mp3")).unwrap().ino(),
        t2_ino
    );
    let m = mapping(&r, "t1.mp3").unwrap();
    assert_eq!(m.card_path, "Music/t1.mp3");
    assert_eq!(
        (m.synced_size, m.synced_hash),
        (new.len() as u64, *blake3::hash(&new).as_bytes())
    );
    let listed = r.card_manifest();
    let t1 = listed["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["path"] == "Music/t1.mp3")
        .unwrap();
    assert_eq!(t1["hash"], blake3::hash(&new).to_hex().to_string());
    assert!(leftovers(&r).is_empty(), "{:?}", leftovers(&r));
    assert!(r.prepare(false).unwrap().plan.is_empty());
}

// ------------------------------------------------------------------- moves

// Prevents: a library rename being applied as a second copy (the old file
// left behind, the data written twice): the card file is renamed, the
// folder it leaves is removed, the mapping, the manifest and the playlist
// follow, and the next plan is empty.
#[test]
fn a_library_rename_moves_the_card_file_and_removes_the_folder_it_leaves() {
    let mut r = Rig::new(0);
    r.env.add("old/a.mp3", Kind::Mp3, 1);
    r.env.add("t2.mp3", Kind::Mp3, 2);
    r.env.scan();
    r.adopt_empty();
    r.playlist("Rock", &["old/a.mp3", "t2.mp3"]);
    r.full_sync();
    let ino = fs::metadata(r.card.join("Music/old/a.mp3")).unwrap().ino();
    let bytes = r.read("Music/old/a.mp3");

    fs::create_dir(r.env.music.join("new")).unwrap();
    fs::rename(r.env.music.join("old/a.mp3"), r.env.music.join("new/a.mp3")).unwrap();
    fs::remove_dir(r.env.music.join("old")).unwrap();
    r.env.scan();
    let p = r.prepare(false).unwrap();
    assert_eq!(p.plan.moves.len(), 1);
    assert_eq!(p.plan.moves[0].from, "Music/old/a.mp3");
    assert_eq!(p.plan.moves[0].to, "Music/new/a.mp3");
    assert!(p.plan.copy.is_empty() && p.plan.remove.is_empty());
    let done = r.run(&p, Options::default()).unwrap();
    assert_eq!((done.moved, done.copied, done.removed), (1, 0, 0));

    assert!(r.read("Music/new/a.mp3") == bytes);
    assert_eq!(
        fs::metadata(r.card.join("Music/new/a.mp3")).unwrap().ino(),
        ino,
        "the file was copied, not renamed"
    );
    assert!(!r.exists("Music/old"), "the emptied folder is still there");
    assert!(r.exists("Music"));
    assert_eq!(r.mapped()["new/a.mp3"], "Music/new/a.mp3");
    assert_eq!(
        manifest_tracks(&r),
        set(&["Music/new/a.mp3", "Music/t2.mp3"])
    );
    assert_eq!(
        locations(&r.read("Rock.m3u8")),
        ["Music/new/a.mp3", "Music/t2.mp3"]
    );
    assert!(r.prepare(false).unwrap().plan.is_empty());
}

// Prevents: a swap of two card files overwriting one of them, leaving the
// hidden hop file, or recording the wrong paths: three renames through
// `.fiio-tunes-move-<n>.move`, each file ends where the library puts it,
// the mappings agree, and the hop counts as no move. work.json holds the
// three renames in order.
#[test]
fn a_swap_goes_through_the_hidden_hop_and_the_mappings_end_right() {
    let r = swap_world();
    let p = r.prepare(false).unwrap();
    assert_eq!(p.plan.moves.len(), 3, "{:?}", p.plan.moves);
    let m = &p.plan.moves;
    assert!(card::leftover(m[0].to.rsplit('/').next().unwrap()) == Some(card::Leftover::Hop));
    assert_eq!(m[0].to, m[2].from);
    assert_eq!((&m[1].to, &m[1].from), (&m[0].from, &m[2].to));

    let done = r.run(&p, Options::default()).unwrap();
    assert_eq!((done.moved, done.copied, done.removed), (2, 0, 0));
    assert!(r.read("Music/t1.mp3") == lib(&r, "t1.mp3"));
    assert!(r.read("Music/t2.mp3") == lib(&r, "t2.mp3"));
    assert!(leftovers(&r).is_empty(), "{:?}", leftovers(&r));
    assert_eq!(
        r.mapped(),
        BTreeMap::from([
            ("t1.mp3".to_owned(), "Music/t1.mp3".to_owned()),
            ("t2.mp3".to_owned(), "Music/t2.mp3".to_owned()),
        ])
    );
    assert_eq!(manifest_tracks(&r), set(&["Music/t1.mp3", "Music/t2.mp3"]));
    assert!(r.prepare(false).unwrap().plan.is_empty());

    let w = work(&r, done.backup.as_ref().unwrap());
    let moves = w["moves"].as_array().unwrap();
    assert_eq!(moves.len(), 3);
    for (got, want) in moves.iter().zip(&p.plan.moves) {
        assert_eq!(got["from"], want.from.as_str());
        assert_eq!(got["to"], want.to.as_str());
        assert_eq!(got["track_id"], want.track_id);
    }
}

// Prevents: the mappings being recorded only at the end: a kill between two
// renames of a swap would leave the index naming files that are not there.
// Seen from the progress callback (which runs before each move), the
// mapping of the move just made is already in the index, the hop under its
// hidden name.
#[test]
fn every_move_is_recorded_before_the_next_one() {
    let r = swap_world();
    let p = r.prepare(false).unwrap();
    let m = p.plan.moves.clone();
    let seen: RefCell<Vec<(usize, BTreeMap<i64, String>)>> = RefCell::default();
    let done = r
        .run_with(
            &p,
            Options::default(),
            &|pr| {
                if let Progress::Moving { done, .. } = pr {
                    let now = devices::track_mappings(r.conn(), r.device().id)
                        .unwrap()
                        .into_iter()
                        .map(|t| (t.track_id, t.card_path))
                        .collect();
                    seen.borrow_mut().push((done, now));
                }
            },
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(done.moved, 2);
    let (x, y) = (m[0].track_id, m[1].track_id);
    let seen = seen.into_inner();
    assert_eq!(seen.len(), 3);
    // Before the first move: as adopted.
    assert_eq!(seen[0].1[&x], m[0].from);
    assert_eq!(seen[0].1[&y], m[1].from);
    // After it: the hop's hidden name.
    assert_eq!(seen[1].1[&x], m[0].to);
    assert_eq!(seen[1].1[&y], m[1].from);
    // After the second: the freed name taken.
    assert_eq!(seen[2].1[&x], m[0].to);
    assert_eq!(seen[2].1[&y], m[1].to);
}

// ---------------------------------------------------------------- removals

// Prevents: removals touching more than they should: a managed file out of
// scope, one whose library file is gone, and an unmanaged file with
// `remove_unmanaged` all go; the folders they empty under Music go too, but
// Music itself, a folder that still holds a file and a folder outside
// Music stay; their mappings go and the others stay.
#[test]
fn removals_clean_the_folders_under_music_and_keep_the_others() {
    let mut r = Rig::new(0);
    for (rel, v) in [
        ("gone/g.mp3", 1),
        ("scoped/s.mp3", 2),
        ("keep/k.mp3", 3),
        ("x.mp3", 4),
    ] {
        r.env.add(rel, Kind::Mp3, v);
    }
    r.env.scan();
    let dev = r.adopt_empty();
    r.adopt_file(dev, "gone/g.mp3", "Music/gone/g.mp3");
    r.adopt_file(dev, "scoped/s.mp3", "Music/scoped/s.mp3");
    r.adopt_file(dev, "keep/k.mp3", "Music/keep/k.mp3");
    r.adopt_file(dev, "x.mp3", "Other/x.mp3");
    r.put("Music/junk/j.mp3", b"unmanaged");
    r.put("Music/keep/mine.mp3", b"unmanaged, stays in the folder");
    r.env.cfg.sync.remove_unmanaged = true;
    r.playlist("Keep", &["keep/k.mp3"]);
    // g leaves the library; s and x are out of scope (only Keep is in).
    fs::remove_file(r.env.music.join("gone/g.mp3")).unwrap();
    r.env.scan();
    let scope = DeviceScope {
        kind: Scope::Playlists,
        playlists: vec!["Keep".into()],
    };
    devices::set_scope(r.conn(), dev, Some(&scope)).unwrap();

    let p = r.prepare(false).unwrap();
    let mut reasons: Vec<(String, RemoveReason)> = p
        .plan
        .remove
        .iter()
        .map(|o| (o.path.clone(), o.reason))
        .collect();
    reasons.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        reasons,
        [
            (
                "Music/gone/g.mp3".to_owned(),
                RemoveReason::MissingFromLibrary
            ),
            ("Music/junk/j.mp3".to_owned(), RemoveReason::Unmanaged),
            ("Music/keep/mine.mp3".to_owned(), RemoveReason::Unmanaged),
            ("Music/scoped/s.mp3".to_owned(), RemoveReason::OutOfScope),
            ("Other/x.mp3".to_owned(), RemoveReason::OutOfScope),
        ]
    );
    let done = r.run(&p, Options::default()).unwrap();
    assert_eq!(done.removed, 5);
    assert_eq!(done.copied, 0);

    assert!(!r.exists("Music/gone"), "emptied folder kept");
    assert!(!r.exists("Music/scoped"), "emptied folder kept");
    assert!(!r.exists("Music/junk"), "emptied folder kept");
    assert!(!r.exists("Other/x.mp3"));
    assert!(r.exists("Other"), "a folder outside Music was removed");
    assert!(r.exists("Music"), "the music folder itself was removed");
    assert!(r.read("Music/keep/k.mp3") == lib(&r, "keep/k.mp3"));
    assert_eq!(r.mapped().keys().collect::<Vec<_>>(), ["keep/k.mp3"]);
    assert_eq!(manifest_tracks(&r), set(&["Music/keep/k.mp3"]));
    assert!(r.prepare(false).unwrap().plan.is_empty());
}

// Prevents: an unmanaged file under Music being removed, or its folder
// kept or over-removed, when `remove_unmanaged` is off: the file stays and
// so does the plan's silence about it.
#[test]
fn unmanaged_files_stay_without_the_option() {
    let r = Rig::new(2);
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
    r.put("Music/junk/j.mp3", b"unmanaged");
    let p = r.prepare(false).unwrap();
    assert!(p.plan.remove.is_empty());
    r.run(&p, Options::default()).unwrap();
    assert_eq!(r.read("Music/junk/j.mp3"), b"unmanaged");
}

// --------------------------------------------------- everything at once

// Prevents: the order of work drifting (removals, moves, then copies and
// replaces, then the playlists, then the check) and work.json listing the
// operations in another order or leaving a kind out. All four kinds in one
// plan: the progress callback sees them in order, work.json names each
// operation and lists remove, moves, copy, replace in that order, and the
// card ends as the library.
#[test]
fn removals_moves_copies_and_replaces_in_one_sync_in_order() {
    let mut r = Rig::new(0);
    for (rel, v) in [
        ("keep.mp3", 1),
        ("rep.mp3", 2),
        ("mov/x.mp3", 3),
        ("gone.mp3", 4),
    ] {
        r.env.add(rel, Kind::Mp3, v);
    }
    r.env.scan();
    r.adopt_empty();
    r.playlist("All", &["keep.mp3", "rep.mp3", "mov/x.mp3"]);
    r.full_sync();

    let new_rep = fs::read(fixture(Kind::Mp3, 9)).unwrap();
    fs::write(r.env.music.join("rep.mp3"), &new_rep).unwrap();
    fs::create_dir(r.env.music.join("mov2")).unwrap();
    fs::rename(
        r.env.music.join("mov/x.mp3"),
        r.env.music.join("mov2/x.mp3"),
    )
    .unwrap();
    fs::remove_file(r.env.music.join("gone.mp3")).unwrap();
    r.env.add("new.mp3", Kind::Mp3, 5);
    r.env.scan();

    let p = r.prepare(false).unwrap();
    assert_eq!(
        (
            p.plan.remove.len(),
            p.plan.moves.len(),
            p.plan.copy.len(),
            p.plan.replace.len()
        ),
        (1, 1, 1, 1)
    );
    let kinds: RefCell<Vec<&'static str>> = RefCell::default();
    let done = r
        .run_with(
            &p,
            Options::default(),
            &|pr| {
                let k = match pr {
                    Progress::Removing { .. } => "removing",
                    Progress::Moving { .. } => "moving",
                    Progress::Copying { .. } => "copying",
                    Progress::Playlists => "playlists",
                    Progress::Verifying => "verifying",
                };
                kinds.borrow_mut().push(k);
            },
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(
        kinds.into_inner(),
        [
            "removing",
            "moving",
            "copying",
            "copying",
            "playlists",
            "verifying"
        ]
    );
    assert_eq!(
        (done.removed, done.moved, done.copied, done.recopied),
        (1, 1, 1, 1)
    );

    let b = done.backup.as_ref().unwrap();
    let text = fs::read_to_string(b.dir.join("work.json")).unwrap();
    let at = |k: &str| text.find(&format!("\"{k}\"")).unwrap();
    assert!(at("remove") < at("moves") && at("moves") < at("copy") && at("copy") < at("replace"));
    let w = work(&r, b);
    assert_eq!(w["remove"][0]["path"], "Music/gone.mp3");
    assert_eq!(w["remove"][0]["reason"], "missing_from_library");
    assert_eq!(w["moves"][0]["from"], "Music/mov/x.mp3");
    assert_eq!(w["moves"][0]["to"], "Music/mov2/x.mp3");
    assert_eq!(w["copy"][0]["to"], "Music/new.mp3");
    assert_eq!(w["copy"][0]["size"], lib(&r, "new.mp3").len());
    assert_eq!(w["replace"][0]["to"], "Music/rep.mp3");
    assert_eq!(w["replace"][0]["size"], new_rep.len());
    for k in ["remove", "moves", "copy", "replace"] {
        assert_eq!(w[k].as_array().unwrap().len(), 1, "{k}");
    }

    let files: BTreeSet<String> = r
        .snapshot()
        .into_keys()
        .filter(|k| k.starts_with("Music/"))
        .collect();
    assert_eq!(
        files,
        set(&[
            "Music/keep.mp3",
            "Music/rep.mp3",
            "Music/mov2/x.mp3",
            "Music/new.mp3"
        ])
    );
    assert!(r.read("Music/rep.mp3") == new_rep);
    assert!(r.prepare(false).unwrap().plan.is_empty());
}

// Prevents: the removals being recorded only with the moves or the copies
// that follow: once they are done, the index no longer names the removed
// file. Seen from the progress callback, before the first move.
#[test]
fn removals_are_recorded_before_the_moves_begin() {
    let mut r = Rig::new(0);
    r.env.add("gone.mp3", Kind::Mp3, 1);
    r.env.add("mov/x.mp3", Kind::Mp3, 2);
    r.env.scan();
    r.adopt_empty();
    r.full_sync();
    fs::remove_file(r.env.music.join("gone.mp3")).unwrap();
    fs::create_dir(r.env.music.join("mov2")).unwrap();
    fs::rename(
        r.env.music.join("mov/x.mp3"),
        r.env.music.join("mov2/x.mp3"),
    )
    .unwrap();
    r.env.scan();
    let p = r.prepare(false).unwrap();
    assert_eq!((p.plan.remove.len(), p.plan.moves.len()), (1, 1));
    let seen: RefCell<Option<BTreeSet<String>>> = RefCell::default();
    r.run_with(
        &p,
        Options::default(),
        &|pr| {
            if let Progress::Moving { done: 0, .. } = pr {
                let now = devices::track_mappings(r.conn(), r.device().id).unwrap();
                *seen.borrow_mut() = Some(now.iter().map(|m| m.card_path.clone()).collect());
            }
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        seen.into_inner().expect("no move was announced"),
        set(&["Music/mov/x.mp3"])
    );
}

// Prevents: a mapping whose file is gone from the card being kept by a
// sync that does not copy it again (the track left the scope): the sync
// drops it, the manifest agrees, and the check passes.
#[test]
fn a_gone_mapping_of_a_track_out_of_scope_is_dropped_by_the_sync() {
    let r = Rig::new(3);
    let dev = r.adopt_empty();
    r.playlist("A", &["t1.mp3"]);
    r.playlist("B", &["t2.mp3", "t3.mp3"]);
    r.full_sync();
    let scope = DeviceScope {
        kind: Scope::Playlists,
        playlists: vec!["A".into()],
    };
    devices::set_scope(r.conn(), dev, Some(&scope)).unwrap();
    fs::remove_file(r.card.join("Music/t2.mp3")).unwrap();
    let p = r.prepare(false).unwrap();
    assert_eq!(p.plan.stale_mappings.len(), 1);
    assert_eq!(p.plan.remove.len(), 1);
    r.run(&p, Options::default()).unwrap();
    assert_eq!(r.mapped().keys().collect::<Vec<_>>(), ["t1.mp3"]);
    assert_eq!(manifest_tracks(&r), set(&["Music/t1.mp3"]));
    assert!(r.prepare(false).unwrap().plan.is_empty());
}

// Prevents: the same mapping lingering when the plan has no other track
// work (technical document, 11.2: mappings whose file is gone are dropped
// once the removals are done). Only the playlists change here (B is
// deleted), and the sync must still drop the stale mapping, as it does when
// a removal comes with it.
#[test]
fn a_gone_mapping_is_dropped_even_when_the_plan_has_no_track_work() {
    let r = Rig::new(2);
    let dev = r.adopt_empty();
    r.playlist("A", &["t1.mp3"]);
    r.playlist("B", &["t2.mp3"]);
    r.full_sync();
    let scope = DeviceScope {
        kind: Scope::Playlists,
        playlists: vec!["A".into()],
    };
    devices::set_scope(r.conn(), dev, Some(&scope)).unwrap();
    fs::remove_file(r.card.join("Music/t2.mp3")).unwrap();
    let p = r.prepare(false).unwrap();
    assert_eq!(p.plan.stale_mappings.len(), 1);
    assert!(p.plan.remove.is_empty() && p.plan.copy.is_empty());
    r.run(&p, Options::default()).unwrap();
    assert_eq!(r.mapped().keys().collect::<Vec<_>>(), ["t1.mp3"]);
    assert_eq!(manifest_tracks(&r), set(&["Music/t1.mp3"]));
}

// Prevents: a plan whose only change is a mapping whose file is gone being
// called empty (nothing recorded, "up to date" forever): no track and no
// playlist operation, only the stale mapping. The sync backs up, drops the
// mapping, rewrites the manifest, and the next plan is empty.
#[test]
fn a_plan_with_only_a_gone_mapping_is_applied() {
    let r = Rig::new(2);
    let dev = r.adopt_empty();
    r.playlist("A", &["t1.mp3"]);
    r.full_sync();
    // t2 is in no playlist: with only A in scope it leaves; its file is
    // already gone from the card, and a track in no playlist has no
    // playlist file to delete.
    let scope = DeviceScope {
        kind: Scope::Playlists,
        playlists: vec!["A".into()],
    };
    devices::set_scope(r.conn(), dev, Some(&scope)).unwrap();
    fs::remove_file(r.card.join("Music/t2.mp3")).unwrap();
    let backups = r.backups().len();
    let p = r.prepare(false).unwrap();
    assert!(p.plan.copy.is_empty() && p.plan.replace.is_empty());
    assert!(p.plan.moves.is_empty() && p.plan.remove.is_empty());
    assert!(p.plan.playlists.is_empty());
    assert_eq!(p.plan.stale_mappings.len(), 1);
    assert!(!p.plan.is_empty());
    let done = r.run(&p, Options::default()).unwrap();
    assert!(done.backup.is_some());
    assert_eq!(r.backups().len(), backups + 1);
    assert_eq!(r.mapped().keys().collect::<Vec<_>>(), ["t1.mp3"]);
    assert_eq!(manifest_tracks(&r), set(&["Music/t1.mp3"]));
    assert!(r.prepare(false).unwrap().plan.is_empty());
}

// ----------------------------------------------------------- skipped copies

// Prevents: a library file that cannot be copied (gone after the plan,
// changed to other bytes of the same size, or grown) being copied anyway,
// stopping the sync, or left in the playlists: it is skipped and reported,
// nothing of it is left on the card (not its folder, not a part file), it
// is not mapped, the playlist leaves it out, the check passes and the other
// tracks are done.
#[test]
fn a_library_file_that_changed_or_went_after_the_plan_is_skipped() {
    for case in ["gone", "same size", "grown"] {
        let mut r = Rig::new(2);
        r.env.add("sub/t4.mp3", Kind::Mp3, 4);
        r.env.scan();
        r.adopt_empty();
        r.playlist("Rock", &["t1.mp3", "sub/t4.mp3", "t2.mp3"]);
        let p = r.prepare(false).unwrap();
        let path = r.env.music.join("sub/t4.mp3");
        let mut bytes = fs::read(&path).unwrap();
        match case {
            "gone" => fs::remove_file(&path).unwrap(),
            "same size" => {
                let last = bytes.len() - 1;
                bytes[last] ^= 0xff;
                fs::write(&path, &bytes).unwrap();
            }
            _ => {
                bytes.push(0);
                fs::write(&path, &bytes).unwrap();
            }
        }
        let done = r
            .run(&p, Options::default())
            .unwrap_or_else(|f| panic!("{case}: {}", f.error));
        assert_eq!(done.copied, 2, "{case}");
        assert_eq!(done.skipped.len(), 1, "{case}: {:?}", done.skipped);
        assert_eq!(done.skipped[0].0, path, "{case}");
        if case != "gone" {
            assert!(done.skipped[0].1.contains("changed"), "{case}");
        }
        assert!(!r.exists("Music/sub"), "{case}: folder left");
        assert!(leftovers(&r).is_empty(), "{case}: {:?}", leftovers(&r));
        assert!(mapping(&r, "sub/t4.mp3").is_none(), "{case}");
        assert!(mapping(&r, "t1.mp3").is_some() && mapping(&r, "t2.mp3").is_some());
        assert_eq!(
            locations(&r.read("Rock.m3u8")),
            ["Music/t1.mp3", "Music/t2.mp3"],
            "{case}"
        );
        assert_eq!(
            manifest_tracks(&r),
            set(&["Music/t1.mp3", "Music/t2.mp3"]),
            "{case}"
        );

        // The next scan and sync take what changed.
        if case != "gone" {
            r.env.scan();
            let done = r.full_sync();
            assert_eq!(done.copied, 1, "{case}");
            assert!(r.read("Music/sub/t4.mp3") == bytes, "{case}");
            assert_eq!(
                locations(&r.read("Rock.m3u8")),
                ["Music/t1.mp3", "Music/sub/t4.mp3", "Music/t2.mp3"],
                "{case}"
            );
        }
    }
}

// Prevents: a skipped replace being treated as done: the old card file
// stays as it was, mapped with its old hash, the check expects its old
// size, and the next sync copies it again.
#[test]
fn a_skipped_replace_leaves_the_old_file_and_its_mapping() {
    let mut r = Rig::new(2);
    r.adopt_empty();
    r.full_sync();
    let old = r.read("Music/t1.mp3");
    let old_map = mapping(&r, "t1.mp3").unwrap();
    let new = fs::read(fixture(Kind::Mp3, 9)).unwrap();
    fs::write(r.env.music.join("t1.mp3"), &new).unwrap();
    r.env.scan();
    let p = r.prepare(false).unwrap();
    assert_eq!(p.plan.replace.len(), 1);
    // Changed again after the scan.
    let mut other = new.clone();
    other.push(1);
    fs::write(r.env.music.join("t1.mp3"), &other).unwrap();
    let done = r.run(&p, Options::default()).unwrap();
    assert_eq!((done.recopied, done.skipped.len()), (0, 1));
    assert!(r.read("Music/t1.mp3") == old);
    assert_eq!(mapping(&r, "t1.mp3").unwrap(), old_map);
    assert!(leftovers(&r).is_empty());
    r.env.scan();
    let done = r.full_sync();
    assert_eq!(done.recopied, 1);
    assert!(r.read("Music/t1.mp3") == other);
}

// --------------------------------------------------------------------- stop

// Prevents: a stop that loses work or half-finishes: cancel set while the
// third copy begins ends after that copy; the three files are on the card,
// mapped and in the manifest, the rest absent, the playlists untouched
// (not written, not mapped), `stopped` is set; the next sync copies the
// remaining two and writes the playlist.
#[test]
fn a_stop_during_the_copies_records_them_and_leaves_the_playlists() {
    let r = Rig::new(5);
    r.adopt_empty();
    r.playlist("Rock", &["t1.mp3", "t2.mp3", "t3.mp3", "t4.mp3", "t5.mp3"]);
    let p = r.prepare(false).unwrap();
    let order: Vec<String> = p.plan.copy.iter().map(|c| c.to.clone()).collect();
    assert_eq!(order.len(), 5);
    let cancel = AtomicBool::new(false);
    let done = r
        .run_with(
            &p,
            Options::default(),
            &|pr| {
                if let Progress::Copying { done: 2, .. } = pr {
                    cancel.store(true, Ordering::SeqCst);
                }
            },
            &cancel,
        )
        .unwrap();
    assert!(done.stopped.is_some());
    assert_eq!(done.copied, 3);
    assert_eq!((done.written, done.renamed, done.deleted), (0, 0, 0));
    let on_card: BTreeSet<String> = r.mapped().into_values().collect();
    assert_eq!(on_card, order[..3].iter().cloned().collect());
    for to in &order[..3] {
        assert!(r.exists(to), "{to}");
    }
    for to in &order[3..] {
        assert!(!r.exists(to), "{to} was copied after the stop");
    }
    assert_eq!(manifest_tracks(&r), on_card);
    assert!(!r.exists("Rock.m3u8"));
    assert!(
        devices::playlist_mappings(r.conn(), r.device().id)
            .unwrap()
            .is_empty()
    );
    assert!(leftovers(&r).is_empty());

    let p = r.prepare(false).unwrap();
    let rest: Vec<String> = p.plan.copy.iter().map(|c| c.to.clone()).collect();
    assert_eq!(rest, order[3..]);
    let done = r.run(&p, Options::default()).unwrap();
    assert!(done.stopped.is_none());
    assert_eq!(done.copied, 2);
    assert_eq!(
        locations(&r.read("Rock.m3u8")),
        [
            "Music/t1.mp3",
            "Music/t2.mp3",
            "Music/t3.mp3",
            "Music/t4.mp3",
            "Music/t5.mp3"
        ]
    );
    assert!(r.prepare(false).unwrap().plan.is_empty());
}

// Prevents: a stop requested before the first copy still copying, or
// failing the check: nothing is copied, the card is checked against the
// plan less every copy, and the next sync does it all.
#[test]
fn a_stop_before_the_first_copy_copies_nothing() {
    let r = Rig::new(2);
    r.adopt_empty();
    r.playlist("Rock", &["t1.mp3"]);
    let p = r.prepare(false).unwrap();
    let cancel = AtomicBool::new(true);
    let done = r
        .run_with(&p, Options::default(), &|_| {}, &cancel)
        .unwrap();
    assert!(done.stopped.is_some());
    assert_eq!(done.copied, 0);
    assert!(r.mapped().is_empty());
    assert!(!r.snapshot().keys().any(|k| k.starts_with("Music/")));
    assert!(!r.exists("Rock.m3u8"));
    let done = r.full_sync();
    assert_eq!((done.copied, done.written), (2, 1));
}

// Prevents: a stopped replace being counted as done: the old file stays
// in place, byte for byte, and the check expects it so.
#[test]
fn a_stop_leaves_a_file_that_was_to_be_replaced_as_it_was() {
    let mut r = Rig::new(2);
    r.adopt_empty();
    r.full_sync();
    let old = r.read("Music/t1.mp3");
    fs::write(
        r.env.music.join("t1.mp3"),
        fs::read(fixture(Kind::Mp3, 9)).unwrap(),
    )
    .unwrap();
    r.env.scan();
    let p = r.prepare(false).unwrap();
    assert_eq!(p.plan.replace.len(), 1);
    let done = r
        .run_with(&p, Options::default(), &|_| {}, &AtomicBool::new(true))
        .unwrap();
    assert!(done.stopped.is_some() && done.recopied == 0);
    assert!(r.read("Music/t1.mp3") == old);
    let done = r.full_sync();
    assert_eq!(done.recopied, 1);
}

// -------------------------------------------------------------- card failure

// Prevents: a card error mid-copy losing what was already done: the first
// copy is mapped and in the manifest, the failure names the backup and the
// folder, no part file is left, the playlist is not written; with the
// folder writable again the next sync finishes.
#[test]
fn a_card_failure_mid_copy_records_what_was_done() {
    let mut r = Rig::new(0);
    r.env.add("a/x.mp3", Kind::Mp3, 1);
    r.env.add("z/y.mp3", Kind::Mp3, 2);
    r.env.scan();
    r.adopt_empty();
    r.playlist("Rock", &["a/x.mp3", "z/y.mp3"]);
    let p = r.prepare(false).unwrap();
    assert_eq!(p.plan.copy[0].to, "Music/a/x.mp3");
    assert_eq!(p.plan.copy[1].to, "Music/z/y.mp3");
    let z = r.card.join("Music/z");
    fs::create_dir_all(&z).unwrap();
    if !readonly_dir_blocks_writes(&z) {
        eprintln!("SKIPPED: directory permissions do not stop this user");
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&z, fs::Permissions::from_mode(0o555)).unwrap();
    let res = r.run(&p, Options::default());
    fs::set_permissions(&z, fs::Permissions::from_mode(0o755)).unwrap();
    let f = failed(res);
    match &f.error {
        Error::Io { path, .. } => assert!(path.starts_with(&z), "{path:?}"),
        e => panic!("{e:?}"),
    }
    assert!(f.backup.is_some());
    assert_eq!(
        r.mapped(),
        BTreeMap::from([("a/x.mp3".to_owned(), "Music/a/x.mp3".to_owned())])
    );
    assert!(r.read("Music/a/x.mp3") == lib(&r, "a/x.mp3"));
    assert_eq!(manifest_tracks(&r), set(&["Music/a/x.mp3"]));
    assert!(!r.exists("Music/z/y.mp3"));
    assert!(!r.exists("Rock.m3u8"));
    assert!(leftovers(&r).is_empty(), "{:?}", leftovers(&r));

    let done = r.full_sync();
    assert_eq!(done.copied, 1);
    assert!(r.read("Music/z/y.mp3") == lib(&r, "z/y.mp3"));
    assert_eq!(
        locations(&r.read("Rock.m3u8")),
        ["Music/a/x.mp3", "Music/z/y.mp3"]
    );
}

// Prevents: a new copy replacing a file that appeared on the card after
// the plan was made (the owner's file would be lost): the sync stops with
// WouldOverwrite naming the path, the file keeps its bytes, no part file
// is left, the track is not mapped and a backup is named.
#[test]
fn a_new_copy_never_lands_on_a_file_that_appeared_after_the_plan() {
    let r = Rig::new(2);
    r.adopt_empty();
    let p = r.prepare(false).unwrap();
    let target = p.plan.copy[0].to.clone();
    r.put(&target, b"the owner's file");
    let f = failed(r.run(&p, Options::default()));
    match &f.error {
        Error::WouldOverwrite(path) => assert_eq!(path, &target),
        e => panic!("{e:?}"),
    }
    assert!(f.backup.is_some());
    assert_eq!(r.read(&target), b"the owner's file");
    assert!(leftovers(&r).is_empty(), "{:?}", leftovers(&r));
    assert!(r.mapped().is_empty());
}

// Prevents: a copy writing into a `.part` file a crashed sync left (its
// bytes would be lost, or mixed with the new ones): the copy takes the
// first free number and the old part files keep their bytes.
#[test]
fn a_copy_leaves_the_part_files_of_a_crashed_sync_alone() {
    let r = Rig::new(1);
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
    let mut r = r;
    r.env.add("t9.mp3", Kind::Mp3, 9);
    r.env.scan();
    r.put("Music/.fiio-tunes-1.part", b"half a file");
    r.put("Music/.fiio-tunes-2.part", b"another");
    let p = r.prepare(false).unwrap();
    assert_eq!(p.plan.copy.len(), 1);
    let done = r.run(&p, Options::default()).unwrap();
    assert_eq!(done.copied, 1);
    assert_eq!(r.read("Music/.fiio-tunes-1.part"), b"half a file");
    assert_eq!(r.read("Music/.fiio-tunes-2.part"), b"another");
    assert!(r.read("Music/t9.mp3") == lib(&r, "t9.mp3"));
    assert_eq!(
        leftovers(&r),
        ["Music/.fiio-tunes-1.part", "Music/.fiio-tunes-2.part"]
    );
}

// -------------------------------------------------------------- checkpoints

// Prevents: the mappings and the manifest being recorded only at the end
// of the copies: a long sync killed half way would lose everything. After
// 500 copies they are in the index and in the manifest, before that they
// are not.
#[test]
fn the_mappings_are_checkpointed_every_500_copies() {
    let mut r = Rig::new(0);
    let base = fs::read(fixture(Kind::Mp3, 1)).unwrap();
    for i in 0..501u32 {
        let mut b = base.clone();
        b.extend(i.to_le_bytes());
        r.env.add_bytes(&format!("f{i:03}.mp3"), &b);
    }
    r.env.scan();
    r.adopt_empty();
    let p = r.prepare(false).unwrap();
    assert_eq!(p.plan.copy.len(), 501);
    let seen: RefCell<BTreeMap<usize, (usize, usize)>> = RefCell::default();
    let done = r
        .run_with(
            &p,
            Options::default(),
            &|pr| {
                if let Progress::Copying { done, .. } = pr
                    && (done == 499 || done == 500)
                {
                    let mapped = devices::mapped_count(r.conn(), r.device().id).unwrap();
                    seen.borrow_mut().insert(
                        done,
                        (
                            mapped,
                            r.card_manifest()["tracks"].as_array().unwrap().len(),
                        ),
                    );
                }
            },
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(done.copied, 501);
    let seen = seen.into_inner();
    assert_eq!(seen[&499], (0, 0), "recorded too early");
    assert_eq!(seen[&500], (500, 500), "not recorded after 500 copies");
    assert_eq!(devices::mapped_count(r.conn(), r.device().id).unwrap(), 501);
    assert_eq!(r.card_manifest()["tracks"].as_array().unwrap().len(), 501);
}

// ---------------------------------------------------------------- take-back

// Prevents: a sync killed between two checkpoints costing a whole file: the
// file it copied to the card path with no mapping is mapped again (size
// and hash), not copied a second time and not left as an unmanaged file in
// the way.
#[test]
fn a_file_at_a_tracks_card_path_with_no_mapping_is_taken_back() {
    let r = Rig::new(3);
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
    r.put("Music/t2.mp3", &lib(&r, "t2.mp3"));
    let ino = fs::metadata(r.card.join("Music/t2.mp3")).unwrap().ino();
    let p = r.prepare(false).unwrap();
    assert_eq!(p.taken_back, 1);
    let copies: Vec<&str> = p.plan.copy.iter().map(|c| c.to.as_str()).collect();
    assert_eq!(copies, ["Music/t3.mp3"]);
    let t2 = r.env.track(&r.env.music.join("t2.mp3"));
    let m = mapping(&r, "t2.mp3").expect("mapped again");
    assert_eq!(m.card_path, "Music/t2.mp3");
    assert_eq!((m.synced_size, m.synced_hash), (t2.size, t2.hash));

    let done = r.run(&p, Options::default()).unwrap();
    assert_eq!(done.copied, 1);
    assert_eq!(
        fs::metadata(r.card.join("Music/t2.mp3")).unwrap().ino(),
        ino,
        "the taken file was rewritten"
    );
    assert_eq!(
        manifest_tracks(&r),
        set(&["Music/t1.mp3", "Music/t2.mp3", "Music/t3.mp3"])
    );
    assert!(r.prepare(false).unwrap().plan.is_empty());
}

// Prevents: a file that is not the track being taken: other bytes of the
// same size, or another size, at the card path. Neither is mapped, read as
// the track, nor touched; the planner leaves its path alone.
#[test]
fn a_file_that_is_not_the_tracks_content_is_not_taken_back() {
    for case in ["same size", "shorter"] {
        let r = Rig::new(2);
        let dev = r.adopt_empty();
        r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
        let mut other = lib(&r, "t2.mp3");
        match case {
            "same size" => {
                let last = other.len() - 1;
                other[last] ^= 0xff;
            }
            _ => {
                other.pop();
            }
        };
        r.put("Music/t2.mp3", &other);
        // A file of another size is not even read.
        let path = r.card.join("Music/t2.mp3");
        if case == "shorter" {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        }
        let p = r.prepare(false);
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        let p = p.unwrap_or_else(|e| panic!("{case}: {e}"));
        assert_eq!(p.taken_back, 0, "{case}");
        assert!(mapping(&r, "t2.mp3").is_none(), "{case}");
        assert!(p.plan.copy.is_empty(), "{case}: {:?}", p.plan.copy);
        r.run(&p, Options::default()).unwrap();
        assert!(r.read("Music/t2.mp3") == other, "{case}: file touched");
        assert!(mapping(&r, "t2.mp3").is_none(), "{case}");
    }
}

// Prevents: a copy of a track already on the card being taken as its home:
// when the mapped file is there, an identical file at the target path is
// someone else's and stays unmanaged.
#[test]
fn a_track_whose_mapped_file_is_there_does_not_take_another_file() {
    let mut r = Rig::new(0);
    r.env.add("sub/t1.mp3", Kind::Mp3, 1);
    r.env.add("t2.mp3", Kind::Mp3, 2);
    r.env.scan();
    let dev = r.adopt_empty();
    r.adopt_file(dev, "sub/t1.mp3", "Music/old/t1.mp3");
    r.adopt_file(dev, "t2.mp3", "Music/t2.mp3");
    r.put("Music/sub/t1.mp3", &lib(&r, "sub/t1.mp3"));
    let p = r.prepare(false).unwrap();
    assert_eq!(p.taken_back, 0);
    assert_eq!(r.mapped()["sub/t1.mp3"], "Music/old/t1.mp3");
}

// Prevents: a file being taken back for a track when another track's
// mapping already names that path: two mappings on one card path break the
// index's uniqueness and the sync would fail. Track t2 is mapped at t1's
// path (its card file was swapped by another program for t1's bytes);
// nothing is taken, and the mapping stays t2's.
#[test]
fn a_path_another_track_is_mapped_to_is_not_taken_back() {
    let r = Rig::new(2);
    let dev = r.adopt_empty();
    let t2 = r.env.track(&r.env.music.join("t2.mp3"));
    r.put("Music/t1.mp3", &lib(&r, "t1.mp3"));
    let mine = TrackMapping {
        track_id: t2.id,
        card_path: "Music/t1.mp3".into(),
        synced_hash: t2.hash,
        synced_size: t2.size,
    };
    devices::replace_track_mappings(r.conn(), dev, std::slice::from_ref(&mine)).unwrap();
    let p = r.prepare(false).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(p.taken_back, 0);
    assert_eq!(devices::track_mappings(r.conn(), dev).unwrap(), [mine]);
}

// Prevents: a crash between the rename of a move and the record of it
// leaving a stale mapping that makes the next sync copy the file again and
// leave the moved one as an unmanaged duplicate: the file at the new path
// is taken back, nothing is copied or moved, the playlist follows.
#[test]
fn a_moved_file_whose_mapping_is_stale_is_taken_back_not_copied() {
    let mut r = Rig::new(0);
    r.env.add("old/t1.mp3", Kind::Mp3, 1);
    r.env.add("t2.mp3", Kind::Mp3, 2);
    r.env.scan();
    r.adopt_empty();
    r.playlist("Rock", &["old/t1.mp3", "t2.mp3"]);
    r.full_sync();
    fs::create_dir(r.env.music.join("new")).unwrap();
    fs::rename(
        r.env.music.join("old/t1.mp3"),
        r.env.music.join("new/t1.mp3"),
    )
    .unwrap();
    r.env.scan();
    // The sync renamed the card file, then died before recording it.
    fs::create_dir_all(r.card.join("Music/new")).unwrap();
    fs::rename(
        r.card.join("Music/old/t1.mp3"),
        r.card.join("Music/new/t1.mp3"),
    )
    .unwrap();
    fs::remove_dir(r.card.join("Music/old")).unwrap();
    let ino = fs::metadata(r.card.join("Music/new/t1.mp3")).unwrap().ino();
    assert_eq!(r.mapped()["new/t1.mp3"], "Music/old/t1.mp3");

    let p = r.prepare(false).unwrap();
    assert_eq!(p.taken_back, 1);
    assert_eq!(r.mapped()["new/t1.mp3"], "Music/new/t1.mp3");
    assert!(p.plan.copy.is_empty() && p.plan.moves.is_empty() && p.plan.remove.is_empty());
    let done = r.run(&p, Options::default()).unwrap();
    assert_eq!((done.copied, done.moved, done.removed), (0, 0, 0));
    assert_eq!(
        fs::metadata(r.card.join("Music/new/t1.mp3")).unwrap().ino(),
        ino
    );
    assert_eq!(
        locations(&r.read("Rock.m3u8")),
        ["Music/new/t1.mp3", "Music/t2.mp3"]
    );
    assert!(r.prepare(false).unwrap().plan.is_empty());
}

// Prevents: take-back running after the NothingMatched refusal: a first
// sync killed before its first checkpoint leaves audio on the card and no
// mapping, which `prepare` used to refuse. Files that are the library's
// are taken; other audio still refuses.
#[test]
fn take_back_comes_before_the_nothing_matched_refusal() {
    let r = Rig::new(2);
    r.adopt_empty();
    r.put("Music/t1.mp3", &lib(&r, "t1.mp3"));
    r.put("Music/t2.mp3", &lib(&r, "t2.mp3"));
    let p = r.prepare(false).unwrap();
    assert_eq!(p.taken_back, 2);
    assert!(p.plan.copy.is_empty());
    assert_eq!(r.mapped().len(), 2);

    let r = Rig::new(2);
    r.adopt_empty();
    r.put("Music/other.mp3", b"somebody else's song");
    match r.prepare(false) {
        Err(Error::NothingMatched { .. }) => {}
        other => panic!("{:?}", other.map(|p| p.taken_back)),
    }
    assert!(r.mapped().is_empty());
}

// --------------------------------------------------------------- hop files

// Prevents: a sync planned over a card holding a cycle's hidden file: the
// file's track would look gone and be copied again. Both kinds of hop file
// (track and playlist) are refused with their count, in either mode, and
// nothing is changed, not the card, not the mappings.
#[test]
fn prepare_refuses_a_card_holding_a_hop_file() {
    let r = Rig::new(2);
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
    r.adopt_file(dev, "t2.mp3", "Music/t2.mp3");
    r.sync();
    r.put("Music/.fiio-tunes-move-3.move", b"x");
    let maps = devices::track_mappings(r.conn(), dev).unwrap();
    let before = r.snapshot();
    for only in [true, false] {
        match r.prepare(only) {
            Err(Error::StoppedMidMove(1)) => {}
            other => panic!("{:?}", other.map(|p| p.taken_back)),
        }
    }
    r.put(".fiio-tunes-move-4.m3u8", b"x");
    match r.prepare(false) {
        Err(Error::StoppedMidMove(2)) => {}
        other => panic!("{:?}", other.map(|p| p.taken_back)),
    }
    let mut after = r.snapshot();
    after.remove(".fiio-tunes-move-4.m3u8");
    assert_eq!(after, before);
    assert_eq!(devices::track_mappings(r.conn(), dev).unwrap(), maps);
}

// ------------------------------------------------------------- store pieces

// Prevents: `replace_track_mappings` touching the playlist mappings, or
// leaving a mapping the new list does not hold; and `set_card_path` moving
// more than one track's path, or leaving the lookup key (FAT's, case
// folded) on the old name.
#[test]
fn the_store_replaces_track_mappings_and_sets_one_card_path() {
    let r = Rig::new(3);
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
    r.adopt_file(dev, "t2.mp3", "Music/t2.mp3");
    r.playlist("Rock", &["t1.mp3"]);
    let rock = r.env.playlist_id("Rock").unwrap();
    let lists = [PlaylistMapping {
        playlist_id: rock,
        card_name: "Rock.m3u8".into(),
    }];
    let tracks = devices::track_mappings(r.conn(), dev).unwrap();
    devices::replace_mappings(r.conn(), dev, &tracks, &lists).unwrap();
    // Another device with the same track mapped.
    let other = devices::upsert_device(r.conn(), "folder:/elsewhere", None, None).unwrap();
    devices::replace_track_mappings(r.conn(), other, &tracks[..1]).unwrap();

    let t3 = r.env.track(&r.env.music.join("t3.mp3"));
    let new = TrackMapping {
        track_id: t3.id,
        card_path: "Music/t3.mp3".into(),
        synced_hash: t3.hash,
        synced_size: t3.size,
    };
    devices::replace_track_mappings(r.conn(), dev, std::slice::from_ref(&new)).unwrap();
    assert_eq!(
        devices::track_mappings(r.conn(), dev).unwrap(),
        std::slice::from_ref(&new)
    );
    assert_eq!(devices::playlist_mappings(r.conn(), dev).unwrap(), lists);
    assert_eq!(
        devices::track_mappings(r.conn(), other).unwrap(),
        tracks[..1]
    );

    devices::replace_track_mappings(r.conn(), dev, &tracks).unwrap();
    let t1 = tracks[0].track_id;
    devices::set_card_path(r.conn(), dev, t1, "Music/Sub/New.mp3").unwrap();
    let got = devices::track_mappings(r.conn(), dev).unwrap();
    let by: BTreeMap<i64, &str> = got
        .iter()
        .map(|m| (m.track_id, m.card_path.as_str()))
        .collect();
    assert_eq!(by[&t1], "Music/Sub/New.mp3");
    assert_eq!(by[&tracks[1].track_id], "Music/t2.mp3");
    let key: String = r
        .conn()
        .query_row(
            "SELECT card_key FROM device_tracks WHERE device_id = ?1 AND track_id = ?2",
            [dev, t1],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(key, fiio_core::paths::card_key("music/sub/new.MP3"));
    assert_eq!(
        devices::track_mappings(r.conn(), other).unwrap()[0].card_path,
        "Music/t1.mp3"
    );
    // The hash and size ride along unchanged.
    assert_eq!(
        got.iter().find(|m| m.track_id == t1).unwrap().synced_hash,
        tracks[0].synced_hash
    );
}

// Prevents: a scope that is not `all` leaving the playlists of out-of-scope
// tracks pointing at files that were just removed: the playlist is written
// from what is on the card after the removals.
#[test]
fn a_playlist_never_names_a_file_the_same_sync_removed() {
    let r = Rig::new(2);
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
    r.adopt_file(dev, "t2.mp3", "Music/t2.mp3");
    r.playlist("A", &["t1.mp3"]);
    r.playlist("B", &["t2.mp3"]);
    r.full_sync();
    let scope = DeviceScope {
        kind: Scope::Playlists,
        playlists: vec!["A".into()],
    };
    devices::set_scope(r.conn(), dev, Some(&scope)).unwrap();
    r.full_sync();
    assert!(!r.exists("Music/t2.mp3"));
    assert!(!r.exists("B.m3u8"), "{:?}", r.snapshot().keys());
    assert_eq!(locations(&r.read("A.m3u8")), ["Music/t1.mp3"]);
    assert!(r.prepare(false).unwrap().plan.is_empty());
}
