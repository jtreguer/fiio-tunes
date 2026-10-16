//! `sync.rs` (prepare, the card checks, scope problems, `describe`) and the
//! device store functions F4 added (scope, devices, restoring,
//! library_playlists). Cards are folders in temp dirs; nothing here can find
//! the real X3 (detection is not used: the card is handed to `prepare`).

mod common;

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use common::{Env, FakeCard, Kind};
use fiio_core::config::Scope as Kind_;
use fiio_core::device::Found;
use fiio_core::manifest::Identity;
use fiio_core::mounts::Mount;
use fiio_core::plan::{
    self, CopyOp, LibPlaylist, MoveOp, Plan, PlaylistOp, RemoveOp, RemoveReason, ReplaceOp,
    ReplaceReason, SkipReason, Skipped, Space, SpaceCheck, StaleMapping,
};
use fiio_core::store::devices::{self, DeviceRow, DeviceScope, PlaylistMapping, TrackMapping};
use fiio_core::store::{self, TrackRow};
use fiio_core::sync::{self, Prepared};
use fiio_core::{Error, playlists};

struct World {
    env: Env,
    card: FakeCard,
    _dir: tempfile::TempDir,
}

impl World {
    /// A library of MP3 fixtures `1..=n` at `t<i>.mp3`, scanned, and an
    /// empty card folder.
    fn new(n: u32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("card");
        fs::create_dir(&root).unwrap();
        let mut w = World {
            env: Env::new(),
            card: FakeCard::new(root),
            _dir: dir,
        };
        for i in 1..=n {
            w.env.add(&format!("t{i}.mp3"), Kind::Mp3, i);
        }
        w.env.scan();
        w.env.cfg.device.mountpoint = w.card.root.clone();
        w
    }

    fn conn(&self) -> &rusqlite::Connection {
        self.env.store.conn()
    }

    fn found(&self) -> Found {
        Found {
            mount: Mount {
                mountpoint: self.card.root.clone(),
                fstype: String::new(),
                read_only: false,
                root: PathBuf::new(),
            },
            source: None,
            usb: None,
        }
    }

    fn key(&self) -> String {
        format!("folder:{}", self.card.root.display())
    }

    fn adopt(&self) -> DeviceRow {
        let id = devices::upsert_device(self.conn(), &self.key(), None, None).unwrap();
        self.row(id)
    }

    fn row(&self, id: i64) -> DeviceRow {
        devices::devices(self.conn())
            .unwrap()
            .into_iter()
            .find(|d| d.id == id)
            .unwrap()
    }

    fn prepare(&self) -> fiio_core::Result<Prepared> {
        sync::prepare(self.conn(), &self.env.cfg, &self.found())
    }

    fn track(&self, rel: &str) -> TrackRow {
        self.env.track(&self.env.music.join(rel))
    }

    /// The library file `rel` put on the card at `Music/<rel>` and mapped,
    /// as a sync would leave it.
    fn synced(&mut self, _dev: i64, rel: &str) -> TrackMapping {
        let t = self.track(rel);
        let card_path = format!("Music/{rel}");
        self.card.put(&card_path, &fs::read(&t.path).unwrap());
        TrackMapping {
            track_id: t.id,
            card_path,
            synced_hash: t.hash,
            synced_size: t.size,
        }
    }

    fn map(&self, dev: i64, tracks: &[TrackMapping], lists: &[PlaylistMapping]) {
        devices::replace_mappings(self.conn(), dev, tracks, lists).unwrap();
    }

    fn load_playlist(&self, name: &str, files: &[&str]) {
        let text: String = files
            .iter()
            .map(|f| format!("{}\n", self.env.music.join(f).display()))
            .collect();
        self.env.write_playlist(name, &text);
        playlists::sync_from_disk(self.conn(), &self.env.cfg, true).unwrap();
    }
}

fn names(n: &[&str]) -> Vec<String> {
    n.iter().map(|s| (*s).to_owned()).collect()
}

fn scope(kind: Kind_, n: &[&str]) -> DeviceScope {
    DeviceScope {
        kind,
        playlists: names(n),
    }
}

fn device_row(card_id: Option<&str>) -> DeviceRow {
    DeviceRow {
        id: 1,
        device_key: "usb:A".into(),
        card_id: card_id.map(str::to_owned),
        name: None,
        model: None,
        last_sync_at: None,
    }
}

fn ident(key: &str, id: &str) -> Identity {
    Identity {
        device_key: key.into(),
        card_id: id.into(),
    }
}

// ------------------------------------------------------------ check_card

// Prevents: a card being synced as another one: every combination of
// recorded card id and manifest, and a manifest of another key or id.
#[test]
fn check_card_accepts_only_agreeing_identities() {
    let wrong = |r: fiio_core::Result<()>| matches!(r, Err(Error::WrongCard(k)) if k == "usb:A");
    let ok = sync::check_card;
    assert!(ok(&device_row(None), None).is_ok());
    assert!(wrong(ok(&device_row(None), Some(&ident("usb:A", "x")))));
    assert!(wrong(ok(&device_row(Some("x")), None)));
    assert!(ok(&device_row(Some("x")), Some(&ident("usb:A", "x"))).is_ok());
    assert!(wrong(ok(
        &device_row(Some("x")),
        Some(&ident("usb:A", "y"))
    )));
    assert!(wrong(ok(
        &device_row(Some("x")),
        Some(&ident("usb:B", "x"))
    )));
}

// ----------------------------------------------------- scope and its words

fn lib(w: &World) -> (Vec<TrackRow>, Vec<LibPlaylist>) {
    (
        store::all_tracks(w.conn()).unwrap(),
        devices::library_playlists(w.conn()).unwrap(),
    )
}

// Prevents: a scope that would plan the removal of the whole card passing
// as fine, and the own/config wording swapped (the user is told which
// setting to change).
#[test]
fn scope_problem_names_each_problem_and_where_to_fix_it() {
    let w = World::new(3);
    w.load_playlist("A", &["t1.mp3", "t2.mp3"]);
    w.load_playlist("B", &["t1.mp3", "t2.mp3", "t3.mp3"]);
    w.load_playlist("Empty", &[]);
    let (tracks, lists) = lib(&w);
    let problem = |s: &DeviceScope, own| sync::scope_problem(s, own, &tracks, &lists);

    assert_eq!(problem(&scope(Kind_::All, &["Nope"]), true), None);
    assert_eq!(problem(&scope(Kind_::Playlists, &["A"]), true), None);
    assert_eq!(problem(&scope(Kind_::Except, &["A"]), false), None);
    assert_eq!(problem(&scope(Kind_::Except, &[]), true), None);

    // Each problem: a message for the player's own scope that does not point
    // to the config, and one for the config that does.
    let both = |s: DeviceScope| (problem(&s, true).unwrap(), problem(&s, false).unwrap());
    let cases = [
        (scope(Kind_::Playlists, &[]), None),
        (scope(Kind_::Playlists, &["A", "Nope"]), Some("Nope")),
        (scope(Kind_::Except, &["Nope"]), Some("Nope")),
        (scope(Kind_::Playlists, &["Empty"]), None),
        (scope(Kind_::Except, &["B"]), None),
    ];
    let mut seen = std::collections::HashSet::new();
    for (s, mention) in cases {
        let (own, cfg) = both(s.clone());
        assert!(own.contains("this player"), "{s:?}: {own}");
        assert!(!own.contains("sync."), "{s:?}: {own}");
        assert!(cfg.contains("sync."), "{s:?}: {cfg}");
        assert!(!cfg.contains("this player"), "{s:?}: {cfg}");
        if let Some(m) = mention {
            assert!(own.contains(m) && cfg.contains(m), "{s:?}: {own} / {cfg}");
        }
        seen.insert(own);
    }
    assert_eq!(seen.len(), 4, "four different messages: {seen:?}");
}

// Prevents: a missing track counting as a track the scope sends, which
// would let a scope that keeps nothing pass.
#[test]
fn a_scope_holding_only_missing_tracks_is_a_problem() {
    let w = World::new(2);
    w.load_playlist("A", &["t1.mp3"]);
    fs::remove_file(w.env.music.join("t1.mp3")).unwrap();
    let mut w = w;
    w.env.scan();
    assert!(w.track("t1.mp3").missing_since.is_some());
    let (tracks, lists) = lib(&w);
    assert!(sync::scope_problem(&scope(Kind_::Playlists, &["A"]), true, &tracks, &lists).is_some());
}

// Prevents: the device's own scope being ignored, or [sync] being ignored
// when it has none; and the flag saying which was used being wrong.
#[test]
fn device_scope_is_the_devices_own_else_the_config() {
    let mut w = World::new(1);
    let dev = w.adopt().id;
    w.env.cfg.sync.scope = Kind_::Except;
    w.env.cfg.sync.playlists = names(&["X"]);
    let got = sync::device_scope(w.conn(), &w.env.cfg, dev).unwrap();
    assert_eq!(got, (scope(Kind_::Except, &["X"]), false));

    devices::set_scope(w.conn(), dev, Some(&scope(Kind_::Playlists, &["Y", "Z"]))).unwrap();
    let got = sync::device_scope(w.conn(), &w.env.cfg, dev).unwrap();
    assert_eq!(got, (scope(Kind_::Playlists, &["Y", "Z"]), true));

    devices::set_scope(w.conn(), dev, Some(&scope(Kind_::All, &[]))).unwrap();
    let got = sync::device_scope(w.conn(), &w.env.cfg, dev).unwrap();
    assert_eq!(got, (scope(Kind_::All, &[]), true), "an own `all` is own");

    devices::set_scope(w.conn(), dev, None).unwrap();
    let got = sync::device_scope(w.conn(), &w.env.cfg, dev).unwrap();
    assert!(!got.1);
}

// Prevents: the scope being described wrongly in the summary line the user
// reads before a sync, singular and plural included.
#[test]
fn a_scope_describes_itself_and_counts_its_tracks() {
    let w = World::new(3);
    w.load_playlist("A", &["t1.mp3", "t2.mp3"]);
    w.load_playlist("B", &["t2.mp3"]);
    let (tracks, lists) = lib(&w);
    let d = |k, n: &[&str]| scope(k, n).describe();
    assert_eq!(d(Kind_::All, &[]), "the whole library");
    assert_eq!(d(Kind_::Except, &[]), "the whole library");
    let one = d(Kind_::Playlists, &["A"]);
    assert!(one.contains("1 playlist") && !one.contains("1 playlists"));
    assert!(one.contains('A'));
    let two = d(Kind_::Playlists, &["A", "B"]);
    assert!(two.contains("2 playlists") && two.contains("A, B"), "{two}");
    let ex = d(Kind_::Except, &["B"]);
    assert!(ex.contains("except") && ex.contains("1 playlist") && ex.contains('B'));

    let size = |s: DeviceScope| s.size(&tracks, &lists);
    let t = |rel: &str| w.track(rel).size;
    assert_eq!(
        size(scope(Kind_::All, &[])),
        (3, t("t1.mp3") + t("t2.mp3") + t("t3.mp3"))
    );
    assert_eq!(
        size(scope(Kind_::Playlists, &["A"])),
        (2, t("t1.mp3") + t("t2.mp3"))
    );
    assert_eq!(
        size(scope(Kind_::Except, &["B"])),
        (2, t("t1.mp3") + t("t3.mp3"))
    );
}

// Prevents: sizes printed in the wrong unit, or at the wrong boundary.
#[test]
fn bytes_are_shown_in_decimal_units() {
    assert_eq!(sync::bytes(0), "0 B");
    assert_eq!(sync::bytes(999), "999 B");
    assert_eq!(sync::bytes(1_000), "1.0 kB");
    assert_eq!(sync::bytes(1_500_000), "1.5 MB");
    assert_eq!(sync::bytes(2_000_000_000), "2.0 GB");
}

// --------------------------------------------------------------- prepare

// Prevents: a card never adopted being planned for (everything on it would
// be copied again), and the card being touched on the way to the refusal.
#[test]
fn prepare_refuses_a_card_that_was_not_adopted_and_writes_nothing() {
    let mut w = World::new(2);
    w.card.copy("x.mp3", Kind::Mp3, 9);
    w.card.age();
    let err = w.prepare().err().expect("refused");
    assert!(
        matches!(&err, Error::NotAdopted(k) if *k == w.key()),
        "{err}"
    );
    w.card.assert_untouched();
}

// Prevents: an adoption that matched nothing on a card holding songs being
// planned: every song would be copied a second time. An empty card is fine.
#[test]
fn prepare_refuses_nothing_matched_on_a_card_with_audio_but_not_an_empty_one() {
    let mut w = World::new(2);
    let dev = w.adopt().id;
    let p = w.prepare().expect("an empty card is allowed");
    assert_eq!(p.plan.copy.len(), 2);
    assert_eq!(p.device.id, dev);
    assert!(p.tracks_map.is_empty() && p.playlists_map.is_empty());
    assert!(!p.scope.1);

    w.card.copy("x.mp3", Kind::Mp3, 9);
    w.card.copy("deep/y.flac", Kind::Flac, 9);
    w.card.age();
    let err = w.prepare().err().expect("refused");
    match &err {
        Error::NothingMatched { key, files } => {
            assert_eq!((key.as_str(), *files), (w.key().as_str(), 2));
        }
        other => panic!("{other}"),
    }
    w.card.assert_untouched();

    // A single mapping is enough.
    let m = w.synced(dev, "t1.mp3");
    w.map(dev, &[m], &[]);
    w.card.age();
    let p = w.prepare().expect("something is mapped");
    assert_eq!(p.plan.unmanaged.files, ["deep/y.flac", "x.mp3"]);
    w.card.assert_untouched();
}

// Prevents: a sync planned over an unfinished restore, whose mappings are
// not the card's.
#[test]
fn prepare_refuses_an_unfinished_restore() {
    let w = World::new(1);
    let dev = w.adopt().id;
    assert_eq!(devices::restoring(w.conn(), dev).unwrap(), None);
    w.conn()
        .execute(
            "UPDATE devices SET restoring = '20261016-101500' WHERE id = ?1",
            [dev],
        )
        .unwrap();
    assert_eq!(
        devices::restoring(w.conn(), dev).unwrap().as_deref(),
        Some("20261016-101500")
    );
    let err = w.prepare().err().expect("refused");
    assert!(
        matches!(&err, Error::RestoreUnfinished(s) if s == "20261016-101500"),
        "{err}"
    );
    // Cleared, it plans again.
    w.conn()
        .execute("UPDATE devices SET restoring = NULL", [])
        .unwrap();
    assert!(w.prepare().is_ok());
}

// Prevents: a card with another identity than the adopted one being planned
// for: a manifest where none was recorded, none where one was, another id.
#[test]
fn prepare_refuses_a_card_whose_identity_is_not_the_adopted_one() {
    let mut w = World::new(1);
    let dev = w.adopt().id;
    let manifest = |key: &str, id: &str| {
        format!(r#"{{"device_key": "{key}", "card_id": "{id}"}}"#).into_bytes()
    };
    let wrong = |w: &World| matches!(w.prepare().err(), Some(Error::WrongCard(_)));

    // A manifest on a card adopted without one: the key it names is used.
    w.card
        .put(".fiio-tunes/manifest.json", &manifest("folder:k", "c1"));
    w.card.age();
    assert!(matches!(w.prepare().err(), Some(Error::NotAdopted(k)) if k == "folder:k"));
    let k = devices::upsert_device(w.conn(), "folder:k", None, None).unwrap();
    assert!(wrong(&w), "the device has no card id, the card has one");

    devices::set_card_id(w.conn(), k, Some("c1")).unwrap();
    assert!(w.prepare().is_ok());
    devices::set_card_id(w.conn(), k, Some("c2")).unwrap();
    assert!(wrong(&w), "another card id");

    // A card with no manifest where the device recorded one.
    w.card.remove(".fiio-tunes/manifest.json");
    devices::set_card_id(w.conn(), dev, Some("c1")).unwrap();
    assert!(wrong(&w));
}

// Prevents: a scope that names nothing real being planned (it would remove
// every managed file), and the own scope not pointing to the `scope` command.
#[test]
fn prepare_refuses_a_bad_scope_for_the_device_or_the_config() {
    let mut w = World::new(2);
    w.load_playlist("A", &["t1.mp3"]);
    let dev = w.adopt().id;

    w.env.cfg.sync.scope = Kind_::Playlists;
    w.env.cfg.sync.playlists = names(&["Nope"]);
    let msg = match w.prepare().err() {
        Some(Error::BadScope(m)) => m,
        other => panic!("{other:?}"),
    };
    assert!(msg.contains("Nope") && msg.contains("sync."), "{msg}");
    assert!(!msg.contains("fiio-tunes scope"), "{msg}");

    devices::set_scope(w.conn(), dev, Some(&scope(Kind_::Playlists, &["Nope"]))).unwrap();
    let msg = match w.prepare().err() {
        Some(Error::BadScope(m)) => m,
        other => panic!("{other:?}"),
    };
    assert!(
        msg.contains("Nope") && msg.contains("fiio-tunes scope"),
        "{msg}"
    );

    // The device's own valid scope wins over the bad config.
    devices::set_scope(w.conn(), dev, Some(&scope(Kind_::Playlists, &["A"]))).unwrap();
    let p = w.prepare().unwrap();
    assert_eq!(p.scope, (scope(Kind_::Playlists, &["A"]), true));
    assert_eq!(p.plan.copy.len(), 1);
    assert_eq!(p.plan.copy[0].to, "Music/t1.mp3");
}

// Prevents: an unreadable folder looking like missing files (every managed
// file in it would be planned for a second copy).
#[test]
fn prepare_refuses_a_card_listing_with_problems() {
    let mut w = World::new(1);
    let dev = w.adopt().id;
    let m = w.synced(dev, "t1.mp3");
    w.map(dev, &[m], &[]);
    w.card.put("Locked/z.mp3", b"z");
    w.card.age();
    let locked = w.card.root.join("Locked");
    if !common::chmod_000_blocks_reads(&locked) {
        eprintln!("skipped: permission bits do not stop this user (root?)");
        return;
    }
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let r = w.prepare();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    match r.err() {
        Some(Error::CardIncomplete(m)) => assert!(m.contains("Locked"), "{m}"),
        other => panic!("{other:?}"),
    }
    w.card.age();
    w.card.assert_untouched();
}

// Prevents: prepare feeding the planner the wrong things: the mappings, the
// card's real listing and playlist bytes, the layout from the config, the
// card's space; and writing to the card.
#[test]
fn prepare_makes_the_plan_from_the_index_and_the_card() {
    let mut w = World::new(4);
    w.load_playlist("Rock", &["t1.mp3", "t2.mp3"]);
    let dev = w.adopt().id;
    let m1 = w.synced(dev, "t1.mp3");
    // t2 was copied, then edited on the card.
    let m2 = w.synced(dev, "t2.mp3");
    w.card.put("Music/t2.mp3", b"edited by someone");
    // t3 is mapped, but the card file is gone.
    let m3 = TrackMapping {
        track_id: w.track("t3.mp3").id,
        card_path: "Music/t3.mp3".into(),
        synced_hash: w.track("t3.mp3").hash,
        synced_size: 10,
    };
    // A playlist whose mapped file is already right.
    let rock = w.env.playlist_id("Rock").unwrap();
    w.map(
        dev,
        &[m1.clone(), m2.clone(), m3],
        &[PlaylistMapping {
            playlist_id: rock,
            card_name: "Rock.m3u8".into(),
        }],
    );
    w.card.put("Rock.m3u8", b"old");
    w.card.put("Unmanaged/u.mp3", b"u");
    w.card.age();

    let p = w.prepare().unwrap();
    let plan = &p.plan;
    assert_eq!(
        plan.copy
            .iter()
            .map(|c| (c.track_id, c.to.as_str()))
            .collect::<Vec<_>>(),
        [
            (w.track("t3.mp3").id, "Music/t3.mp3"),
            (w.track("t4.mp3").id, "Music/t4.mp3")
        ]
    );
    assert_eq!(plan.replace.len(), 1);
    assert_eq!(plan.replace[0].track_id, m2.track_id);
    assert_eq!(plan.replace[0].reason, ReplaceReason::ChangedOnCard);
    assert_eq!(plan.replace[0].old_size, 17);
    assert_eq!(plan.stale_mappings.len(), 1);
    assert_eq!(plan.unmanaged.files, ["Unmanaged/u.mp3"]);
    match plan.playlists.as_slice() {
        [
            PlaylistOp::Write {
                path,
                text,
                entries,
                ..
            },
        ] => {
            assert_eq!(path, "Rock.m3u8");
            assert_eq!(*entries, 2);
            assert!(text.contains("Music/t1.mp3\n") && text.contains("Music/t2.mp3\n"));
        }
        other => panic!("{other:?}"),
    }
    // The space comes from the card's file system.
    let space = plan.space.device.expect("space known");
    assert!(space.capacity >= space.free && space.capacity > 0);
    // What was handed back is what the plan was made from.
    assert_eq!(p.tracks_map.len(), 3);
    assert_eq!(p.playlists_map.len(), 1);
    assert_eq!(p.library.len(), 4);
    assert_eq!(p.contents.audio.len(), 3);
    assert_eq!(p.card, w.card.root);
    assert_eq!(p.device.device_key, w.key());
    w.card.assert_untouched();
}

// Prevents: the bytes of a managed card playlist not being read (it would be
// written again on every sync), or read from the wrong folder.
#[test]
fn prepare_compares_a_card_playlist_with_its_rendered_bytes() {
    let mut w = World::new(1);
    w.load_playlist("Rock", &["t1.mp3"]);
    let dev = w.adopt().id;
    let m = w.synced(dev, "t1.mp3");
    let rock = w.env.playlist_id("Rock").unwrap();
    w.map(
        dev,
        &[m],
        &[PlaylistMapping {
            playlist_id: rock,
            card_name: "Rock.m3u8".into(),
        }],
    );
    w.card.put("Rock.m3u8", b"stale");
    let text = match w.prepare().unwrap().plan.playlists.as_slice() {
        [PlaylistOp::Write { text, .. }] => text.clone(),
        other => panic!("{other:?}"),
    };
    w.card.put("Rock.m3u8", text.as_bytes());
    w.card.age();
    let p = w.prepare().unwrap();
    assert!(p.plan.is_empty(), "{:?}", p.plan.playlists);
    w.card.assert_untouched();

    // With a playlists folder, the root's file is not the playlist's.
    w.env.cfg.card.playlists_dir = "Lists".into();
    let p = w.prepare().unwrap();
    assert_eq!(p.plan.unmanaged.playlists, ["Rock.m3u8"]);
    match p.plan.playlists.as_slice() {
        [PlaylistOp::Write { path, text, .. }] => {
            assert_eq!(path, "Lists/Rock.m3u8");
            assert!(text.contains("../Music/t1.mp3\n"), "{text}");
        }
        other => panic!("{other:?}"),
    }
}

// Prevents: a tombstone not reaching the planner (the card file of a deleted
// playlist would stay for ever).
#[test]
fn prepare_deletes_the_card_file_of_a_deleted_playlist() {
    let mut w = World::new(1);
    w.load_playlist("Gone", &["t1.mp3"]);
    let dev = w.adopt().id;
    let m = w.synced(dev, "t1.mp3");
    let gone = w.env.playlist_id("Gone").unwrap();
    w.map(
        dev,
        &[m],
        &[PlaylistMapping {
            playlist_id: gone,
            card_name: "Gone.m3u8".into(),
        }],
    );
    w.card.put("Gone.m3u8", b"#EXTM3U\n");
    fs::remove_file(w.env.playlist_file("Gone")).unwrap();
    playlists::sync_from_disk(w.conn(), &w.env.cfg, false).unwrap();
    let p = w.prepare().unwrap();
    assert_eq!(p.plan.playlists.len(), 1);
    assert!(matches!(
        &p.plan.playlists[0],
        PlaylistOp::Delete { playlist_id: Some(id), path, .. } if *id == gone && path == "Gone.m3u8"
    ));
}

// Prevents: remove_unmanaged from the config being ignored.
#[test]
fn prepare_honours_remove_unmanaged() {
    let mut w = World::new(1);
    let dev = w.adopt().id;
    let m = w.synced(dev, "t1.mp3");
    w.map(dev, &[m], &[]);
    w.card.copy("Other/o.mp3", Kind::Mp3, 9);
    assert!(w.prepare().unwrap().plan.remove.is_empty());
    w.env.cfg.sync.remove_unmanaged = true;
    let p = w.prepare().unwrap();
    assert_eq!(p.plan.remove.len(), 1);
    assert_eq!(p.plan.remove[0].path, "Other/o.mp3");
    assert_eq!(p.plan.remove[0].reason, RemoveReason::Unmanaged);
}

// --------------------------------------------------------------- describe

fn track_row(w: &World, rel: &str) -> TrackRow {
    w.track(rel)
}

/// A `Prepared` around a hand-built plan, for `describe`.
fn prepared(w: &World, plan: Plan, lib: &[TrackRow], mapped_lists: &[i64]) -> Prepared {
    Prepared {
        plan,
        device: DeviceRow {
            id: 1,
            device_key: "usb:SER1".into(),
            card_id: None,
            name: Some("My X3".into()),
            model: None,
            last_sync_at: None,
        },
        card: w.card.root.clone(),
        contents: Default::default(),
        tracks_map: (0..10)
            .map(|i| TrackMapping {
                track_id: 1000 + i,
                card_path: format!("Music/m{i}.mp3"),
                synced_hash: [0; 32],
                synced_size: 1,
            })
            .collect(),
        playlists_map: mapped_lists
            .iter()
            .map(|&id| PlaylistMapping {
                playlist_id: id,
                card_name: format!("{id}.m3u8"),
            })
            .collect(),
        library: lib
            .iter()
            .cloned()
            .map(|t| (t.id, t))
            .collect::<HashMap<_, _>>(),
        scope: (scope(Kind_::All, &[]), false),
    }
}

fn line_with<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line with {needle:?} in:\n{text}"))
}

fn busy_plan(w: &World) -> (Plan, Vec<TrackRow>) {
    let t1 = track_row(w, "t1.mp3");
    let rm = |path: &str, reason, id| RemoveOp {
        path: path.into(),
        track_id: id,
        size: 100,
        reason,
    };
    let write = |id, path: &str, entries| PlaylistOp::Write {
        playlist_id: id,
        path: path.into(),
        text: String::new(),
        entries,
    };
    let plan = Plan {
        copy: vec![
            CopyOp {
                track_id: t1.id,
                from: t1.path.clone(),
                to: "Music/t1.mp3".into(),
                size: 1_500,
            },
            CopyOp {
                track_id: 77,
                from: "/lib/other.mp3".into(),
                to: "Music/other.mp3".into(),
                size: 500,
            },
        ],
        replace: vec![ReplaceOp {
            track_id: 5,
            from: "/lib/r.mp3".into(),
            to: "Music/r.mp3".into(),
            size: 10,
            old_size: 9,
            reason: ReplaceReason::ChangedOnCard,
        }],
        moves: vec![MoveOp {
            track_id: 6,
            from: "Old/m.mp3".into(),
            to: "Music/m.mp3".into(),
        }],
        remove: vec![
            rm("Music/gone.mp3", RemoveReason::MissingFromLibrary, Some(7)),
            rm("Music/out.mp3", RemoveReason::OutOfScope, Some(8)),
            rm("Music/old.mp3", RemoveReason::Replaced, Some(9)),
            rm("Stray/u.mp3", RemoveReason::Unmanaged, None),
        ],
        playlists: vec![
            PlaylistOp::Delete {
                playlist_id: Some(4),
                path: "Dead.m3u8".into(),
                reason: fiio_core::plan::PlaylistDeleteReason::FileDeleted,
            },
            PlaylistOp::Rename {
                playlist_id: 2,
                from: "Old.m3u8".into(),
                to: "New.m3u8".into(),
            },
            write(1, "Fresh.m3u8", 3),
            write(2, "New.m3u8", 1),
        ],
        skipped: vec![Skipped {
            track_id: t1.id,
            path: "/lib/drm.m4a".into(),
            reason: SkipReason::FairPlay,
        }],
        stale_mappings: vec![
            StaleMapping {
                track_id: t1.id,
                card_path: "Music/lost.mp3".into(),
            },
            StaleMapping {
                track_id: 4242,
                card_path: "Music/lost2.mp3".into(),
            },
        ],
        space: SpaceCheck {
            needed: 1_000,
            peak: 2_000,
            device: Some(Space {
                capacity: 8_000_000_000,
                free: 3_000_000,
            }),
            fits: true,
        },
        ..Plan::default()
    };
    (plan, vec![t1])
}

// Prevents: the summary lying about the plan: each count on its line, plural
// and singular, new versus changed playlists, the space at the peak.
#[test]
fn describe_summarises_every_kind_of_change() {
    let w = World::new(1);
    let (plan, lib) = busy_plan(&w);
    let p = prepared(&w, plan, &lib, &[2]);
    let text = sync::describe(&p, false);

    assert!(text.starts_with("My X3 (usb:SER1)\n"), "{text}");
    let l = line_with(&text, "to copy (");
    assert!(l.contains("2 tracks") && l.contains("2.0 kB"), "{l}");
    assert!(line_with(&text, "to move").contains("1 track"));
    assert!(!line_with(&text, "to move").contains("1 tracks"));
    assert!(line_with(&text, "to copy again").contains("1 track"));
    // Removals: missing + out of scope only.
    assert!(line_with(&text, "to remove").contains("2 tracks"));
    let l = line_with(&text, "playlists:");
    assert!(
        l.contains("1 new")
            && l.contains("1 changed")
            && l.contains("1 renamed")
            && l.contains("1 removed"),
        "{l}"
    );
    assert!(line_with(&text, "to remove (remove_unmanaged)").contains('1'));
    let l = line_with(&text, "unmanaged on the card");
    assert!(l.contains("0 files") && l.contains("0 playlists"), "{l}");
    let l = line_with(&text, "skipped");
    assert!(
        l.contains("1 track") && l.contains("0 playlist entries"),
        "{l}"
    );
    assert!(line_with(&text, "no longer on the card").contains('2'));
    // Peak and net differ: both are told, and the free space.
    let l = line_with(&text, "space");
    assert!(
        l.contains("peak") && l.contains("2.0 kB") && l.contains("1.0 kB"),
        "{l}"
    );
    assert!(l.contains("3.0 MB free"), "{l}");
    assert!(!text.contains("DOES NOT FIT"));
    assert!(!text.contains("allow-mass-removal"));
    assert!(!text.contains("=="), "no sections without verbose:\n{text}");
}

// Prevents: a plan that does not fit, or removes too much, going unnoticed.
#[test]
fn describe_warns_when_the_plan_does_not_fit_or_removes_too_much() {
    let w = World::new(1);
    let (mut plan, lib) = busy_plan(&w);
    plan.space.fits = false;
    plan.space.peak = 500;
    plan.space.needed = -500;
    plan.space.device = None;
    // 10 managed tracks; 6 removals is more than five and more than a tenth.
    plan.remove = (0..6)
        .map(|i| RemoveOp {
            path: format!("Music/r{i}.mp3"),
            track_id: Some(i),
            size: 1,
            reason: RemoveReason::OutOfScope,
        })
        .collect();
    let p = prepared(&w, plan, &lib, &[]);
    let text = sync::describe(&p, false);
    assert!(text.contains("DOES NOT FIT"), "{text}");
    assert!(text.contains("--allow-mass-removal"), "{text}");
    let l = line_with(&text, "space");
    assert!(
        l.contains("frees") && l.contains("500 B") && !l.contains("free,"),
        "{l}"
    );

    // One fewer removal: below the threshold.
    let mut p = p;
    p.plan.remove.pop();
    assert!(!sync::describe(&p, false).contains("--allow-mass-removal"));

    // Same peak and net: one figure only.
    p.plan.space.peak = 700;
    p.plan.space.needed = 700;
    let l = line_with(&sync::describe(&p, false), "space").to_owned();
    assert!(l.contains("needs 700 B") && !l.contains("peak"), "{l}");
}

// Prevents: a verbose listing missing a section, naming the wrong files, or
// printing sections that are empty.
#[test]
fn verbose_describe_lists_every_change_in_its_section() {
    let w = World::new(1);
    let (plan, lib) = busy_plan(&w);
    let t1_path = lib[0].path.display().to_string();
    let p = prepared(&w, plan, &lib, &[2]);
    let text = sync::describe(&p, true);

    let section = |title: &str| -> Vec<&str> {
        let mut it = text
            .lines()
            .skip_while(|l| !l.starts_with(&format!("== {title}")));
        let head = it
            .next()
            .unwrap_or_else(|| panic!("no section {title:?}:\n{text}"));
        let n: usize = head
            .rsplit_once('(')
            .and_then(|(_, n)| n.trim_end_matches(')').parse().ok())
            .unwrap();
        let body: Vec<&str> = it
            .take_while(|l| l.starts_with("  "))
            .map(str::trim)
            .collect();
        assert_eq!(body.len(), n, "{title}: count in the heading");
        body
    };
    let copy = section("Copy");
    assert_eq!(copy[0], format!("{t1_path} -> Music/t1.mp3"));
    assert_eq!(copy[1], "/lib/other.mp3 -> Music/other.mp3");
    assert_eq!(section("Move"), ["Old/m.mp3 -> Music/m.mp3"]);
    let again = section("Copy again");
    assert!(
        again[0].starts_with("Music/r.mp3") && again[0].contains("card"),
        "{again:?}"
    );
    let remove = section("Remove");
    assert_eq!(remove.len(), 4);
    let reason = |path: &str| {
        remove
            .iter()
            .find(|l| l.starts_with(path))
            .unwrap()
            .to_string()
    };
    let reasons: Vec<String> = [
        "Music/gone.mp3",
        "Music/out.mp3",
        "Music/old.mp3",
        "Stray/u.mp3",
    ]
    .iter()
    .map(|p| reason(p))
    .collect();
    assert_eq!(
        reasons
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        4,
        "four different reasons: {reasons:?}"
    );
    let lists = section("Playlists");
    assert_eq!(lists.len(), 4);
    assert!(lists[0].starts_with("remove Dead.m3u8"));
    assert_eq!(lists[1], "rename Old.m3u8 to New.m3u8");
    assert!(lists[2].starts_with("write Fresh.m3u8") && lists[2].contains("3 entries"));
    assert!(lists[3].contains("1 entry") && !lists[3].contains("1 entries"));
    let skipped = section("Skipped tracks");
    assert!(skipped[0].starts_with("/lib/drm.m4a"), "{skipped:?}");
    let stale = section("Mapped files no longer on the card");
    assert_eq!(stale[0], format!("Music/lost.mp3 ({t1_path})"));
    assert_eq!(
        stale[1], "Music/lost2.mp3 (track 4242)",
        "an unknown track by its id"
    );
    for empty in [
        "Unmanaged files kept",
        "Unmanaged playlists kept",
        "Skipped playlist entries",
    ] {
        assert!(
            !text.contains(&format!("== {empty}")),
            "{empty} is empty:\n{text}"
        );
    }
}

// Prevents: the unmanaged and skipped-entry sections losing their lines.
#[test]
fn verbose_describe_lists_unmanaged_files_and_skipped_entries() {
    let w = World::new(1);
    let mut plan = Plan::default();
    plan.unmanaged.files = names(&["A/a.mp3", "b.flac"]);
    plan.unmanaged.playlists = names(&["Theirs.m3u8"]);
    plan.entries_skipped = vec![
        plan::EntrySkipped {
            playlist: "Rock".into(),
            position: 3,
            reason: plan::EntryReason::Unresolved,
        },
        plan::EntrySkipped {
            playlist: "Rock".into(),
            position: 9,
            reason: plan::EntryReason::NotOnCard,
        },
    ];
    let p = prepared(&w, plan, &[], &[]);
    let text = sync::describe(&p, true);
    assert!(line_with(&text, "unmanaged on the card").contains("2 files, 1 playlist"));
    assert!(line_with(&text, "skipped").contains("0 tracks, 2 playlist entries"));
    assert!(
        text.contains("== Unmanaged files kept on the card (2)\n  A/a.mp3\n  b.flac\n"),
        "{text}"
    );
    assert!(
        text.contains("== Unmanaged playlists kept on the card (1)\n  Theirs.m3u8\n"),
        "{text}"
    );
    assert!(line_with(&text, "Rock #3").starts_with("  Rock #3"));
    assert!(text.contains("Rock #9"));
    // Without verbose: nothing of the lists.
    let short = sync::describe(&p, false);
    assert!(
        !short.contains("A/a.mp3") && !short.contains("Rock #3"),
        "{short}"
    );
}

// Prevents: a name-less device printing nothing for its name.
#[test]
fn describe_names_an_unnamed_device_x3() {
    let w = World::new(1);
    let mut p = prepared(&w, Plan::default(), &[], &[]);
    p.device.name = None;
    assert!(sync::describe(&p, false).starts_with("X3 (usb:SER1)\n"));
}

// ------------------------------------------------------- the device store

// Prevents: scope not surviving a round trip, names kept after a narrower
// set, names kept for `all`, and one device's scope leaking into another.
#[test]
fn scope_round_trips_through_the_store() {
    let w = World::new(1);
    let a = devices::upsert_device(w.conn(), "usb:A", None, None).unwrap();
    let b = devices::upsert_device(w.conn(), "usb:B", None, None).unwrap();
    assert_eq!(
        devices::scope(w.conn(), a).unwrap(),
        None,
        "new devices follow [sync]"
    );

    let only = scope(Kind_::Playlists, &["Zed", "Alpha", "Zed"]);
    devices::set_scope(w.conn(), a, Some(&only)).unwrap();
    assert_eq!(
        devices::scope(w.conn(), a).unwrap(),
        Some(scope(Kind_::Playlists, &["Alpha", "Zed"])),
        "sorted, without duplicates"
    );
    assert_eq!(devices::scope(w.conn(), b).unwrap(), None);

    devices::set_scope(w.conn(), b, Some(&scope(Kind_::Except, &["Q"]))).unwrap();
    devices::set_scope(w.conn(), a, Some(&scope(Kind_::Except, &["Only"]))).unwrap();
    assert_eq!(
        devices::scope(w.conn(), a).unwrap(),
        Some(scope(Kind_::Except, &["Only"])),
        "the old names are replaced, not added to"
    );
    assert_eq!(
        devices::scope(w.conn(), b).unwrap(),
        Some(scope(Kind_::Except, &["Q"]))
    );

    // `all` drops names it was given.
    devices::set_scope(w.conn(), a, Some(&scope(Kind_::All, &["Ignored"]))).unwrap();
    assert_eq!(
        devices::scope(w.conn(), a).unwrap(),
        Some(scope(Kind_::All, &[]))
    );
    let rows: i64 = w
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM device_scope_playlists WHERE device_id = ?1",
            [a],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0);

    devices::set_scope(w.conn(), a, None).unwrap();
    assert_eq!(devices::scope(w.conn(), a).unwrap(), None);
    assert_eq!(
        devices::scope(w.conn(), b).unwrap(),
        Some(scope(Kind_::Except, &["Q"])),
        "the other device is untouched"
    );
}

// Prevents: an unknown scope value in the index being read as a default (it
// would sync the wrong part of the library).
#[test]
fn an_unknown_scope_value_is_an_error_not_a_default() {
    let w = World::new(1);
    let a = devices::upsert_device(w.conn(), "usb:A", None, None).unwrap();
    w.conn()
        .execute("UPDATE devices SET scope = 'bogus' WHERE id = ?1", [a])
        .unwrap();
    match devices::scope(w.conn(), a) {
        Err(Error::BadScope(m)) => assert!(m.contains("bogus"), "{m}"),
        other => panic!("{other:?}"),
    }
    // And `prepare` and `device_scope` pass it on.
    assert!(matches!(
        sync::device_scope(w.conn(), &w.env.cfg, a),
        Err(Error::BadScope(_))
    ));
}

// Prevents: the device list missing a device or out of order.
#[test]
fn devices_lists_every_device_in_creation_order() {
    let w = World::new(1);
    assert!(devices::devices(w.conn()).unwrap().is_empty());
    let b = devices::upsert_device(w.conn(), "usb:B", Some("Bee"), Some("m")).unwrap();
    let a = devices::upsert_device(w.conn(), "usb:A", None, None).unwrap();
    devices::set_card_id(w.conn(), a, Some("cid")).unwrap();
    let all = devices::devices(w.conn()).unwrap();
    assert_eq!(all.iter().map(|d| d.id).collect::<Vec<_>>(), [b, a]);
    assert_eq!(all[0].device_key, "usb:B");
    assert_eq!(all[0].name.as_deref(), Some("Bee"));
    assert_eq!(all[0].model.as_deref(), Some("m"));
    assert_eq!(all[1].card_id.as_deref(), Some("cid"));
    assert_eq!(all[1], devices::device(w.conn(), "usb:A").unwrap().unwrap());
}

// Prevents: library_playlists mixing up positions, holes and members: an
// empty playlist has no member, an unresolved entry is a `None` member in
// place, a deleted playlist (tombstone) is not listed.
#[test]
fn library_playlists_resolve_members_in_order() {
    let w = World::new(3);
    let id = |rel: &str| w.track(rel).id;
    let (t1, t2, t3) = (id("t1.mp3"), id("t2.mp3"), id("t3.mp3"));
    let missing = w.env.music.join("not-indexed.mp3");
    w.env.write_playlist(
        "B mixed",
        &format!(
            "{}\n{}\n{}\n{}\n",
            w.env.music.join("t3.mp3").display(),
            missing.display(),
            w.env.music.join("t1.mp3").display(),
            w.env.music.join("t3.mp3").display(),
        ),
    );
    w.env.write_playlist("A empty", "#EXTM3U\n");
    w.env.write_playlist(
        "C dead",
        &format!("{}\n", w.env.music.join("t2.mp3").display()),
    );
    playlists::sync_from_disk(w.conn(), &w.env.cfg, true).unwrap();

    let got = devices::library_playlists(w.conn()).unwrap();
    let flat: Vec<(&str, &[Option<i64>])> = got
        .iter()
        .map(|p| (p.name.as_str(), p.members.as_slice()))
        .collect();
    assert_eq!(
        flat,
        [
            ("A empty", &[][..]),
            ("B mixed", &[Some(t3), None, Some(t1), Some(t3)][..]),
            ("C dead", &[Some(t2)][..]),
        ]
    );
    assert_eq!(got[1].id, w.env.playlist_id("B mixed").unwrap());

    // Delete C's file with a device mapping it: a tombstone, not listed.
    let dev = w.adopt().id;
    let c = w.env.playlist_id("C dead").unwrap();
    w.map(
        dev,
        &[],
        &[PlaylistMapping {
            playlist_id: c,
            card_name: "C dead.m3u8".into(),
        }],
    );
    fs::remove_file(w.env.playlist_file("C dead")).unwrap();
    playlists::sync_from_disk(w.conn(), &w.env.cfg, false).unwrap();
    let got = devices::library_playlists(w.conn()).unwrap();
    assert_eq!(
        got.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        ["A empty", "B mixed"]
    );
    assert_eq!(
        store::removed_playlists(w.conn()).unwrap(),
        [(c, "C dead".to_owned())]
    );
}

// Prevents: the unit being chosen before rounding ("1000.0 kB"), and the
// rounding boundary moving.
#[test]
fn bytes_choose_the_unit_after_rounding() {
    assert_eq!(sync::bytes(999_949), "999.9 kB");
    assert_eq!(sync::bytes(999_950), "1.0 MB");
    assert_eq!(sync::bytes(999_999), "1.0 MB");
    assert_eq!(sync::bytes(999_949_999), "999.9 MB");
    assert_eq!(sync::bytes(999_999_999), "1.0 GB");
    assert_eq!(sync::bytes(1_000_000_000), "1.0 GB");
    assert_eq!(sync::bytes(12_345_000_000_000), "12345.0 GB");
}

// Prevents: another card (it holds songs, and the device recorded a card id)
// being reported as "nothing matched" instead of a wrong card.
#[test]
fn a_different_card_with_audio_is_a_wrong_card_not_nothing_matched() {
    let mut w = World::new(1);
    let dev = w.adopt().id;
    devices::set_card_id(w.conn(), dev, Some("c1")).unwrap();
    w.card.copy("x.mp3", Kind::Mp3, 9);
    w.card.age();
    assert!(matches!(w.prepare().err(), Some(Error::WrongCard(k)) if k == w.key()),);
    w.card.assert_untouched();
}

// Prevents: the summary counting move operations: a swap goes through a
// hidden hop (3 operations) but moves 2 tracks.
#[test]
fn describe_counts_tracks_that_move_not_move_operations() {
    let w = World::new(1);
    let mv = |track_id, from: &str, to: &str| MoveOp {
        track_id,
        from: from.into(),
        to: to.into(),
    };
    let plan = Plan {
        moves: vec![
            mv(2, "Music/a.mp3", "Music/.fiio-tunes-move-1.move"),
            mv(1, "Music/b.mp3", "Music/a.mp3"),
            mv(2, "Music/.fiio-tunes-move-1.move", "Music/b.mp3"),
        ],
        ..Plan::default()
    };
    let p = prepared(&w, plan, &[], &[]);
    let text = sync::describe(&p, false);
    let l = line_with(&text, "to move");
    assert!(l.contains("2 tracks"), "{l}");
}
