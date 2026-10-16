//! End to end tests of `fiio-tunes device info` and `fiio-tunes probe` on a
//! card that is a temp folder (`device.allow_folder`). Every run gets its own
//! HOME, XDG_CONFIG_HOME and XDG_DATA_HOME in a temp dir, and
//! FIIO_TUNES_NO_DETECT is always set: nothing here can find the real X3.
//! Stdin is closed unless a test says otherwise.

#[path = "../../fiio-core/tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use common::{Kind, fixture};
use fiio_core::config::Config;

/// Sets the flag when dropped.
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

type Tree = BTreeMap<String, Option<Vec<u8>>>;

struct Sandbox {
    dir: tempfile::TempDir,
    music: PathBuf,
    lists: PathBuf,
    card: PathBuf,
}

impl Sandbox {
    /// `init` with a library of `mp3s` MP3 files, scanned. The config names
    /// the card folder as `device.mountpoint` with `allow_folder`.
    fn new(mp3s: u32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let sb = Sandbox {
            music: base.join("music"),
            lists: base.join("lists"),
            card: base.join("card"),
            dir,
        };
        for d in [&sb.music, &sb.lists, &sb.card] {
            fs::create_dir(d).unwrap();
        }
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
            c.device.mountpoint = sb.card.clone();
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

    fn record(&self) -> PathBuf {
        self.base().join("data/fiio-tunes/probe.toml")
    }

    fn edit_config(&self, f: impl FnOnce(&mut Config)) {
        let mut c = Config::load(&self.config_file()).unwrap();
        f(&mut c);
        c.save(&self.config_file()).unwrap();
    }

    /// `program` with the sandbox's environment.
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

    fn card_tree(&self) -> Tree {
        tree(&self.card)
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

/// The card with a few files of its own, so "left as before" means something.
fn seed_card(sb: &Sandbox) {
    for (p, bytes) in [
        ("Music/a.mp3", &[1u8; 1000][..]),
        ("Music/Album/b.flac", &[2; 500]),
        ("Other/c.mp3", &[3; 10]),
        ("best.m3u8", b"#EXTM3U\n"),
        ("cover.jpg", b"jpg"),
        (".fiio-tunes/manifest.json", b"{}"),
    ] {
        let p = sb.card.join(p);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }
}

// ---- device info

// Prevents: `device info` misreporting a folder card: its mount, key,
// write permission, manifest and counts are the first thing the owner reads
// before the first sync. It must also write nothing.
#[test]
fn device_info_describes_a_folder_card_and_writes_nothing() {
    let sb = Sandbox::new(1);
    seed_card(&sb);
    let before = sb.card_tree();
    let o = sb.run(&["device", "info"]);
    assert_ok(&o);
    let text = out(&o);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "not a USB device", "{text}");
    assert!(lines[1].starts_with("key: none"), "{text}");
    assert_eq!(lines[2], format!("folder {}, not a mount", s(&sb.card)));
    assert!(
        lines[3].starts_with("space: ") && lines[3].contains(" free of "),
        "{text}"
    );
    assert_eq!(lines[4], "writes: allowed");
    assert_eq!(lines[5], "manifest: present");
    assert!(
        lines[6].starts_with("on the card: 3 audio files (")
            && lines[6].ends_with("), 2 in Music, 1 playlists"),
        "{text}"
    );
    assert_eq!(&lines[7..], ["playlists: 1", "  best.m3u8"], "{text}");
    assert_eq!(sb.card_tree(), before);
}

// Prevents: a card without a manifest or files being reported as having
// them, and the hint being missing when no sync has happened.
#[test]
fn device_info_on_an_empty_card_has_no_manifest_and_no_files() {
    let sb = Sandbox::new(1);
    let o = sb.run(&["device", "info"]);
    assert_ok(&o);
    let text = out(&o);
    assert!(text.contains("\nmanifest: none"), "{text}");
    assert!(text.contains("on the card: 0 audio files ("), "{text}");
    assert!(text.contains("), 0 in Music, 0 playlists"), "{text}");
    assert!(
        !text.contains("playlists:"),
        "no list for no playlists: {text}"
    );
    assert!(sb.card_tree().is_empty());
}

// Prevents: `device info` hiding that writes would be refused (it is the
// command to run to find out why) or failing instead of saying so.
#[test]
fn device_info_says_writes_are_refused_for_a_folder_without_allow_folder() {
    let sb = Sandbox::new(1);
    seed_card(&sb);
    sb.edit_config(|c| c.device.allow_folder = false);
    let before = sb.card_tree();
    let o = sb.run(&["device", "info"]);
    assert_ok(&o);
    let text = out(&o);
    assert!(text.contains("\nwrites: refused ("), "{text}");
    assert!(!text.contains("writes: allowed"), "{text}");
    assert_eq!(sb.card_tree(), before);
}

// Prevents: a mountpoint that does not exist being described as an empty
// card.
#[test]
fn device_info_on_a_missing_mountpoint_fails() {
    let sb = Sandbox::new(1);
    let gone = sb.base().join("no-such-card");
    sb.edit_config(|c| c.device.mountpoint = gone.clone());
    let o = sb.run(&["device", "info"]);
    assert_code(&o, 1);
    assert!(err(&o).contains(s(&gone)), "{}", err(&o));
    assert!(!gone.exists());
}

// Prevents: detection being quietly skipped, or the hint about
// `device.mountpoint` lost, when nothing names the card and detection is
// off (as it is for everything cargo runs).
#[test]
fn with_detection_off_and_no_mountpoint_both_commands_fail_with_the_hint() {
    let sb = Sandbox::new(1);
    sb.edit_config(|c| c.device.mountpoint = PathBuf::new());
    seed_card(&sb);
    let before = sb.card_tree();
    for args in [
        &["device", "info"][..],
        &["probe", "--yes"],
        &["probe", "--clean", "--yes"],
    ] {
        let o = sb.run(args);
        assert_code(&o, 1);
        assert!(
            err(&o).contains("device.mountpoint"),
            "{args:?}: {}",
            err(&o)
        );
        assert!(out(&o).is_empty(), "{args:?}: {}", out(&o));
    }
    assert_eq!(sb.card_tree(), before);
    assert!(!sb.record().exists());
}

// ---- probe

// Prevents: the probe leaving anything on the card after `--clean`, or
// writing less/more than it says; the owner's files must survive both.
#[test]
fn probe_then_clean_leaves_the_card_as_it_was() {
    let sb = Sandbox::new(1);
    seed_card(&sb);
    let library = tree(&sb.music);
    let before = sb.card_tree();

    let o = sb.run(&["probe", "--yes"]);
    assert_ok(&o);
    let text = out(&o);
    assert!(text.contains("12 playlists:"), "{text}");
    assert!(text.contains("zz-probe-12.m3u8"), "{text}");
    assert!(text.contains("wrote 18 files"), "{text}");
    assert!(
        text.contains("\"04 é.mp3\" and \"04 É.mp3\" as two files"),
        "{text}"
    );
    let after = sb.card_tree();
    let added: Vec<&String> = after.keys().filter(|k| !before.contains_key(*k)).collect();
    assert_eq!(
        added.iter().filter(|k| k.ends_with(".m3u8")).count(),
        12,
        "{added:?}"
    );
    assert_eq!(
        added.iter().filter(|k| k.ends_with(".mp3")).count(),
        6,
        "{added:?}"
    );
    for (k, v) in &before {
        assert_eq!(after.get(k), Some(v), "{k} changed");
    }
    // The first playlist carries the duration of the source, not a guess.
    let first = String::from_utf8(after["zz-probe-01.m3u8"].clone().unwrap()).unwrap();
    assert!(first.starts_with("#EXTM3U\n#EXTINF:"), "{first}");
    assert!(
        first.ends_with("fiio-tunes - Probe 01\nMusic/zz-probe/01 Probe.mp3\n"),
        "{first}"
    );
    assert!(sb.record().is_file());
    // A copy is a copy of the library file.
    let src = fs::read(sb.music.join("t1.mp3")).unwrap();
    assert_eq!(
        after["Music/zz-probe/01 Probe.mp3"].as_deref(),
        Some(&src[..])
    );

    let o = sb.run(&["probe", "--clean", "--yes"]);
    assert_ok(&o);
    assert!(out(&o).contains("removed 18 files"), "{}", out(&o));
    assert_eq!(sb.card_tree(), before);
    assert!(!sb.record().exists());
    assert_eq!(tree(&sb.music), library, "the library was touched");
}

// Prevents: the same, on an empty card, where the folders the probe makes
// (`Music`, `Playlists`) must go too.
#[test]
fn probe_then_clean_on_an_empty_card_leaves_it_empty() {
    let sb = Sandbox::new(1);
    assert_ok(&sb.run(&["probe", "--yes"]));
    assert!(sb.card.join("Playlists/zz-probe-08.m3u8").is_file());
    assert_ok(&sb.run(&["probe", "--clean", "--yes"]));
    assert!(sb.card_tree().is_empty(), "{:?}", sb.card_tree().keys());
}

// Prevents: a script or a closed stdin writing to a card without having been
// asked: no terminal and no --yes is an error before any byte is written.
#[test]
fn probe_without_yes_and_without_a_terminal_asks_for_yes_and_writes_nothing() {
    let sb = Sandbox::new(1);
    seed_card(&sb);
    let before = sb.card_tree();
    let o = sb.run(&["probe"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--yes"), "{}", err(&o));
    assert_eq!(sb.card_tree(), before);
    assert!(!sb.record().exists());

    // The same for --clean: the probe stays where it is.
    assert_ok(&sb.run(&["probe", "--yes"]));
    let probed = sb.card_tree();
    let o = sb.run(&["probe", "--clean"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--yes"), "{}", err(&o));
    assert_eq!(sb.card_tree(), probed);
}

// Prevents: a second `probe` stacking onto the first (or printing a plan it
// will not carry out): it refuses before showing the plan, changing nothing,
// and says how to get out.
#[test]
fn a_second_probe_refuses_before_printing_the_plan() {
    let sb = Sandbox::new(1);
    assert_ok(&sb.run(&["probe", "--yes"]));
    let probed = sb.card_tree();
    let record = fs::read(sb.record()).unwrap();
    let o = sb.run(&["probe", "--yes"]);
    assert_code(&o, 1);
    assert!(out(&o).is_empty(), "a plan was printed: {}", out(&o));
    assert!(err(&o).contains("--clean"), "{}", err(&o));
    assert_eq!(sb.card_tree(), probed);
    assert_eq!(fs::read(sb.record()).unwrap(), record);
    // Even one probe file is enough to refuse.
    assert_ok(&sb.run(&["probe", "--clean", "--yes"]));
    fs::write(sb.card.join("zz-probe-12.m3u8"), b"old").unwrap();
    let o = sb.run(&["probe", "--yes"]);
    assert_code(&o, 1);
    assert!(out(&o).is_empty(), "{}", out(&o));
    assert_eq!(sb.card_tree().len(), 1);
}

// Prevents: the safety check being bypassed by a plain folder: without
// allow_folder neither writing nor cleaning may touch it.
#[test]
fn probe_is_refused_on_a_plain_folder_without_allow_folder() {
    let sb = Sandbox::new(1);
    seed_card(&sb);
    let before = sb.card_tree();
    sb.edit_config(|c| c.device.allow_folder = false);
    let o = sb.run(&["probe", "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains(s(&sb.card)), "{}", err(&o));
    assert!(out(&o).is_empty(), "{}", out(&o));
    assert_eq!(sb.card_tree(), before);
    assert!(!sb.record().exists());

    // A probe that is on the card is not cleaned either.
    sb.edit_config(|c| c.device.allow_folder = true);
    assert_ok(&sb.run(&["probe", "--yes"]));
    let probed = sb.card_tree();
    let record = fs::read(sb.record()).unwrap();
    sb.edit_config(|c| c.device.allow_folder = false);
    let o = sb.run(&["probe", "--clean", "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains(s(&sb.card)), "{}", err(&o));
    assert_eq!(sb.card_tree(), probed);
    assert_eq!(fs::read(sb.record()).unwrap(), record);
}

// Prevents: the probe copying something that is not an MP3 (or nothing at
// all) when the library has no MP3: it stops with a message about MP3s.
#[test]
fn probe_without_an_mp3_in_the_library_fails_and_writes_nothing() {
    let sb = Sandbox::new(0);
    fs::copy(fixture(Kind::Flac, 1), sb.music.join("only.flac")).unwrap();
    assert_ok(&sb.run(&["scan"]));
    seed_card(&sb);
    let before = sb.card_tree();
    let o = sb.run(&["probe", "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("MP3"), "{}", err(&o));
    assert_eq!(sb.card_tree(), before);
    assert!(!sb.record().exists());
}

// Prevents: a refused plan (the owner answers no) writing anyway, and the
// refusal being reported as a failure (exit 1) rather than as a decline
// (exit 4). Needs a terminal: util-linux `script` provides one.
#[test]
fn answering_no_at_the_prompt_exits_4_and_writes_nothing() {
    let sb = Sandbox::new(1);
    seed_card(&sb);
    let before = sb.card_tree();
    let Some(o) = sb.run_tty(&["probe"], "n\n") else {
        eprintln!("skipped: `script` is not installed");
        return;
    };
    let text = format!("{}{}", out(&o), err(&o));
    if !text.contains("write the probe to the card?") {
        // The command ran without a terminal (the plan was printed, or it
        // asked for --yes): that is a failure of what is tested. Only a
        // `script` that could not provide a terminal at all skips.
        assert!(
            !text.contains("zz-probe-") && !text.contains("needs confirmation"),
            "the command did not ask on a terminal: {text}"
        );
        eprintln!("skipped: no terminal could be provided: {text}");
        return;
    }
    assert_code(&o, 4);
    assert_eq!(sb.card_tree(), before);
    assert!(!sb.record().exists());

    // And "yes" at the same prompt does write.
    let o = sb.run_tty(&["probe"], "y\n").unwrap();
    assert_ok(&o);
    assert!(sb.card.join("zz-probe-01.m3u8").is_file());

    // Declining the clean leaves the probe.
    let probed = sb.card_tree();
    let o = sb.run_tty(&["probe", "--clean"], "n\n").unwrap();
    assert_code(&o, 4);
    assert_eq!(sb.card_tree(), probed);
    assert!(sb.record().is_file());
}

// Prevents: a copy that reads back wrong being reported as an ordinary
// error (exit 1) or, worse, as success: the contract is exit code 3 and
// no playlist written after it. A second thread cuts the first copy short
// as soon as it appears; the source is big so the read-back sees it, and
// the attempt is repeated in the unlikely case the thread was too slow.
#[test]
fn a_copy_that_reads_back_wrong_exits_3() {
    let mut last = String::new();
    for attempt in 0..8 {
        let sb = Sandbox::new(1);
        // The library MP3 grows by 64 MB of padding after its tags and audio.
        let big = sb.music.join("t1.mp3");
        let mut bytes = fs::read(&big).unwrap();
        bytes.resize(bytes.len() + (64 << 20), 0);
        fs::write(&big, bytes).unwrap();
        assert_ok(&sb.run(&["scan"]));
        let target = sb.card.join("Music/zz-probe/01 Probe.mp3");
        let stop = AtomicBool::new(false);
        let o = std::thread::scope(|scope| {
            // Set on any exit from this closure, a panic included: the
            // scope waits for the thread, which would spin for ever.
            let _stop = StopOnDrop(&stop);
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(f) = fs::OpenOptions::new().write(true).open(&target) {
                        let _ = f.set_len(1);
                    }
                }
            });
            sb.run(&["probe", "--yes"])
        });
        if code(&o) == 3 {
            assert!(err(&o).contains("01 Probe.mp3"), "{}", err(&o));
            assert!(!out(&o).contains("wrote "), "{}", out(&o));
            let t = sb.card_tree();
            assert!(!t.keys().any(|k| k.ends_with(".m3u8")), "{:?}", t.keys());
            // And the half probe is cleaned the usual way.
            assert_ok(&sb.run(&["probe", "--clean", "--yes"]));
            assert!(sb.card_tree().is_empty());
            return;
        }
        last = format!("attempt {attempt}: exit {}: {}", code(&o), err(&o));
    }
    panic!("the damage was never seen by the read-back: {last}");
}

// Prevents: probing a second card while the first card's probe is still
// recorded: the plan must not even be printed, nothing may be written to
// the second card, and the record of the first must be kept.
#[test]
fn probe_refuses_before_the_plan_when_the_record_names_another_card() {
    let sb = Sandbox::new(1);
    assert_ok(&sb.run(&["probe", "--yes"]));
    let record = fs::read(sb.record()).unwrap();
    let second = sb.base().join("card2");
    fs::create_dir(&second).unwrap();
    sb.edit_config(|c| c.device.mountpoint = second.clone());
    let o = sb.run(&["probe", "--yes"]);
    assert_code(&o, 1);
    assert!(out(&o).is_empty(), "a plan was printed: {}", out(&o));
    assert!(err(&o).contains(s(&sb.card)), "{}", err(&o));
    assert!(tree(&second).is_empty());
    assert_eq!(fs::read(sb.record()).unwrap(), record);
    // The first card is cleaned as usual, and then the second may be probed.
    sb.edit_config(|c| c.device.mountpoint = sb.card.clone());
    assert_ok(&sb.run(&["probe", "--clean", "--yes"]));
    assert!(sb.card_tree().is_empty());
    sb.edit_config(|c| c.device.mountpoint = second.clone());
    assert_ok(&sb.run(&["probe", "--yes"]));
    assert!(second.join("zz-probe-01.m3u8").is_file());
}

// Prevents: a card folder spelled `music` (FAT keeps the case it was given,
// and compares without it) not being counted as the music folder.
#[test]
fn device_info_counts_the_music_folder_without_regard_to_case() {
    let sb = Sandbox::new(1);
    for p in [
        "music/a.mp3",
        "MUSIC/Sub/b.flac",
        "Musica/c.mp3",
        "Other/music/d.mp3",
    ] {
        let p = sb.card.join(p);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"x").unwrap();
    }
    let o = sb.run(&["device", "info"]);
    assert_ok(&o);
    let text = out(&o);
    assert!(text.contains("on the card: 4 audio files ("), "{text}");
    assert!(text.contains("), 2 in Music, 0 playlists"), "{text}");
}

// Prevents: sizes under a gigabyte printed as `0.0 GB`: they are in MB.
#[test]
fn device_info_prints_megabytes_under_a_gigabyte_and_gigabytes_above() {
    let sb = Sandbox::new(1);
    let big = |name: &str, len: u64| {
        let f = fs::File::create(sb.card.join(name)).unwrap();
        f.set_len(len).unwrap(); // sparse: nothing is written
    };
    big("a.mp3", 2_500_000);
    let text = out(&sb.run(&["device", "info"]));
    assert!(
        text.contains("on the card: 1 audio files (2.5 MB)"),
        "{text}"
    );
    big("b.flac", 1_500_000_000);
    let text = out(&sb.run(&["device", "info"]));
    assert!(
        text.contains("on the card: 2 audio files (1.5 GB)"),
        "{text}"
    );
}

// Prevents: the same stale record blocking the CLI: after the first card's
// probe files are removed by hand, probing another card works, and the
// record then names that card (so --clean there empties it).
#[test]
fn probe_replaces_a_stale_record_of_another_card() {
    let sb = Sandbox::new(1);
    assert_ok(&sb.run(&["probe", "--yes"]));
    fs::remove_dir_all(&sb.card).unwrap();
    fs::create_dir(&sb.card).unwrap();
    let second = sb.base().join("card2");
    fs::create_dir(&second).unwrap();
    sb.edit_config(|c| c.device.mountpoint = second.clone());
    assert_ok(&sb.run(&["probe", "--yes"]));
    assert!(second.join("zz-probe-01.m3u8").is_file());
    assert!(sb.card_tree().is_empty(), "the first card was written");
    assert_ok(&sb.run(&["probe", "--clean", "--yes"]));
    assert!(tree(&second).is_empty(), "{:?}", tree(&second).keys());
}
