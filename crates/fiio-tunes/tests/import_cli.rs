//! End to end tests of `fiio-tunes import iprune` and `import ipod`. Every run
//! gets its own HOME, XDG_CONFIG_HOME and XDG_DATA_HOME in a temp dir, where
//! the fake iprune lives too (`<config>/iprune/config.toml`,
//! `<data>/iprune/iprune.sqlite`), so nothing touches the real home, the real
//! iprune or a real iPod. Stdin is closed.

#[path = "../../fiio-core/tests/common/mod.rs"]
mod common;
#[path = "../../fiio-core/tests/common/ipod.rs"]
mod fake;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{Kind, fixture};
use fake::{FakeIpod, IpodTrack, Iprune, Spec, Stat, snapshot};
use fiio_core::config::{Config, Scope};
use fiio_core::store::{self, Store, TrackRow};

const A: u64 = 0xA1;
const B: u64 = 0xB2;
const C: u64 = 0xC3;
const X: u64 = 0xE5;

type Tree = BTreeMap<PathBuf, Option<Stat>>;

struct Sandbox {
    dir: tempfile::TempDir,
    music: PathBuf,
    lists: PathBuf,
    ip: Iprune,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let music = base.join("music");
        let lists = base.join("lists");
        fs::create_dir(&music).unwrap();
        fs::create_dir(&lists).unwrap();
        let ip = Iprune::new(
            &base.join("config"),
            &base.join("data"),
            &base.join("iprune-playlists"),
            "",
        );
        Sandbox {
            dir,
            music,
            lists,
            ip,
        }
    }

    fn base(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fiio-tunes"))
            .args(args)
            .env("HOME", self.base().join("home"))
            .env("XDG_CONFIG_HOME", self.base().join("config"))
            .env("XDG_DATA_HOME", self.base().join("data"))
            .env("FIIO_TUNES_NO_DETECT", "1")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// `init` and a scan of three tracks a.mp3, b.mp3, c.mp3.
    fn library() -> (Self, Vec<TrackRow>) {
        let sb = Sandbox::new();
        assert_ok(&sb.run(&[
            "init",
            "--library",
            s(&sb.music),
            "--playlists",
            s(&sb.lists),
        ]));
        for (i, n) in ["a.mp3", "b.mp3", "c.mp3"].iter().enumerate() {
            fs::copy(fixture(Kind::Mp3, i as u32 + 1), sb.music.join(n)).unwrap();
        }
        assert_ok(&sb.run(&["scan"]));
        let store = Store::open(&sb.base().join("data/fiio-tunes/fiio-tunes.sqlite")).unwrap();
        let mut rows = store::all_tracks(store.conn()).unwrap();
        rows.sort_by(|x, y| x.path.cmp(&y.path));
        assert_eq!(rows.len(), 3);
        (sb, rows)
    }

    fn playlist_file(&self, name: &str) -> PathBuf {
        self.lists.join(format!("{name}.m3u8"))
    }

    fn locations(&self, name: &str) -> Vec<String> {
        fs::read_to_string(self.playlist_file(name))
            .unwrap()
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(String::from)
            .collect()
    }

    fn config(&self) -> Config {
        Config::load(&self.base().join("config/fiio-tunes/config.toml")).unwrap()
    }

    /// iprune's three folders, which no import may change.
    fn iprune_trees(&self) -> Vec<Tree> {
        [
            self.ip.config_file.parent().unwrap(),
            self.ip.database.parent().unwrap(),
            &self.ip.playlists,
        ]
        .iter()
        .map(|p| snapshot(p, false))
        .collect()
    }

    fn lists_tree(&self) -> Tree {
        snapshot(&self.lists, false)
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn assert_code(o: &Output, code: i32) {
    assert_eq!(
        o.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        out(o),
        err(o)
    );
}

fn assert_ok(o: &Output) {
    assert_code(o, 0);
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn strings(p: &[&Path]) -> Vec<String> {
    p.iter().map(|p| s(p).to_owned()).collect()
}

// ---- import iprune

#[test]
fn import_iprune_copies_playlists_keeps_unresolved_lines_and_leaves_iprune_alone() {
    let (sb, t) = Sandbox::library();
    sb.ip.write_playlist(
        "Road",
        &format!("#EXTM3U\n{}\n/nowhere/ghost.mp3\n", t[0].path.display()),
    );
    sb.ip.write_playlist("Other", "#EXTM3U\n");
    sb.ip.index().device("1", "p", None);
    let before = sb.iprune_trees();

    let o = sb.run(&["import", "iprune"]);
    assert_ok(&o);
    assert_eq!(sb.locations("Road"), [s(&t[0].path), "/nowhere/ghost.mp3"]);
    assert!(sb.playlist_file("Other").is_file());
    assert!(out(&o).contains("/nowhere/ghost.mp3"), "{}", out(&o));
    assert_eq!(sb.iprune_trees(), before);
}

#[test]
fn import_iprune_with_names_imports_only_those() {
    let (sb, t) = Sandbox::library();
    sb.ip
        .write_playlist("Road", &format!("#EXTM3U\n{}\n", t[0].path.display()));
    sb.ip.write_playlist("Other", "#EXTM3U\n");
    assert_ok(&sb.run(&["import", "iprune", "road"]));
    assert!(sb.playlist_file("Road").is_file());
    assert!(!sb.playlist_file("Other").exists());
}

#[test]
fn import_iprune_list_writes_nothing_and_shows_names_and_counts() {
    let (sb, t) = Sandbox::library();
    sb.ip.write_playlist(
        "Road",
        &format!(
            "#EXTM3U\n{}\n{}\n",
            t[0].path.display(),
            t[1].path.display()
        ),
    );
    sb.ip.write_playlist("Taken", "#EXTM3U\n");
    fs::write(sb.playlist_file("Taken"), "#EXTM3U\n").unwrap();
    let lists = sb.lists_tree();
    let ip = sb.iprune_trees();
    let o = sb.run(&["import", "iprune", "--list"]);
    assert_ok(&o);
    let text = out(&o);
    let road = text.lines().find(|l| l.contains("Road")).expect(&text);
    assert_eq!(road.split_whitespace().next(), Some("2"), "{text}");
    let taken = text.lines().find(|l| l.contains("Taken")).expect(&text);
    assert!(taken.contains("taken"), "{taken}");
    assert!(!road.contains("taken"), "{road}");
    assert_eq!(sb.lists_tree(), lists);
    assert_eq!(sb.iprune_trees(), ip);
}

#[test]
fn import_iprune_conflict_modes() {
    let (sb, t) = Sandbox::library();
    let mine = "#EXTM3U\n/mine.mp3\n";
    fs::write(sb.playlist_file("Road"), mine).unwrap();
    sb.ip
        .write_playlist("Road", &format!("#EXTM3U\n{}\n", t[1].path.display()));

    // The default keeps what is there, and that is not an error.
    assert_ok(&sb.run(&["import", "iprune"]));
    assert_eq!(fs::read_to_string(sb.playlist_file("Road")).unwrap(), mine);
    assert_eq!(fs::read_dir(&sb.lists).unwrap().count(), 1);

    assert_ok(&sb.run(&["import", "iprune", "--on-conflict", "rename"]));
    assert_eq!(fs::read_to_string(sb.playlist_file("Road")).unwrap(), mine);
    assert_eq!(sb.locations("Road (iPod)"), [s(&t[1].path)]);

    assert_ok(&sb.run(&["import", "iprune", "--on-conflict", "replace"]));
    assert_eq!(sb.locations("Road"), [s(&t[1].path)]);
}

#[test]
fn import_iprune_failures_exit_1_or_2_and_write_nothing() {
    let (sb, t) = Sandbox::library();
    sb.ip
        .write_playlist("Road", &format!("#EXTM3U\n{}\n", t[0].path.display()));
    let lists = sb.lists_tree();

    // Unknown name: exit 1, and the good name given with it is not imported.
    assert_code(&sb.run(&["import", "iprune", "Road", "Nope"]), 1);
    assert_eq!(sb.lists_tree(), lists);
    // Bad option value: a usage error.
    assert_code(&sb.run(&["import", "iprune", "--on-conflict", "bogus"]), 2);
    assert_eq!(sb.lists_tree(), lists);
    // iprune's folder is fiio-tunes' own.
    fs::write(
        &sb.ip.config_file,
        format!("[playlists]\ndir = {:?}\n", sb.lists),
    )
    .unwrap();
    assert_code(&sb.run(&["import", "iprune"]), 1);
    assert_eq!(sb.lists_tree(), lists);
    // No iprune config at all.
    fs::remove_file(&sb.ip.config_file).unwrap();
    assert_code(&sb.run(&["import", "iprune"]), 1);
    assert_eq!(sb.lists_tree(), lists);
}

#[test]
fn import_iprune_reports_an_unreadable_playlist_and_imports_the_rest() {
    let (sb, t) = Sandbox::library();
    sb.ip
        .write_playlist("Good", &format!("#EXTM3U\n{}\n", t[0].path.display()));
    fs::write(sb.ip.playlists.join("Bad.m3u8"), b"#EXTM3U\n/caf\xe9.mp3\n").unwrap();
    let o = sb.run(&["import", "iprune"]);
    assert_ok(&o);
    assert!(sb.playlist_file("Good").is_file());
    assert!(!sb.playlist_file("Bad").exists());
    assert!(out(&o).contains("Bad.m3u8"), "{}", out(&o));
}

// ---- import ipod

/// An iPod of the three library tracks and a stranger: playlists Rock
/// [A, B, X] and Jazz [C].
fn ipod_spec(t: &[TrackRow]) -> Spec {
    Spec::new("Julien's iPod")
        .track(IpodTrack::of(A, &t[0]))
        .track(IpodTrack::of(B, &t[1]))
        .track(IpodTrack::of(C, &t[2]))
        .track(IpodTrack::stranger(X))
        .playlist("Rock", &[A, B, X])
        .smart("Jazz", &[C])
}

fn ipod_trees(fake: &FakeIpod) -> Tree {
    snapshot(&fake.root(), false)
}

#[test]
fn import_ipod_file_imports_the_traced_tracks_and_reports_what_is_left_out() {
    let (sb, t) = Sandbox::library();
    let fake = ipod_spec(&t).file_only();
    let (ip, ipod) = (sb.iprune_trees(), ipod_trees(&fake));
    let o = sb.run(&["import", "ipod", "--file", s(&fake.itunesdb())]);
    assert_ok(&o);
    assert_eq!(sb.locations("Rock"), [s(&t[0].path), s(&t[1].path)]);
    assert_eq!(sb.locations("Jazz"), [s(&t[2].path)]);
    // The track with no library file is named in the report.
    assert!(out(&o).contains("Stranger"), "{}", out(&o));
    assert_eq!(sb.iprune_trees(), ip);
    assert_eq!(ipod_trees(&fake), ipod);
}

#[test]
fn import_ipod_mountpoint_compares_files_and_leaves_the_ipod_alone() {
    let (sb, t) = Sandbox::library();
    // Tags and duration say nothing; only the file can.
    let spec = Spec::new("P")
        .track(
            IpodTrack::of(A, &t[0])
                .title("zzz")
                .artist("yyy")
                .len_ms(t[0].info.duration_ms.unwrap() + 20_000),
        )
        .playlist("Rock", &[A]);
    let fake = spec.mounted();
    let before = ipod_trees(&fake);
    assert_ok(&sb.run(&["import", "ipod", "--mountpoint", s(&fake.root())]));
    assert_eq!(sb.locations("Rock"), [s(&t[0].path)]);
    assert_eq!(ipod_trees(&fake), before);
}

#[test]
fn import_ipod_goes_through_iprunes_index_found_by_overlap() {
    let (sb, t) = Sandbox::library();
    let spec = Spec::new("P")
        .track(IpodTrack::stranger(A))
        .track(IpodTrack::stranger(B))
        .playlist("Rock", &[A, B]);
    let fake = spec.file_only();
    let idx = sb.ip.index();
    let dev = idx.device("0x000a27001234abcd", "Julien", None);
    idx.map(dev, A, &t[1].path);
    idx.map(dev, B, &t[2].path);
    drop(idx);
    let ip = sb.iprune_trees();
    assert_ok(&sb.run(&["import", "ipod", "--file", s(&fake.itunesdb())]));
    assert_eq!(sb.locations("Rock"), [s(&t[1].path), s(&t[2].path)]);
    assert_eq!(sb.iprune_trees(), ip);
}

#[test]
fn import_ipod_with_an_unreadable_index_still_imports_and_says_so() {
    let (sb, t) = Sandbox::library();
    let fake = ipod_spec(&t).file_only();
    fs::write(&sb.ip.database, b"this is not a database at all, honest").unwrap();
    let o = sb.run(&["import", "ipod", "--file", s(&fake.itunesdb())]);
    assert_ok(&o);
    assert!(out(&o).contains("could not be read"), "{}", out(&o));
    assert_eq!(sb.locations("Rock"), [s(&t[0].path), s(&t[1].path)]);
}

#[test]
fn import_ipod_list_writes_nothing_and_shows_counts_smart_and_taken() {
    let (sb, t) = Sandbox::library();
    let fake = ipod_spec(&t).mounted();
    fs::write(sb.playlist_file("Jazz"), "#EXTM3U\n").unwrap();
    let (lists, ip, ipod) = (sb.lists_tree(), sb.iprune_trees(), ipod_trees(&fake));
    let o = sb.run(&["import", "ipod", "--mountpoint", s(&fake.root()), "--list"]);
    assert_ok(&o);
    let text = out(&o);
    let rock = text.lines().find(|l| l.contains("Rock")).expect(&text);
    // 2 of its 3 members have a library file.
    assert_eq!(rock.split_whitespace().next(), Some("2/3"), "{text}");
    let jazz = text.lines().find(|l| l.contains("Jazz")).expect(&text);
    assert!(jazz.contains("smart") && jazz.contains("taken"), "{jazz}");
    assert_eq!(sb.lists_tree(), lists);
    assert_eq!(sb.iprune_trees(), ip);
    assert_eq!(ipod_trees(&fake), ipod);
}

#[test]
fn import_ipod_conflict_modes_and_names() {
    let (sb, t) = Sandbox::library();
    let fake = ipod_spec(&t).file_only();
    let file = s(&fake.itunesdb()).to_owned();
    let mine = "#EXTM3U\n/mine.mp3\n";
    fs::write(sb.playlist_file("Rock"), mine).unwrap();

    assert_ok(&sb.run(&["import", "ipod", "--file", &file, "rock"]));
    assert_eq!(fs::read_to_string(sb.playlist_file("Rock")).unwrap(), mine);
    assert_ok(&sb.run(&[
        "import",
        "ipod",
        "--file",
        &file,
        "Rock",
        "--on-conflict",
        "rename",
    ]));
    assert_eq!(sb.locations("Rock (iPod)"), [s(&t[0].path), s(&t[1].path)]);
    assert_ok(&sb.run(&[
        "import",
        "ipod",
        "--file",
        &file,
        "Rock",
        "--on-conflict",
        "replace",
    ]));
    assert_eq!(sb.locations("Rock"), [s(&t[0].path), s(&t[1].path)]);
    assert!(
        !sb.playlist_file("Jazz").exists(),
        "only Rock was asked for"
    );

    let lists = sb.lists_tree();
    assert_code(
        &sb.run(&["import", "ipod", "--file", &file, "Rock", "Nope"]),
        1,
    );
    assert_eq!(sb.lists_tree(), lists);
}

#[test]
fn import_ipod_without_a_source_fails_when_detection_is_off() {
    let (sb, t) = Sandbox::library();
    let _ = t;
    let lists = sb.lists_tree();
    let o = sb.run(&["import", "ipod"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("FIIO_TUNES_NO_DETECT"), "{}", err(&o));
    assert_eq!(sb.lists_tree(), lists);
}

#[test]
fn import_ipod_bad_invocations_and_bad_sources() {
    let (sb, t) = Sandbox::library();
    let fake = ipod_spec(&t).file_only();
    let lists = sb.lists_tree();
    // --file and --mountpoint together, --scope with --list: usage errors.
    assert_code(
        &sb.run(&[
            "import",
            "ipod",
            "--file",
            s(&fake.itunesdb()),
            "--mountpoint",
            s(&fake.root()),
        ]),
        2,
    );
    assert_code(
        &sb.run(&[
            "import",
            "ipod",
            "--file",
            s(&fake.itunesdb()),
            "--scope",
            "--list",
        ]),
        2,
    );
    // A file that is not an iTunesDB, one that is not there, a folder with no iPod.
    let junk = sb.base().join("junk");
    fs::write(&junk, vec![7u8; 4096]).unwrap();
    assert_code(&sb.run(&["import", "ipod", "--file", s(&junk)]), 1);
    assert_code(
        &sb.run(&["import", "ipod", "--file", s(&sb.base().join("absent"))]),
        1,
    );
    assert_code(
        &sb.run(&["import", "ipod", "--mountpoint", s(&sb.music)]),
        1,
    );
    assert_eq!(sb.lists_tree(), lists);
}

// ---- the scope

/// An iprune that syncs the iPod of `ipod_spec` with `scope`, naming `names`.
fn iprune_scope(sb: &Sandbox, scope: &str, names: &[&str]) {
    let idx = sb.ip.index();
    let dev = idx.device("000A27001234ABCD", "Julien", Some(scope));
    for n in names {
        idx.scope_playlist(dev, n);
    }
}

#[test]
fn import_ipod_scope_copies_iprunes_scope_into_the_sync_section() {
    let (sb, t) = Sandbox::library();
    let fake = ipod_spec(&t).mounted();
    iprune_scope(&sb, "playlists", &["Rock", "jazz"]);
    assert_eq!(sb.config().sync.scope, Scope::All);
    let ip = sb.iprune_trees();
    let o = sb.run(&["import", "ipod", "--mountpoint", s(&fake.root()), "--scope"]);
    assert_ok(&o);
    let cfg = sb.config();
    assert_eq!(cfg.sync.scope, Scope::Playlists);
    assert_eq!(cfg.sync.playlists, ["Jazz", "Rock"]);
    // The rest of the config is as init made it.
    assert_eq!(cfg.library.roots, std::slice::from_ref(&sb.music));
    assert_eq!(cfg.playlists.dir, sb.lists);
    assert_eq!(sb.iprune_trees(), ip);
}

#[test]
fn import_ipod_scope_naming_a_playlist_not_imported_exits_1_after_importing_the_rest() {
    let (sb, t) = Sandbox::library();
    let fake = ipod_spec(&t).mounted();
    iprune_scope(&sb, "playlists", &["Rock", "Jazz"]);
    fs::write(sb.playlist_file("Jazz"), "#EXTM3U\n/mine.mp3\n").unwrap(); // skipped
    let config_before = fs::read(sb.base().join("config/fiio-tunes/config.toml")).unwrap();
    let o = sb.run(&["import", "ipod", "--mountpoint", s(&fake.root()), "--scope"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("Jazz"), "{}", err(&o));
    // Rock was imported all the same, Jazz kept, the config not touched.
    assert_eq!(sb.locations("Rock"), strings(&[&t[0].path, &t[1].path]));
    assert_eq!(
        fs::read_to_string(sb.playlist_file("Jazz")).unwrap(),
        "#EXTM3U\n/mine.mp3\n"
    );
    assert_eq!(
        fs::read(sb.base().join("config/fiio-tunes/config.toml")).unwrap(),
        config_before
    );
}

#[test]
fn import_ipod_scope_without_an_iprune_record_exits_1_after_importing() {
    let (sb, t) = Sandbox::library();
    let fake = ipod_spec(&t).file_only();
    let o = sb.run(&["import", "ipod", "--file", s(&fake.itunesdb()), "--scope"]);
    assert_code(&o, 1);
    assert!(sb.playlist_file("Rock").is_file());
    assert_eq!(sb.config().sync.scope, Scope::All);
}

// ---- a playlist that cannot be written

/// A playlist name whose file name, with `.m3u8`, no file system takes.
fn too_long() -> String {
    "L".repeat(300)
}

// Prevents a playlist that cannot be written from going unnoticed: the
// summary and the list come first, the others are imported, the exit is 1.
#[test]
fn import_iprune_reports_a_playlist_it_cannot_write_imports_the_rest_and_exits_1() {
    let (sb, t) = Sandbox::library();
    let long = "L".repeat(250);
    let text = format!("#EXTM3U\n{}\n", t[0].path.display());
    sb.ip.write_playlist("First", &text);
    sb.ip.write_playlist(&long, &text);
    sb.ip.write_playlist("Last", &text);
    // Taken, so the rename has to add to a name already as long as can be.
    fs::write(sb.playlist_file(&long), "#EXTM3U\n/mine.mp3\n").unwrap();
    let ip = sb.iprune_trees();
    let o = sb.run(&["import", "iprune", "--on-conflict", "rename"]);
    assert_code(&o, 1);
    let stdout = out(&o);
    assert!(stdout.contains("imported: 2 new"), "{stdout}");
    assert!(stdout.contains("playlists not written"), "{stdout}");
    assert!(stdout.contains(&long), "{stdout}");
    assert!(err(&o).contains(&long), "{}", err(&o));
    assert_eq!(sb.locations("First"), strings(&[&t[0].path]));
    assert_eq!(sb.locations("Last"), strings(&[&t[0].path]));
    assert_eq!(
        fs::read_to_string(sb.playlist_file(&long)).unwrap(),
        "#EXTM3U\n/mine.mp3\n"
    );
    assert_eq!(fs::read_dir(&sb.lists).unwrap().count(), 3);
    assert_eq!(sb.iprune_trees(), ip);
}

#[test]
fn import_iprune_with_nothing_failing_does_not_print_the_failed_list() {
    let (sb, t) = Sandbox::library();
    sb.ip
        .write_playlist("Road", &format!("#EXTM3U\n{}\n", t[0].path.display()));
    let o = sb.run(&["import", "iprune"]);
    assert_ok(&o);
    assert!(!out(&o).contains("not written"), "{}", out(&o));
}

#[test]
fn import_ipod_reports_a_playlist_it_cannot_write_imports_the_rest_and_exits_1() {
    let (sb, t) = Sandbox::library();
    let long = too_long();
    let fake = ipod_spec(&t).playlist(&long, &[C]).file_only();
    let o = sb.run(&["import", "ipod", "--file", s(&fake.itunesdb())]);
    assert_code(&o, 1);
    let stdout = out(&o);
    assert!(stdout.contains("imported: 2 new"), "{stdout}");
    assert!(stdout.contains("playlists not written"), "{stdout}");
    assert!(stdout.contains(&long), "{stdout}");
    assert!(err(&o).contains(&long), "{}", err(&o));
    assert_eq!(sb.locations("Rock"), strings(&[&t[0].path, &t[1].path]));
    // Rock, and the smart playlist Jazz; nothing for the long one.
    assert_eq!(fs::read_dir(&sb.lists).unwrap().count(), 2);
}

// Prevents the exit 1 for an unwritten playlist from cutting `--scope` short:
// the scope of what was imported is still copied.
#[test]
fn import_ipod_scope_is_copied_before_the_exit_1_for_an_unwritten_playlist() {
    let (sb, t) = Sandbox::library();
    let fake = ipod_spec(&t).playlist(&too_long(), &[C]).mounted();
    iprune_scope(&sb, "playlists", &["Rock"]);
    let o = sb.run(&["import", "ipod", "--mountpoint", s(&fake.root()), "--scope"]);
    assert_code(&o, 1);
    assert!(out(&o).contains("playlists not written"), "{}", out(&o));
    let cfg = sb.config();
    assert_eq!(cfg.sync.scope, Scope::Playlists);
    assert_eq!(cfg.sync.playlists, ["Rock"]);
}

// Prevents a failed write from hiding behind the missing-scope message: the
// scoped playlist cannot be written, so stderr must name it as not imported.
#[test]
fn import_ipod_scope_with_a_scoped_playlist_that_failed_names_it_and_changes_no_config() {
    let (sb, t) = Sandbox::library();
    let long = too_long();
    let fake = ipod_spec(&t).playlist(&long, &[C]).mounted();
    iprune_scope(&sb, "playlists", &["Rock", &long]);
    let config_before = fs::read(sb.base().join("config/fiio-tunes/config.toml")).unwrap();
    let o = sb.run(&["import", "ipod", "--mountpoint", s(&fake.root()), "--scope"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("playlists not imported"), "{}", err(&o));
    assert!(err(&o).contains(&long), "{}", err(&o));
    assert_eq!(sb.locations("Rock"), strings(&[&t[0].path, &t[1].path]));
    assert_eq!(
        fs::read(sb.base().join("config/fiio-tunes/config.toml")).unwrap(),
        config_before
    );
}

// ---- the scope, with the X3 adopted (F7)

impl Sandbox {
    /// A card folder `name` holding a copy of a.mp3, set as the card
    /// (`allow_folder`) and adopted.
    fn adopt_card(&self, name: &str) {
        let card = self.base().join(name);
        fs::create_dir(&card).unwrap();
        fs::copy(fixture(Kind::Mp3, 1), card.join("a.mp3")).unwrap();
        let file = self.base().join("config/fiio-tunes/config.toml");
        let mut c = Config::load(&file).unwrap();
        c.device.mountpoint = card;
        c.device.allow_folder = true;
        c.save(&file).unwrap();
        assert_ok(&self.run(&["adopt"]));
    }

    /// `(device key, scope, scope playlists)` of every device in the index.
    fn device_scopes(&self) -> Vec<(String, Option<String>, Vec<String>)> {
        let db = rusqlite::Connection::open(self.base().join("data/fiio-tunes/fiio-tunes.sqlite"))
            .unwrap();
        let mut stmt = db
            .prepare("SELECT id, device_key, scope FROM devices ORDER BY id")
            .unwrap();
        let rows: Vec<(i64, String, Option<String>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        rows.into_iter()
            .map(|(id, key, scope)| {
                let mut stmt = db
                    .prepare(
                        "SELECT name FROM device_scope_playlists WHERE device_id = ?1 ORDER BY name",
                    )
                    .unwrap();
                let names = stmt
                    .query_map([id], |r| r.get(0))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                (key, scope, names)
            })
            .collect()
    }
}

// Prevents: `--scope` writing `[sync]` while an adopted X3 has (or can have)
// its own scope, which wins over `[sync]`: the copy would change nothing the
// user sees. With exactly one X3 adopted it is that X3's own scope, and the
// config's `[sync]` stays as it was.
#[test]
fn import_ipod_scope_goes_to_the_only_adopted_x3_and_not_to_the_config() {
    let (sb, t) = Sandbox::library();
    sb.adopt_card("card");
    let fake = ipod_spec(&t).mounted();
    iprune_scope(&sb, "playlists", &["Rock", "jazz"]);
    let ip = sb.iprune_trees();
    let before = sb.device_scopes();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].1, None, "the X3 follows [sync] before the import");

    let o = sb.run(&["import", "ipod", "--mountpoint", s(&fake.root()), "--scope"]);
    assert_ok(&o);
    let key = before[0].0.clone();
    assert!(out(&o).contains(&key), "stdout names the X3: {}", out(&o));
    assert_eq!(
        sb.device_scopes(),
        [(
            key,
            Some("playlists".to_owned()),
            vec!["Jazz".to_owned(), "Rock".to_owned()]
        )]
    );
    let cfg = sb.config();
    assert_eq!(cfg.sync.scope, Scope::All, "[sync] is untouched");
    assert!(cfg.sync.playlists.is_empty());
    assert_eq!(sb.iprune_trees(), ip);
}

// Prevents: the scope landing on one X3 of two, chosen by luck: with two
// adopted it goes to `[sync]`, and neither X3 gets a scope of its own.
#[test]
fn import_ipod_scope_with_two_adopted_x3s_goes_to_the_config() {
    let (sb, t) = Sandbox::library();
    sb.adopt_card("card1");
    sb.adopt_card("card2");
    assert_eq!(sb.device_scopes().len(), 2);
    let fake = ipod_spec(&t).mounted();
    iprune_scope(&sb, "playlists", &["Rock"]);

    let o = sb.run(&["import", "ipod", "--mountpoint", s(&fake.root()), "--scope"]);
    assert_ok(&o);
    let cfg = sb.config();
    assert_eq!(cfg.sync.scope, Scope::Playlists);
    assert_eq!(cfg.sync.playlists, ["Rock"]);
    assert!(
        sb.device_scopes()
            .iter()
            .all(|(_, scope, _)| scope.is_none()),
        "{:?}",
        sb.device_scopes()
    );
}
