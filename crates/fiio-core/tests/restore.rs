//! `restore::run` (technical document, 11.4): putting a backup back on a
//! card that is a temp folder, and the record of the card in the index.

mod common;
mod rig;

use std::collections::BTreeMap;
use std::fs;

use fiio_core::plan::{PlaylistDeleteReason, PlaylistOp};
use fiio_core::store::devices::{self, PlaylistMapping};
use fiio_core::{Error, backup, playlists, restore, store};
use rig::Rig;

/// A card with a history. Library: `A`, `B` of one track each. Sync 1 (the
/// first, after an adoption) writes A and B; then B is deleted, A edited and
/// C created, and sync 2 writes A and C and deletes B. Afterwards the owner
/// edits the unmanaged Other.m3u8 and adds Z.m3u8.
struct History {
    r: Rig,
    /// The backup sync 1 made: the card before it, no manifest, no id.
    s1: String,
    /// The backup sync 2 made: the card after sync 1.
    s2: String,
    a1: Vec<u8>,
    b1: Vec<u8>,
    manifest1: Vec<u8>,
    card_id: String,
    audio: BTreeMap<String, Vec<u8>>,
}

fn history() -> History {
    let r = Rig::new(3);
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
    r.adopt_file(dev, "t2.mp3", "Old/t2.mp3");
    r.put("Other.m3u8", b"#EXTM3U\noriginal\n");
    r.playlist("A", &["t1.mp3"]);
    r.playlist("B", &["t2.mp3"]);
    let s1 = r.sync().backup.expect("a backup").stamp;
    let (a1, b1) = (r.read("A.m3u8"), r.read("B.m3u8"));
    let manifest1 = r.read(".fiio-tunes/manifest.json");
    let card_id = r.device().card_id.unwrap();

    playlists::delete(r.conn(), &r.env.cfg, "B").unwrap();
    r.playlist("A", &["t2.mp3", "t1.mp3"]);
    r.playlist("C", &["t1.mp3"]);
    let s2 = r.sync().backup.expect("a backup").stamp;
    assert_ne!(s1, s2);
    assert!(!r.exists("B.m3u8"));

    r.put("Other.m3u8", b"#EXTM3U\nedited since\n");
    r.put("Z.m3u8", b"#EXTM3U\nz\n");
    let audio = r
        .snapshot()
        .into_iter()
        .filter(|(k, _)| k.ends_with(".mp3"))
        .collect();
    History {
        r,
        s1,
        s2,
        a1,
        b1,
        manifest1,
        card_id,
        audio,
    }
}

fn restore_to(h: &History, stamp: &str) -> fiio_core::Result<restore::Restored> {
    restore::run(h.r.conn(), &h.r.env.cfg, &h.r.paths(), &h.r.found(), stamp)
}

fn card_names(r: &Rig) -> Vec<String> {
    devices::playlist_mappings(r.conn(), r.device().id)
        .unwrap()
        .into_iter()
        .map(|m| m.card_name)
        .collect()
}

fn audio_files(r: &Rig) -> BTreeMap<String, Vec<u8>> {
    r.snapshot()
        .into_iter()
        .filter(|(k, _)| k.ends_with(".mp3"))
        .collect()
}

// Prevents: a restore that puts back the wrong playlists or leaves the
// wrong ones: the backup's managed playlists come back byte for byte
// (including one deleted since), a managed playlist the backup did not hold
// goes, an unmanaged one edited since stays as edited, audio is untouched,
// the manifest is the backup's.
#[test]
fn a_restore_puts_back_the_managed_playlists_and_the_manifest_only() {
    let h = history();
    let r = &h.r;
    let rest = restore_to(&h, &h.s2).unwrap();
    assert_eq!((rest.written, rest.deleted), (2, 1));

    assert_eq!(r.read("A.m3u8"), h.a1);
    assert_eq!(r.read("B.m3u8"), h.b1);
    assert!(
        !r.exists("C.m3u8"),
        "a playlist the backup did not hold stayed"
    );
    assert_eq!(r.read("Other.m3u8"), b"#EXTM3U\nedited since\n");
    assert_eq!(r.read("Z.m3u8"), b"#EXTM3U\nz\n");
    assert_eq!(r.read(".fiio-tunes/manifest.json"), h.manifest1);
    assert_eq!(audio_files(r), h.audio);
}

// Prevents: a restore that cannot be undone: the card is backed up first
// (as it was, edits included), the new backup is the newest, and the
// backup restored is still there.
#[test]
fn a_restore_backs_the_card_up_first() {
    let h = history();
    let r = &h.r;
    let before_card = r.snapshot();
    let before_mappings =
        backup::saved(&backup::find(&r.data.join("backups"), &r.key(), &h.s2).unwrap()).unwrap();
    let count = r.backups().len();
    let rest = restore_to(&h, &h.s2).unwrap();
    let all = r.backups();
    assert_eq!(all.len(), count + 1);
    assert_eq!(all.last().unwrap(), &rest.before);
    assert_ne!(rest.before.stamp, h.s2);
    assert!(all.iter().any(|b| b.stamp == h.s2));

    // It holds the card as it was just before: C, the edited Other, Z...
    let held: BTreeMap<String, Vec<u8>> = backup::playlists(&rest.before)
        .unwrap()
        .into_iter()
        .collect();
    let want: BTreeMap<String, Vec<u8>> = before_card
        .iter()
        .filter(|(k, _)| k.ends_with(".m3u8"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    assert_eq!(held, want);
    assert_eq!(held["Other.m3u8"], b"#EXTM3U\nedited since\n");
    assert_eq!(
        backup::manifest(&rest.before).unwrap().unwrap(),
        before_card[".fiio-tunes/manifest.json"]
    );
    // Its record is the index as it was: A and C mapped.
    let saved = backup::saved(&rest.before).unwrap();
    let names: Vec<&str> = saved
        .playlists
        .iter()
        .map(|p| p.card_name.as_str())
        .collect();
    assert_eq!(names, ["A.m3u8", "C.m3u8"]);
    assert_eq!(saved.card_id.as_deref(), Some(h.card_id.as_str()));
    // And the backup restored is unchanged by it.
    let again =
        backup::saved(&backup::find(&r.data.join("backups"), &r.key(), &h.s2).unwrap()).unwrap();
    assert_eq!(again, before_mappings);
}

// Prevents: the index keeping the mappings of the state undone: the
// backup's playlist mappings come back, the one whose library playlist is
// gone becomes a tombstone mapped to its card file, the restoring flag is
// clear, and the next sync deletes that file again and writes the changed
// ones.
#[test]
fn a_restore_replaces_the_mappings_and_stands_a_tombstone_for_a_deleted_playlist() {
    let h = history();
    let r = &h.r;
    let dev = r.device();
    let rest = restore_to(&h, &h.s2).unwrap();
    assert_eq!(rest.tombstones, 1);
    assert_eq!(rest.dropped, 0);
    assert_eq!(card_names(r), ["A.m3u8", "B.m3u8"]);
    let maps = devices::playlist_mappings(r.conn(), dev.id).unwrap();
    assert_eq!(maps[0].playlist_id, r.env.playlist_id("A").unwrap());
    let tombs = store::removed_playlists(r.conn()).unwrap();
    assert_eq!(tombs.len(), 1);
    assert_eq!(tombs[0].1, "B");
    assert_eq!(maps[1].playlist_id, tombs[0].0);
    assert_eq!(devices::restoring(r.conn(), dev.id).unwrap(), None);
    assert_eq!(r.device().card_id.as_deref(), Some(h.card_id.as_str()));

    let p = r.prepare(true).unwrap();
    assert!(p.plan.playlists.iter().any(|o| matches!(
        o,
        PlaylistOp::Delete { path, reason: PlaylistDeleteReason::FileDeleted, .. } if path == "B.m3u8"
    )), "{:?}", p.plan.playlists);
    let done = r.run(&p, fiio_core::exec::Options::default()).unwrap();
    assert_eq!(done.deleted, 1);
    assert!(!r.exists("B.m3u8"));
    assert!(r.exists("A.m3u8") && r.exists("C.m3u8"));
    assert_eq!(r.read("Other.m3u8"), b"#EXTM3U\nedited since\n");
    assert!(store::removed_playlists(r.conn()).unwrap().is_empty());
}

// Prevents: a restore of the card as it was before its first sync leaving
// the identity of the later state: the manifest goes (the card had none),
// the card id is the backup's (none), every managed playlist goes, and the
// tracks mapped then are mapped again, so the next sync is a first sync.
#[test]
fn restoring_the_state_before_the_first_sync_removes_manifest_and_card_id() {
    let h = history();
    let r = &h.r;
    let rest = restore_to(&h, &h.s1).unwrap();
    assert_eq!((rest.written, rest.deleted), (0, 2));
    assert!(!r.exists(".fiio-tunes/manifest.json"));
    assert_eq!(r.device().card_id, None);
    assert!(card_names(r).is_empty());
    assert!(!r.exists("A.m3u8") && !r.exists("C.m3u8"));
    assert_eq!(r.read("Other.m3u8"), b"#EXTM3U\nedited since\n");
    assert!(r.exists("Z.m3u8"));
    assert_eq!(
        devices::track_mappings(r.conn(), r.device().id)
            .unwrap()
            .len(),
        2
    );
    assert_eq!(audio_files(r), h.audio);
    // A first sync again.
    let done = r.sync();
    assert!(done.first);
    assert_ne!(r.device().card_id.as_deref(), Some(h.card_id.as_str()));
}

// Prevents: a mapping of a track the library no longer has being put back
// (it would break the index, or map a file to nothing): it is dropped and
// counted; the others come back.
#[test]
fn tracks_gone_from_the_library_are_dropped_from_the_restored_record() {
    let h = history();
    let r = &h.r;
    let t2 = r.env.track(&r.env.music.join("t2.mp3")).id;
    let c = r.conn();
    c.execute("DELETE FROM device_tracks WHERE track_id = ?1", [t2])
        .unwrap();
    c.execute("DELETE FROM playlist_items WHERE track_id = ?1", [t2])
        .unwrap();
    c.execute("DELETE FROM tracks WHERE id = ?1", [t2]).unwrap();
    let rest = restore_to(&h, &h.s2).unwrap();
    assert_eq!(rest.dropped, 1);
    let left = devices::track_mappings(c, r.device().id).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].card_path, "Music/t1.mp3");
    // The backup itself still holds both.
    let b = backup::find(&r.data.join("backups"), &r.key(), &h.s2).unwrap();
    assert_eq!(backup::saved(&b).unwrap().tracks.len(), 2);
}

// Prevents: a restore that fails half way leaving the device looking
// healthy: the restoring flag stays set (so prepare refuses with the
// stamp), the failure leaves the pre-restore backup, and running the
// restore again after the cause is gone completes and clears the flag.
#[test]
fn a_restore_that_fails_midway_leaves_the_flag_set_until_it_completes() {
    let h = history();
    let r = &h.r;
    // A folder where a managed playlist the restore must delete is.
    fs::remove_file(r.card.join("C.m3u8")).unwrap();
    fs::create_dir(r.card.join("C.m3u8")).unwrap();
    let count = r.backups().len();
    let err = restore_to(&h, &h.s2).unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "{err}");
    assert_eq!(
        devices::restoring(r.conn(), r.device().id)
            .unwrap()
            .as_deref(),
        Some(h.s2.as_str())
    );
    assert_eq!(
        r.backups().len(),
        count + 1,
        "the backup taken before the failed restore is missing"
    );
    match r.prepare(true) {
        Err(Error::RestoreUnfinished(s)) => assert_eq!(s, h.s2),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("prepare accepted a half-restored card"),
    }

    fs::remove_dir(r.card.join("C.m3u8")).unwrap();
    let rest = restore_to(&h, &h.s2).unwrap();
    assert_eq!(rest.written, 2);
    assert_eq!(devices::restoring(r.conn(), r.device().id).unwrap(), None);
    assert!(r.prepare(true).is_ok());
}

// Prevents: a restore of another player's backup (or a key mixed up by the
// folder name): it is refused, writes nothing, makes no backup, does not
// set the flag.
#[test]
fn a_backup_of_another_key_is_refused_and_nothing_changes() {
    let h = history();
    let r = &h.r;
    let src = backup::find(&r.data.join("backups"), &r.key(), &h.s2).unwrap();
    let crafted = src.dir.parent().unwrap().join("20991231-235959");
    fs::create_dir(&crafted).unwrap();
    for e in fs::read_dir(&src.dir).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_file() {
            fs::copy(e.path(), crafted.join(e.file_name())).unwrap();
        }
    }
    let mut saved: serde_json::Value =
        serde_json::from_slice(&fs::read(crafted.join("mappings.json")).unwrap()).unwrap();
    saved["device_key"] = "usb:other".into();
    fs::write(crafted.join("mappings.json"), saved.to_string()).unwrap();

    let card = r.snapshot();
    let count = r.backups().len();
    let mappings = card_names(r);
    match restore_to(&h, "20991231-235959") {
        Err(Error::BadBackup(m)) => {
            assert!(m.contains("usb:other") && m.contains(&r.key()), "{m}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.snapshot(), card);
    assert_eq!(r.backups().len(), count);
    assert_eq!(card_names(r), mappings);
    assert_eq!(devices::restoring(r.conn(), r.device().id).unwrap(), None);
}

// Prevents: an unknown stamp restoring something, or making a backup first:
// NoSuchBackup, and the card and the index as they were.
#[test]
fn an_unknown_backup_is_refused_and_nothing_changes() {
    let h = history();
    let r = &h.r;
    let card = r.snapshot();
    let count = r.backups().len();
    for stamp in ["20000101-000000", "", "../x"] {
        match restore_to(&h, stamp) {
            Err(Error::NoSuchBackup(s)) => assert_eq!(s, stamp),
            other => panic!("{stamp:?}: {other:?}"),
        }
    }
    assert_eq!(r.snapshot(), card);
    assert_eq!(r.backups().len(), count);
    assert_eq!(devices::restoring(r.conn(), r.device().id).unwrap(), None);
}

// Prevents: a restore on a card that was never adopted creating state for
// it, and one on a folder that is not allowed as a card writing anything.
#[test]
fn a_restore_needs_an_adopted_writable_card() {
    let r = Rig::new(1);
    r.put("a.m3u8", b"x");
    let err = restore::run(
        r.conn(),
        &r.env.cfg,
        &r.paths(),
        &r.found(),
        "20200101-000000",
    )
    .unwrap_err();
    assert!(matches!(err, Error::NotAdopted(_)), "{err}");
    assert!(r.backup_entries().is_empty());

    let h = history();
    let mut cfg = h.r.env.cfg.clone();
    cfg.device.allow_folder = false;
    let card = h.r.snapshot();
    let err = restore::run(h.r.conn(), &cfg, &h.r.paths(), &h.r.found(), &h.s2).unwrap_err();
    assert!(matches!(err, Error::NotWritable(_)), "{err}");
    assert_eq!(h.r.snapshot(), card);
}

// Prevents: the playlists folder being ignored by a restore: the managed
// playlist comes back in the folder, an unmanaged playlist there stays as
// edited, and a managed one the backup lacked goes.
#[test]
fn a_restore_works_in_the_playlists_folder() {
    let mut r = Rig::new(2);
    r.env.cfg.card.playlists_dir = "Playlists".into();
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
    r.put("Playlists/Mine.m3u8", b"mine");
    r.playlist("A", &["t1.mp3"]);
    r.sync();
    let a1 = r.read("Playlists/A.m3u8");
    r.playlist("A", &["t1.mp3", "t1.mp3"]);
    r.playlist("New", &["t1.mp3"]);
    let s2 = r.sync().backup.expect("a backup").stamp;
    assert_ne!(r.read("Playlists/A.m3u8"), a1);
    r.put("Playlists/Mine.m3u8", b"edited");
    let rest = restore::run(r.conn(), &r.env.cfg, &r.paths(), &r.found(), &s2).unwrap();
    assert_eq!((rest.written, rest.deleted), (1, 1));
    assert_eq!(r.read("Playlists/A.m3u8"), a1);
    assert!(!r.exists("Playlists/New.m3u8"));
    assert_eq!(r.read("Playlists/Mine.m3u8"), b"edited");
    assert_eq!(card_names(&r), ["A.m3u8"]);
}

// Prevents: the restore changing the ids of the playlists it maps back: a
// playlist still in the library keeps its id, the deleted one gets a new
// tombstone id, and the card names are the backup's.
#[test]
fn restored_playlist_mappings_keep_the_backups_ids_and_card_names() {
    let h = history();
    let r = &h.r;
    let b = backup::find(&r.data.join("backups"), &r.key(), &h.s2).unwrap();
    let want = backup::playlist_mappings(&backup::saved(&b).unwrap());
    restore_to(&h, &h.s2).unwrap();
    let got = devices::playlist_mappings(r.conn(), r.device().id).unwrap();
    let ids: Vec<(i64, &str)> = got
        .iter()
        .map(|m| (m.playlist_id, m.card_name.as_str()))
        .collect();
    let a = r.env.playlist_id("A").unwrap();
    // B was deleted since, so its id is a new tombstone; A keeps its id.
    assert_eq!(want.len(), 2);
    assert_eq!(
        want[0],
        PlaylistMapping {
            playlist_id: a,
            card_name: "A.m3u8".into()
        }
    );
    assert_eq!(ids[0], (a, "A.m3u8"));
    assert_eq!(ids[1].1, "B.m3u8");
}

// Prevents: a sync that fails its verification leaving the playlists it
// wrote on the card after the restore its error proposes (they are not yet
// recorded, so the index does not know them; the next sync would find an
// unmanaged Rock.m3u8 and write "Rock (2).m3u8"). The plan saved in the
// backup names them.
#[test]
fn restoring_after_a_failed_sync_removes_the_playlists_it_wrote() {
    let r = Rig::new(1);
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
    r.put("Other.m3u8", b"#EXTM3U\nmine\n");
    r.playlist("Rock", &["t1.mp3"]);
    let p = r.prepare(true).unwrap();
    r.put("Extra/new.mp3", b"appeared after the plan");
    let before = r.snapshot();
    let f = match r.run(&p, fiio_core::exec::Options::default()) {
        Err(f) => f,
        Ok(d) => panic!("{d:?}"),
    };
    assert!(matches!(f.error, Error::VerifyFailed(_)));
    let stamp = f.backup.unwrap().stamp;
    assert!(r.exists("Rock.m3u8"), "the failed sync wrote it");

    restore::run(r.conn(), &r.env.cfg, &r.paths(), &r.found(), &stamp).unwrap();
    let mut after = r.snapshot();
    // The empty manifest folder is not a file.
    after.retain(|k, _| !k.contains("fiio-tunes"));
    let mut was = before;
    was.retain(|k, _| !k.contains("fiio-tunes"));
    was.remove("Rock.m3u8");
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        was.keys().collect::<Vec<_>>()
    );
}

// ------------------------------------------------ F5 follow-up

/// Nothing changed anywhere after a refused restore.
struct Frozen {
    card: BTreeMap<String, Vec<u8>>,
    backups: usize,
}

fn freeze(r: &Rig) -> Frozen {
    Frozen {
        card: r.snapshot(),
        backups: r.backups().len(),
    }
}

fn assert_unchanged(r: &Rig, f: &Frozen) {
    assert_eq!(r.snapshot(), f.card, "the card changed");
    assert_eq!(r.backups().len(), f.backups, "a backup was taken");
    assert_eq!(devices::restoring(r.conn(), r.device().id).unwrap(), None);
}

fn no_manifest_and_no_backed_up_playlists(h: &History) {
    fs::remove_file(h.r.card.join(".fiio-tunes/manifest.json")).unwrap();
    for f in ["A.m3u8", "B.m3u8"] {
        let _ = fs::remove_file(h.r.card.join(f));
    }
}

// Prevents: a backup being put on another card that has the same USB
// serial: a manifest naming a card id that is neither the index's nor the
// backup's is refused as WrongCard, with nothing changed and no backup.
// A manifest with the backup's id, or the index's, is the card.
#[test]
fn a_restore_refuses_a_card_with_another_card_id() {
    let h = history();
    let r = &h.r;
    let manifest = r.read(".fiio-tunes/manifest.json");
    let mut m: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
    m["card_id"] = "ffffffffffffffffffffffffffffffff".into();
    r.put(".fiio-tunes/manifest.json", m.to_string().as_bytes());
    let frozen = freeze(r);
    match restore_to(&h, &h.s2) {
        Err(Error::WrongCard(k)) => assert_eq!(k, r.key()),
        other => panic!("{other:?}"),
    }
    assert_unchanged(r, &frozen);

    // The backup's id on the card, the index holding another: allowed.
    r.put(".fiio-tunes/manifest.json", &manifest);
    devices::set_card_id(r.conn(), r.device().id, Some("0123")).unwrap();
    restore_to(&h, &h.s2).unwrap();
}

// Prevents: the blank second card being restored onto: no manifest, the
// index and the backup both have card ids, and none of the backup's
// managed playlists is on the card.
#[test]
fn a_restore_refuses_a_blank_card_when_both_sides_have_card_ids() {
    let h = history();
    let r = &h.r;
    no_manifest_and_no_backed_up_playlists(&h);
    let frozen = freeze(r);
    assert!(matches!(restore_to(&h, &h.s2), Err(Error::WrongCard(_))));
    assert_unchanged(r, &frozen);
}

// Prevents: the identity check refusing the cases where there is nothing to
// compare: no manifest and the index has no card id; no manifest and the
// backup was taken before the first sync; no manifest but one of the
// backup's managed playlists still on the card.
#[test]
fn a_restore_without_a_manifest_is_allowed_when_nothing_contradicts_the_backup() {
    // The index has no card id.
    let h = history();
    no_manifest_and_no_backed_up_playlists(&h);
    devices::set_card_id(h.r.conn(), h.r.device().id, None).unwrap();
    restore_to(&h, &h.s2).unwrap();
    assert_eq!(h.r.read("A.m3u8"), h.a1);

    // The backup has none (taken before the first sync).
    let h = history();
    no_manifest_and_no_backed_up_playlists(&h);
    restore_to(&h, &h.s1).unwrap();
    assert_eq!(h.r.device().card_id, None);

    // A managed playlist of the backup is still there.
    let h = history();
    fs::remove_file(h.r.card.join(".fiio-tunes/manifest.json")).unwrap();
    assert!(h.r.exists("A.m3u8"));
    restore_to(&h, &h.s2).unwrap();
    assert_eq!(h.r.read("A.m3u8"), h.a1);
}

// Prevents: playlists being restored to the folder the config names today
// instead of where the backup found them; and an old backup without the
// field not falling back to the config.
#[test]
fn a_restore_uses_the_playlists_folder_the_backup_recorded() {
    let mut r = Rig::new(2);
    r.env.cfg.card.playlists_dir = "Playlists".into();
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
    r.playlist("A", &["t1.mp3"]);
    r.sync();
    let a1 = r.read("Playlists/A.m3u8");
    r.playlist("A", &["t1.mp3", "t1.mp3"]);
    let s2 = r.sync().backup.expect("a backup").stamp;
    let backup_dir = backup::find(&r.data.join("backups"), &r.key(), &s2)
        .unwrap()
        .dir;
    assert_eq!(
        backup::saved(&backup::find(&r.data.join("backups"), &r.key(), &s2).unwrap())
            .unwrap()
            .playlists_dir
            .as_deref(),
        Some("Playlists")
    );

    // The config changed since: the backup still knows where they were.
    r.env.cfg.card.playlists_dir = "Elsewhere".into();
    restore::run(r.conn(), &r.env.cfg, &r.paths(), &r.found(), &s2).unwrap();
    assert_eq!(r.read("Playlists/A.m3u8"), a1);
    assert!(!r.exists("Elsewhere"));

    // An old backup, without the field, follows the config.
    let path = backup_dir.join("mappings.json");
    let mut m: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    m.as_object_mut().unwrap().remove("playlists_dir");
    fs::write(&path, m.to_string()).unwrap();
    r.playlist("A", &["t1.mp3", "t1.mp3"]);
    r.env.cfg.card.playlists_dir = "Playlists".into();
    r.sync();
    assert_ne!(r.read("Playlists/A.m3u8"), a1);
    restore::run(r.conn(), &r.env.cfg, &r.paths(), &r.found(), &s2).unwrap();
    assert_eq!(r.read("Playlists/A.m3u8"), a1);
}

// Prevents: a restore starting and then finding a backup file it cannot
// write: every managed file (UTF-8) and the manifest are checked first.
// BadBackup, the card untouched, the flag not set, no backup taken. A file
// it does not restore (unmanaged) is not checked.
#[test]
fn a_backup_with_a_file_that_is_not_utf8_is_refused_before_anything_changes() {
    for victim in ["playlists/A.m3u8", "manifest.json"] {
        let h = history();
        let r = &h.r;
        let b = backup::find(&r.data.join("backups"), &r.key(), &h.s2).unwrap();
        fs::write(b.dir.join(victim), [0xff, 0xfe, 0x00]).unwrap();
        let frozen = freeze(r);
        match restore_to(&h, &h.s2) {
            Err(Error::BadBackup(m)) => assert!(m.contains("UTF-8"), "{victim}: {m}"),
            other => panic!("{victim}: {other:?}"),
        }
        assert_unchanged(r, &frozen);
    }
    let h = history();
    let b = backup::find(&h.r.data.join("backups"), &h.r.key(), &h.s2).unwrap();
    fs::write(b.dir.join("playlists/Other.m3u8"), [0xff, 0xfe]).unwrap();
    restore_to(&h, &h.s2).unwrap();
}

// Prevents: a restore keeping the files a failed sync created: a playlist
// it wrote or renamed to, and the hidden hop of a rename cycle, go; a file
// the backup held is never removed because the plan names it.
#[test]
fn a_restore_removes_a_hidden_rename_hop_a_failed_sync_left() {
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
    let stamp = r.sync().backup.expect("a backup").stamp;
    // The cycle's hop, as a sync stopped between two renames leaves it.
    let hop = backup::find(&r.data.join("backups"), &r.key(), &stamp).unwrap();
    let plan = backup::plan(&hop).unwrap().unwrap();
    let hop_name = plan["playlists"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|o| o["to"].as_str())
        .find(|t| t.starts_with('.'))
        .expect("a hidden hop in the plan")
        .to_owned();
    assert!(hop_name.contains("fiio-tunes-move"), "{hop_name}");
    r.put(&hop_name, &a);
    let rest = restore::run(r.conn(), &r.env.cfg, &r.paths(), &r.found(), &stamp).unwrap();
    assert!(!r.exists(&hop_name), "the hop stayed");
    assert!(rest.deleted >= 1);
    // The backup's own files are back, not removed.
    assert_eq!(r.read("Beta.m3u8"), a);
    assert_eq!(r.read("Alpha.m3u8"), b);
}

// Prevents: the owner's own playlist being lost by a failed sync: with
// remove_unmanaged the plan deletes the unmanaged Rock.m3u8 and writes a
// managed one at the same path; the sync fails verification; the restore
// puts the owner's original bytes back (not just a file of that name). A
// plain delete of an unmanaged playlist, with nothing written over it, is
// put back too, from a sync that succeeded.
#[test]
fn a_restore_puts_back_the_unmanaged_playlists_a_sync_deleted_or_wrote_over() {
    let mut r = Rig::new(1);
    r.env.cfg.sync.remove_unmanaged = true;
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
    let rock = b"#EXTM3U\nunmanaged original\n".to_vec();
    r.put("Rock.m3u8", &rock);
    r.put("sub/Gone.m3u8", b"#EXTM3U\nowner's\n");
    r.playlist("Rock", &["t1.mp3"]);
    let p = r.prepare(true).unwrap();
    assert!(p.plan.playlists.len() >= 3, "{:?}", p.plan.playlists);
    r.put("Extra/new.mp3", b"appeared after the plan");
    let f = match r.run(&p, fiio_core::exec::Options::default()) {
        Err(f) => f,
        Ok(d) => panic!("{d:?}"),
    };
    assert!(matches!(f.error, Error::VerifyFailed(_)));
    let stamp = f.backup.unwrap().stamp;
    assert!(!r.exists("sub/Gone.m3u8"));
    assert_ne!(r.read("Rock.m3u8"), rock, "the failed sync wrote it");
    restore::run(r.conn(), &r.env.cfg, &r.paths(), &r.found(), &stamp).unwrap();
    assert_eq!(r.read("Rock.m3u8"), rock);
    assert_eq!(r.read("sub/Gone.m3u8"), b"#EXTM3U\nowner's\n");

    // A plain delete, in a sync that went through.
    let r = Rig::new(1);
    let mut r = r;
    r.env.cfg.sync.remove_unmanaged = true;
    let dev = r.adopt_empty();
    r.adopt_file(dev, "t1.mp3", "Music/t1.mp3");
    r.put("Gone.m3u8", b"#EXTM3U\nowner's\n");
    r.put("Keep.txt", b"not a playlist");
    r.playlist("Mine", &["t1.mp3"]);
    let stamp = r.sync().backup.expect("a backup").stamp;
    assert!(!r.exists("Gone.m3u8"));
    restore::run(r.conn(), &r.env.cfg, &r.paths(), &r.found(), &stamp).unwrap();
    assert_eq!(r.read("Gone.m3u8"), b"#EXTM3U\nowner's\n");
    assert!(!r.exists("Mine.m3u8"));
    assert_eq!(r.read("Keep.txt"), b"not a playlist");
}

// Prevents: a damaged or hostile backup making a restore write or delete
// outside the card's playlists: every path read from it (a managed card
// name, a file under playlists/, a path in plan.json) must be a relative
// plain-name path ending in .m3u8 (any case). BadBackup before anything
// changes: card untouched, no backup, flag not set, nothing outside the card.
#[test]
fn a_backup_with_an_unsafe_path_is_refused_before_anything_changes() {
    type Damage = Box<dyn Fn(&std::path::Path)>;
    let edit_json = |dir: &std::path::Path, file: &str, f: &dyn Fn(&mut serde_json::Value)| {
        let p = dir.join(file);
        let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        f(&mut v);
        fs::write(p, v.to_string()).unwrap();
    };
    let cases: Vec<(&str, Damage)> = vec![
        (
            "card name with ..",
            Box::new(move |d| {
                edit_json(d, "mappings.json", &|v| {
                    v["playlists"][0]["card_name"] = "x/../../evil.m3u8".into()
                })
            }),
        ),
        (
            "absolute card name",
            Box::new(move |d| {
                edit_json(d, "mappings.json", &|v| {
                    v["playlists"][0]["card_name"] = "/tmp/evil.m3u8".into()
                })
            }),
        ),
        (
            "card name not a playlist",
            Box::new(move |d| {
                edit_json(d, "mappings.json", &|v| {
                    v["playlists"][0]["card_name"] = "evil.txt".into()
                })
            }),
        ),
        (
            "file under playlists/ not a playlist",
            Box::new(|d| fs::write(d.join("playlists/notes.txt"), b"x").unwrap()),
        ),
        (
            "plan path with ..",
            Box::new(move |d| {
                edit_json(d, "plan.json", &|v| {
                    v["playlists"] = serde_json::json!([{"op": "write", "path": "../evil.m3u8"}])
                })
            }),
        ),
        (
            "plan rename to an absolute path",
            Box::new(move |d| {
                edit_json(
                    d,
                    "plan.json",
                    &|v| v["playlists"] = serde_json::json!([{"op": "rename", "from": "A.m3u8", "to": "/tmp/e.m3u8"}]),
                )
            }),
        ),
    ];
    for (what, damage) in cases {
        let h = history();
        let b = backup::find(&h.r.data.join("backups"), &h.r.key(), &h.s2).unwrap();
        damage(&b.dir);
        let frozen = freeze(&h.r);
        match restore_to(&h, &h.s2) {
            Err(Error::BadBackup(m)) => assert!(m.contains("playlist"), "{what}: {m}"),
            other => panic!("{what}: {other:?}"),
        }
        assert_unchanged(&h.r, &frozen);
        assert!(
            !h.r.card.parent().unwrap().join("evil.m3u8").exists(),
            "{what}"
        );
    }
    // Upper-case extensions are playlists too.
    let h = history();
    let b = backup::find(&h.r.data.join("backups"), &h.r.key(), &h.s2).unwrap();
    fs::write(b.dir.join("playlists/Loud.M3U8"), b"#EXTM3U\n").unwrap();
    restore_to(&h, &h.s2).unwrap();
}

// Prevents: old backups piling up after a restore: they are pruned to
// keep_backups afterwards, never the backup just taken, without a warning.
#[test]
fn a_restore_prunes_old_backups_afterwards() {
    let mut h = history();
    h.r.env.cfg.sync.keep_backups = 2;
    assert_eq!(h.r.backups().len(), 2);
    let rest = restore_to(&h, &h.s2).unwrap();
    assert!(rest.warnings.is_empty(), "{:?}", rest.warnings);
    let kept: Vec<String> = h.r.backups().into_iter().map(|b| b.stamp).collect();
    assert_eq!(kept, [h.s2.clone(), rest.before.stamp.clone()]);
    assert!(!kept.contains(&h.s1));
}
