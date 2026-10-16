//! `Data` without a display. Nothing here sees the real config, the real
//! index or the real X3: `open` runs in a child process whose home and XDG
//! folders are a temp dir (the test threads share one environment, so it
//! cannot be changed in place), and the other tests run on a hand-built
//! `Data` whose `device.mountpoint` is a temp dir.

use std::path::{Path, PathBuf};
use std::process::Command;

use fiio_core::config::Config;
use fiio_core::store::devices;
use fiio_core::store::{self, FileRecord, Store};
use fiio_core::tags::{AudioInfo, FileKind, Tags};

use super::*;

const KEY: &str = "usb:TEST";

/// Paths the tests never use: the sync worker is not run here.
fn unused_paths() -> fiio_core::config::Paths {
    fiio_core::config::Paths {
        config_file: PathBuf::from("/nonexistent/fiio-tunes/config.toml"),
        data_dir: PathBuf::from("/nonexistent/fiio-tunes"),
    }
}

// ------------------------------------------------------------------ open

const CHILD: &str = "FIIO_TUNES_GUI_TEST_CHILD";

fn child(test: &str, home: &Path) -> std::process::Output {
    Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--ignored", "--nocapture"])
        .env(CHILD, "1")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .output()
        .unwrap()
}

fn run_child(test: &str, home: &Path) {
    let out = child(test, home);
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("1 passed"),
        "child failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn in_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

#[test]
fn open_without_a_config_says_so_and_creates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    run_child("data::tests::open_without_config_child", tmp.path());
    assert!(!tmp.path().join("data").exists(), "no index may be created");
    assert!(!tmp.path().join("config").exists());
}

#[test]
#[ignore = "run in a child process by open_without_a_config_says_so_and_creates_nothing"]
fn open_without_config_child() {
    assert!(in_child(), "only meant to run as a child");
    let err = Data::open().err().expect("open must fail").to_string();
    assert!(err.contains("no config"), "{err}");
    assert!(err.contains("fiio-tunes init"), "{err}");
}

fn write_config(home: &Path, playlists: &Path, mountpoint: &Path) {
    let mut c = Config::default();
    c.library.roots = vec![home.join("music")];
    c.playlists.dir = playlists.to_owned();
    c.device.mountpoint = mountpoint.to_owned();
    c.save(&home.join("config/fiio-tunes/config.toml")).unwrap();
}

#[test]
fn open_with_a_config_gives_an_empty_library_and_no_device() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(home.join("lists")).unwrap();
    std::fs::create_dir_all(home.join("nomount")).unwrap();
    write_config(&home, &home.join("lists"), &home.join("nomount"));
    run_child("data::tests::open_ok_child", &home);
    assert!(home.join("data/fiio-tunes/fiio-tunes.sqlite").is_file());
}

#[test]
#[ignore = "run in a child process by open_with_a_config_gives_an_empty_library_and_no_device"]
fn open_ok_child() {
    assert!(in_child());
    let d = Data::open().unwrap();
    assert_eq!(d.warning, None);
    assert!(d.library().unwrap().is_empty());
    assert!(d.playlist_names().unwrap().is_empty());
    assert!(d.device().unwrap().is_none());
}

#[test]
fn an_unreadable_playlists_folder_is_a_warning_not_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(home.join("nomount")).unwrap();
    // The playlists folder does not exist, as when its disk is unmounted.
    write_config(&home, &home.join("absent-lists"), &home.join("nomount"));
    run_child("data::tests::open_warning_child", &home);
}

#[test]
#[ignore = "run in a child process by an_unreadable_playlists_folder_is_a_warning_not_an_error"]
fn open_warning_child() {
    assert!(in_child());
    let d = Data::open().expect("an unreadable folder must not be fatal");
    let w = d.warning.clone().expect("the failure is reported");
    assert!(w.contains("playlists folder"), "{w}");
    assert!(d.library().unwrap().is_empty());
}

// ------------------------------------------------------ playlist editing

/// A `Data` whose playlists folder and tracks live in a temp dir.
struct Sandbox {
    _tmp: tempfile::TempDir,
    data: Data,
    lists: PathBuf,
    tracks: Vec<PathBuf>,
}

fn sandbox(n: usize) -> Sandbox {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let lists = root.join("lists");
    std::fs::create_dir_all(&lists).unwrap();
    let mut config = Config::default();
    config.library.roots = vec![root.join("music")];
    config.playlists.dir = lists.clone();
    config.device.mountpoint = root.join("nomount");
    let store = Store::open_in_memory().unwrap();
    let mut tracks = Vec::new();
    for i in 0..n {
        let path = root.join(format!("music/t{i}.mp3"));
        let key = path.to_str().unwrap().to_owned();
        let f = FileRecord {
            path: path.clone(),
            path_key: key,
            size: 1,
            mtime_ns: 0,
            hash: [i as u8 + 1; 32],
            info: AudioInfo {
                kind: FileKind::Mp3,
                duration_ms: Some(1000),
                bitrate: None,
                samplerate: None,
            },
            tags: Tags::default(),
            tag_error: None,
            drm: false,
        };
        store::insert_track(store.conn(), &f, 0).unwrap();
        tracks.push(path);
    }
    Sandbox {
        _tmp: tmp,
        data: Data {
            config: config.into(),
            paths: unused_paths(),
            store,
            warning: None,
        },
        lists,
        tracks,
    }
}

impl Sandbox {
    fn file(&self, name: &str) -> PathBuf {
        self.lists.join(format!("{name}.m3u8"))
    }
    fn text(&self, name: &str) -> String {
        std::fs::read_to_string(self.file(name)).unwrap()
    }
    /// Track paths of a playlist as the window would show them.
    fn shown(&self, name: &str) -> Vec<PathBuf> {
        self.data
            .playlist(name)
            .unwrap()
            .into_iter()
            .map(|r| r.path)
            .collect()
    }
    fn pick(&self, idx: &[usize]) -> Vec<PathBuf> {
        idx.iter().map(|&i| self.tracks[i].clone()).collect()
    }
}

#[test]
fn playlists_dir_is_the_configured_folder() {
    let sb = sandbox(0);
    assert_eq!(sb.data.playlists_dir(), sb.lists.as_path());
}

#[test]
fn create_makes_an_empty_playlist_that_is_listed_and_a_second_create_fails() {
    let sb = sandbox(1);
    sb.data.create("mix").unwrap();
    assert_eq!(sb.text("mix"), "#EXTM3U\n");
    assert_eq!(sb.data.playlist_names().unwrap(), ["mix"]);
    assert!(sb.data.playlist("mix").unwrap().is_empty());

    sb.data.add("mix", &sb.pick(&[0]), None).unwrap();
    let before = sb.text("mix");
    assert!(sb.data.create("mix").is_err());
    assert_eq!(sb.text("mix"), before, "the existing playlist is untouched");
    assert!(sb.data.create("bad/name").is_err());
    assert_eq!(std::fs::read_dir(&sb.lists).unwrap().count(), 1);
}

#[test]
fn add_appends_or_inserts_and_the_window_rows_follow() {
    let sb = sandbox(4);
    sb.data.create("mix").unwrap();
    sb.data.add("mix", &sb.pick(&[0, 1]), None).unwrap();
    assert_eq!(sb.shown("mix"), sb.pick(&[0, 1]));
    sb.data.add("mix", &sb.pick(&[2, 3]), Some(2)).unwrap();
    assert_eq!(sb.shown("mix"), sb.pick(&[0, 2, 3, 1]));
    let rows = sb.data.playlist("mix").unwrap();
    assert_eq!(
        rows.iter().map(|r| r.position).collect::<Vec<_>>(),
        [Some(1), Some(2), Some(3), Some(4)]
    );
    // the file says the same as the rows
    let text = sb.text("mix");
    let on_disk: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
    assert_eq!(
        on_disk,
        sb.pick(&[0, 2, 3, 1])
            .iter()
            .map(|p| p.to_str().unwrap())
            .collect::<Vec<_>>()
    );
}

#[test]
fn add_of_an_unindexed_file_or_to_an_unknown_playlist_fails_and_writes_nothing() {
    let sb = sandbox(1);
    sb.data.create("mix").unwrap();
    let before = sb.text("mix");
    let stranger = sb.lists.join("../music/stranger.mp3");
    assert!(sb.data.add("mix", &[stranger], None).is_err());
    assert!(sb.data.add("nope", &sb.pick(&[0]), None).is_err());
    assert!(sb.data.add("mix", &sb.pick(&[0]), Some(5)).is_err());
    assert_eq!(sb.text("mix"), before);
    assert!(!sb.file("nope").exists());
}

#[test]
fn move_entries_reorders_the_file_and_the_rows() {
    let sb = sandbox(5);
    sb.data.create("mix").unwrap();
    sb.data
        .add("mix", &sb.pick(&[0, 1, 2, 3, 4]), None)
        .unwrap();
    sb.data.move_entries("mix", &[2, 4], 1).unwrap();
    assert_eq!(sb.shown("mix"), sb.pick(&[1, 3, 0, 2, 4]));
    sb.data.move_entries("mix", &[1], 6).unwrap();
    assert_eq!(sb.shown("mix"), sb.pick(&[3, 0, 2, 4, 1]));

    // a bad position changes nothing
    let before = sb.text("mix");
    assert!(sb.data.move_entries("mix", &[9], 1).is_err());
    assert!(sb.data.move_entries("mix", &[1], 7).is_err());
    assert_eq!(sb.text("mix"), before);
}

#[test]
fn remove_drops_exactly_those_entries() {
    let sb = sandbox(4);
    sb.data.create("mix").unwrap();
    sb.data.add("mix", &sb.pick(&[0, 1, 2, 3]), None).unwrap();
    sb.data.remove("mix", &[2, 4, 2]).unwrap();
    assert_eq!(sb.shown("mix"), sb.pick(&[0, 2]));
    let before = sb.text("mix");
    assert!(sb.data.remove("mix", &[1, 3]).is_err());
    assert_eq!(sb.text("mix"), before, "a bad position removes nothing");
    // the library still has all four tracks
    assert_eq!(sb.data.library().unwrap().len(), 4);
}

#[test]
fn rename_moves_the_file_and_keeps_the_entries() {
    let sb = sandbox(2);
    sb.data.create("old").unwrap();
    sb.data.add("old", &sb.pick(&[1, 0]), None).unwrap();
    let text = sb.text("old");
    sb.data.rename("old", "new").unwrap();
    assert!(!sb.file("old").exists());
    assert_eq!(sb.text("new"), text);
    assert_eq!(sb.data.playlist_names().unwrap(), ["new"]);
    assert_eq!(sb.shown("new"), sb.pick(&[1, 0]));

    // onto an existing one, from an unknown one: nothing changes
    sb.data.create("other").unwrap();
    assert!(sb.data.rename("new", "other").is_err());
    assert!(sb.data.rename("ghost", "x").is_err());
    assert_eq!(sb.text("new"), text);
    assert_eq!(sb.text("other"), "#EXTM3U\n");
    assert!(!sb.file("x").exists());
}

#[test]
fn delete_removes_the_file_and_the_listing_but_not_the_tracks() {
    let sb = sandbox(2);
    sb.data.create("keep").unwrap();
    sb.data.create("gone").unwrap();
    sb.data.add("gone", &sb.pick(&[0, 1]), None).unwrap();
    let keep = sb.text("keep");
    sb.data.delete("gone").unwrap();
    assert!(!sb.file("gone").exists());
    assert_eq!(sb.data.playlist_names().unwrap(), ["keep"]);
    assert_eq!(sb.text("keep"), keep);
    assert_eq!(sb.data.library().unwrap().len(), 2);
    assert!(sb.data.delete("gone").is_err());
    assert!(sb.data.playlist("gone").is_err());
}

#[test]
fn playlist_mtime_follows_every_kind_of_edit_and_is_none_for_unknown_playlists() {
    let sb = sandbox(3);
    assert_eq!(sb.data.playlist_mtime("mix"), None);
    sb.data.create("mix").unwrap();
    let m0 = sb.data.playlist_mtime("mix").unwrap();
    assert!(m0 > 0);
    // Make the previous mtime unreachable by "now", whatever the clock grain.
    let set_past = |secs: u64| {
        std::fs::File::options()
            .write(true)
            .open(sb.file("mix"))
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .unwrap();
    };
    set_past(1_000_000);
    let past = sb.data.playlist_mtime("mix").unwrap();
    assert_ne!(past, m0);

    sb.data.add("mix", &sb.pick(&[0, 1, 2]), None).unwrap();
    let after_add = sb.data.playlist_mtime("mix").unwrap();
    assert_ne!(after_add, past);
    set_past(1_000_000);
    sb.data.move_entries("mix", &[3], 1).unwrap();
    let after_move = sb.data.playlist_mtime("mix").unwrap();
    assert_ne!(after_move, past);
    set_past(1_000_000);
    sb.data.remove("mix", &[1]).unwrap();
    assert_ne!(sb.data.playlist_mtime("mix").unwrap(), past);

    // equals what the file system says, so a monitor can compare it
    let fs_mtime = std::fs::metadata(sb.file("mix"))
        .unwrap()
        .modified()
        .unwrap();
    let ns = fs_mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    assert_eq!(sb.data.playlist_mtime("mix"), Some(ns));
    sb.data.delete("mix").unwrap();
    assert_eq!(sb.data.playlist_mtime("mix"), None);
}

#[test]
fn reload_playlists_picks_up_outside_edits_and_deletions_and_writes_no_file() {
    let sb = sandbox(3);
    sb.data.create("mix").unwrap();
    sb.data.add("mix", &sb.pick(&[0]), None).unwrap();
    sb.data.create("doomed").unwrap();

    // Another program rewrites one list, adds one, deletes one.
    let body = |idx: &[usize]| {
        let mut s = String::from("#EXTM3U\n");
        for p in sb.pick(idx) {
            s.push_str(&format!("{}\n", p.display()));
        }
        s
    };
    std::fs::write(sb.file("mix"), body(&[2, 1])).unwrap();
    std::fs::File::options()
        .write(true)
        .open(sb.file("mix"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
        .unwrap();
    std::fs::write(sb.file("fresh"), body(&[0])).unwrap();
    std::fs::remove_file(sb.file("doomed")).unwrap();
    let mix_text = sb.text("mix");

    let report = sb.data.reload_playlists().unwrap();
    assert_eq!(report.removed, ["doomed"]);
    assert!(report.loaded >= 2, "{report:?}");
    assert_eq!(sb.data.playlist_names().unwrap(), ["fresh", "mix"]);
    assert_eq!(sb.shown("mix"), sb.pick(&[2, 1]));
    assert_eq!(sb.text("mix"), mix_text, "reload never rewrites a file");
    assert!(!sb.file("doomed").exists());

    // a second reload with nothing changed loads nothing
    assert_eq!(sb.data.reload_playlists().unwrap().loaded, 0);
}

// ------------------------------------------------------------- selection

fn picked(kind: Scope, names: &[&str]) -> devices::DeviceScope {
    devices::DeviceScope {
        kind,
        playlists: names.iter().map(|n| (*n).to_owned()).collect(),
    }
}

#[test]
fn selection_summary_for_every_kind_in_singular_and_plural() {
    let cases = [
        (picked(Scope::All, &[]), "syncs the whole library"),
        (picked(Scope::Playlists, &["A"]), "syncs 1 playlist"),
        (
            picked(Scope::Playlists, &["A", "B", "C"]),
            "syncs 3 playlists",
        ),
        (picked(Scope::Except, &["A"]), "syncs all but 1 playlist"),
        (
            picked(Scope::Except, &["A", "B"]),
            "syncs all but 2 playlists",
        ),
        // Excluding nothing is the whole library.
        (picked(Scope::Except, &[]), "syncs the whole library"),
        // Names are not shown for All.
        (picked(Scope::All, &["A"]), "syncs the whole library"),
    ];
    for (s, want) in cases {
        assert_eq!(selection_summary(&s), want, "{s:?}");
    }
}

/// Two playlists: "has" holds track 0, "empty" holds nothing.
fn with_playlists() -> Sandbox {
    let sb = sandbox(2);
    sb.data.create("has").unwrap();
    sb.data.add("has", &sb.pick(&[0]), None).unwrap();
    sb.data.create("empty").unwrap();
    sb
}

fn one_device(sb: &Sandbox) -> i64 {
    devices::upsert_device(sb.data.store.conn(), KEY, Some("Mine"), None).unwrap()
}

#[test]
fn set_selection_stores_a_good_selection_and_selection_reads_it_back() {
    let sb = with_playlists();
    let id = one_device(&sb);
    let (s, own) = sb.data.selection(id).unwrap();
    assert!(!own, "follows the config until one is set");
    assert_eq!(s.kind, Scope::All);

    sb.data
        .set_selection(id, &picked(Scope::Except, &["has"]))
        .unwrap();
    let (s, own) = sb.data.selection(id).unwrap();
    assert!(own);
    assert_eq!(s, picked(Scope::Except, &["has"]));
    assert_eq!(devices::scope(sb.data.store.conn(), id).unwrap(), Some(s));

    // Excluding everything is allowed, and so is keeping one playlist.
    sb.data
        .set_selection(id, &picked(Scope::Playlists, &["has", "empty"]))
        .unwrap();
    assert_eq!(
        sb.data.selection(id).unwrap().0,
        picked(Scope::Playlists, &["empty", "has"])
    );
}

#[test]
fn set_selection_refuses_a_bad_one_and_keeps_the_old() {
    let sb = with_playlists();
    let id = one_device(&sb);
    let good = picked(Scope::Playlists, &["has"]);
    sb.data.set_selection(id, &good).unwrap();
    let bad = [
        ("gone", picked(Scope::Playlists, &["has", "gone"]), "gone"),
        ("gone, except", picked(Scope::Except, &["gone"]), "gone"),
        ("no names", picked(Scope::Playlists, &[]), "names none"),
        (
            "no track",
            picked(Scope::Playlists, &["empty"]),
            "no library track",
        ),
    ];
    for (what, s, why) in bad {
        let e = sb.data.set_selection(id, &s).unwrap_err().to_string();
        assert!(e.contains(why), "{what}: {e}");
        assert_eq!(
            sb.data.selection(id).unwrap(),
            (good.clone(), true),
            "{what}"
        );
    }
}

#[test]
fn selection_inputs_hold_the_library_and_the_playlists_as_indexed() {
    let sb = with_playlists();
    let (tracks, lists) = sb.data.selection_inputs().unwrap();
    assert_eq!(tracks.len(), 2);
    let mut names: Vec<(&str, usize)> = lists
        .iter()
        .map(|p| (p.name.as_str(), p.members.len()))
        .collect();
    names.sort();
    assert_eq!(names, [("empty", 0), ("has", 1)]);
}

fn track_id_of(sb: &Sandbox, i: usize) -> i64 {
    store::all_tracks(sb.data.store.conn())
        .unwrap()
        .into_iter()
        .find(|t| t.path == sb.tracks[i])
        .unwrap()
        .id
}

#[test]
fn playlists_holding_follows_the_playlists_edited_through_data() {
    let sb = sandbox(3);
    let (t0, t1, t2) = (
        track_id_of(&sb, 0),
        track_id_of(&sb, 1),
        track_id_of(&sb, 2),
    );
    for name in ["zulu", "alpha", "mike"] {
        sb.data.create(name).unwrap();
    }
    sb.data.add("zulu", &sb.pick(&[0, 1]), None).unwrap();
    sb.data.add("alpha", &sb.pick(&[0]), None).unwrap();
    // twice in one playlist: still listed once
    sb.data.add("mike", &sb.pick(&[0, 0]), None).unwrap();

    assert_eq!(
        sb.data.playlists_holding(t0).unwrap(),
        ["alpha", "mike", "zulu"]
    );
    assert_eq!(sb.data.playlists_holding(t1).unwrap(), ["zulu"]);
    assert!(sb.data.playlists_holding(t2).unwrap().is_empty());

    // removing the entries, or the playlist, takes it out of the answer
    sb.data.remove("alpha", &[1]).unwrap();
    sb.data.delete("zulu").unwrap();
    assert_eq!(sb.data.playlists_holding(t0).unwrap(), ["mike"]);
    assert!(sb.data.playlists_holding(t1).unwrap().is_empty());
}

// ---------------------------------------------------------------- device()
//
// The card is a folder in the sandbox's temp dir. `device()` reads
// `/proc/self/mountinfo` through `device::locate`, which describes a folder
// that is not a mount as having no source and no USB device. With the
// mountpoint empty it looks for mounted cards, which `FIIO_TUNES_NO_DETECT`
// (set for everything cargo runs) turns off: the tests that rely on that
// check it first, so a run outside cargo fails instead of looking at the
// real mounts. A FiiO card on a real USB device cannot be made in a test:
// "connected and a FiiO card" is only covered through `allow_folder`.

/// A folder `card` in the sandbox, set as the card with `allow_folder`
/// `allow`. Returns its path as `locate` reports it.
fn card_in(sb: &Sandbox, allow: bool) -> PathBuf {
    let card = sb._tmp.path().canonicalize().unwrap().join("card");
    std::fs::create_dir_all(&card).unwrap();
    let mut c = sb.data.config.borrow_mut();
    c.device.mountpoint = card.clone();
    c.device.allow_folder = allow;
    card
}

fn no_card(sb: &Sandbox) {
    assert!(
        fiio_core::device::detection_off(),
        "run through cargo: .cargo/config.toml sets FIIO_TUNES_NO_DETECT"
    );
    sb.data.config.borrow_mut().device.mountpoint = PathBuf::new();
}

/// An adopted X3 `key` with one track mapped; its device id.
fn adopted(sb: &Sandbox, key: &str, name: Option<&str>) -> i64 {
    let conn = sb.data.store.conn();
    let id = devices::upsert_device(conn, key, name, None).unwrap();
    let tracks = [devices::TrackMapping {
        track_id: track_id_of(sb, 0),
        card_path: "Music/t0.mp3".into(),
        synced_hash: [1; 32],
        synced_size: 1,
    }];
    devices::replace_mappings(conn, id, &tracks, &[]).unwrap();
    id
}

fn key_of_folder(card: &Path) -> String {
    format!("folder:{}", card.display())
}

// Prevents: a window that shows a card when there is none, or an X3 when
// nothing was ever adopted.
#[test]
fn device_is_none_with_no_card_and_nothing_adopted() {
    let sb = sandbox(1);
    no_card(&sb);
    assert!(sb.data.device().unwrap().is_none());
    // A mountpoint that does not exist is not a card either, whatever
    // allow_folder says.
    {
        let mut c = sb.data.config.borrow_mut();
        c.device.mountpoint = sb._tmp.path().join("not-there");
        c.device.allow_folder = true;
    }
    assert!(sb.data.device().unwrap().is_none());
}

// Prevents: a plain folder (a mistyped mountpoint, a copy of a card) being
// shown as the connected X3: without `allow_folder` only a FiiO card counts.
// The adopted X3 is shown instead, not connected.
#[test]
fn a_folder_is_not_a_card_without_allow_folder() {
    let sb = sandbox(1);
    card_in(&sb, false);
    assert!(sb.data.device().unwrap().is_none());

    adopted(&sb, "usb:SER1", Some("Mine"));
    let d = sb.data.device().unwrap().expect("the adopted X3");
    assert!(!d.connected, "{d:?}");
    assert!(d.adopted);
    assert_eq!(d.name, "Mine");
    assert_eq!((d.free, d.capacity), (None, None));
}

// Prevents: a connected card that is not adopted being shown as adopted, or
// hidden behind an adopted X3 that is elsewhere: the card in the player's
// place is what the window shows, with Adopt… on it.
#[test]
fn a_connected_card_not_adopted_is_shown_even_when_another_x3_is_adopted() {
    let sb = sandbox(1);
    card_in(&sb, true);
    let d = sb.data.device().unwrap().expect("the card");
    assert!(d.connected && !d.adopted, "{d:?}");
    assert_eq!((d.id, d.selection.clone()), (None, None));
    assert_eq!((d.mapped_tracks, d.last_sync), (0, None));
    assert!(d.capacity.is_some_and(|c| c > 0), "{d:?}");
    assert!(d.free.is_some());

    adopted(&sb, "usb:ELSEWHERE", Some("Elsewhere"));
    let d = sb.data.device().unwrap().expect("the card");
    assert!(d.connected && !d.adopted, "{d:?}");
    assert_eq!(d.id, None);
}

// Prevents: the X3 connected and adopted showing the wrong numbers: its row,
// its mapped tracks, its last sync, its selection and the card's space.
#[test]
fn a_connected_adopted_card_shows_its_row_mappings_and_space() {
    let sb = sandbox(1);
    let card = card_in(&sb, true);
    let id = adopted(&sb, &key_of_folder(&card), Some("My X3"));
    devices::set_last_sync(sb.data.store.conn(), id, 1234).unwrap();

    let d = sb.data.device().unwrap().expect("the X3");
    assert!(d.connected && d.adopted, "{d:?}");
    assert_eq!(d.name, "My X3");
    assert_eq!(d.id, Some(id));
    assert_eq!(d.mapped_tracks, 1);
    assert_eq!(d.last_sync, Some(1234));
    let (capacity, free) = device::space(&card).unwrap();
    assert_eq!(d.capacity, Some(capacity));
    assert!(d.free.is_some_and(|f| f <= capacity), "{d:?} free {free}");
    // Follows the config until the X3 has a selection of its own.
    let follows = devices::DeviceScope {
        kind: Scope::All,
        playlists: vec![],
    };
    assert_eq!(d.selection, Some(selection_summary(&follows)));

    // A name that was not read is "X3".
    let sb2 = sandbox(1);
    let card2 = card_in(&sb2, true);
    adopted(&sb2, &key_of_folder(&card2), None);
    assert_eq!(sb2.data.device().unwrap().unwrap().name, "X3");
}

// Prevents: the selection shown being the config's when the X3 has its own,
// or the own one when it has none: the summary comes from the same scope
// the sync would use.
#[test]
fn the_selection_shown_is_the_x3s_own_else_the_configs() {
    let sb = sandbox(1);
    let card = card_in(&sb, true);
    let id = adopted(&sb, &key_of_folder(&card), None);
    {
        let mut c = sb.data.config.borrow_mut();
        c.sync.scope = Scope::Except;
        c.sync.playlists = vec!["Podcasts".into()];
    }
    let configs = picked(Scope::Except, &["Podcasts"]);
    let own = picked(Scope::Playlists, &["A", "B"]);
    assert_ne!(selection_summary(&configs), selection_summary(&own));
    assert_eq!(
        sb.data.device().unwrap().unwrap().selection,
        Some(selection_summary(&configs))
    );
    devices::set_scope(sb.data.store.conn(), id, Some(&own)).unwrap();
    assert_eq!(
        sb.data.device().unwrap().unwrap().selection,
        Some(selection_summary(&own))
    );
}

// Prevents: the card being recognised by where it is mounted instead of by
// who it is: a card whose manifest names the device key is that device
// wherever it is plugged, and a manifest naming another key is another card
// even where the folder was adopted before.
#[test]
fn the_card_is_known_by_the_key_in_its_manifest_not_by_its_path() {
    let sb = sandbox(1);
    let card = card_in(&sb, true);
    std::fs::create_dir_all(card.join(".fiio-tunes")).unwrap();
    std::fs::write(
        card.join(".fiio-tunes/manifest.json"),
        r#"{"device_key": "usb:SER1", "card_id": "c1"}"#,
    )
    .unwrap();
    // Adopted by the path only: the manifest says the card is someone else.
    adopted(&sb, &key_of_folder(&card), Some("By path"));
    let d = sb.data.device().unwrap().expect("the card");
    assert!(d.connected && !d.adopted, "{d:?}");

    // Adopted under the manifest's key: that is this card.
    let id = adopted(&sb, "usb:SER1", Some("By manifest"));
    let d = sb.data.device().unwrap().expect("the card");
    assert!(d.connected && d.adopted, "{d:?}");
    assert_eq!((d.id, d.name.as_str()), (Some(id), "By manifest"));
}

// Prevents: an unreadable manifest being taken for "no manifest" and the
// card shown as a new one: the card is shown as connected, not adopted, and
// nothing is written to it.
#[test]
fn a_manifest_that_does_not_parse_shows_the_card_as_not_adopted() {
    let sb = sandbox(1);
    let card = card_in(&sb, true);
    std::fs::create_dir_all(card.join(".fiio-tunes")).unwrap();
    std::fs::write(card.join(".fiio-tunes/manifest.json"), "{ not json").unwrap();
    adopted(&sb, &key_of_folder(&card), None);
    let before = std::fs::read(card.join(".fiio-tunes/manifest.json")).unwrap();

    let d = sb.data.device().unwrap().expect("the card");
    assert!(d.connected && !d.adopted, "{d:?}");
    assert_eq!(
        std::fs::read(card.join(".fiio-tunes/manifest.json")).unwrap(),
        before
    );
}

// Prevents: with no card connected, the window showing none of the adopted
// X3s, or the wrong one: the first adopted, not connected, with no space.
#[test]
fn with_no_card_the_first_adopted_x3_is_shown_not_connected() {
    let sb = sandbox(1);
    no_card(&sb);
    let first = adopted(&sb, "usb:ONE", Some("One"));
    adopted(&sb, "usb:TWO", Some("Two"));
    let d = sb.data.device().unwrap().expect("an adopted X3");
    assert!(!d.connected && d.adopted, "{d:?}");
    assert_eq!((d.id, d.name.as_str()), (Some(first), "One"));
    assert_eq!(d.mapped_tracks, 1);
    assert_eq!((d.free, d.capacity), (None, None));
}

// -------------------------------------------------------------- reload_config

/// The sandbox with a config file at a path of its own, holding what the
/// sandbox's config holds.
fn with_config_file(sb: &mut Sandbox) -> PathBuf {
    let file = sb._tmp.path().join("config/config.toml");
    sb.data.paths.config_file = file.clone();
    sb.data.config.borrow().save(&file).unwrap();
    file
}

// Prevents: a scope copied by the import (written to the config file by the
// worker) not showing in the window, which holds its own copy of the config:
// everything read through `config()` and the folder accessors follows.
#[test]
fn reload_config_picks_up_what_another_writer_saved() {
    let mut sb = sandbox(1);
    let file = with_config_file(&mut sb);
    assert_eq!(sb.data.config().sync.scope, Scope::All);

    let mut changed = sb.data.config();
    changed.sync.scope = Scope::Playlists;
    changed.sync.playlists = vec!["Rock".into()];
    let elsewhere = sb._tmp.path().join("other-lists");
    std::fs::create_dir_all(&elsewhere).unwrap();
    changed.playlists.dir = elsewhere.clone();
    changed.save(&file).unwrap();
    // Not before it is asked for.
    assert_eq!(sb.data.config().sync.scope, Scope::All);

    sb.data.reload_config().unwrap();
    assert_eq!(sb.data.config().sync.scope, Scope::Playlists);
    assert_eq!(sb.data.config().sync.playlists, ["Rock"]);
    assert_eq!(sb.data.playlists_dir(), elsewhere);
}

// Prevents: a config file that is gone or no longer valid replacing the
// working config with nothing: the error is returned and the config the
// window has stays.
#[test]
fn reload_config_that_fails_keeps_the_config_in_use() {
    let mut sb = sandbox(1);
    let file = with_config_file(&mut sb);
    let before = sb.data.config();

    // Not toml.
    std::fs::write(&file, "this is [not toml").unwrap();
    assert!(sb.data.reload_config().is_err());
    assert_eq!(sb.data.config(), before);

    // Toml that is not a valid config: no library.
    let mut bad = before.clone();
    bad.library.roots.clear();
    std::fs::write(&file, toml::to_string(&bad).unwrap()).unwrap();
    assert!(sb.data.reload_config().is_err());
    assert_eq!(sb.data.config(), before);

    // Gone.
    std::fs::remove_file(&file).unwrap();
    assert!(sb.data.reload_config().is_err());
    assert_eq!(sb.data.config(), before);
}
