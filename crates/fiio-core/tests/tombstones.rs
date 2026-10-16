//! Playlist tombstones: a deleted playlist whose library playlist is mapped
//! to a card playlist stays in the index, hidden, so the next sync can delete
//! the card's file. The device store (`store/devices.rs`) does not exist yet,
//! so the mappings are raw `device_playlists` rows.

mod common;

use common::{Env, Kind};
use fiio_core::paths::card_key;
use fiio_core::store;
use fiio_core::{Error, playlists};
use rusqlite::Connection;

fn env() -> Env {
    let mut env = Env::new();
    env.add("a.mp3", Kind::Mp3, 1);
    env.add("b.mp3", Kind::Mp3, 2);
    env.scan();
    env
}

/// Writes the playlist files `(name, track file)` and loads them.
fn load(env: &Env, lists: &[(&str, &str)]) {
    for (name, file) in lists {
        env.write_playlist(name, &format!("{}\n", env.music.join(file).display()));
    }
    playlists::sync_from_disk(env.store.conn(), &env.cfg, true).unwrap();
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn live_names(conn: &Connection) -> Vec<String> {
    store::playlists(conn)
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect()
}

fn device(conn: &Connection, key: &str) -> i64 {
    conn.execute("INSERT INTO devices (device_key) VALUES (?1)", [key])
        .unwrap();
    conn.last_insert_rowid()
}

fn map(conn: &Connection, dev: i64, playlist: i64, card_name: &str) {
    conn.execute(
        "INSERT INTO device_playlists (device_id, playlist_id, card_name, card_key) \
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![dev, playlist, card_name, card_key(card_name)],
    )
    .unwrap();
}

/// `(playlist_id, card_name)` rows of a device.
fn mappings(conn: &Connection, dev: i64) -> Vec<(i64, String)> {
    let mut stmt = conn
        .prepare("SELECT playlist_id, card_name FROM device_playlists WHERE device_id = ?1 ORDER BY playlist_id")
        .unwrap();
    stmt.query_map([dev], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn items(conn: &Connection, id: i64) -> i64 {
    count(
        conn,
        &format!("SELECT COUNT(*) FROM playlist_items WHERE playlist_id = {id}"),
    )
}

/// "mix" (one track) is mapped on a device as card file "Mix.m3u8"; "other"
/// is not mapped.
fn mapped_mix() -> (Env, i64, i64) {
    let env = env();
    load(&env, &[("mix", "a.mp3"), ("other", "b.mp3")]);
    let conn = env.store.conn();
    let id = env.playlist_id("mix").unwrap();
    assert_eq!(items(conn, id), 1, "precondition");
    let dev = device(conn, "usb:T1");
    map(conn, dev, id, "Mix.m3u8");
    (env, id, dev)
}

// Prevents: deleting a playlist that is mapped to a card playlist failing on
// the foreign key of device_playlists, or losing the only record of which
// card file the next sync must delete.
#[test]
fn deleting_a_mapped_playlist_file_keeps_a_hidden_tombstone() {
    let (env, id, dev) = mapped_mix();
    let conn = env.store.conn();
    std::fs::remove_file(env.playlist_file("mix")).unwrap();

    let report = playlists::sync_from_disk(conn, &env.cfg, false).unwrap();
    assert_eq!(report.removed, vec!["mix"]);

    assert_eq!(live_names(conn), vec!["other"]);
    let summaries: Vec<String> = store::playlist_summaries(conn)
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(summaries, vec!["other"]);
    assert!(env.playlist_id("mix").is_none());

    assert_eq!(
        store::removed_playlists(conn).unwrap(),
        vec![(id, "mix".to_owned())]
    );
    assert_eq!(mappings(conn, dev), vec![(id, "Mix.m3u8".to_owned())]);
    assert_eq!(items(conn, id), 0, "items are dropped");
    let removed_at: Option<i64> = conn
        .query_row(
            "SELECT removed_at FROM playlists WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(removed_at.is_some());

    // A second scan does not trip over it, nor report it removed again.
    let again = playlists::sync_from_disk(conn, &env.cfg, true).unwrap();
    assert!(again.removed.is_empty(), "{:?}", again.removed);
    assert_eq!(store::removed_playlists(conn).unwrap().len(), 1);
}

#[test]
fn the_playlist_delete_command_leaves_a_tombstone_too() {
    let (env, id, _) = mapped_mix();
    let conn = env.store.conn();
    playlists::delete(conn, &env.cfg, "mix").unwrap();
    assert!(!env.playlist_file("mix").exists());
    assert_eq!(
        store::removed_playlists(conn).unwrap(),
        vec![(id, "mix".to_owned())]
    );
    assert_eq!(live_names(conn), vec!["other"]);
}

// Prevents: tombstones piling up for playlists no card knows.
#[test]
fn deleting_an_unmapped_playlist_really_deletes_it_and_leaves_the_mapped_one() {
    let (env, id, dev) = mapped_mix();
    let conn = env.store.conn();
    let other = env.playlist_id("other").unwrap();
    std::fs::remove_file(env.playlist_file("other")).unwrap();
    let report = playlists::sync_from_disk(conn, &env.cfg, false).unwrap();
    assert_eq!(report.removed, vec!["other"]);
    assert_eq!(
        count(
            conn,
            &format!("SELECT COUNT(*) FROM playlists WHERE id = {other}")
        ),
        0
    );
    assert_eq!(items(conn, other), 0);
    assert!(store::removed_playlists(conn).unwrap().is_empty());
    assert_eq!(live_names(conn), vec!["mix"]);
    assert_eq!(mappings(conn, dev), vec![(id, "Mix.m3u8".to_owned())]);
    assert_eq!(items(conn, id), 1);
}

#[test]
fn deleting_twice_keeps_one_tombstone() {
    let (env, id, _) = mapped_mix();
    let conn = env.store.conn();
    std::fs::remove_file(env.playlist_file("mix")).unwrap();
    store::delete_playlist(conn, "mix").unwrap();
    store::delete_playlist(conn, "mix").unwrap();
    assert_eq!(
        store::removed_playlists(conn).unwrap(),
        vec![(id, "mix".to_owned())]
    );
}

// Prevents: a file that comes back getting a new id, which would orphan the
// mapping and write a second card playlist next to the old one.
#[test]
fn a_file_coming_back_revives_the_same_id_and_mapping() {
    let (env, id, dev) = mapped_mix();
    let conn = env.store.conn();
    std::fs::remove_file(env.playlist_file("mix")).unwrap();
    playlists::sync_from_disk(conn, &env.cfg, false).unwrap();
    assert_eq!(store::removed_playlists(conn).unwrap().len(), 1);

    load(&env, &[("mix", "b.mp3")]);
    assert_eq!(env.playlist_id("mix"), Some(id));
    assert!(store::removed_playlists(conn).unwrap().is_empty());
    let removed_at: Option<i64> = conn
        .query_row(
            "SELECT removed_at FROM playlists WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(removed_at, None);
    assert_eq!(mappings(conn, dev), vec![(id, "Mix.m3u8".to_owned())]);
    let item = env.indexed_items("mix");
    assert_eq!(item.len(), 1);
    assert!(item[0].1.is_some(), "the entry resolves to a track");
}

#[test]
fn creating_a_playlist_over_a_tombstone_revives_it_too() {
    let (env, id, dev) = mapped_mix();
    let conn = env.store.conn();
    playlists::delete(conn, &env.cfg, "mix").unwrap();
    playlists::create(conn, &env.cfg, "mix").unwrap();
    assert_eq!(env.playlist_id("mix"), Some(id));
    assert!(store::removed_playlists(conn).unwrap().is_empty());
    assert_eq!(mappings(conn, dev).len(), 1);
}

// Prevents: renaming a playlist onto a deleted one's name failing on the
// unique name, or silently dropping the tombstone and with it the only record
// of which card playlist to delete.
#[test]
fn renaming_onto_a_tombstones_name_sets_the_tombstone_aside_and_keeps_its_mapping() {
    let (env, mix, dev) = mapped_mix();
    let conn = env.store.conn();
    let other = env.playlist_id("other").unwrap();
    map(conn, dev, other, "other.m3u8");
    playlists::delete(conn, &env.cfg, "mix").unwrap();

    playlists::rename(conn, &env.cfg, "other", "mix").unwrap();

    // The renamed playlist keeps its id, and so its card playlist.
    assert_eq!(env.playlist_id("mix"), Some(other));
    assert!(env.playlist_file("mix").exists());
    assert!(!env.playlist_file("other").exists());
    let removed = store::removed_playlists(conn).unwrap();
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0].0, mix);
    assert_eq!(removed[0].1, format!(".removed-{mix}"));
    assert_eq!(
        mappings(conn, dev),
        vec![
            (mix, "Mix.m3u8".to_owned()),
            (other, "other.m3u8".to_owned())
        ]
    );
    assert_eq!(live_names(conn), vec!["mix"]);
    assert_eq!(
        env.indexed_items("mix").len(),
        1,
        "the renamed one keeps its items"
    );

    // The name is free for another round: delete it and rename onto it again.
    playlists::delete(conn, &env.cfg, "mix").unwrap();
    assert_eq!(store::removed_playlists(conn).unwrap().len(), 2);
}

// Prevents: renaming onto the name of a live playlist replacing it.
#[test]
fn renaming_onto_a_live_playlist_is_refused_and_changes_nothing() {
    let (env, id, dev) = mapped_mix();
    let conn = env.store.conn();
    let other = env.playlist_id("other").unwrap();
    let r = playlists::rename(conn, &env.cfg, "other", "mix");
    assert!(
        matches!(r, Err(Error::PlaylistExists(ref n)) if n == "mix"),
        "{r:?}"
    );
    assert_eq!(env.playlist_id("mix"), Some(id));
    assert_eq!(env.playlist_id("other"), Some(other));
    assert!(store::removed_playlists(conn).unwrap().is_empty());
    assert_eq!(mappings(conn, dev), vec![(id, "Mix.m3u8".to_owned())]);
}

// Prevents: a restored card playlist whose library playlist is gone not being
// deleted again by the next sync.
#[test]
fn insert_tombstone_uses_the_name_when_free_and_an_internal_name_when_taken() {
    let (env, live, dev) = mapped_mix();
    let conn = env.store.conn();
    let free = store::insert_tombstone(conn, "ghost").unwrap();
    let taken = store::insert_tombstone(conn, "mix").unwrap();
    map(conn, dev, free, "Ghost.m3u8");
    assert_ne!(free, taken);
    let mut removed = store::removed_playlists(conn).unwrap();
    removed.sort();
    let mut want = vec![
        (free, "ghost".to_owned()),
        (taken, format!(".removed-{taken}")),
    ];
    want.sort();
    assert_eq!(removed, want);
    // The live playlist is untouched and the tombstones are not shown.
    assert_eq!(env.playlist_id("mix"), Some(live));
    assert_eq!(live_names(conn), vec!["mix", "other"]);
}
