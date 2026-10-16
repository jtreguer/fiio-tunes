mod common;

use std::path::{Path, PathBuf};

use common::{Env, Kind};
use fiio_core::store::FileRecord;
use fiio_core::store::{self, Store};
use fiio_core::tags::{AudioInfo, FileKind, Tags};
use fiio_core::{Error, playlists};

fn names(store: &Store) -> Vec<String> {
    store::playlists(store.conn())
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect()
}

fn put(conn: &rusqlite::Connection, name: &str) -> fiio_core::Result<i64> {
    store::put_playlist(conn, name, Path::new(&format!("/p/{name}.m3u8")), 1, &[])
}

#[test]
fn savepoint_rolls_back_everything_on_error_outside_a_transaction() {
    let s = Store::open_in_memory().unwrap();
    let r: fiio_core::Result<()> = store::savepoint(s.conn(), || {
        put(s.conn(), "a")?;
        put(s.conn(), "b")?;
        Err(Error::NoSuchPlaylist("boom".into()))
    });
    assert!(matches!(r, Err(Error::NoSuchPlaylist(ref n)) if n == "boom"));
    assert!(names(&s).is_empty());
    // The connection is usable afterwards (savepoint released).
    put(s.conn(), "c").unwrap();
    assert_eq!(names(&s), ["c"]);
}

#[test]
fn savepoint_keeps_writes_on_success() {
    let s = Store::open_in_memory().unwrap();
    store::savepoint(s.conn(), || put(s.conn(), "a").map(|_| ())).unwrap();
    assert_eq!(names(&s), ["a"]);
}

#[test]
fn savepoint_inside_a_transaction_rolls_back_only_its_own_writes() {
    let mut s = Store::open_in_memory().unwrap();
    {
        let tx = s.transaction().unwrap();
        put(&tx, "before").unwrap();
        let r: fiio_core::Result<()> = store::savepoint(&tx, || {
            put(&tx, "inner")?;
            Err(Error::NoSuchPlaylist("boom".into()))
        });
        assert!(r.is_err());
        // A successful nested savepoint is kept with the outer transaction.
        store::savepoint(&tx, || put(&tx, "kept").map(|_| ())).unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(names(&s), ["before", "kept"]);
}

#[test]
fn successful_savepoint_is_still_undone_by_the_outer_rollback() {
    let mut s = Store::open_in_memory().unwrap();
    {
        let tx = s.transaction().unwrap();
        store::savepoint(&tx, || put(&tx, "a").map(|_| ())).unwrap();
        tx.rollback().unwrap();
    }
    assert!(names(&s).is_empty());
}

#[test]
fn failed_put_playlist_keeps_the_previous_items() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    let id = env.track(&a).id;
    let conn = env.store.conn();
    let file = Path::new("/p/x.m3u8");
    store::put_playlist(conn, "x", file, 1, &[(Some(id), "one".into())]).unwrap();
    // The second item references a track that does not exist: foreign key error
    // after the old items were deleted and the first new one inserted.
    let r = store::put_playlist(
        conn,
        "x",
        file,
        2,
        &[(Some(id), "new1".into()), (Some(987_654), "new2".into())],
    );
    assert!(matches!(r, Err(Error::Db(_))), "{r:?}");
    assert_eq!(
        env.indexed_items("x"),
        vec![(0, Some(id), "one".to_string())]
    );
    assert_eq!(store::playlists(conn).unwrap()[0].file_mtime_ns, 1);
}

#[test]
fn sync_from_disk_that_fails_midway_changes_nothing() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    env.write_playlist("old", &format!("{}\n", a.display()));
    playlists::sync_from_disk(env.store.conn(), &env.cfg, true).unwrap();
    let old_items = env.indexed_items("old");
    // "old" is deleted from disk, "a-new" loads fine, then "b-bad" fails in SQLite.
    std::fs::remove_file(env.playlist_file("old")).unwrap();
    env.write_playlist("a-new", &format!("{}\n", a.display()));
    env.write_playlist("b-bad", "/x/boom.mp3\n");
    env.store
        .conn()
        .execute_batch(
            "CREATE TRIGGER boom BEFORE INSERT ON playlist_items \
             WHEN NEW.raw_entry LIKE '%boom%' BEGIN SELECT RAISE(ABORT, 'boom'); END;",
        )
        .unwrap();

    let r = playlists::sync_from_disk(env.store.conn(), &env.cfg, true);
    assert!(matches!(r, Err(Error::Db(_))), "{r:?}");
    assert!(
        env.playlist_id("a-new").is_none(),
        "half-loaded playlist left behind"
    );
    assert!(env.playlist_id("b-bad").is_none());
    assert!(
        env.playlist_id("old").is_some(),
        "deletion must not be half-applied"
    );
    assert_eq!(env.indexed_items("old"), old_items);
}

fn record(name: &str, tag_error: Option<&str>, drm: bool) -> FileRecord {
    FileRecord {
        path: PathBuf::from(format!("/m/{name}")),
        path_key: format!("/m/{name}"),
        size: 10,
        mtime_ns: 20,
        hash: [7; 32],
        info: AudioInfo {
            kind: FileKind::Aac,
            duration_ms: Some(1000),
            bitrate: None,
            samplerate: None,
        },
        tags: Tags::default(),
        tag_error: tag_error.map(String::from),
        drm,
    }
}

fn row(s: &Store, name: &str) -> store::TrackRow {
    store::all_tracks(s.conn())
        .unwrap()
        .into_iter()
        .find(|t| t.path == Path::new("/m").join(name))
        .unwrap()
}

#[test]
fn track_row_round_trips_tag_error_and_drm_through_insert_and_update() {
    let s = Store::open_in_memory().unwrap();
    let a = store::insert_track(s.conn(), &record("a", Some("bad tag"), true), 1).unwrap();
    let b = store::insert_track(s.conn(), &record("b", None, false), 1).unwrap();
    let (ra, rb) = (row(&s, "a"), row(&s, "b"));
    assert_eq!((ra.tag_error.as_deref(), ra.drm), (Some("bad tag"), true));
    assert_eq!((rb.tag_error.as_deref(), rb.drm), (None, false));

    // Update writes both fields in both directions.
    store::update_track(s.conn(), a, &record("a", None, false)).unwrap();
    store::update_track(s.conn(), b, &record("b", Some("later"), true)).unwrap();
    let (ra, rb) = (row(&s, "a"), row(&s, "b"));
    assert_eq!((ra.tag_error, ra.drm), (None, false));
    assert_eq!((rb.tag_error.as_deref(), rb.drm), (Some("later"), true));
}

#[test]
fn flagged_tracks_lists_present_flagged_rows_only() {
    let s = Store::open_in_memory().unwrap();
    store::insert_track(s.conn(), &record("clean", None, false), 1).unwrap();
    store::insert_track(s.conn(), &record("untagged", Some("why"), false), 1).unwrap();
    store::insert_track(s.conn(), &record("drm", None, true), 1).unwrap();
    let gone = store::insert_track(s.conn(), &record("gone", Some("x"), true), 1).unwrap();
    store::set_missing(s.conn(), gone, Some(5)).unwrap();
    let f = store::flagged_tracks(s.conn()).unwrap();
    assert_eq!(
        f.untagged,
        vec![(PathBuf::from("/m/untagged"), "why".to_string())]
    );
    assert_eq!(f.drm, vec![PathBuf::from("/m/drm")]);
}

fn add_device(conn: &rusqlite::Connection, key: &str) -> i64 {
    conn.execute("INSERT INTO devices (device_key) VALUES (?1)", [key])
        .unwrap();
    conn.last_insert_rowid()
}

fn add_track_row(conn: &rusqlite::Connection, n: i64) -> i64 {
    conn.execute(
        "INSERT INTO tracks (path, path_key, size, mtime_ns, hash, filetype, tags_hash, added_at) \
         VALUES (?1, ?1, 1, 1, x'00', 'mp3', x'00', 1)",
        [format!("/m/{n}.mp3")],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn map_card_path(
    conn: &rusqlite::Connection,
    device: i64,
    track: i64,
    path: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO device_tracks (device_id, track_id, card_path, card_key, synced_hash, synced_size) \
         VALUES (?1, ?2, ?3, ?4, x'00', 1)",
        rusqlite::params![device, track, path, fiio_core::paths::card_key(path)],
    )
}

fn add_playlist_row(conn: &rusqlite::Connection, name: &str) -> i64 {
    store::put_playlist(conn, name, Path::new(&format!("/p/{name}.m3u8")), 1, &[]).unwrap()
}

fn map_card_playlist(
    conn: &rusqlite::Connection,
    device: i64,
    playlist: i64,
    name: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO device_playlists (device_id, playlist_id, card_name, card_key) \
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![device, playlist, name, fiio_core::paths::card_key(name)],
    )
}

fn card_paths(conn: &rusqlite::Connection) -> Vec<(i64, String)> {
    let mut stmt = conn
        .prepare("SELECT device_id, card_path FROM device_tracks ORDER BY device_id, card_path")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn is_unique_violation(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _)
        if f.code == rusqlite::ErrorCode::ConstraintViolation)
}

// Prevents: two library tracks mapped to card paths FAT32 sees as one name
// (ASCII case, Unicode case, or NFC/NFD), so the second copy would overwrite
// the first on the card.
#[test]
fn a_device_refuses_two_card_paths_with_the_same_card_key() {
    let s = Store::open_in_memory().unwrap();
    let c = s.conn();
    let dev = add_device(c, "usb:A");
    let tracks: Vec<i64> = (1..=5).map(|n| add_track_row(c, n)).collect();
    map_card_path(c, dev, tracks[0], "Music/Rock/Song.mp3").unwrap();
    map_card_path(c, dev, tracks[1], "Music/Rock/Émile.mp3").unwrap();
    for (i, clash) in [
        "music/ROCK/song.MP3",
        "Music/rock/émile.mp3",
        "Music/Rock/E\u{301}mile.mp3",
    ]
    .iter()
    .enumerate()
    {
        let err = map_card_path(c, dev, tracks[2 + i], clash).unwrap_err();
        assert!(is_unique_violation(&err), "{clash}: {err}");
    }
    // The refused rows left nothing behind.
    assert_eq!(
        card_paths(c),
        [
            (dev, "Music/Rock/Song.mp3".to_string()),
            (dev, "Music/Rock/Émile.mp3".to_string())
        ]
    );
    // A genuinely different name is fine.
    map_card_path(c, dev, tracks[2], "Music/Rock/Song2.mp3").unwrap();
    assert_eq!(card_paths(c).len(), 3);
}

// Prevents: the card key uniqueness being made global, so a second card
// could not hold a file the first one holds.
#[test]
fn different_devices_may_use_card_paths_with_the_same_card_key() {
    let s = Store::open_in_memory().unwrap();
    let c = s.conn();
    let (a, b) = (add_device(c, "usb:A"), add_device(c, "usb:B"));
    let (t1, t2) = (add_track_row(c, 1), add_track_row(c, 2));
    map_card_path(c, a, t1, "Rock/Émile.mp3").unwrap();
    map_card_path(c, b, t2, "rock/émile.mp3").unwrap();
    assert_eq!(card_paths(c).len(), 2);
    // The same name on the same device is still refused.
    assert!(map_card_path(c, b, t1, "ROCK/ÉMILE.MP3").is_err());
    assert_eq!(card_paths(c).len(), 2);
}

// Prevents: two library playlists written to one card file name.
#[test]
fn a_device_refuses_two_playlist_names_with_the_same_card_key_and_other_devices_allow_it() {
    let s = Store::open_in_memory().unwrap();
    let c = s.conn();
    let (a, b) = (add_device(c, "usb:A"), add_device(c, "usb:B"));
    let (p1, p2, p3) = (
        add_playlist_row(c, "one"),
        add_playlist_row(c, "two"),
        add_playlist_row(c, "three"),
    );
    map_card_playlist(c, a, p1, "Évasion.m3u8").unwrap();
    for (p, clash) in [(p2, "évasion.M3U8"), (p3, "E\u{301}vasion.m3u8")] {
        let err = map_card_playlist(c, a, p, clash).unwrap_err();
        assert!(is_unique_violation(&err), "{clash}: {err}");
    }
    let n: i64 = c
        .query_row("SELECT COUNT(*) FROM device_playlists", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    // Another device, and another name on this one, are fine.
    map_card_playlist(c, b, p2, "évasion.m3u8").unwrap();
    map_card_playlist(c, a, p2, "Autre.m3u8").unwrap();
}

// Prevents: two rows for one player (the key is how a card is recognised).
#[test]
fn a_device_key_is_unique() {
    let s = Store::open_in_memory().unwrap();
    add_device(s.conn(), "usb:A");
    let err = s
        .conn()
        .execute("INSERT INTO devices (device_key) VALUES ('usb:A')", [])
        .unwrap_err();
    assert!(err.to_string().contains("UNIQUE"), "{err}");
}

// Prevents: a database written by a newer fiio-tunes being opened and then
// migrated or written to by an older one.
#[test]
fn a_database_from_a_newer_schema_is_refused_and_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fiio-tunes.sqlite");
    drop(Store::open(&path).unwrap());
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.pragma_update(None, "user_version", 2).unwrap();
    }
    let err = Store::open(&path).err().expect("must refuse");
    assert!(
        matches!(err, Error::Db(_)) && err.to_string().contains("newer"),
        "{err}"
    );
    let c = rusqlite::Connection::open(&path).unwrap();
    let v: i64 = c
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(v, 2);
}

// Prevents: a fresh index not being stamped, so every start re-ran the
// migration; and the current version being refused.
#[test]
fn a_fresh_database_is_version_1_and_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fiio-tunes.sqlite");
    drop(Store::open(&path).unwrap());
    let c = rusqlite::Connection::open(&path).unwrap();
    let v: i64 = c
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(v, 1);
    drop(c);
    Store::open(&path).expect("reopens at the current version");
}

// Prevents: foreign keys silently off, which lets a card mapping outlive the
// device or track it names.
#[test]
fn foreign_keys_are_enforced_in_memory_and_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let on_disk = Store::open(&dir.path().join("db/fiio-tunes.sqlite")).unwrap();
    let in_memory = Store::open_in_memory().unwrap();
    for s in [&on_disk, &in_memory] {
        let c = s.conn();
        let on: i64 = c
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))
            .unwrap();
        assert_eq!(on, 1);
        let t = add_track_row(c, 1);
        let err = map_card_path(c, 999, t, "Music/a.mp3").unwrap_err();
        assert!(
            matches!(&err, rusqlite::Error::SqliteFailure(f, _)
                if f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
            "{err}"
        );
        assert!(card_paths(c).is_empty());
    }
}

// Prevents: a freed track or playlist id being handed out again, which would
// attach an old card file or backup mapping to an unrelated song or playlist.
#[test]
fn ids_of_deleted_tracks_and_playlists_are_never_reused() {
    let s = Store::open_in_memory().unwrap();
    let c = s.conn();
    let t1 = add_track_row(c, 1);
    let t2 = add_track_row(c, 2);
    c.execute("DELETE FROM tracks WHERE id = ?1", [t2]).unwrap();
    let t3 = add_track_row(c, 3);
    assert!(t3 > t2, "{t3} reused or went below {t2} (first was {t1})");

    let p1 = add_playlist_row(c, "one");
    let p2 = add_playlist_row(c, "two");
    store::delete_playlist(c, "two").unwrap();
    assert_eq!(names(&s), ["one"]);
    let p3 = add_playlist_row(c, "three");
    assert!(p3 > p2, "{p3} reused or went below {p2} (first was {p1})");
}
