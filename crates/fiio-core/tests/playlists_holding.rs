//! `store::playlists_holding`: which playlists a library track is in, for
//! the window's "this track is in N playlists" warning before a removal.

mod common;

use common::{Env, Kind};
use fiio_core::store::devices::{self, PlaylistMapping};
use fiio_core::{playlists, store};

struct World {
    env: Env,
    a: i64,
    b: i64,
}

impl World {
    fn new() -> Self {
        let mut env = Env::new();
        let pa = env.add("a.mp3", Kind::Mp3, 1);
        let pb = env.add("b.mp3", Kind::Mp3, 2);
        env.scan();
        let (a, b) = (env.track(&pa).id, env.track(&pb).id);
        World { env, a, b }
    }

    /// Writes the playlist `name` holding `files` (names under the music
    /// folder, repeats and a missing file allowed) and loads it.
    fn playlist(&self, name: &str, files: &[&str]) {
        let text: String = files
            .iter()
            .map(|f| format!("{}\n", self.env.music.join(f).display()))
            .collect();
        self.env.write_playlist(name, &text);
        playlists::sync_from_disk(self.env.store.conn(), &self.env.cfg, true).unwrap();
    }

    fn holding(&self, track: i64) -> Vec<String> {
        store::playlists_holding(self.env.store.conn(), track).unwrap()
    }
}

// Prevents: a track's playlists being listed more than once (a playlist
// holding it twice), out of name order, or including playlists that do not
// hold it.
#[test]
fn a_track_is_listed_in_each_playlist_holding_it_once_by_name() {
    let w = World::new();
    w.playlist("Rock", &["a.mp3", "b.mp3"]);
    w.playlist("Mix", &["a.mp3", "a.mp3"]);
    w.playlist("Jazz", &["a.mp3"]);
    w.playlist("Only B", &["b.mp3"]);
    assert_eq!(w.holding(w.a), ["Jazz", "Mix", "Rock"]);
    assert_eq!(w.holding(w.b), ["Only B", "Rock"]);
}

// Prevents: an unknown track or one in no playlist reporting something, and
// a playlist of unresolved entries holding everything.
#[test]
fn a_track_in_no_playlist_or_unknown_is_in_none() {
    let w = World::new();
    w.playlist("Lonely", &["gone/ghost.mp3"]);
    assert!(w.holding(w.a).is_empty());
    assert!(w.holding(w.b).is_empty());
    assert!(w.holding(w.b + 1000).is_empty());
}

// Prevents: a playlist deleted but kept as a tombstone (until the card's
// file is deleted at the next sync) still being listed as holding the track:
// the user deleted it.
#[test]
fn a_deleted_playlist_kept_as_a_tombstone_is_not_listed() {
    let w = World::new();
    w.playlist("Keep", &["a.mp3"]);
    w.playlist("Drop", &["a.mp3"]);
    let conn = w.env.store.conn();
    let dev = devices::upsert_device(conn, "folder:x", None, None).unwrap();
    let drop_id = w.env.playlist_id("Drop").unwrap();
    devices::replace_playlist_mappings(
        conn,
        dev,
        &[PlaylistMapping {
            playlist_id: drop_id,
            card_name: "Drop.m3u8".into(),
        }],
    )
    .unwrap();
    playlists::delete(conn, &w.env.cfg, "Drop").unwrap();
    // The tombstone is still in the index, as the next sync needs it.
    assert_eq!(store::removed_playlists(conn).unwrap().len(), 1);
    assert_eq!(w.holding(w.a), ["Keep"]);
}

// Prevents: the answer going stale after an edit: it follows the entries.
#[test]
fn removing_the_entry_takes_the_playlist_out() {
    let w = World::new();
    w.playlist("Rock", &["a.mp3", "b.mp3"]);
    assert_eq!(w.holding(w.a), ["Rock"]);
    playlists::remove(w.env.store.conn(), &w.env.cfg, "Rock", &[1]).unwrap();
    assert!(w.holding(w.a).is_empty());
    assert_eq!(w.holding(w.b), ["Rock"]);
}
