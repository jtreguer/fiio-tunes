//! End to end tests of `fiio-tunes sync --plan-only` and `fiio-tunes scope`
//! on a card that is a temp folder (`device.allow_folder`). Every run gets
//! its own HOME, XDG_CONFIG_HOME and XDG_DATA_HOME in a temp dir, and
//! FIIO_TUNES_NO_DETECT is always set: nothing here can find the real X3.
//! Stdin is closed.

#[path = "../../fiio-core/tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
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

    /// `new`, with the empty card adopted.
    fn adopted(mp3s: u32) -> Self {
        let mut sb = Self::new(mp3s);
        assert_ok(&sb.run(&["adopt"]));
        sb.card.age();
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

    fn plan_file(&self) -> PathBuf {
        self.data_dir().join("plan.json")
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
            // A fake notify-send first: no test sync shows a real notification.
            .env("PATH", common::fake_notify_send_path(&self.base()))
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

    fn device_key(&self) -> String {
        self.db()
            .query_row("SELECT device_key FROM devices", [], |r| r.get(0))
            .unwrap()
    }

    /// The device's own scope as stored: the kind and the names.
    fn stored_scope(&self) -> (Option<String>, Vec<String>) {
        let db = self.db();
        let kind = db
            .query_row("SELECT scope FROM devices", [], |r| r.get(0))
            .unwrap();
        let mut stmt = db
            .prepare("SELECT name FROM device_scope_playlists ORDER BY name")
            .unwrap();
        let names = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        (kind, names)
    }

    /// A playlist file in the library's playlists folder holding `t<i>.mp3`.
    fn playlist(&self, name: &str, tracks: &[u32]) {
        let text: String = tracks
            .iter()
            .map(|i| format!("{}\n", self.music.join(format!("t{i}.mp3")).display()))
            .collect();
        fs::write(self.lists.join(format!("{name}.m3u8")), text).unwrap();
    }

    fn plan_json(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.plan_file()).unwrap()).unwrap()
    }

    /// Every file under the music and playlists folders: bytes and mtime.
    fn library_snapshot(&self) -> BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)> {
        let mut out = BTreeMap::new();
        for root in [&self.music, &self.lists] {
            let mut stack = vec![root.clone()];
            while let Some(d) = stack.pop() {
                for e in fs::read_dir(&d).unwrap() {
                    let p = e.unwrap().path();
                    if p.is_dir() {
                        stack.push(p);
                    } else {
                        let md = fs::metadata(&p).unwrap();
                        out.insert(p.clone(), (fs::read(&p).unwrap(), md.modified().unwrap()));
                    }
                }
            }
        }
        out
    }
}

use serde_json::Value;

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

fn line_with<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line with {needle:?} in:\n{text}"))
}

fn copy_targets(plan: &Value) -> Vec<String> {
    plan["copy"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["to"].as_str().unwrap().to_owned())
        .collect()
}

// -------------------------------------------------------------- sync

// Prevents: `sync` writing to the card without a confirmation: with no
// terminal to ask on and no --yes, it shows the plan, exits 1 and leaves
// the card and the mappings alone.
#[test]
fn sync_without_yes_and_no_terminal_writes_nothing() {
    let mut sb = Sandbox::adopted(2);
    let tracks = sb.count("device_tracks");
    let o = sb.run(&["sync"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--yes"), "{}", err(&o));
    assert!(out(&o).contains("to copy"), "{}", out(&o));
    assert_eq!(sb.count("device_tracks"), tracks);
    sb.card.assert_untouched();
}

// Prevents: the plan being wrong, unsaved, or the run writing anywhere but
// plan.json: the card (bytes, times, entries), the library, the mappings.
#[test]
fn plan_only_prints_the_summary_saves_the_plan_and_writes_nothing_else() {
    let mut sb = Sandbox::adopted(3);
    sb.playlist("Rock", &[1, 2]);
    let lib = sb.library_snapshot();
    assert!(!sb.plan_file().exists());

    let o = sb.run(&["sync", "--plan-only"]);
    assert_ok(&o);
    let text = out(&o);
    assert!(line_with(&text, "to copy").contains("3 tracks"), "{text}");
    assert!(line_with(&text, "playlists:").contains("1 new"), "{text}");
    assert!(text.contains("nothing changed"), "{text}");
    assert!(text.contains(s(&sb.plan_file())), "{text}");
    assert!(
        line_with(&text, "syncs").contains("whole library"),
        "{text}"
    );
    assert!(
        line_with(&text, "syncs").contains("[sync]"),
        "no own scope: told it follows the config: {text}"
    );
    assert!(
        !text.contains("=="),
        "no sections without --verbose: {text}"
    );

    let plan = sb.plan_json();
    assert_eq!(
        copy_targets(&plan),
        ["Music/t1.mp3", "Music/t2.mp3", "Music/t3.mp3"]
    );
    let sizes: Vec<_> = plan["copy"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["size"].as_u64())
        .collect();
    assert!(sizes.iter().all(|s| s.is_some_and(|n| n > 0)), "{sizes:?}");
    assert_eq!(plan["playlists"].as_array().unwrap().len(), 1);
    assert_eq!(plan["playlists"][0]["op"], "write");
    assert_eq!(plan["playlists"][0]["path"], "Rock.m3u8");

    sb.card.assert_untouched();
    assert_eq!(sb.library_snapshot(), lib, "the library changed");
    assert_eq!(sb.count("device_tracks"), 0);
    assert_eq!(sb.count("device_playlists"), 0);
    let synced: Option<i64> = sb
        .db()
        .query_row("SELECT last_sync_at FROM devices", [], |r| r.get(0))
        .unwrap();
    assert_eq!(synced, None, "a plan is not a sync");
}

// Prevents: --json printing something else than the saved plan, or prose
// around it.
#[test]
fn json_prints_exactly_the_saved_plan() {
    let sb = Sandbox::adopted(2);
    sb.playlist("Rock", &[1]);
    let o = sb.run(&["sync", "--plan-only", "--json"]);
    assert_ok(&o);
    let printed = serde_json::from_str(&out(&o)).expect("stdout is JSON");
    assert_eq!(printed, sb.plan_json());
    assert_eq!(copy_targets(&printed), ["Music/t1.mp3", "Music/t2.mp3"]);
    for key in [
        "copy",
        "replace",
        "moves",
        "remove",
        "playlists",
        "unmanaged",
        "skipped",
        "entries_skipped",
        "stale_mappings",
        "space",
        "expected",
    ] {
        assert!(printed.get(key).is_some(), "no {key}");
    }
    assert!(!out(&o).contains("nothing changed"));
}

// Prevents: --verbose not listing the changes, or listing them without
// being asked.
#[test]
fn verbose_lists_the_changes() {
    let sb = Sandbox::adopted(2);
    sb.playlist("Rock", &[1]);
    let short = out(&sb.run(&["sync", "--plan-only"]));
    assert!(
        !short.contains("Music/t1.mp3") && !short.contains("=="),
        "{short}"
    );

    let o = sb.run(&["sync", "--plan-only", "--verbose"]);
    assert_ok(&o);
    let text = out(&o);
    assert!(text.contains("== Copy (2)"), "{text}");
    assert!(
        text.contains(&format!(
            "{} -> Music/t1.mp3",
            sb.music.join("t1.mp3").display()
        )),
        "{text}"
    );
    assert!(text.contains("== Playlists (1)"), "{text}");
    assert!(
        text.contains("write Rock.m3u8, 2 entries") || text.contains("write Rock.m3u8, 1 entry"),
        "{text}"
    );
    assert!(text.contains("nothing changed"));
}

// Prevents: the plan being made from a stale index: `sync` scans first, so a
// file added after the last scan is in the plan, and a removed one is not.
#[test]
fn plan_only_scans_the_library_first() {
    let sb = Sandbox::adopted(1);
    fs::copy(fixture(Kind::Mp3, 5), sb.music.join("new.mp3")).unwrap();
    let o = sb.run(&["sync", "--plan-only"]);
    assert_ok(&o);
    assert!(
        err(&o).contains("library:") && err(&o).contains("1 added"),
        "{}",
        err(&o)
    );
    assert_eq!(
        copy_targets(&sb.plan_json()),
        ["Music/new.mp3", "Music/t1.mp3"]
    );
}

// Prevents: a card that is already in sync producing work, and the plan of
// a synced card not showing what the index maps: files and playlist bytes
// present and right give an empty plan.
#[test]
fn a_card_already_holding_the_library_gives_an_empty_plan() {
    let mut sb = Sandbox::new(2);
    sb.card.copy("old/one.mp3", Kind::Mp3, 1);
    sb.card.copy("old/two.mp3", Kind::Mp3, 2);
    sb.card.age();
    assert_ok(&sb.run(&["adopt"]));
    sb.card.assert_untouched();
    let o = sb.run(&["sync", "--plan-only"]);
    assert_ok(&o);
    let plan = sb.plan_json();
    // Adopted files outside Music/ are moved there, not copied again.
    assert!(copy_targets(&plan).is_empty(), "{plan:?}");
    let moves: Vec<_> = plan["moves"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["from"].as_str().unwrap().to_owned(),
                m["to"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        moves,
        [
            ("old/one.mp3".to_owned(), "Music/t1.mp3".to_owned()),
            ("old/two.mp3".to_owned(), "Music/t2.mp3".to_owned())
        ]
    );
    assert!(line_with(&out(&o), "to move").contains("2 tracks"));
    sb.card.assert_untouched();
}

// ----------------------------------------------------------- refusals

// Prevents: a refusal writing a plan, replacing the previous one, or
// touching the card, and refusals exiting with another code.
#[test]
fn refusals_exit_1_and_leave_the_card_and_the_last_plan_alone() {
    let mut sb = Sandbox::new(2);
    let lib = sb.library_snapshot();

    // Not adopted.
    let o = sb.run(&["sync", "--plan-only"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("adopt"), "{}", err(&o));
    assert!(out(&o).is_empty());
    assert!(!sb.plan_file().exists());
    assert_eq!(sb.count("devices"), 0);

    assert_ok(&sb.run(&["adopt"]));
    sb.card.age();
    assert_ok(&sb.run(&["sync", "--plan-only"]));
    let good = fs::read(sb.plan_file()).unwrap();

    // Another card: a manifest where none was recorded.
    sb.card.put(
        ".fiio-tunes/manifest.json",
        format!(
            r#"{{"device_key": "{}", "card_id": "other"}}"#,
            sb.device_key()
        )
        .as_bytes(),
    );
    sb.card.age();
    let o = sb.run(&["sync", "--plan-only"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("not the one adopted"), "{}", err(&o));
    assert_eq!(
        fs::read(sb.plan_file()).unwrap(),
        good,
        "plan.json was rewritten"
    );
    sb.card.assert_untouched();
    sb.card.remove(".fiio-tunes/manifest.json");
    fs::remove_dir(sb.card.root.join(".fiio-tunes")).unwrap();
    sb.card.age();

    // A scope naming a playlist that does not exist.
    sb.edit_config(|c| {
        c.sync.scope = fiio_core::config::Scope::Playlists;
        c.sync.playlists = vec!["Nope".into()];
    });
    let o = sb.run(&["sync", "--plan-only"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("Nope"), "{}", err(&o));
    assert_eq!(fs::read(sb.plan_file()).unwrap(), good);
    sb.edit_config(|c| c.sync = Default::default());

    // An unfinished restore.
    sb.db()
        .execute("UPDATE devices SET restoring = '20261016-1'", [])
        .unwrap();
    let o = sb.run(&["sync", "--plan-only"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("20261016-1"), "{}", err(&o));
    sb.db()
        .execute("UPDATE devices SET restoring = NULL", [])
        .unwrap();
    assert_ok(&sb.run(&["sync", "--plan-only"]));

    assert_eq!(sb.library_snapshot(), lib);
    sb.card.assert_untouched();
}

// Prevents: an adoption that matched nothing on a card with songs being
// planned (every song would be copied a second time).
#[test]
fn a_card_with_unmatched_songs_adopted_with_nothing_mapped_is_refused() {
    let mut sb = Sandbox::new(2);
    sb.card.copy("stranger.flac", Kind::Flac, 9);
    sb.card.age();
    let a = sb.run(&["adopt"]);
    assert_ok(&a);
    sb.card.assert_untouched();
    assert_eq!(sb.count("device_tracks"), 0);
    let o = sb.run(&["sync", "--plan-only"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("1 audio files"), "{}", err(&o));
    assert!(!sb.plan_file().exists());
    sb.card.assert_untouched();
}

// Prevents: a sync planning while another fiio-tunes writes the card.
#[test]
fn plan_only_takes_the_card_lock_and_fails_while_it_is_held() {
    let mut sb = Sandbox::adopted(1);
    let paths = Paths {
        config_file: sb.config_file(),
        data_dir: sb.data_dir(),
    };
    let held = fiio_core::lock::card(&paths).unwrap();
    let o = sb.run(&["sync", "--plan-only"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("another fiio-tunes"), "{}", err(&o));
    assert!(!sb.plan_file().exists());
    drop(held);
    assert_ok(&sb.run(&["sync", "--plan-only"]));
    sb.card.assert_untouched();
}

// Prevents: bad arguments being accepted, or reported as a runtime error.
#[test]
fn bad_arguments_exit_2() {
    let sb = Sandbox::adopted(1);
    assert_code(&sb.run(&["sync", "--bogus"]), 2);
    assert_code(&sb.run(&["scope", "only"]), 2);
    assert_code(&sb.run(&["scope", "sideways"]), 2);
    assert!(!sb.plan_file().exists());
}

// ----------------------------------------------------------- scope

// Prevents: the device scope not being used by the plan, or reported wrongly:
// set, shown, planned with, and cleared.
#[test]
fn scope_shows_sets_and_clears_the_devices_scope_and_the_plan_follows() {
    let mut sb = Sandbox::adopted(3);
    sb.playlist("Rock", &[1, 2]);
    sb.playlist("Pop", &[2, 3]);

    let o = sb.run(&["scope"]);
    assert_ok(&o);
    let text = out(&o);
    assert!(
        line_with(&text, "syncs").contains("whole library"),
        "{text}"
    );
    assert!(line_with(&text, "syncs").contains("[sync]"), "{text}");
    assert!(line_with(&text, "tracks").contains("3 tracks"), "{text}");
    assert!(
        line_with(&text, "tracks").contains("the card holds"),
        "{text}"
    );
    assert_eq!(sb.stored_scope(), (None, vec![]));

    let o = sb.run(&["scope", "only", "Rock"]);
    assert_ok(&o);
    let text = out(&o);
    assert!(
        line_with(&text, "syncs").contains("only 1 playlist: Rock"),
        "{text}"
    );
    assert!(
        line_with(&text, "syncs").contains("set for this player"),
        "{text}"
    );
    assert!(line_with(&text, "tracks").contains("2 tracks"), "{text}");
    assert_eq!(
        sb.stored_scope(),
        (Some("playlists".into()), vec!["Rock".into()])
    );
    // Shown again without arguments.
    assert!(line_with(&out(&sb.run(&["scope"])), "syncs").contains("only 1 playlist: Rock"));

    let o = sb.run(&["sync", "--plan-only"]);
    assert_ok(&o);
    assert_eq!(
        copy_targets(&sb.plan_json()),
        ["Music/t1.mp3", "Music/t2.mp3"]
    );
    let text = out(&o);
    assert!(
        line_with(&text, "syncs").contains("only 1 playlist: Rock"),
        "{text}"
    );
    assert!(!line_with(&text, "syncs").contains("[sync]"), "{text}");
    let written: Vec<_> = sb.plan_json()["playlists"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["path"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(written, ["Rock.m3u8"], "Pop is out of scope");

    // Except: Rock's tracks go, Pop's other track stays only if not in Rock.
    assert_ok(&sb.run(&["scope", "except", "Rock", "Rock"]));
    assert_eq!(
        sb.stored_scope(),
        (Some("except".into()), vec!["Rock".into()])
    );
    assert_ok(&sb.run(&["sync", "--plan-only"]));
    assert_eq!(copy_targets(&sb.plan_json()), ["Music/t3.mp3"]);

    assert_ok(&sb.run(&["scope", "only", "Pop", "Rock"]));
    assert_eq!(
        sb.stored_scope(),
        (Some("playlists".into()), vec!["Pop".into(), "Rock".into()])
    );

    // `all` is the device's own scope, whatever [sync] says.
    sb.edit_config(|c| {
        c.sync.scope = fiio_core::config::Scope::Playlists;
        c.sync.playlists = vec!["Rock".into()];
    });
    let o = sb.run(&["scope", "all"]);
    assert_ok(&o);
    assert!(
        line_with(&out(&o), "syncs").contains("whole library, set for this player"),
        "{}",
        out(&o)
    );
    assert_eq!(sb.stored_scope(), (Some("all".into()), vec![]));
    assert_ok(&sb.run(&["sync", "--plan-only"]));
    assert_eq!(copy_targets(&sb.plan_json()).len(), 3);

    // `default` follows the config again, here "only Rock".
    let o = sb.run(&["scope", "default"]);
    assert_ok(&o);
    let text = out(&o);
    assert!(
        line_with(&text, "syncs").contains("only 1 playlist: Rock"),
        "{text}"
    );
    assert!(line_with(&text, "syncs").contains("[sync]"), "{text}");
    assert_eq!(sb.stored_scope(), (None, vec![]));
    assert_ok(&sb.run(&["sync", "--plan-only"]));
    assert_eq!(
        copy_targets(&sb.plan_json()),
        ["Music/t1.mp3", "Music/t2.mp3"]
    );
    sb.card.assert_untouched();
}

// Prevents: a scope that selects nothing real being saved (the next sync
// would plan to empty the card), and the old scope being lost when refused.
#[test]
fn a_bad_scope_is_refused_with_nothing_changed() {
    let mut sb = Sandbox::adopted(2);
    sb.playlist("Rock", &[1]);
    sb.playlist("Empty", &[]);
    sb.playlist("Everything", &[1, 2]);
    assert_ok(&sb.run(&["scope", "only", "Rock"]));
    let kept = (Some("playlists".to_owned()), vec!["Rock".to_owned()]);
    assert_eq!(sb.stored_scope(), kept);

    for args in [
        &["scope", "only", "Nope"][..],
        &["scope", "only", "Rock", "Nope"],
        &["scope", "except", "Nope"],
        &["scope", "only", "Empty"],
        &["scope", "except", "Everything"],
    ] {
        let o = sb.run(args);
        assert_code(&o, 1);
        assert!(err(&o).contains("nothing changed"), "{args:?}: {}", err(&o));
        assert!(out(&o).is_empty(), "{args:?}: {}", out(&o));
        assert_eq!(sb.stored_scope(), kept, "{args:?} changed the scope");
    }
    // Clearing and `all` are never refused.
    assert_ok(&sb.run(&["scope", "default"]));
    assert_ok(&sb.run(&["scope", "all"]));
    sb.card.assert_untouched();
}

// Prevents: `scope` needing the card when told which player, and acting on
// the wrong or an unknown player.
#[test]
fn scope_with_device_works_without_a_card_and_refuses_an_unknown_player() {
    let mut sb = Sandbox::adopted(2);
    sb.playlist("Rock", &[1]);
    let key = sb.device_key();
    sb.card.age();

    // No card found (nothing mounted, no mountpoint): the player is named.
    sb.edit_config(|c| c.device.mountpoint = PathBuf::new());
    let o = sb.run(&["scope"]);
    assert_code(&o, 1);
    assert_eq!(sb.stored_scope(), (None, vec![]));

    let o = sb.run(&["scope", "--device", &key]);
    assert_ok(&o);
    let text = out(&o);
    assert!(
        line_with(&text, "syncs").contains("whole library"),
        "{text}"
    );
    assert!(
        !text.contains("the card holds"),
        "no card, no capacity: {text}"
    );

    let o = sb.run(&["scope", "only", "Rock", "--device", &key]);
    assert_ok(&o);
    assert!(line_with(&out(&o), "syncs").contains("only 1 playlist: Rock"));
    assert_eq!(
        sb.stored_scope(),
        (Some("playlists".into()), vec!["Rock".into()])
    );
    let o = sb.run(&["scope", "--device", &key, "default"]);
    assert_ok(&o);
    assert_eq!(sb.stored_scope(), (None, vec![]));

    let o = sb.run(&["scope", "only", "Rock", "--device", "usb:NOPE"]);
    assert_code(&o, 1);
    assert!(
        err(&o).contains("usb:NOPE") && err(&o).contains("adopt"),
        "{}",
        err(&o)
    );
    assert_eq!(sb.stored_scope(), (None, vec![]));
    assert_eq!(sb.count("devices"), 1);
}

// Prevents: `scope` on a card never adopted creating a player or a scope.
#[test]
fn scope_on_a_card_not_adopted_exits_1() {
    let mut sb = Sandbox::new(1);
    sb.card.age();
    let o = sb.run(&["scope", "all"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("adopt"), "{}", err(&o));
    assert_eq!(sb.count("devices"), 0);
    sb.card.assert_untouched();
}
