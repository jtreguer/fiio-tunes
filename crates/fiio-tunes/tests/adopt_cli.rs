//! End to end tests of `fiio-tunes adopt` on a card that is a temp folder
//! (`device.allow_folder`). Every run gets its own HOME, XDG_CONFIG_HOME and
//! XDG_DATA_HOME in a temp dir, and FIIO_TUNES_NO_DETECT is always set:
//! nothing here can find the real X3. Stdin is closed.

#[path = "../../fiio-core/tests/common/mod.rs"]
mod common;

use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{FakeCard, Kind, fixture};
use fiio_core::config::{Config, Paths};

struct Sandbox {
    dir: tempfile::TempDir,
    music: PathBuf,
    lists: PathBuf,
    card: FakeCard,
}

impl Sandbox {
    /// `init` with a library of `mp3s` MP3 files (`t<i>.mp3`), scanned. The
    /// config names the card folder as `device.mountpoint` with
    /// `allow_folder`.
    fn new(mp3s: u32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let card_root = base.join("card");
        let (music, lists) = (base.join("music"), base.join("lists"));
        for d in [&music, &lists, &card_root] {
            fs::create_dir(d).unwrap();
        }
        let sb = Sandbox {
            card: FakeCard::new(card_root),
            music,
            lists,
            dir,
        };
        assert_ok(&sb.run(&[
            "init",
            "--library",
            s(&sb.music),
            "--playlists",
            s(&sb.lists),
        ]));
        for i in 1..=mp3s {
            fs::copy(fixture(Kind::Mp3, i), sb.music.join(format!("t{i}.mp3"))).unwrap();
        }
        assert_ok(&sb.run(&["scan"]));
        sb.edit_config(|c| {
            c.device.mountpoint = sb.card.root.clone();
            c.device.allow_folder = true;
        });
        sb
    }

    fn base(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn config_file(&self) -> PathBuf {
        self.base().join("config/fiio-tunes/config.toml")
    }

    fn data_dir(&self) -> PathBuf {
        self.base().join("data/fiio-tunes")
    }

    fn report(&self) -> PathBuf {
        self.data_dir().join("adopt-report.txt")
    }

    fn edit_config(&self, f: impl FnOnce(&mut Config)) {
        let mut c = Config::load(&self.config_file()).unwrap();
        f(&mut c);
        c.save(&self.config_file()).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fiio-tunes"))
            .env("HOME", self.base().join("home"))
            .env("XDG_CONFIG_HOME", self.base().join("config"))
            .env("XDG_DATA_HOME", self.base().join("data"))
            .env("FIIO_TUNES_NO_DETECT", "1")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.data_dir().join("fiio-tunes.sqlite")).unwrap()
    }

    fn count(&self, table: &str) -> i64 {
        self.db()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    /// `(card_path)` of the device tracks, sorted.
    fn card_paths(&self) -> Vec<String> {
        let db = self.db();
        let mut stmt = db
            .prepare("SELECT card_path FROM device_tracks ORDER BY card_path")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn device_key(&self) -> Option<String> {
        self.db()
            .query_row("SELECT device_key FROM devices", [], |r| r.get(0))
            .ok()
    }
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn code(o: &Output) -> i32 {
    o.status
        .code()
        .unwrap_or_else(|| panic!("killed by signal {:?}", o.status.signal()))
}

fn assert_code(o: &Output, want: i32) {
    assert_eq!(code(o), want, "stdout: {}\nstderr: {}", out(o), err(o));
}

fn assert_ok(o: &Output) {
    assert_code(o, 0);
}

// Prevents: the command saving the wrong thing, or reporting a save it did
// not make: tracks keyed by the card's own paths, playlists matched, a
// report written, and the card never written.
#[test]
fn adopt_saves_matches_prints_the_summary_and_writes_the_report() {
    let mut sb = Sandbox::new(3);
    fs::write(sb.lists.join("Rock.m3u8"), "#EXTM3U\n").unwrap();
    sb.card.copy("Old layout/first song.mp3", Kind::Mp3, 1);
    sb.card.copy("zz/t2.mp3", Kind::Mp3, 2);
    sb.card.put("Rock.m3u8", b"#EXTM3U\n");
    sb.card.put("Other.m3u8", b"#EXTM3U\n");
    sb.card.age();

    let o = sb.run(&["adopt"]);
    assert_ok(&o);
    let text = out(&o);
    assert!(text.contains("card audio files: 2"), "{text}");
    assert!(text.contains("matched: 2 (2 by identical file)"), "{text}");
    assert!(text.contains("proposals: 0"), "{text}");
    assert!(
        text.contains(
            "playlists: 1 matched, 1 only on the card (unmanaged), 0 only in the library"
        ),
        "{text}"
    );
    assert!(text.contains("manifest: none"), "{text}");
    assert!(
        text.contains("saved: 2 tracks and 1 playlists mapped to folder:"),
        "{text}"
    );
    assert!(!text.contains("Nothing saved"), "{text}");

    assert_eq!(sb.card_paths(), ["Old layout/first song.mp3", "zz/t2.mp3"]);
    assert_eq!(sb.count("device_playlists"), 1);
    let (card_name, card_key): (String, String) = sb
        .db()
        .query_row(
            "SELECT card_name, card_key FROM device_playlists",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(card_name, "Rock.m3u8");
    assert_eq!(card_key, fiio_core::paths::card_key("Rock.m3u8"));
    assert_eq!(
        sb.device_key(),
        Some(format!("folder:{}", sb.card.root.display()))
    );

    let report = fs::read_to_string(sb.report()).unwrap();
    for section in [
        "== Proposals (0)",
        "== Card files with no library file (0)",
        "== Playlists matched (1)",
        "== Card playlists with no library playlist (1)",
        "Other.m3u8",
    ] {
        assert!(report.contains(section), "missing {section:?}\n{report}");
    }
    assert!(
        text.contains(&format!("full report: {}", sb.report().display())),
        "{text}"
    );
    sb.card.assert_untouched();
}

// Prevents: pending proposals being saved without the user's say: the
// command must stop with "Nothing saved", and only --accept-proposals saves.
#[test]
fn pending_proposals_save_nothing_until_accepted() {
    let mut sb = Sandbox::new(2);
    sb.card.copy("one.mp3", Kind::Mp3, 1);
    sb.card.copy("two.mp3", Kind::Mp3, 1);
    sb.card.copy("fine.mp3", Kind::Mp3, 2);
    sb.card.age();

    let o = sb.run(&["adopt"]);
    assert_ok(&o);
    let text = out(&o);
    assert!(text.contains("proposals: 2"), "{text}");
    assert!(text.contains("2 proposals need a decision"), "{text}");
    assert!(text.contains("Nothing saved"), "{text}");
    assert!(!text.contains("saved:"), "{text}");
    assert_eq!(sb.count("devices"), 0);
    assert_eq!(sb.count("device_tracks"), 0);
    // The report is written anyway: it is what the user reviews.
    let report = fs::read_to_string(sb.report()).unwrap();
    assert!(report.contains("== Proposals (2)"), "{report}");
    sb.card.assert_untouched();

    let o = sb.run(&["adopt", "--accept-proposals"]);
    assert_ok(&o);
    let text = out(&o);
    assert!(text.contains("saved: 2 tracks"), "{text}");
    // The proposal for the first copy took t1; the second lost it.
    assert_eq!(sb.card_paths(), ["fine.mp3", "one.mp3"]);
    sb.card.assert_untouched();
}

// Prevents: a second adoption replacing mappings without --force, and
// --force failing to replace them.
#[test]
fn a_second_adopt_exits_1_without_force_and_replaces_with_it() {
    let mut sb = Sandbox::new(2);
    sb.card.copy("a.mp3", Kind::Mp3, 1);
    sb.card.age();
    assert_ok(&sb.run(&["adopt"]));
    assert_eq!(sb.card_paths(), ["a.mp3"]);

    sb.card.remove("a.mp3");
    sb.card.copy("b.mp3", Kind::Mp3, 2);
    sb.card.age();
    let o = sb.run(&["adopt"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--force"), "{}", err(&o));
    assert!(!out(&o).contains("saved:"), "{}", out(&o));
    assert_eq!(sb.card_paths(), ["a.mp3"], "a refused adopt changed rows");
    sb.card.assert_untouched();

    assert_ok(&sb.run(&["adopt", "--force"]));
    assert_eq!(sb.card_paths(), ["b.mp3"]);
    assert_eq!(sb.count("devices"), 1);
    sb.card.assert_untouched();
}

// Prevents: an empty card locking its device (exit 1 at the second run).
#[test]
fn an_empty_card_adopts_twice_without_force() {
    let mut sb = Sandbox::new(1);
    for _ in 0..2 {
        let o = sb.run(&["adopt"]);
        assert_ok(&o);
        assert!(
            out(&o).contains("saved: 0 tracks and 0 playlists"),
            "{}",
            out(&o)
        );
    }
    assert_eq!(sb.count("devices"), 1);
    sb.card.age();
    sb.card.assert_untouched();
}

// Prevents: a plain folder being adopted as if it were a player when
// allow_folder is off.
#[test]
fn a_folder_without_allow_folder_exits_1_and_saves_nothing() {
    let mut sb = Sandbox::new(1);
    sb.card.copy("a.mp3", Kind::Mp3, 1);
    sb.card.age();
    sb.edit_config(|c| c.device.allow_folder = false);
    let o = sb.run(&["adopt"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("X3"), "{}", err(&o));
    assert_eq!(sb.count("devices"), 0);
    assert!(!sb.report().exists(), "the card was read before refusing");
    sb.card.assert_untouched();
}

// Prevents: a damaged manifest being read as no manifest.
#[test]
fn an_unparsable_manifest_exits_1_and_saves_nothing() {
    let mut sb = Sandbox::new(1);
    sb.card.copy("a.mp3", Kind::Mp3, 1);
    sb.card.put(".fiio-tunes/manifest.json", b"{ not json");
    sb.card.age();
    let o = sb.run(&["adopt"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("manifest.json"), "{}", err(&o));
    assert_eq!(sb.count("devices"), 0);
    sb.card.assert_untouched();
}

// Prevents: the key of a folder card not following its manifest (the
// mappings would not be found by the next sync), and the card id not
// reaching the index.
#[test]
fn a_folder_card_takes_the_key_and_card_id_of_its_manifest() {
    let mut sb = Sandbox::new(1);
    sb.card.copy("a.mp3", Kind::Mp3, 1);
    sb.card.put(
        ".fiio-tunes/manifest.json",
        br#"{"device_key": "folder:earlier", "card_id": "card-5"}"#,
    );
    sb.card.age();
    let o = sb.run(&["adopt"]);
    assert_ok(&o);
    assert!(
        out(&o).contains("manifest: present, card id card-5"),
        "{}",
        out(&o)
    );
    let (key, card_id): (String, Option<String>) = sb
        .db()
        .query_row("SELECT device_key, card_id FROM devices", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(
        (key.as_str(), card_id.as_deref()),
        ("folder:earlier", Some("card-5"))
    );
    sb.card.assert_untouched();
}

// Prevents: adopting an empty index "successfully" with every card file
// unmanaged.
#[test]
fn an_empty_index_exits_1() {
    let mut sb = Sandbox::new(0);
    sb.card.copy("a.mp3", Kind::Mp3, 1);
    sb.card.age();
    let o = sb.run(&["adopt"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("scan"), "{}", err(&o));
    assert_eq!(sb.count("devices"), 0);
    sb.card.assert_untouched();
}

// Prevents: bad arguments being accepted, or reported as a runtime error.
#[test]
fn unknown_options_exit_2() {
    let sb = Sandbox::new(1);
    assert_code(&sb.run(&["adopt", "--bogus"]), 2);
    assert_code(&sb.run(&["adopt", "extra"]), 2);
    assert_eq!(sb.count("devices"), 0);
}

// Prevents: adoption reading the card while a sync or restore writes it.
#[test]
fn adopt_takes_the_card_lock_and_fails_with_card_busy_while_it_is_held() {
    let mut sb = Sandbox::new(1);
    sb.card.copy("a.mp3", Kind::Mp3, 1);
    sb.card.age();
    let paths = Paths {
        config_file: sb.config_file(),
        data_dir: sb.data_dir(),
    };
    let held = fiio_core::lock::card(&paths).unwrap();
    let o = sb.run(&["adopt"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("another fiio-tunes"), "{}", err(&o));
    assert_eq!(sb.count("devices"), 0);
    assert!(!sb.report().exists());
    drop(held);
    assert_ok(&sb.run(&["adopt"]));
    assert_eq!(sb.card_paths(), ["a.mp3"]);
    sb.card.assert_untouched();
}
