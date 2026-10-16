//! `exec::check` and `exec::run` (technical document, 11.2, with the
//! decisions taken in F5): the playlists-only executor on cards that are
//! temp folders (`device.allow_folder`). The data folder (backups) is a
//! temp folder too; nothing here can reach the real X3 or the real home.

mod common;
mod rig;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;

use common::Kind;
use fiio_core::exec::{self, Failed, Options};
use fiio_core::plan::{
    PlaylistDeleteReason, PlaylistOp, RemoveOp, RemoveReason, Space, SpaceCheck,
};
use fiio_core::store;
use fiio_core::store::devices::{self, PlaylistMapping};
use fiio_core::sync::Prepared;
use fiio_core::{Error, backup, playlists};
use rig::Rig;

/// Three tracks; `t1` adopted at `Music/t1.mp3` and `t2` at `Old/t2.mp3`
/// (outside Music), `t3` not on the card; a library playlist Rock of the
/// three; unmanaged files of every kind on the card. The device has no
/// manifest and no card id: the next sync is the first.
fn first_world() -> Rig {
    let r = Rig::new(3);
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
    r.adopt_file(dev, "t2.mp3", "Old/t2.mp3");
    r.playlist("Rock", &["t1.mp3", "t2.mp3", "t3.mp3"]);
    r.put("Other.m3u8", b"#EXTM3U\nmine\n");
    r.put("sub/theirs.m3u8", b"#EXTM3U\ntheirs\n");
    r.put("zz-probe-01.m3u8", b"#EXTM3U\nprobe\n");
    r.put("Music/zz-probe/01 Probe.mp3", b"probe audio");
    r.put("Stray/s.flac", b"stray");
    r
}

/// `first_world` after its first sync.
fn synced_world() -> Rig {
    let r = first_world();
    r.sync();
    r
}

fn locations(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(String::from)
        .collect()
}

fn unmanaged() -> [&'static str; 5] {
    [
        "Other.m3u8",
        "sub/theirs.m3u8",
        "zz-probe-01.m3u8",
        "Music/zz-probe/01 Probe.mp3",
        "Stray/s.flac",
    ]
}

/// Bytes and modification time of the files at `paths`.
fn stamps(r: &Rig, paths: &[&str]) -> BTreeMap<String, (Vec<u8>, std::time::SystemTime)> {
    paths
        .iter()
        .map(|p| {
            let m = fs::metadata(r.card.join(p)).unwrap();
            ((*p).to_owned(), (r.read(p), m.modified().unwrap()))
        })
        .collect()
}

fn failed(res: Result<exec::Done, Failed>) -> Failed {
    match res {
        Ok(d) => panic!("expected a failure, got {d:?}"),
        Err(f) => f,
    }
}

// ----------------------------------------------------------------- check

/// A prepared playlists-only plan of an adopted empty card, to be
/// reshaped by the test.
fn blank() -> (Rig, Prepared) {
    let r = Rig::new(3);
    r.adopt_empty();
    let p = r.prepare(true).unwrap();
    (r, p)
}

fn delete_ops(n: usize, reason: PlaylistDeleteReason) -> Vec<PlaylistOp> {
    (0..n)
        .map(|i| PlaylistOp::Delete {
            playlist_id: Some(i as i64),
            path: format!("P{i}.m3u8"),
            reason,
        })
        .collect()
}

fn mappings(n: usize) -> Vec<PlaylistMapping> {
    (0..n)
        .map(|i| PlaylistMapping {
            playlist_id: i as i64,
            card_name: format!("P{i}.m3u8"),
        })
        .collect()
}

// Prevents: a plan with track changes being refused now that the
// executor applies them.
#[test]
fn check_accepts_track_changes() {
    let r = Rig::new(3);
    r.adopt_empty();
    let full = r.prepare(false).unwrap();
    assert_eq!(full.plan.copy.len(), 3);
    assert!(exec::check(&full, Options::default()).is_ok());
}

// Prevents: a plan that does not fit being applied, or the error giving
// the wrong figures (needed at the peak, free on the card; none known
// counts as nothing free).
#[test]
fn check_refuses_a_plan_that_does_not_fit_with_the_figures() {
    let (_r, mut p) = blank();
    p.plan.space = SpaceCheck {
        needed: 1,
        peak: 1_234,
        device: Some(Space {
            capacity: 10_000,
            free: 77,
        }),
        fits: false,
    };
    match exec::check(&p, Options::default()) {
        Err(Error::NoSpace {
            needed: 1_234,
            free: 77,
        }) => {}
        other => panic!("{other:?}"),
    }
    // The option is about removals, not about room.
    assert!(matches!(
        exec::check(
            &p,
            Options {
                allow_mass_removal: true
            }
        ),
        Err(Error::NoSpace { .. })
    ));
    p.plan.space.device = None;
    assert!(matches!(
        exec::check(&p, Options::default()),
        Err(Error::NoSpace {
            needed: 1_234,
            free: 0
        })
    ));
    p.plan.space.fits = true;
    assert!(exec::check(&p, Options::default()).is_ok());
}

// Prevents: the playlist mass-removal guard moving: it needs more than five
// removals AND more than a tenth of the managed playlists; only deletes of
// playlists that left the library or the scope count; the option lifts it.
#[test]
fn check_refuses_mass_removal_of_playlists_at_the_thresholds() {
    let mass = |n: usize, of: usize, reason, opts| {
        let (_r, mut p) = blank();
        p.plan.playlists = delete_ops(n, reason);
        p.playlists_map = mappings(of);
        exec::check(&p, opts)
    };
    let none = Options::default();
    let allow = Options {
        allow_mass_removal: true,
    };
    use PlaylistDeleteReason::{FileDeleted, OutOfScope, Unmanaged};
    for reason in [FileDeleted, OutOfScope, Unmanaged] {
        assert!(
            mass(5, 10, reason, none).is_ok(),
            "five is not more than five"
        );
        match mass(6, 10, reason, none) {
            Err(Error::MassRemoval(m)) => {
                assert!(m.contains('6') && m.contains("10"), "{m}");
                assert!(m.contains("playlists"), "{m}");
            }
            other => panic!("{other:?}"),
        }
        assert!(
            mass(6, 60, reason, none).is_ok(),
            "a tenth exactly is not more"
        );
        assert!(matches!(
            mass(6, 59, reason, none),
            Err(Error::MassRemoval(_))
        ));
        assert!(mass(6, 1_000, reason, none).is_ok());
        assert!(mass(6, 10, reason, allow).is_ok());
    }
}

// Prevents: the track branch of the mass-removal guard being skipped: six
// managed tracks removed out of none left is refused without the option
// and accepted with it.
#[test]
fn removing_many_tracks_needs_the_option() {
    let (_r, mut p) = blank();
    p.plan.remove = (0..6)
        .map(|i| RemoveOp {
            path: format!("Music/{i}.mp3"),
            track_id: Some(i),
            size: 1,
            reason: RemoveReason::OutOfScope,
        })
        .collect();
    assert!(matches!(
        exec::check(&p, Options::default()),
        Err(Error::MassRemoval(_))
    ));
    let opts = Options {
        allow_mass_removal: true,
    };
    assert!(exec::check(&p, opts).is_ok());
}

// ------------------------------------------------------------ refusals

// Prevents: run writing to a folder that is not allowed as a card: nothing
// may be written, not even a backup, and the error says so.
#[test]
fn run_refuses_a_card_that_is_not_writable_before_anything_else() {
    let r = first_world();
    let p = r.prepare(true).unwrap();
    let before = r.snapshot();
    let mut cfg = r.env.cfg.clone();
    cfg.device.allow_folder = false;
    let f = failed(exec::run(
        r.conn(),
        &cfg,
        &r.paths(),
        &r.found(),
        &p,
        Options::default(),
        &|_| {},
        &std::sync::atomic::AtomicBool::new(false),
    ));
    assert!(matches!(f.error, Error::NotWritable(_)), "{}", f.error);
    assert!(f.backup.is_none());
    assert_eq!(r.snapshot(), before);
    assert!(r.backup_entries().is_empty());
    assert!(r.device().card_id.is_none());
}

// Prevents: a plan refused by check still writing a backup or a manifest
// when run is called: every refusal leaves the card and the data folder as
// they were.
#[test]
fn run_applies_the_checks_and_writes_nothing_when_they_fail() {
    let r = Rig::new(3);
    r.adopt_empty();
    let before = r.snapshot();
    let mut full = r.prepare(false).unwrap();
    full.plan.space.fits = false;
    let f = failed(r.run(&full, Options::default()));
    assert!(matches!(f.error, Error::NoSpace { .. }), "{}", f.error);
    assert!(f.backup.is_none());
    assert_eq!(r.snapshot(), before);
    assert!(r.backup_entries().is_empty());
    assert!(r.device().card_id.is_none() && r.device().last_sync_at.is_none());
}

// Prevents: the mass-removal guard being checked only in `check`: run must
// refuse six of ten managed playlists deleted, writing nothing, and apply
// the same plan with the option.
#[test]
fn run_refuses_mass_removal_of_playlists_unless_allowed() {
    let r = Rig::new(1);
    r.adopt_empty();
    for i in 1..=10 {
        r.playlist(&format!("P{i:02}"), &["t1.mp3"]);
    }
    r.sync();
    assert_eq!(
        devices::playlist_mappings(r.conn(), r.device().id)
            .unwrap()
            .len(),
        10
    );
    for i in 1..=6 {
        playlists::delete(r.conn(), &r.env.cfg, &format!("P{i:02}")).unwrap();
    }
    let p = r.prepare(true).unwrap();
    let before = r.snapshot();
    let backups = r.backups().len();
    let f = failed(r.run(&p, Options::default()));
    assert!(matches!(f.error, Error::MassRemoval(_)), "{}", f.error);
    assert!(f.backup.is_none());
    assert_eq!(r.snapshot(), before);
    assert_eq!(r.backups().len(), backups);
    assert_eq!(
        devices::playlist_mappings(r.conn(), r.device().id)
            .unwrap()
            .len(),
        10
    );

    let done = r
        .run(
            &p,
            Options {
                allow_mass_removal: true,
            },
        )
        .unwrap();
    assert_eq!(done.deleted, 6);
    let left: Vec<String> = devices::playlist_mappings(r.conn(), r.device().id)
        .unwrap()
        .into_iter()
        .map(|m| m.card_name)
        .collect();
    assert_eq!(left, ["P07.m3u8", "P08.m3u8", "P09.m3u8", "P10.m3u8"]);
    assert!(!r.exists("P01.m3u8") && r.exists("P07.m3u8"));
}

// ------------------------------------------------------- the first sync

// Prevents: the first sync after an adoption without manifest losing
// something: the id (32 hex digits, in the index and in the manifest), the
// manifest content (version, key, tracks with hex hashes, playlists), the
// playlist naming files where they are (Old/ is outside Music), the
// mappings and last_sync_at, the files it must not touch, and the backup
// holding the state before.
#[test]
fn the_first_sync_gives_the_card_its_id_and_writes_manifest_and_playlist() {
    let r = first_world();
    let before = r.snapshot();
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let tracks_before = devices::track_mappings(r.conn(), r.device().id).unwrap();
    let p = r.prepare(true).unwrap();
    let done = r.run(&p, Options::default()).unwrap();
    assert!(done.first);
    assert_eq!((done.written, done.renamed, done.deleted), (1, 0, 0));

    let dev = r.device();
    let id = dev.card_id.clone().expect("card id recorded");
    assert_eq!(id.len(), 32);
    assert!(
        id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
        "{id}"
    );
    assert!(dev.last_sync_at.unwrap() >= started);

    let m = r.card_manifest();
    assert_eq!(m["version"], 1);
    assert_eq!(m["device_key"], r.key());
    assert_eq!(m["card_id"], id);
    assert!(m["written_at"].as_i64().unwrap() >= started);
    let tracks = m["tracks"].as_array().unwrap();
    assert_eq!(tracks.len(), 2);
    for (t, (path, bytes)) in tracks.iter().zip([
        ("Music/t1.mp3", r.read("Music/t1.mp3")),
        ("Old/t2.mp3", r.read("Old/t2.mp3")),
    ]) {
        assert_eq!(t["path"], path);
        assert_eq!(t["size"], bytes.len());
        assert_eq!(t["hash"], blake3::hash(&bytes).to_hex().to_string());
    }
    assert_eq!(m["playlists"], serde_json::json!(["Rock.m3u8"]));

    assert_eq!(
        locations(&r.read("Rock.m3u8")),
        ["Music/t1.mp3", "Old/t2.mp3"]
    );
    assert_eq!(
        r.read("Rock.m3u8"),
        p.plan.expected.playlists[0].text.as_bytes()
    );

    let rock = r.env.playlist_id("Rock").unwrap();
    assert_eq!(
        devices::playlist_mappings(r.conn(), dev.id).unwrap(),
        [PlaylistMapping {
            playlist_id: rock,
            card_name: "Rock.m3u8".into()
        }]
    );
    assert_eq!(
        devices::track_mappings(r.conn(), dev.id).unwrap(),
        tracks_before
    );

    // Only the manifest and the playlist are new; nothing else changed.
    let after = r.snapshot();
    let added: Vec<&String> = after.keys().filter(|k| !before.contains_key(*k)).collect();
    assert_eq!(added, [".fiio-tunes/manifest.json", "Rock.m3u8"]);
    for (k, v) in &before {
        assert_eq!(&after[k], v, "{k} changed");
    }

    // The backup is the card before: no manifest, no id, every playlist file.
    let b = &r.backups()[0];
    assert_eq!(b.stamp, done.backup.as_ref().expect("a backup").stamp);
    assert_eq!(b.dir, done.backup.as_ref().expect("a backup").dir);
    let saved = backup::saved(b).unwrap();
    assert_eq!(saved.card_id, None);
    assert_eq!(saved.device_key, r.key());
    assert_eq!(saved.tracks.len(), 2);
    assert!(saved.playlists.is_empty());
    assert_eq!(backup::manifest(b).unwrap(), None);
    let held: Vec<String> = backup::playlists(b)
        .unwrap()
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    assert_eq!(held, ["Other.m3u8", "sub/theirs.m3u8", "zz-probe-01.m3u8"]);
    let plan_json: serde_json::Value =
        serde_json::from_slice(&fs::read(b.dir.join("plan.json")).unwrap()).unwrap();
    assert!(plan_json["playlists"].is_array());
    let work: serde_json::Value =
        serde_json::from_slice(&fs::read(b.dir.join("work.json")).unwrap()).unwrap();
    for k in ["remove", "moves", "copy", "replace"] {
        assert_eq!(work[k], serde_json::json!([]), "{k}");
    }
    let listing = fs::read_to_string(b.dir.join("listing.txt")).unwrap();
    assert!(listing.contains("Music/t1.mp3\t"), "{listing}");
    assert!(listing.contains("Other.m3u8\t"), "{listing}");
}

// Prevents: the card id being recorded only after the manifest is written
// (the manifest is built from the index, so it could not carry it), and a
// failed manifest write leaving the id in the index (the card would then
// look like another card on the next try). Provoked by a manifest folder
// that cannot be written to; a trigger logs every change of the id. The
// log shows the id set, then cleared; no playlist was written; the failure
// names a backup; and once the folder is writable a retry succeeds with
// the manifest carrying the id the index then holds.
#[test]
fn the_card_id_is_recorded_before_the_manifest_and_forgotten_if_it_fails() {
    for other_file in [true, false] {
        card_id_forgotten_after_failed_manifest(other_file);
    }
}

/// With `other_file`, the state folder holds another file and stays; without,
/// it is empty and goes with the manifest.
fn card_id_forgotten_after_failed_manifest(other_file: bool) {
    let r = first_world();
    let dir = r.card.join(".fiio-tunes");
    fs::create_dir(&dir).unwrap();
    if other_file {
        fs::write(dir.join("keep.txt"), b"kept").unwrap();
    }
    if !rig::readonly_dir_blocks_writes(&dir) {
        eprintln!(
            "SKIPPED: permissions do not stop this user (running as root?); this test checked nothing"
        );
        return;
    }
    r.conn()
        .execute_batch(
            "CREATE TABLE id_log (n INTEGER PRIMARY KEY, card_id TEXT);
             CREATE TRIGGER log_id AFTER UPDATE OF card_id ON devices
             BEGIN INSERT INTO id_log (card_id) VALUES (NEW.card_id); END;",
        )
        .unwrap();
    let log = |r: &Rig| -> Vec<Option<String>> {
        let mut st = r
            .conn()
            .prepare("SELECT card_id FROM id_log ORDER BY n")
            .unwrap();
        st.query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let p = r.prepare(true).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    let res = r.run(&p, Options::default());
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o755));
    let f = failed(res);
    assert!(matches!(f.error, Error::Io { .. }), "{}", f.error);
    assert!(f.backup.is_some());
    let entries = log(&r);
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert!(entries[0].is_some() && entries[1].is_none(), "{entries:?}");
    assert_eq!(r.device().card_id, None, "the id stayed in the index");
    assert!(
        !r.exists("Rock.m3u8"),
        "a playlist was written before the manifest"
    );
    assert!(!r.exists(".fiio-tunes/manifest.json"));
    assert_eq!(r.exists(".fiio-tunes/keep.txt"), other_file);
    assert_eq!(r.exists(".fiio-tunes"), other_file);
    assert!(r.device().last_sync_at.is_none());

    // Still the card adopted: the retry is a first sync again.
    let p = r.prepare(true).unwrap();
    let done = r.run(&p, Options::default()).unwrap();
    assert!(done.first);
    let id = r.device().card_id.expect("recorded");
    assert_eq!(r.card_manifest()["card_id"], id);
    let entries = log(&r);
    assert_eq!(entries.last().unwrap().as_deref(), Some(id.as_str()));
}

/// Runs the plan and checks nothing changed anywhere: no backup, no id, no
/// byte on the card, no mapping.
fn refused_first_sync(r: &Rig, p: &Prepared) -> Error {
    let before = r.snapshot();
    let tracks = devices::track_mappings(r.conn(), r.device().id).unwrap();
    let f = failed(r.run(p, Options::default()));
    assert!(f.backup.is_none(), "a backup was taken");
    assert_eq!(r.snapshot(), before, "the card was written");
    assert!(r.backup_entries().is_empty());
    let dev = r.device();
    assert!(dev.card_id.is_none() && dev.last_sync_at.is_none());
    assert_eq!(devices::track_mappings(r.conn(), dev.id).unwrap(), tracks);
    assert!(
        devices::playlist_mappings(r.conn(), dev.id)
            .unwrap()
            .is_empty()
    );
    f.error
}

// Prevents: the adopted files being trusted by size alone (a file replaced
// by another of the same length passes the listing), or by presence alone.
#[test]
fn the_first_sync_refuses_an_adopted_file_with_other_bytes_of_the_same_size() {
    let r = first_world();
    let p = r.prepare(true).unwrap();
    let mut bytes = r.read("Music/t1.mp3");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    r.put("Music/t1.mp3", &bytes);
    match refused_first_sync(&r, &p) {
        Error::CardChanged(m) => assert!(m.contains("Music/t1.mp3"), "{m}"),
        e => panic!("{e}"),
    }
}

// Prevents: a changed size, or an adopted file that left, going unnoticed.
#[test]
fn the_first_sync_refuses_an_adopted_file_that_changed_size_or_is_gone() {
    let r = first_world();
    let p = r.prepare(true).unwrap();
    let mut bytes = r.read("Old/t2.mp3");
    bytes.push(0);
    r.put("Old/t2.mp3", &bytes);
    match refused_first_sync(&r, &p) {
        Error::CardChanged(m) => assert!(m.contains("Old/t2.mp3"), "{m}"),
        e => panic!("{e}"),
    }
    fs::remove_file(r.card.join("Old/t2.mp3")).unwrap();
    match refused_first_sync(&r, &p) {
        Error::CardChanged(m) => assert!(m.contains("Old/t2.mp3"), "{m}"),
        e => panic!("{e}"),
    }
}

// Prevents: a card that gained a manifest since the plan (another copy of
// fiio-tunes synced it) being given a second identity.
#[test]
fn the_first_sync_refuses_a_manifest_that_appeared() {
    let r = first_world();
    let p = r.prepare(true).unwrap();
    let manifest = serde_json::json!({
        "version": 1, "device_key": r.key(), "card_id": "0123", "tracks": [], "playlists": []
    });
    r.put(".fiio-tunes/manifest.json", manifest.to_string().as_bytes());
    // Not the card adopted: the manifest names an id the index does not have.
    let before = r.snapshot();
    assert!(matches!(refused_first_sync(&r, &p), Error::WrongCard(k) if k == r.key()));
    assert_eq!(
        r.read(".fiio-tunes/manifest.json"),
        before[".fiio-tunes/manifest.json"]
    );
}

// Prevents: the hash read covering files adoption did not map: an
// unmanaged file may be of any size and content, and must not stop a sync.
#[test]
fn the_first_sync_does_not_read_unmanaged_audio() {
    let r = first_world();
    let p = r.prepare(true).unwrap();
    // Same size as before, other bytes: only the planned size is compared.
    r.put("Stray/s.flac", b"STRAY");
    let done = r.run(&p, Options::default()).unwrap();
    assert!(done.first);
    assert_eq!(r.read("Stray/s.flac"), b"STRAY");
}

// ------------------------------------------------------ later syncs

// Prevents: a card taken for the adopted one after its identity changed
// between the plan and the write (the card swapped while the prompt was
// open): another id, another key, or no manifest at all. Nothing is
// written, no backup is made.
#[test]
fn a_later_sync_refuses_a_card_whose_identity_changed_since_the_plan() {
    type Tamper = fn(&Rig);
    let tampers: [(&str, Tamper); 3] = [
        ("another card id", |r| {
            let mut m = r.card_manifest();
            m["card_id"] = "ffffffffffffffffffffffffffffffff".into();
            r.put(".fiio-tunes/manifest.json", m.to_string().as_bytes());
        }),
        ("another key", |r| {
            let mut m = r.card_manifest();
            m["device_key"] = "usb:other".into();
            r.put(".fiio-tunes/manifest.json", m.to_string().as_bytes());
        }),
        ("no manifest", |r| {
            fs::remove_file(r.card.join(".fiio-tunes/manifest.json")).unwrap();
        }),
    ];
    for (what, tamper) in tampers {
        let r = synced_world();
        r.playlist("Rock", &["t2.mp3", "t1.mp3"]);
        let p = r.prepare(true).unwrap();
        assert!(!p.plan.playlists.is_empty());
        tamper(&r);
        let before = r.snapshot();
        let last = r.device().last_sync_at;
        let backups = r.backups().len();
        let f = failed(r.run(&p, Options::default()));
        assert!(
            matches!(&f.error, Error::WrongCard(k) if *k == r.key()),
            "{what}: {}",
            f.error
        );
        assert!(f.backup.is_none(), "{what}");
        assert_eq!(r.snapshot(), before, "{what}: the card was written");
        assert_eq!(r.backups().len(), backups, "{what}");
        assert_eq!(r.device().last_sync_at, last, "{what}");
    }
}

// Prevents: the second sync losing a kind of operation, or touching what
// is not its own: deletes (tombstone), a rename that keeps its content, new
// and changed writes; the unmanaged audio and playlists (and a probe file)
// stay byte for byte with their modification times; mappings, the tombstone,
// last_sync_at, the manifest and the backup of the state before are right;
// and a third plan is empty.
#[test]
fn a_later_sync_deletes_renames_and_writes_and_leaves_the_rest_alone() {
    let r = first_world();
    r.playlist("Dead", &["t1.mp3"]);
    r.playlist("Mover", &["t2.mp3"]);
    r.sync();
    let first_snapshot = r.snapshot();
    let first_manifest = r.read(".fiio-tunes/manifest.json");
    let first_backups = r.backups().len();
    let keep = stamps(&r, &unmanaged());
    let mover_bytes = r.read("Mover.m3u8");

    playlists::delete(r.conn(), &r.env.cfg, "Dead").unwrap();
    playlists::rename(r.conn(), &r.env.cfg, "Mover", "Moved").unwrap();
    r.playlist("Rock", &["t2.mp3", "t1.mp3", "t3.mp3"]);
    r.playlist("New", &["t1.mp3"]);
    let tracks = devices::track_mappings(r.conn(), r.device().id).unwrap();

    let p = r.prepare(true).unwrap();
    let done = r.run(&p, Options::default()).unwrap();
    assert!(!done.first);
    assert_eq!((done.written, done.renamed, done.deleted), (2, 1, 1));

    assert!(!r.exists("Dead.m3u8") && !r.exists("Mover.m3u8"));
    assert_eq!(r.read("Moved.m3u8"), mover_bytes);
    assert_eq!(
        locations(&r.read("Rock.m3u8")),
        ["Old/t2.mp3", "Music/t1.mp3"]
    );
    assert_eq!(locations(&r.read("New.m3u8")), ["Music/t1.mp3"]);
    assert_eq!(stamps(&r, &unmanaged()), keep);

    let dev = r.device();
    let names: Vec<String> = devices::playlist_mappings(r.conn(), dev.id)
        .unwrap()
        .into_iter()
        .map(|m| m.card_name)
        .collect();
    assert_eq!(names, ["Moved.m3u8", "New.m3u8", "Rock.m3u8"]);
    assert!(
        store::removed_playlists(r.conn()).unwrap().is_empty(),
        "tombstone kept"
    );
    assert!(dev.last_sync_at.is_some());
    assert_eq!(devices::track_mappings(r.conn(), dev.id).unwrap(), tracks);
    assert_eq!(
        r.card_manifest()["playlists"],
        serde_json::json!(["Moved.m3u8", "New.m3u8", "Rock.m3u8"])
    );
    assert_eq!(r.card_manifest()["card_id"], dev.card_id.clone().unwrap());

    // The backup holds what the card had: managed and unmanaged playlists,
    // the manifest of the first sync, the record of its mappings.
    let backups = r.backups();
    assert_eq!(backups.len(), first_backups + 1);
    let b = backups.last().unwrap();
    assert_eq!(b.stamp, done.backup.as_ref().expect("a backup").stamp);
    let held: BTreeMap<String, Vec<u8>> = backup::playlists(b).unwrap().into_iter().collect();
    let want: BTreeMap<String, Vec<u8>> = first_snapshot
        .iter()
        .filter(|(k, _)| k.ends_with(".m3u8"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    assert_eq!(held, want);
    assert!(held.contains_key("Dead.m3u8") && held.contains_key("Other.m3u8"));
    assert_eq!(backup::manifest(b).unwrap(), Some(first_manifest));
    let saved = backup::saved(b).unwrap();
    assert_eq!(saved.card_id, dev.card_id);
    let saved_names: Vec<&str> = saved
        .playlists
        .iter()
        .map(|p| p.card_name.as_str())
        .collect();
    assert_eq!(saved_names, ["Dead.m3u8", "Mover.m3u8", "Rock.m3u8"]);

    // Nothing left to do, and checking says so without writing.
    let after = r.snapshot();
    let again = r.prepare(true).unwrap();
    assert!(again.plan.is_empty(), "{:?}", again.plan.playlists);
    assert_eq!(r.snapshot(), after);
}

// Prevents: a rename that only changes case being refused as an overwrite
// (on FAT the two names are one file), or leaving both names.
#[test]
fn a_case_only_playlist_rename_is_one_rename() {
    let r = synced_world();
    playlists::rename(r.conn(), &r.env.cfg, "Rock", "rock").unwrap();
    let bytes = r.read("Rock.m3u8");
    let p = r.prepare(true).unwrap();
    let done = r.run(&p, Options::default()).unwrap();
    assert_eq!((done.renamed, done.written, done.deleted), (1, 0, 0));
    let names: Vec<String> = fs::read_dir(&r.card)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.to_lowercase().starts_with("rock"))
        .collect();
    assert_eq!(names, ["rock.m3u8"]);
    assert_eq!(r.read("rock.m3u8"), bytes);
    assert_eq!(
        devices::playlist_mappings(r.conn(), r.device().id).unwrap()[0].card_name,
        "rock.m3u8"
    );
}

// Prevents: a swap of two playlist names (a cycle of renames) losing a
// file: the planner breaks it through a hidden name; the executor must run
// all three renames and leave no hidden file.
#[test]
fn a_swap_of_two_playlist_names_goes_through_the_hidden_hop() {
    let r = Rig::new(2);
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
    r.adopt_file(dev, "t2.mp3", "Music/t2.mp3");
    r.playlist("Alpha", &["t1.mp3"]);
    r.playlist("Beta", &["t2.mp3"]);
    r.sync();
    let (a, b) = (r.read("Alpha.m3u8"), r.read("Beta.m3u8"));
    assert_ne!(a, b);
    let (ida, idb) = (
        r.env.playlist_id("Alpha").unwrap(),
        r.env.playlist_id("Beta").unwrap(),
    );
    // The index says each playlist sits under the other's name, and the
    // card agrees.
    devices::replace_playlist_mappings(
        r.conn(),
        dev,
        &[
            PlaylistMapping {
                playlist_id: ida,
                card_name: "Beta.m3u8".into(),
            },
            PlaylistMapping {
                playlist_id: idb,
                card_name: "Alpha.m3u8".into(),
            },
        ],
    )
    .unwrap();
    r.put("Beta.m3u8", &a);
    r.put("Alpha.m3u8", &b);

    let p = r.prepare(true).unwrap();
    let renames = p
        .plan
        .playlists
        .iter()
        .filter(|o| matches!(o, PlaylistOp::Rename { .. }))
        .count();
    assert_eq!(renames, 3, "{:?}", p.plan.playlists);
    let done = r.run(&p, Options::default()).unwrap();
    assert_eq!((done.renamed, done.written), (3, 0));
    assert_eq!(r.read("Alpha.m3u8"), a);
    assert_eq!(r.read("Beta.m3u8"), b);
    assert!(
        r.snapshot().keys().all(|k| !k.contains("fiio-tunes-move")),
        "{:?}",
        r.snapshot().keys()
    );
    let names: BTreeMap<i64, String> = devices::playlist_mappings(r.conn(), dev)
        .unwrap()
        .into_iter()
        .map(|m| (m.playlist_id, m.card_name))
        .collect();
    assert_eq!(names[&ida], "Alpha.m3u8");
    assert_eq!(names[&idb], "Beta.m3u8");
    assert!(r.prepare(true).unwrap().plan.is_empty());
}

// Prevents: a rename landing on a file that is not the playlist itself
// (a file that appeared on the card since the plan): the file would be
// lost. The sync stops, names the backup and keeps the other file.
#[test]
fn a_rename_onto_another_existing_file_is_refused() {
    let r = synced_world();
    playlists::rename(r.conn(), &r.env.cfg, "Rock", "Pop").unwrap();
    let p = r.prepare(true).unwrap();
    r.put("Pop.m3u8", b"someone else's");
    let rock = r.read("Rock.m3u8");
    let last = r.device().last_sync_at;
    let f = failed(r.run(&p, Options::default()));
    assert!(
        matches!(&f.error, Error::WouldOverwrite(t) if t == "Pop.m3u8"),
        "{}",
        f.error
    );
    assert!(f.backup.is_some());
    assert_eq!(r.read("Pop.m3u8"), b"someone else's");
    assert_eq!(r.read("Rock.m3u8"), rock);
    assert_eq!(r.device().last_sync_at, last);
    assert_eq!(
        devices::playlist_mappings(r.conn(), r.device().id).unwrap()[0].card_name,
        "Rock.m3u8"
    );
}

// Prevents: a failed write being reported without the backup that lets the
// owner go back, and a half-done sync being recorded as done. A folder
// stands where a playlist is to be written.
#[test]
fn a_write_that_fails_names_the_backup_and_records_nothing() {
    let r = first_world();
    r.playlist("A", &["t1.mp3"]);
    r.playlist("B", &["t2.mp3"]);
    fs::create_dir(r.card.join("B.m3u8")).unwrap();
    let p = r.prepare(true).unwrap();
    let f = failed(r.run(&p, Options::default()));
    assert!(matches!(f.error, Error::Io { .. }), "{}", f.error);
    let b = f.backup.expect("backup named");
    assert!(b.dir.join("mappings.json").is_file());
    assert_eq!(backup::saved(&b).unwrap().card_id, None);
    // Operations are in name order: A was written before B failed.
    assert!(r.exists("A.m3u8"));
    assert!(r.card.join("B.m3u8").is_dir());
    let dev = r.device();
    assert!(dev.last_sync_at.is_none());
    assert!(
        devices::playlist_mappings(r.conn(), dev.id)
            .unwrap()
            .is_empty()
    );
}

// ------------------------------------------------------- verification

// Prevents: verification passing when the card holds an audio file the
// plan does not account for (here one that appeared after the plan), and
// the run being recorded as done. The failure carries the backup.
#[test]
fn an_extra_audio_file_fails_the_verification_and_nothing_is_recorded() {
    let r = first_world();
    let p = r.prepare(true).unwrap();
    r.put("Extra/new.mp3", b"appeared");
    let f = failed(r.run(&p, Options::default()));
    match &f.error {
        Error::VerifyFailed(m) => {
            assert!(m.contains("Extra/new.mp3"), "{m}");
            assert!(m.contains("not in the plan"), "{m}");
        }
        e => panic!("{e}"),
    }
    assert!(f.backup.is_some());
    let dev = r.device();
    assert!(dev.last_sync_at.is_none());
    assert!(
        devices::playlist_mappings(r.conn(), dev.id)
            .unwrap()
            .is_empty()
    );
}

fn verify_message(extra: usize) -> String {
    let r = first_world();
    let p = r.prepare(true).unwrap();
    for i in 0..extra {
        r.put(&format!("Extra/e{i:02}.mp3"), b"x");
    }
    match failed(r.run(&p, Options::default())).error {
        Error::VerifyFailed(m) => m,
        e => panic!("{e}"),
    }
}

// Prevents: a verification report that floods the terminal, or hides how
// much more there is: at most 20 differences, then "and N more".
#[test]
fn the_verification_report_has_at_most_twenty_lines_and_counts_the_rest() {
    let m = verify_message(20);
    assert_eq!(m.lines().count(), 20, "{m}");
    assert!(m.lines().all(|l| l.contains("not in the plan")), "{m}");

    let m = verify_message(21);
    let lines: Vec<&str> = m.lines().collect();
    assert_eq!(lines.len(), 21, "{m}");
    assert_eq!(lines[20], "and 1 more");
    assert!(lines[..20].iter().all(|l| l.contains("not in the plan")));

    let m = verify_message(25);
    let lines: Vec<&str> = m.lines().collect();
    assert_eq!(lines.len(), 21, "{m}");
    assert_eq!(lines[20], "and 5 more");
    assert!(lines[0].contains("Extra/e00.mp3"), "{m}");
    assert!(lines[19].contains("Extra/e19.mp3"), "{m}");
}

// Prevents: a managed playlist edited between the plan and the check going
// unnoticed (the plan found it up to date, so no write corrected it), and
// an unmanaged playlist that changed size being taken for untouched.
#[test]
fn a_playlist_changed_behind_the_plans_back_fails_the_verification() {
    let r = synced_world();
    r.playlist("Other new", &["t1.mp3"]);
    let p = r.prepare(true).unwrap();
    assert_eq!(
        p.plan.playlists.len(),
        1,
        "only the new playlist is written"
    );
    r.put("Rock.m3u8", b"#EXTM3U\nedited\n");
    r.put("Other.m3u8", b"#EXTM3U\nmine, and longer now\n");
    let f = failed(r.run(&p, Options::default()));
    match &f.error {
        Error::VerifyFailed(m) => {
            assert!(m.contains("Rock.m3u8: not the playlist written"), "{m}");
            assert!(
                m.contains("Other.m3u8") && m.contains("unmanaged file changed size"),
                "{m}"
            );
        }
        e => panic!("{e}"),
    }
    assert!(f.backup.is_some());
    // Failed before recording: the new playlist is not mapped.
    let names: Vec<String> = devices::playlist_mappings(r.conn(), r.device().id)
        .unwrap()
        .into_iter()
        .map(|m| m.card_name)
        .collect();
    assert_eq!(names, ["Rock.m3u8"]);
}

// ------------------------------------------------- playlists folder

// Prevents: the playlists folder being ignored by the executor: files in
// it, the manifest naming them with the folder, mappings holding the bare
// file name, locations relative to the folder.
#[test]
fn playlists_go_to_the_playlists_folder_when_there_is_one() {
    let mut r = first_world();
    r.env.cfg.card.playlists_dir = "Playlists".into();
    r.sync();
    assert!(!r.exists("Rock.m3u8"));
    assert_eq!(
        locations(&r.read("Playlists/Rock.m3u8")),
        ["../Music/t1.mp3", "../Old/t2.mp3"]
    );
    assert_eq!(
        r.card_manifest()["playlists"],
        serde_json::json!(["Playlists/Rock.m3u8"])
    );
    assert_eq!(
        devices::playlist_mappings(r.conn(), r.device().id).unwrap()[0].card_name,
        "Rock.m3u8"
    );
    assert!(r.prepare(true).unwrap().plan.is_empty());
}

// Prevents: the playlists-only sync copying a library track to the card
// (three tracks, all in a playlist, none on the card), end to end: no audio
// file appears, the playlist is an empty header, the manifest lists none.
#[test]
fn a_playlists_only_sync_never_copies_a_track() {
    let r = Rig::new(3);
    r.adopt_empty();
    r.playlist("All", &["t1.mp3", "t2.mp3", "t3.mp3"]);
    r.env.add("t4.mp3", Kind::Mp3, 4);
    let done = r.sync();
    assert!(done.first);
    let audio: Vec<String> = r
        .snapshot()
        .into_keys()
        .filter(|k| k.ends_with(".mp3"))
        .collect();
    assert!(audio.is_empty(), "{audio:?}");
    assert_eq!(locations(&r.read("All.m3u8")), Vec::<String>::new());
    let m = r.card_manifest();
    assert_eq!(m["tracks"], serde_json::json!([]));
}

// Prevents: backups piling up for ever: after a sync only `keep_backups`
// remain, the oldest go first, and the one just made is among those kept.
#[test]
fn a_sync_prunes_the_old_backups_to_keep_backups() {
    let mut r = first_world();
    r.env.cfg.sync.keep_backups = 2;
    let first = r.sync();
    assert_eq!(first.pruned, 0);
    r.playlist("Rock", &["t2.mp3"]);
    let second = r.sync();
    assert_eq!(second.pruned, 0);
    r.playlist("Rock", &["t1.mp3"]);
    let third = r.sync();
    assert_eq!(third.pruned, 1);
    let kept: Vec<String> = r.backups().into_iter().map(|b| b.stamp).collect();
    assert_eq!(
        kept,
        [
            second.backup.clone().expect("a backup").stamp,
            third.backup.clone().expect("a backup").stamp
        ]
    );
    assert!(!first.backup.as_ref().expect("a backup").dir.exists());
}

// ------------------------------------------------ F5 follow-up: writes

// Prevents: a playlist the plan creates overwriting a file that appeared at
// that path after the plan was made (someone else's playlist): the write
// fails, names a backup, and the file is untouched.
#[test]
fn a_new_playlist_never_overwrites_a_file_that_appeared_after_the_plan() {
    let r = synced_world();
    r.playlist("Fresh", &["t1.mp3"]);
    let p = r.prepare(true).unwrap();
    r.put("Fresh.m3u8", b"someone else's");
    let f = failed(r.run(&p, Options::default()));
    match &f.error {
        Error::Io { source, .. } => {
            assert_eq!(source.kind(), std::io::ErrorKind::AlreadyExists, "{source}")
        }
        e => panic!("{e}"),
    }
    assert!(f.backup.is_some());
    assert_eq!(r.read("Fresh.m3u8"), b"someone else's");
}

// Prevents: the opposite excess: a file the plan did see (a managed
// playlist to rewrite) not being replaced, or the target of a rename of the
// same plan (renamed, then rewritten under its new name) being refused.
#[test]
fn a_playlist_the_plan_saw_or_renamed_to_is_replaced() {
    let r = synced_world();
    playlists::rename(r.conn(), &r.env.cfg, "Rock", "Metal").unwrap();
    r.playlist("Metal", &["t2.mp3"]);
    let p = r.prepare(true).unwrap();
    let ops: Vec<&str> = p
        .plan
        .playlists
        .iter()
        .map(|o| match o {
            PlaylistOp::Rename { .. } => "rename",
            PlaylistOp::Write { .. } => "write",
            PlaylistOp::Delete { .. } => "delete",
        })
        .collect();
    assert_eq!(ops, ["rename", "write"]);
    let done = r.run(&p, Options::default()).unwrap();
    assert_eq!((done.renamed, done.written), (1, 1));
    assert!(!r.exists("Rock.m3u8"));
    assert_eq!(locations(&r.read("Metal.m3u8")), ["Old/t2.mp3"]);

    // A managed playlist listed when the plan was made is rewritten.
    r.playlist("Metal", &["t1.mp3"]);
    let p = r.prepare(true).unwrap();
    r.run(&p, Options::default()).unwrap();
    assert_eq!(locations(&r.read("Metal.m3u8")), ["Music/t1.mp3"]);
}

// Prevents: an empty plan writing anything: no backup (none in the result,
// none on disk), no manifest, no card id, even on a first sync, and the
// card byte for byte as it was.
#[test]
fn an_empty_plan_writes_nothing_even_on_a_first_sync() {
    let r = Rig::new(2);
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
    r.put("Other.m3u8", b"#EXTM3U\n");
    let p = r.prepare(true).unwrap();
    assert!(p.plan.is_empty());
    let before = r.snapshot();
    let done = r.run(&p, Options::default()).unwrap();
    assert!(done.backup.is_none());
    assert_eq!(
        (done.written, done.renamed, done.deleted, done.pruned),
        (0, 0, 0, 0)
    );
    assert!(!done.first);
    assert_eq!(r.snapshot(), before);
    assert!(r.backup_entries().is_empty());
    assert_eq!(r.device().card_id, None);
    assert!(r.device().last_sync_at.is_none());
}

// Prevents: the manifest written first on a first sync listing playlists
// by bare file name when they live in a playlists folder: it names card
// paths. Observed through a sync that fails right after (an extra audio
// file), so that first manifest is still on the card.
#[test]
fn the_first_manifest_names_playlists_by_card_path() {
    let mut r = Rig::new(1);
    r.env.cfg.card.playlists_dir = "Playlists".into();
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
    r.playlist("Rock", &["t1.mp3"]);
    let rock = r.env.playlist_id("Rock").unwrap();
    devices::replace_playlist_mappings(
        r.conn(),
        dev,
        &[PlaylistMapping {
            playlist_id: rock,
            card_name: "Rock.m3u8".into(),
        }],
    )
    .unwrap();
    r.put("Playlists/Rock.m3u8", b"#EXTM3U\nold\n");
    r.playlist("Pop", &["t1.mp3"]);
    let p = r.prepare(true).unwrap();
    r.put("Extra/new.mp3", b"appeared");
    let f = failed(r.run(&p, Options::default()));
    assert!(matches!(f.error, Error::VerifyFailed(_)), "{}", f.error);
    assert_eq!(
        r.card_manifest()["playlists"],
        serde_json::json!(["Playlists/Rock.m3u8"])
    );
}

// Prevents: a backup that cannot be pruned failing a sync that is written,
// checked and recorded: it is a warning, the sync stands. An old backup
// holding a folder that cannot be emptied provokes it.
#[test]
fn a_backup_that_cannot_be_pruned_is_a_warning_and_the_sync_stands() {
    let mut r = first_world();
    r.env.cfg.sync.keep_backups = 1;
    let first = r.sync().backup.expect("a backup");
    let locked = first.dir.join("playlists");
    if !rig::readonly_dir_blocks_writes(&locked) {
        eprintln!(
            "SKIPPED: permissions do not stop this user (running as root?); this test checked nothing"
        );
        return;
    }
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    r.playlist("Rock", &["t2.mp3"]);
    let res = r.prepare(true).map(|p| r.run(&p, Options::default()));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    let done = res.unwrap().unwrap_or_else(|f| panic!("{}", f.error));
    assert_eq!(done.pruned, 0);
    assert_eq!(done.warnings.len(), 1, "{:?}", done.warnings);
    assert!(
        done.warnings[0].contains("not pruned"),
        "{:?}",
        done.warnings
    );
    assert_eq!(locations(&r.read("Rock.m3u8")), ["Old/t2.mp3"]);
    assert!(r.device().last_sync_at.is_some());
    assert_eq!(
        devices::playlist_mappings(r.conn(), r.device().id)
            .unwrap()
            .len(),
        1
    );
}

// Prevents: a manifest that cannot be rewritten at the end failing a sync
// that is written, checked and recorded: a warning, with the sync stored.
#[test]
fn a_final_manifest_that_cannot_be_rewritten_is_a_warning_and_the_sync_stands() {
    let r = synced_world();
    let dir = r.card.join(".fiio-tunes");
    if !rig::readonly_dir_blocks_writes(&dir) {
        eprintln!(
            "SKIPPED: permissions do not stop this user (running as root?); this test checked nothing"
        );
        return;
    }
    r.playlist("Rock", &["t2.mp3"]);
    r.playlist("New", &["t1.mp3"]);
    let last = r.device().last_sync_at;
    let p = r.prepare(true).unwrap();
    let manifest = r.read(".fiio-tunes/manifest.json");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    let res = r.run(&p, Options::default());
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let done = res.unwrap_or_else(|f| panic!("{}", f.error));
    assert_eq!(done.warnings.len(), 1, "{:?}", done.warnings);
    assert!(done.warnings[0].contains("manifest"), "{:?}", done.warnings);
    assert_eq!(r.read(".fiio-tunes/manifest.json"), manifest);
    assert!(r.exists("New.m3u8"));
    assert!(r.device().last_sync_at >= last && r.device().last_sync_at.is_some());
    let names: Vec<String> = devices::playlist_mappings(r.conn(), r.device().id)
        .unwrap()
        .into_iter()
        .map(|m| m.card_name)
        .collect();
    assert_eq!(names, ["New.m3u8", "Rock.m3u8"]);
}
