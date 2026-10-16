//! End to end tests of `fiio-tunes sync --playlists-only`, `backups` and
//! `restore` on a card that is a temp folder (`device.allow_folder`). Every
//! run gets its own HOME, XDG_CONFIG_HOME and XDG_DATA_HOME in a temp dir,
//! and FIIO_TUNES_NO_DETECT is always set: nothing here can find the real
//! X3, an iPod, iprune's data or the real library. Stdin is closed unless a
//! test asks for a terminal (util-linux `script`).

#[path = "../../fiio-core/tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use common::{FakeCard, Kind, fixture};
use fiio_core::backup;
use fiio_core::config::Config;

type Tree = BTreeMap<String, Option<Vec<u8>>>;

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

    /// `new`, with copies of the library's files on the card (`Music/t<i>.mp3`
    /// and an unmanaged `Other.m3u8`), and the card adopted: the next sync is
    /// the first.
    fn adopted(mp3s: u32) -> Self {
        let mut sb = Self::new(mp3s);
        for i in 1..=mp3s {
            sb.card.copy(&format!("Music/t{i}.mp3"), Kind::Mp3, i);
        }
        sb.card.put("Other.m3u8", b"#EXTM3U\nmine\n");
        assert_ok(&sb.run(&["adopt"]));
        assert_eq!(sb.count("device_tracks"), i64::from(mp3s));
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

    fn edit_config(&self, f: impl FnOnce(&mut Config)) {
        let mut c = Config::load(&self.config_file()).unwrap();
        f(&mut c);
        c.save(&self.config_file()).unwrap();
    }

    fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.env("HOME", self.base().join("home"))
            .env("XDG_CONFIG_HOME", self.base().join("config"))
            .env("XDG_DATA_HOME", self.base().join("data"))
            .env("FIIO_TUNES_NO_DETECT", "1");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_fiio-tunes"))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// Runs with a pseudo terminal as stdin (util-linux `script`), answering
    /// with `input`. `None` when `script` is not installed.
    fn run_tty(&self, args: &[&str], input: &str) -> Option<Output> {
        use std::io::Write;
        let line = format!("{} {}", env!("CARGO_BIN_EXE_fiio-tunes"), args.join(" "));
        let mut child = self
            .command("script")
            .args(["-qefc", &line, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        Some(child.wait_with_output().unwrap())
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.data_dir().join("fiio-tunes.sqlite")).unwrap()
    }

    fn count(&self, table: &str) -> i64 {
        self.db()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn card_id(&self) -> Option<String> {
        self.db()
            .query_row("SELECT card_id FROM devices", [], |r| r.get(0))
            .unwrap()
    }

    fn device_key(&self) -> String {
        self.db()
            .query_row("SELECT device_key FROM devices", [], |r| r.get(0))
            .unwrap()
    }

    fn restoring(&self) -> Option<String> {
        self.db()
            .query_row("SELECT restoring FROM devices", [], |r| r.get(0))
            .unwrap()
    }

    fn card_names(&self) -> Vec<String> {
        let db = self.db();
        let mut stmt = db
            .prepare("SELECT card_name FROM device_playlists ORDER BY card_name")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// Stamps of the device's backups, oldest first.
    fn backups(&self) -> Vec<String> {
        backup::list(&self.data_dir().join("backups"), &self.device_key())
            .unwrap()
            .into_iter()
            .map(|b| b.stamp)
            .collect()
    }

    fn card_tree(&self) -> Tree {
        tree(&self.card.root)
    }

    fn read(&self, rel: &str) -> Vec<u8> {
        fs::read(self.card.root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    /// A playlist file in the library's playlists folder holding `t<i>.mp3`.
    fn playlist(&self, name: &str, tracks: &[u32]) {
        let text: String = tracks
            .iter()
            .map(|i| format!("{}\n", self.music.join(format!("t{i}.mp3")).display()))
            .collect();
        fs::write(self.lists.join(format!("{name}.m3u8")), text).unwrap();
    }

    /// `sync --playlists-only --yes`, expected to succeed.
    fn sync(&self) -> Output {
        let o = self.run(&["sync", "--playlists-only", "--yes"]);
        assert_ok(&o);
        o
    }
}

fn tree(root: &Path) -> Tree {
    fn walk(root: &Path, p: &Path, out: &mut Tree) {
        for e in fs::read_dir(p).unwrap() {
            let e = e.unwrap();
            let path = e.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            if e.file_type().unwrap().is_dir() {
                out.insert(rel, None);
                walk(root, &path, out);
            } else {
                out.insert(rel, Some(fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Tree::new();
    walk(root, root, &mut out);
    out
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

fn locations(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// The stamp a run printed after "backup:".
fn printed_stamp(o: &Output) -> String {
    out(o)
        .lines()
        .find_map(|l| l.strip_prefix("backup:"))
        .unwrap_or_else(|| panic!("no backup line in:\n{}", out(o)))
        .trim()
        .to_owned()
}

/// Whether the command asked on a terminal; a `script` that could not
/// provide one skips the test.
fn asked_on_a_terminal(o: &Output, question: &str) -> bool {
    let text = format!("{}{}", out(o), err(o));
    if text.contains(question) {
        return true;
    }
    eprintln!("SKIPPED: `script` gave no terminal: {text}");
    false
}

// ------------------------------------------------------------------ sync

// Prevents: `sync` with neither --plan-only nor --playlists-only writing
// (copying tracks does not exist yet): exit 1, the card, the index and the
// backups untouched, even with --yes and --allow-mass-removal.
#[test]
fn sync_without_a_mode_exits_1_and_writes_nothing() {
    let mut sb = Sandbox::adopted(2);
    sb.playlist("Rock", &[1]);
    for args in [
        &["sync"][..],
        &["sync", "--yes", "--allow-mass-removal"],
        &["sync", "--verbose"],
    ] {
        let o = sb.run(args);
        assert_code(&o, 1);
        assert!(err(&o).contains("--playlists-only"), "{}", err(&o));
    }
    sb.card.assert_untouched();
    assert!(sb.backups().is_empty());
    assert_eq!(sb.card_id(), None);
    assert_eq!(sb.count("device_playlists"), 0);
}

// Prevents: a script or a closed stdin writing to a card without having
// been asked: without --yes and without a terminal, exit 1 asking for
// --yes, nothing written, no backup, no id.
#[test]
fn playlists_only_without_yes_and_no_terminal_asks_for_yes_and_writes_nothing() {
    let mut sb = Sandbox::adopted(2);
    sb.playlist("Rock", &[1, 2]);
    let o = sb.run(&["sync", "--playlists-only"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--yes"), "{}", err(&o));
    sb.card.assert_untouched();
    assert!(sb.backups().is_empty());
    assert_eq!(sb.card_id(), None);
    assert_eq!(sb.count("device_playlists"), 0);
}

// Prevents: the first sync not writing what the plan says: the playlist
// naming the card's files, the manifest and the card id, the mappings,
// the backup printed and really there, the library left alone; and
// `backups` listing that stamp.
#[test]
fn playlists_only_with_yes_writes_the_playlist_and_prints_the_backup() {
    let sb = Sandbox::adopted(2);
    sb.playlist("Rock", &[1, 2]);
    let other = sb.read("Other.m3u8");
    let lib = fs::read(sb.lists.join("Rock.m3u8")).unwrap();
    let o = sb.sync();
    let stamp = printed_stamp(&o);
    assert_eq!(sb.backups(), std::slice::from_ref(&stamp));
    assert_eq!(
        locations(&sb.read("Rock.m3u8")),
        ["Music/t1.mp3", "Music/t2.mp3"]
    );
    assert_eq!(sb.read("Other.m3u8"), other);
    let manifest: serde_json::Value =
        serde_json::from_slice(&sb.read(".fiio-tunes/manifest.json")).unwrap();
    assert_eq!(
        manifest["card_id"].as_str().map(str::to_owned),
        sb.card_id()
    );
    assert_eq!(manifest["playlists"], serde_json::json!(["Rock.m3u8"]));
    assert_eq!(manifest["tracks"].as_array().unwrap().len(), 2);
    assert_eq!(sb.card_names(), ["Rock.m3u8"]);
    let synced: Option<i64> = sb
        .db()
        .query_row("SELECT last_sync_at FROM devices", [], |r| r.get(0))
        .unwrap();
    assert!(synced.is_some());
    assert_eq!(fs::read(sb.lists.join("Rock.m3u8")).unwrap(), lib);
    assert!(
        !sb.card_tree()
            .keys()
            .any(|k| k.ends_with(".mp3") && k != "Music/t1.mp3" && k != "Music/t2.mp3"),
        "{:?}",
        sb.card_tree().keys()
    );

    let o = sb.run(&["backups"]);
    assert_ok(&o);
    assert!(out(&o).contains(&stamp), "{}", out(&o));
}

// Prevents: a second sync writing again, or taking another backup, when
// nothing changed: exit 0, the card and the backups as they were.
#[test]
fn a_second_sync_with_nothing_to_do_writes_nothing_and_takes_no_backup() {
    let sb = Sandbox::adopted(2);
    sb.playlist("Rock", &[1, 2]);
    sb.sync();
    let card = sb.card_tree();
    let backups = sb.backups();
    let o = sb.run(&["sync", "--playlists-only", "--yes"]);
    assert_ok(&o);
    assert!(!out(&o).contains("backup:"), "{}", out(&o));
    assert_eq!(sb.card_tree(), card);
    assert_eq!(sb.backups(), backups);
    // And without --yes, nothing to confirm either.
    assert_ok(&sb.run(&["sync", "--playlists-only"]));
    assert_eq!(sb.backups(), backups);
}

// Prevents: --plan-only combined with --playlists-only writing: it shows the
// playlists-only plan (no copy of a track), saves it, and changes nothing.
#[test]
fn plan_only_with_playlists_only_shows_the_plan_and_writes_nothing() {
    let mut sb = Sandbox::adopted(2);
    sb.playlist("Rock", &[1, 2]);
    // A library track not on the card: the full plan would copy it.
    fs::copy(fixture(Kind::Mp3, 3), sb.music.join("t3.mp3")).unwrap();
    assert_ok(&sb.run(&["scan"]));
    let o = sb.run(&["sync", "--plan-only", "--playlists-only", "--yes"]);
    assert_ok(&o);
    let plan: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(sb.data_dir().join("plan.json")).unwrap())
            .unwrap();
    assert_eq!(plan["copy"], serde_json::json!([]));
    assert_eq!(plan["playlists"][0]["path"], "Rock.m3u8");
    sb.card.assert_untouched();
    assert!(sb.backups().is_empty() && sb.card_id().is_none());
}

// Prevents: a prompt answered "no" writing anyway, and being reported as a
// failure (exit 1) rather than a decline (exit 4); and "yes" at the same
// prompt not writing. Needs a terminal: util-linux `script` provides one.
#[test]
fn declining_at_the_prompt_exits_4_and_writes_nothing() {
    let mut sb = Sandbox::adopted(2);
    sb.playlist("Rock", &[1, 2]);
    let Some(o) = sb.run_tty(&["sync", "--playlists-only"], "n\n") else {
        eprintln!("skipped: `script` is not installed");
        return;
    };
    if !asked_on_a_terminal(&o, "Apply?") {
        return;
    }
    assert_code(&o, 4);
    sb.card.assert_untouched();
    assert!(sb.backups().is_empty());
    assert_eq!(sb.card_id(), None);
    assert_eq!(sb.count("device_playlists"), 0);

    let o = sb.run_tty(&["sync", "--playlists-only"], "y\n").unwrap();
    assert_ok(&o);
    assert!(sb.card.root.join("Rock.m3u8").is_file());
    assert_eq!(sb.backups().len(), 1);
}

// Prevents: a card that fails verification being reported as an ordinary
// error or as success: the contract is exit 3, the backup named with the
// restore command, no mapping recorded; and the named backup restores the
// manifest, the id and the files it held. A thread makes an audio file flicker on the card; a run whose plan
// and check see it differently exits 3. Attempts repeat until that is seen.
#[test]
fn a_card_that_changes_during_the_sync_exits_3_and_restore_undoes_it() {
    let mut last = String::new();
    for attempt in 0..60 {
        let sb = Sandbox::adopted(1);
        sb.playlist("Rock", &[1]);
        let flicker = sb.card.root.join("Flicker/f.mp3");
        fs::create_dir(flicker.parent().unwrap()).unwrap();
        let before = sb.card_tree();
        let stop = AtomicBool::new(false);
        let o = std::thread::scope(|scope| {
            struct Stop<'a>(&'a AtomicBool);
            impl Drop for Stop<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Relaxed);
                }
            }
            let _stop = Stop(&stop);
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let _ = fs::write(&flicker, b"f");
                    let _ = fs::remove_file(&flicker);
                }
            });
            sb.run(&["sync", "--playlists-only", "--yes"])
        });
        if code(&o) != 3 {
            last = format!("attempt {attempt}: exit {}: {}", code(&o), err(&o));
            continue;
        }
        let e = err(&o);
        assert!(e.contains("verification failed"), "{e}");
        let backups = sb.backups();
        assert_eq!(backups.len(), 1, "{e}");
        assert!(e.contains(&backups[0]), "the backup is not named: {e}");
        assert!(e.contains("restore"), "{e}");
        assert_eq!(
            sb.count("device_playlists"),
            0,
            "recorded despite the failure"
        );

        // The named backup puts the playlists and the manifest back.
        let _ = fs::remove_file(&flicker);
        let o = sb.run(&["restore", &backups[0], "--yes"]);
        assert_ok(&o);
        let mut after = sb.card_tree();
        // The manifest is gone; its (now empty) folder stays.
        assert!(!after.contains_key(".fiio-tunes/manifest.json"));
        after.remove(".fiio-tunes");
        after.retain(|k, _| !k.starts_with("Flicker"));
        let mut was = before;
        was.retain(|k, _| !k.starts_with("Flicker"));
        assert_eq!(
            after.keys().collect::<Vec<_>>(),
            was.keys().collect::<Vec<_>>()
        );
        for (k, v) in &was {
            assert!(after[k] == *v, "{k} differs after the restore");
        }
        assert_eq!(sb.card_id(), None);
        return;
    }
    panic!("the card never changed between the plan and the check: {last}");
}

// Prevents: the playlist mass-removal guard being skipped on the command
// line: six of ten managed playlists removed exit 1 naming the option, with
// the card and the backups as they were; with the option they are applied.
#[test]
fn mass_removal_of_playlists_needs_the_option() {
    let sb = Sandbox::adopted(1);
    for i in 1..=10 {
        sb.playlist(&format!("P{i:02}"), &[1]);
    }
    sb.sync();
    assert_eq!(sb.card_names().len(), 10);
    for i in 1..=6 {
        assert_ok(&sb.run(&["playlist", "rm", &format!("P{i:02}"), "--yes"]));
    }
    let card = sb.card_tree();
    let backups = sb.backups();
    let o = sb.run(&["sync", "--playlists-only", "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--allow-mass-removal"), "{}", err(&o));
    assert_eq!(sb.card_tree(), card);
    assert_eq!(sb.backups(), backups);
    assert_eq!(sb.card_names().len(), 10);

    let o = sb.run(&["sync", "--playlists-only", "--yes", "--allow-mass-removal"]);
    assert_ok(&o);
    assert_eq!(sb.backups().len(), backups.len() + 1);
    let left: Vec<String> = sb
        .card_tree()
        .keys()
        .filter(|k| k.starts_with('P') && k.ends_with(".m3u8"))
        .cloned()
        .collect();
    assert_eq!(left, ["P07.m3u8", "P08.m3u8", "P09.m3u8", "P10.m3u8"]);
    assert_eq!(sb.card_names().len(), 4);
}

// Prevents: a sync of a card that was never adopted writing to it.
#[test]
fn a_card_that_was_not_adopted_is_refused_and_untouched() {
    let mut sb = Sandbox::new(1);
    sb.card.put("Music/a.mp3", b"audio");
    sb.card.age();
    sb.playlist("Rock", &[1]);
    let o = sb.run(&["sync", "--playlists-only", "--yes"]);
    assert_code(&o, 1);
    sb.card.assert_untouched();
    assert!(!sb.data_dir().join("backups").exists());
}

// ------------------------------------------------------ backups, restore

/// A card synced twice: Rock written, then Rock edited and Pop added. The
/// stamps of the two backups, and Rock's bytes after the first sync.
fn two_syncs(sb: &Sandbox) -> (String, String, Vec<u8>) {
    sb.playlist("Rock", &[1]);
    let first = printed_stamp(&sb.sync());
    let rock1 = sb.read("Rock.m3u8");
    sb.playlist("Rock", &[2, 1]);
    sb.playlist("Pop", &[2]);
    let second = printed_stamp(&sb.sync());
    (first, second, rock1)
}

// Prevents: `backups` listing the wrong stamps, in the wrong order, or
// needing a connected card when told which player; an unknown player
// listing nothing without failing.
#[test]
fn backups_lists_the_stamps_oldest_first_and_works_by_device_key() {
    let sb = Sandbox::adopted(2);
    let o = sb.run(&["backups"]);
    assert_ok(&o);
    assert!(!out(&o).contains("20"), "{}", out(&o));
    let (first, second, _) = two_syncs(&sb);
    assert!(first < second);
    let o = sb.run(&["backups"]);
    assert_ok(&o);
    let text = out(&o);
    let at = |stamp: &str| {
        text.find(stamp)
            .unwrap_or_else(|| panic!("{stamp} not in:\n{text}"))
    };
    assert!(at(&first) < at(&second));
    assert_eq!(
        text.lines()
            .filter(|l| l.contains(&first) || l.contains(&second))
            .count(),
        2
    );

    // By key, with no card connected.
    sb.edit_config(|c| c.device.mountpoint = PathBuf::new());
    let o = sb.run(&["backups", "--device", &sb.device_key()]);
    assert_ok(&o);
    assert!(
        out(&o).contains(&first) && out(&o).contains(&second),
        "{}",
        out(&o)
    );
    assert_code(&sb.run(&["backups"]), 1);
    let o = sb.run(&["backups", "--device", "usb:unknown"]);
    assert_ok(&o);
    assert!(!out(&o).contains(&first), "{}", out(&o));
}

// Prevents: a restore that does not bring the card and the index back, or
// asks no confirmation: the older Rock returns, Pop (managed, absent then)
// goes, the unmanaged playlist is untouched, the restore is itself backed
// up, and a sync afterwards converges to the library.
#[test]
fn restore_round_trip() {
    let sb = Sandbox::adopted(2);
    let (_first, second, rock1) = two_syncs(&sb);
    // The backup `second` holds the card as it was after the first sync.
    let other = sb.read("Other.m3u8");
    let before = sb.backups().len();
    let o = sb.run(&["restore", &second, "--yes"]);
    assert_ok(&o);
    assert_eq!(sb.read("Rock.m3u8"), rock1);
    assert!(!sb.card.root.join("Pop.m3u8").exists());
    assert_eq!(sb.read("Other.m3u8"), other);
    assert_eq!(sb.card_names(), ["Rock.m3u8"]);
    assert_eq!(sb.restoring(), None);
    assert_eq!(sb.backups().len(), before + 1, "the restore made no backup");
    assert!(
        out(&o).contains(sb.backups().last().unwrap()),
        "the backup taken first is not named: {}",
        out(&o)
    );

    // The library has moved on: the next sync writes it again.
    sb.sync();
    assert_eq!(
        locations(&sb.read("Rock.m3u8")),
        ["Music/t2.mp3", "Music/t1.mp3"]
    );
    assert_eq!(locations(&sb.read("Pop.m3u8")), ["Music/t2.mp3"]);
}

// Prevents: a restore without --yes and without a terminal changing the
// card; and an unknown stamp exiting 0 or touching anything.
#[test]
fn restore_without_yes_or_with_an_unknown_stamp_changes_nothing() {
    let sb = Sandbox::adopted(2);
    let (_first, second, _) = two_syncs(&sb);
    let card = sb.card_tree();
    let backups = sb.backups();
    let o = sb.run(&["restore", &second]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--yes"), "{}", err(&o));
    let o = sb.run(&["restore", "20000101-000000", "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("20000101-000000"), "{}", err(&o));
    assert_eq!(sb.card_tree(), card);
    assert_eq!(sb.backups(), backups);
    assert_eq!(sb.restoring(), None);
}

// Prevents: a declined restore exiting 1 or restoring anyway.
#[test]
fn declining_a_restore_exits_4_and_changes_nothing() {
    let sb = Sandbox::adopted(2);
    let (_first, second, _) = two_syncs(&sb);
    let card = sb.card_tree();
    let backups = sb.backups();
    let Some(o) = sb.run_tty(&["restore", &second], "n\n") else {
        eprintln!("skipped: `script` is not installed");
        return;
    };
    if !asked_on_a_terminal(&o, &second) {
        return;
    }
    assert_code(&o, 4);
    assert_eq!(sb.card_tree(), card);
    assert_eq!(sb.backups(), backups);
    assert_eq!(sb.restoring(), None);
}

// Prevents: a restore that fails part way leaving the owner without a
// hint: exit 1, and the message says syncs are refused until a restore
// succeeds. The index really refuses the next sync; a second restore, once
// the cause is gone, completes.
#[test]
fn a_restore_that_fails_part_way_says_syncs_are_refused_until_one_succeeds() {
    let sb = Sandbox::adopted(2);
    let (_first, second, _) = two_syncs(&sb);
    // A folder where a managed playlist the restore deletes is.
    fs::remove_file(sb.card.root.join("Pop.m3u8")).unwrap();
    fs::create_dir(sb.card.root.join("Pop.m3u8")).unwrap();
    let o = sb.run(&["restore", &second, "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("syncs are refused"), "{}", err(&o));
    assert_eq!(sb.restoring().as_deref(), Some(second.as_str()));
    let o = sb.run(&["sync", "--playlists-only", "--yes"]);
    assert_code(&o, 1);

    fs::remove_dir(sb.card.root.join("Pop.m3u8")).unwrap();
    assert_ok(&sb.run(&["restore", &second, "--yes"]));
    assert_eq!(sb.restoring(), None);
    sb.sync();
}
