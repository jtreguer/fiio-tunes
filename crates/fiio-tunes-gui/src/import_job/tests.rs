//! The import worker: `load` and `run` for iprune's folder, an iPod and an
//! `iTunesDB` file, the scope copy, and `names_to_import`.
//!
//! The workers find iprune's config and index through `IprunePaths::from_env`
//! (HOME and the XDG folders), and there is no way to hand them other paths
//! without a seam in `src`. So every test that loads or runs an import runs
//! in a child process (this test binary again, one test, `--exact`) whose
//! HOME, `XDG_CONFIG_HOME` and `XDG_DATA_HOME` are a temp dir: the test
//! threads of one process share an environment, which cannot be changed in
//! place. The child checks that both fiio-tunes' and iprune's paths resolve
//! under that temp dir before it does anything, and the fake iprune and the
//! fake iPods live there too. `FIIO_TUNES_NO_DETECT` is set for the child, so
//! a mounted iPod is never looked for.

// The worker tests each include the fixtures: one copy per test module.
#[allow(clippy::duplicate_mod)]
#[path = "../../../fiio-core/tests/common/mod.rs"]
mod common;
#[path = "../../../fiio-core/tests/common/ipod.rs"]
mod fake;
#[allow(clippy::duplicate_mod)]
#[path = "../test_world.rs"]
mod world;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use common::Kind;
use fake::{FakeIpod, IpodTrack, Iprune, Spec, Stat, snapshot};
use fiio_core::config::Config;
use fiio_core::store::{self, TrackRow, devices};
use world::World;

use super::*;
use crate::import_dialog::names_to_import;

// ------------------------------------------------------------- the sandbox

const CHILD: &str = "FIIO_TUNES_GUI_TEST_CHILD";

/// Runs `body` in a child process with HOME and the XDG folders in a temp
/// dir, which it is given. In the child itself (`CHILD` set), runs it.
fn sandboxed(test: &str, body: impl FnOnce(&Path)) {
    if std::env::var_os(CHILD).is_some() {
        let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
        let mine = fiio_core::config::Paths::from_env().expect("fiio-tunes paths");
        let iprune = IprunePaths::from_env().expect("iprune paths");
        for p in [
            &mine.config_file,
            &mine.data_dir,
            &iprune.config_file,
            &iprune.database,
        ] {
            assert!(
                p.starts_with(&home),
                "{} is outside the sandbox {}",
                p.display(),
                home.display()
            );
        }
        assert!(
            fiio_core::device::detection_off(),
            "device detection must be off"
        );
        return body(&home);
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap();
    // The crate name goes: the test filter is the path inside the binary.
    let filter = test.split_once("::").map_or(test, |(_, rest)| rest);
    let out = Command::new(std::env::current_exe().unwrap())
        .args([filter, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("FIIO_TUNES_NO_DETECT", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("1 passed"),
        "child {filter} failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A test that runs `body` in the sandbox, given the sandbox's HOME.
macro_rules! sandboxed_test {
    ($name:ident, |$home:ident| $body:block) => {
        #[test]
        fn $name() {
            sandboxed(
                concat!(module_path!(), "::", stringify!($name)),
                |$home: &Path| $body,
            );
        }
    };
}

// --------------------------------------------------------------- the world

const A: u64 = 0xA1;
const B: u64 = 0xB2;
const C: u64 = 0xC3;
const X: u64 = 0xE5;
const GUID: &str = "000A27001234ABCD";

type Tree = BTreeMap<std::path::PathBuf, Option<Stat>>;

struct Rig {
    w: World,
    ip: Iprune,
    home: std::path::PathBuf,
    /// The library tracks a.mp3, b.mp3, c.mp3.
    rows: Vec<TrackRow>,
}

impl Rig {
    /// Library a, b, c (scanned into the index the workers open), a fake
    /// iprune with no playlists and no index, and no fiio-tunes playlists.
    fn new(home: &Path) -> Rig {
        let w = World::new(0);
        w.env.add("a.mp3", Kind::Mp3, 1);
        w.env.add("b.mp3", Kind::Mp3, 2);
        w.env.add("c.mp3", Kind::Mp3, 3);
        w.scan();
        let mut rows = store::all_tracks(w.store().conn()).unwrap();
        rows.sort_by(|x, y| x.path.cmp(&y.path));
        assert_eq!(rows.len(), 3);
        let ip = Iprune::new(
            &home.join("config"),
            &home.join("data"),
            &home.join("iprune-playlists"),
            "",
        );
        Rig {
            w,
            ip,
            home: home.to_owned(),
            rows,
        }
    }

    fn cfg(&self) -> Config {
        self.w.cfg()
    }

    /// An iPod with Rock [A, B, X] and the smart playlist Jazz [C]; X has no
    /// library file.
    fn ipod(&self) -> FakeIpod {
        Spec::new("Julien's iPod")
            .track(IpodTrack::of(A, &self.rows[0]))
            .track(IpodTrack::of(B, &self.rows[1]))
            .track(IpodTrack::of(C, &self.rows[2]))
            .track(IpodTrack::stranger(X))
            .playlist("Rock", &[A, B, X])
            .smart("Jazz", &[C])
            .mounted()
    }

    /// iprune syncs the iPod with `scope`, naming `names`, and knows which
    /// library file each of A, B and C is.
    fn iprune_index(&self, scope: &str, names: &[&str]) {
        let idx = self.ip.index();
        let dev = idx.device(GUID, "Julien", Some(scope));
        for n in names {
            idx.scope_playlist(dev, n);
        }
        for (dbid, row) in [A, B, C].iter().zip(&self.rows) {
            idx.map(dev, *dbid, &row.path);
        }
    }

    fn lists(&self) -> BTreeSet<String> {
        std::fs::read_dir(&self.w.env.lists)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect()
    }

    fn locations(&self, name: &str) -> Vec<String> {
        self.w.env.locations(name)
    }

    /// What no import may change: iprune's config, index and playlists, and
    /// the iPod.
    fn untouchable(&self, ipod: Option<&FakeIpod>) -> Vec<Tree> {
        let mut v = vec![
            snapshot(&self.home.join("config/iprune"), false),
            snapshot(&self.home.join("data/iprune"), false),
            snapshot(&self.ip.playlists, false),
        ];
        v.extend(ipod.map(|i| snapshot(&i.root(), false)));
        v
    }

    fn load(&self, source: &Source) -> Result<Listing> {
        load(&self.cfg(), &self.w.paths, source, &|_, _| {})
    }

    /// `load` then `run`, both expected to succeed.
    fn import(&self, source: &Source, names: &[&str], mode: OnConflict, scope: bool) -> Done {
        let listing = self.load(source).unwrap_or_else(|e| panic!("load: {e:#}"));
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        run(
            &self.cfg(),
            &self.w.paths,
            listing.loaded,
            &names,
            mode,
            scope,
        )
        .unwrap_or_else(|e| panic!("run: {e:#}"))
    }

    fn own_scope(&self) -> Option<devices::DeviceScope> {
        let store = self.w.store();
        let d = devices::device(store.conn(), &self.w.key()).unwrap()?;
        devices::scope(store.conn(), d.id).unwrap()
    }
}

fn offers(l: &Listing) -> BTreeMap<String, (String, bool)> {
    l.offers
        .iter()
        .map(|o| (o.name.clone(), (o.detail.clone(), o.taken)))
        .collect()
}

fn failure(r: Result<Listing>) -> anyhow::Error {
    match r {
        Err(e) => e,
        Ok(_) => panic!("expected the load to fail"),
    }
}

fn ipod_file(fake: &FakeIpod) -> Source {
    Source::File(fake.itunesdb())
}

// ------------------------------------------------------------- iprune's folder

sandboxed_test!(
    load_iprune_lists_the_folder_marks_taken_names_and_writes_nothing,
    |home| {
        let r = Rig::new(home);
        let (a, b) = (&r.rows[0].path, &r.rows[1].path);
        r.ip.write_playlist(
            "Road",
            &format!(
                "#EXTM3U\n{}\n{}\n/nowhere/ghost.mp3\n",
                a.display(),
                b.display()
            ),
        );
        r.ip.write_playlist("Other", "#EXTM3U\n");
        r.w.env.write_playlist("Road", "#EXTM3U\n");
        let (guarded, lists) = (r.untouchable(None), snapshot(&r.w.env.lists, false));

        let l = r.load(&Source::Iprune).unwrap_or_else(|e| panic!("{e:#}"));

        assert!(matches!(l.loaded, Loaded::Iprune(_)));
        let o = offers(&l);
        assert_eq!(o.keys().cloned().collect::<Vec<_>>(), ["Other", "Road"]);
        assert!(
            o["Road"].0.contains('3'),
            "three entries, one unresolved: {}",
            o["Road"].0
        );
        assert!(o["Other"].0.contains('0'), "{}", o["Other"].0);
        assert!(o["Road"].1, "Road exists in fiio-tunes' folder");
        assert!(!o["Other"].1);
        assert!(
            l.about.contains(&r.ip.playlists.display().to_string()),
            "{}",
            l.about
        );
        // Reading changed nothing, and wrote no config or index of fiio-tunes'.
        assert_eq!(r.untouchable(None), guarded);
        assert_eq!(snapshot(&r.w.env.lists, false), lists);
        assert!(!r.w.paths.config_file.exists());
    }
);

// Prevents: a playlist folder of iprune that is fiio-tunes' own (a config
// copied over) being "imported" onto itself, and a missing iprune config
// being taken for an empty folder.
sandboxed_test!(
    load_iprune_refuses_its_own_folder_and_a_missing_config,
    |home| {
        let r = Rig::new(home);
        r.ip.write_config("");
        // iprune's folder is fiio-tunes' own.
        std::fs::write(
            &r.ip.config_file,
            format!("[playlists]\ndir = {:?}\n", r.w.env.lists),
        )
        .unwrap();
        let e = failure(r.load(&Source::Iprune));
        assert!(
            matches!(
                e.downcast_ref::<fiio_core::Error>(),
                Some(fiio_core::Error::Iprune(_))
            ),
            "{e:#}"
        );
        // No iprune config at all.
        std::fs::remove_file(&r.ip.config_file).unwrap();
        assert!(r.load(&Source::Iprune).is_err());
        assert!(r.lists().is_empty());
    }
);

sandboxed_test!(
    run_iprune_imports_everything_by_default_and_keeps_unresolved_lines,
    |home| {
        let r = Rig::new(home);
        let (a, c) = (&r.rows[0].path, &r.rows[2].path);
        r.ip.write_playlist(
            "Mixed",
            &format!(
                "#EXTM3U\n{}\n/nowhere/ghost.mp3\n{}\n",
                a.display(),
                c.display()
            ),
        );
        r.ip.write_playlist("Plain", &format!("#EXTM3U\n{}\n", c.display()));
        let guarded = r.untouchable(None);

        let done = r.import(&Source::Iprune, &[], OnConflict::Skip, true);

        assert_eq!(done.failed, 0);
        assert_eq!(
            r.locations("Mixed"),
            [
                a.to_str().unwrap(),
                "/nowhere/ghost.mp3",
                c.to_str().unwrap()
            ]
        );
        assert_eq!(r.locations("Plain"), [c.to_str().unwrap()]);
        assert_eq!(r.untouchable(None), guarded);
        // The scope is an iPod's: asking for it from iprune's folder changes
        // neither the config nor a device.
        assert!(!r.w.paths.config_file.exists());
        assert_eq!(r.own_scope(), None);
    }
);

// Prevents: a name list being ignored (everything imported), or a name
// matching the wrong playlist.
sandboxed_test!(run_iprune_with_names_imports_only_those, |home| {
    let r = Rig::new(home);
    let c = &r.rows[2].path;
    for n in ["One", "Two", "Three"] {
        r.ip.write_playlist(n, &format!("#EXTM3U\n{}\n", c.display()));
    }
    let done = r.import(&Source::Iprune, &["Two"], OnConflict::Skip, false);
    assert_eq!(done.failed, 0);
    assert_eq!(r.lists(), BTreeSet::from(["Two.m3u8".to_owned()]));
});

// Prevents: the three ways to meet a name already taken doing each other's
// work: skip keeps the user's playlist, rename keeps it and adds the import,
// replace overwrites it.
sandboxed_test!(run_iprune_conflict_modes_skip_rename_and_replace, |home| {
    let r = Rig::new(home);
    let (a, c) = (&r.rows[0].path, &r.rows[2].path);
    r.ip.write_playlist("Road", &format!("#EXTM3U\n{}\n", c.display()));
    r.w.env
        .write_playlist("Road", &format!("{}\n", a.display()));
    let mine = [a.to_str().unwrap()];
    let theirs = [c.to_str().unwrap()];

    r.import(&Source::Iprune, &[], OnConflict::Skip, false);
    assert_eq!(r.lists(), BTreeSet::from(["Road.m3u8".to_owned()]));
    assert_eq!(r.locations("Road"), mine);

    r.import(&Source::Iprune, &[], OnConflict::Rename, false);
    assert_eq!(r.lists().len(), 2, "{:?}", r.lists());
    assert_eq!(r.locations("Road"), mine, "the user's playlist is kept");
    let other = r.lists().into_iter().find(|n| n != "Road.m3u8").unwrap();
    let other = other.strip_suffix(".m3u8").unwrap().to_owned();
    assert_eq!(r.locations(&other), theirs);

    r.import(&Source::Iprune, &[], OnConflict::Replace, false);
    assert_eq!(r.locations("Road"), theirs);
    assert_eq!(r.lists().len(), 2);
});

// Prevents: a playlist that cannot be written (a name no file system takes)
// stopping the others, or going unreported: the others are imported, the
// failure is counted for the dialog and named in its text.
sandboxed_test!(
    run_iprune_counts_a_playlist_it_cannot_write_and_imports_the_rest,
    |home| {
        let r = Rig::new(home);
        let c = &r.rows[2].path;
        let long = "L".repeat(250);
        for n in ["First", long.as_str(), "Last"] {
            r.ip.write_playlist(n, &format!("#EXTM3U\n{}\n", c.display()));
        }
        // The name is taken, and renaming it makes it longer than a file name
        // can be.
        r.w.env.write_playlist(&long, "#EXTM3U\n");
        let done = r.import(&Source::Iprune, &[], OnConflict::Rename, false);
        assert_eq!(done.failed, 1, "{}", done.text);
        assert!(done.text.contains(&long), "{}", done.text);
        assert_eq!(
            r.lists(),
            BTreeSet::from([
                "First.m3u8".to_owned(),
                "Last.m3u8".to_owned(),
                format!("{long}.m3u8")
            ])
        );
    }
);

// ------------------------------------------------------------------- an iPod

sandboxed_test!(
    load_an_itunesdb_file_lists_playlists_with_what_traces_and_marks_taken,
    |home| {
        let r = Rig::new(home);
        let fake = r.ipod();
        r.w.env.write_playlist("Rock", "#EXTM3U\n");
        let guarded = r.untouchable(Some(&fake));

        let l = r
            .load(&ipod_file(&fake))
            .unwrap_or_else(|e| panic!("{e:#}"));

        assert!(matches!(l.loaded, Loaded::Ipod(_)));
        let o = offers(&l);
        assert_eq!(o.keys().cloned().collect::<Vec<_>>(), ["Jazz", "Rock"]);
        // Rock has three members, two with a library file; Jazz is smart.
        assert!(o["Rock"].0.contains("2 of 3"), "{}", o["Rock"].0);
        assert!(!o["Rock"].0.contains("smart"), "{}", o["Rock"].0);
        assert!(
            o["Jazz"].0.contains("1 of 1") && o["Jazz"].0.contains("smart"),
            "{}",
            o["Jazz"].0
        );
        assert!(o["Rock"].1 && !o["Jazz"].1);
        assert!(l.about.contains("Julien's iPod"), "{}", l.about);
        assert_eq!(
            r.untouchable(Some(&fake)),
            guarded,
            "the iPod and iprune were only read"
        );
        assert!(!r.w.paths.config_file.exists());
    }
);

// Prevents: a taken name being missed because the playlist's name has
// characters a file name cannot hold: the file is named with them replaced.
sandboxed_test!(
    load_an_ipod_playlist_with_a_slash_is_taken_by_its_file_name,
    |home| {
        let r = Rig::new(home);
        let fake = Spec::new("P")
            .track(IpodTrack::of(A, &r.rows[0]))
            .playlist("Dance/Trance", &[A])
            .playlist("Free", &[A])
            .file_only();
        r.w.env.write_playlist("Dance_Trance", "#EXTM3U\n");
        let o = offers(
            &r.load(&ipod_file(&fake))
                .unwrap_or_else(|e| panic!("{e:#}")),
        );
        assert!(o["Dance/Trance"].1, "taken as Dance_Trance");
        assert!(!o["Free"].1);
    }
);

// Prevents: iprune's index being ignored by the window: a member no tag
// matches is traced through iprune's record of the iPod, and a library
// folder that iprune has no record of is traced by tags only.
sandboxed_test!(
    load_traces_through_iprunes_index_when_there_is_one,
    |home| {
        let r = Rig::new(home);
        let fake = r.ipod();
        // Without an index: A and B only; X has no library file.
        let o = offers(
            &r.load(&ipod_file(&fake))
                .unwrap_or_else(|e| panic!("{e:#}")),
        );
        assert!(o["Rock"].0.contains("2 of 3"), "{}", o["Rock"].0);

        // iprune knows that X is c.mp3.
        let idx = r.ip.index();
        let dev = idx.device(GUID, "Julien", None);
        for (dbid, row) in [(A, 0), (B, 1), (X, 2)] {
            idx.map(dev, dbid, &r.rows[row].path);
        }
        let guarded = r.untouchable(Some(&fake));
        let o = offers(
            &r.load(&ipod_file(&fake))
                .unwrap_or_else(|e| panic!("{e:#}")),
        );
        assert!(o["Rock"].0.contains("3 of 3"), "{}", o["Rock"].0);
        assert_eq!(
            r.untouchable(Some(&fake)),
            guarded,
            "iprune's index is only read"
        );
    }
);

// Prevents: "the iPod connected" guessing when detection is off: it is an
// error, not an import from whatever is mounted, and nothing is written.
sandboxed_test!(
    load_the_mounted_ipod_with_detection_off_is_an_error,
    |home| {
        let r = Rig::new(home);
        let e = failure(r.load(&Source::Ipod));
        assert!(
            matches!(
                e.downcast_ref::<fiio_core::Error>(),
                Some(fiio_core::Error::DetectionOff(_))
            ),
            "{e:#}"
        );
        assert!(r.lists().is_empty());
    }
);

// Prevents: a file that is not an iTunesDB, or one that is not there, being
// read as an empty iPod.
sandboxed_test!(load_a_file_that_is_not_an_itunesdb_fails, |home| {
    let r = Rig::new(home);
    let junk = r.home.join("junk-itunesdb");
    std::fs::write(&junk, b"this is not an iTunesDB").unwrap();
    assert!(r.load(&Source::File(junk)).is_err());
    assert!(r.load(&Source::File(r.home.join("absent"))).is_err());
    assert!(r.lists().is_empty());
});

sandboxed_test!(
    run_ipod_imports_what_traces_and_leaves_the_rest_out,
    |home| {
        let r = Rig::new(home);
        let fake = r.ipod();
        // iprune has a scope for this iPod, which a plain import must not copy.
        r.iprune_index("playlists", &["Rock"]);
        let guarded = r.untouchable(Some(&fake));

        let done = r.import(&ipod_file(&fake), &[], OnConflict::Skip, false);

        assert_eq!(done.failed, 0);
        let (a, b, c) = (&r.rows[0].path, &r.rows[1].path, &r.rows[2].path);
        assert_eq!(
            r.locations("Rock"),
            [a.to_str().unwrap(), b.to_str().unwrap()]
        );
        assert_eq!(r.locations("Jazz"), [c.to_str().unwrap()]);
        assert!(
            done.text.contains("Rock") && done.text.contains("Jazz"),
            "{}",
            done.text
        );
        assert!(!r.w.paths.config_file.exists(), "no scope was asked for");
        assert_eq!(r.untouchable(Some(&fake)), guarded);
    }
);

sandboxed_test!(
    run_ipod_counts_a_playlist_it_cannot_write_and_imports_the_rest,
    |home| {
        let r = Rig::new(home);
        let long = "L".repeat(300);
        let fake = Spec::new("P")
            .track(IpodTrack::of(A, &r.rows[0]))
            .playlist("Fine", &[A])
            .playlist(&long, &[A])
            .file_only();
        let done = r.import(&ipod_file(&fake), &[], OnConflict::Skip, false);
        assert_eq!(done.failed, 1, "{}", done.text);
        assert!(done.text.contains(&long), "{}", done.text);
        assert_eq!(r.lists(), BTreeSet::from(["Fine.m3u8".to_owned()]));
    }
);

// ----------------------------------------------------------- the scope copy

/// Imports from the iPod with the scope asked for, iprune syncing it with
/// `playlists` Rock and jazz.
fn import_with_scope(r: &Rig, fake: &FakeIpod, names: &[&str]) -> Done {
    r.import(&ipod_file(fake), names, OnConflict::Skip, true)
}

// Prevents: the scope copied when no X3 is adopted going anywhere but
// `[sync]` in the config file (which does not exist yet), spelt with
// fiio-tunes' names for the playlists.
sandboxed_test!(the_scope_goes_to_the_config_when_no_x3_is_adopted, |home| {
    let r = Rig::new(home);
    let fake = r.ipod();
    r.iprune_index("playlists", &["Rock", "jazz"]);
    let guarded = r.untouchable(Some(&fake));

    let done = import_with_scope(&r, &fake, &[]);

    assert_eq!(done.failed, 0);
    let cfg = Config::load(&r.w.paths.config_file).unwrap();
    assert_eq!(cfg.sync.scope, fiio_core::config::Scope::Playlists);
    assert_eq!(cfg.sync.playlists, ["Jazz", "Rock"]);
    // The config saved is the one the worker was given, not a blank one.
    assert_eq!(cfg.library, r.cfg().library);
    assert_eq!(r.untouchable(Some(&fake)), guarded);
});

// Prevents: the window's copy of the config, older than the file, being saved
// over it when the scope goes to `[sync]`: an edit made elsewhere since (here
// the library roots) would be undone. The file is read afresh, keeps its own
// values and gains the scope.
sandboxed_test!(
    the_scope_saved_to_the_config_keeps_edits_made_since_the_window_read_it,
    |home| {
        let r = Rig::new(home);
        let fake = r.ipod();
        r.iprune_index("playlists", &["Rock", "jazz"]);
        // The file on disk has other roots than the config the worker is given.
        let other = r.home.join("other-music");
        std::fs::create_dir_all(&other).unwrap();
        let mut on_disk = r.cfg();
        on_disk.library.roots = vec![other.clone()];
        on_disk.sync.keep_backups = 9;
        on_disk.save(&r.w.paths.config_file).unwrap();
        assert_ne!(on_disk.library, r.cfg().library);

        let done = import_with_scope(&r, &fake, &[]);

        assert_eq!(done.failed, 0);
        let saved = Config::load(&r.w.paths.config_file).unwrap();
        assert_eq!(saved.library.roots, [other], "the file's own value is kept");
        assert_eq!(saved.sync.keep_backups, 9);
        assert_eq!(saved.sync.scope, fiio_core::config::Scope::Playlists);
        assert_eq!(saved.sync.playlists, ["Jazz", "Rock"]);
    }
);

// Prevents: the copy landing in `[sync]` while the one adopted X3 has its
// own scope, which wins: the user would see no change. It goes to the X3,
// and the config file is not written.
sandboxed_test!(
    the_scope_goes_to_the_only_adopted_x3_and_not_to_the_config,
    |home| {
        let r = Rig::new(home);
        r.w.adopt();
        let fake = r.ipod();
        r.iprune_index("playlists", &["Rock", "jazz"]);

        let done = import_with_scope(&r, &fake, &[]);

        assert_eq!(done.failed, 0);
        assert_eq!(
            r.own_scope(),
            Some(devices::DeviceScope {
                kind: fiio_core::config::Scope::Playlists,
                playlists: vec!["Jazz".into(), "Rock".into()],
            })
        );
        assert!(!r.w.paths.config_file.exists());
        assert!(
            done.text.contains(&r.w.key()),
            "names the X3: {}",
            done.text
        );
    }
);

// Prevents: the scope being copied when the box was not ticked.
sandboxed_test!(no_scope_is_copied_when_it_was_not_asked_for, |home| {
    let r = Rig::new(home);
    r.w.adopt();
    let fake = r.ipod();
    r.iprune_index("playlists", &["Rock", "jazz"]);
    let done = r.import(&ipod_file(&fake), &[], OnConflict::Skip, false);
    assert_eq!(done.failed, 0);
    assert_eq!(r.own_scope(), None);
    assert!(!r.w.paths.config_file.exists());
});

// Prevents: a scope naming a playlist that was not imported being copied
// (the sync would then drop everything the missing playlist held): not
// copied, the import itself still done, and the text says it was not.
sandboxed_test!(
    a_scope_naming_a_playlist_not_imported_is_not_copied,
    |home| {
        let r = Rig::new(home);
        r.w.adopt();
        let fake = r.ipod();
        r.iprune_index("playlists", &["Rock", "jazz"]);

        let done = import_with_scope(&r, &fake, &["Rock"]);

        assert_eq!(done.failed, 0);
        assert!(r.lists().contains("Rock.m3u8") && !r.lists().contains("Jazz.m3u8"));
        assert_eq!(r.own_scope(), None);
        assert!(!r.w.paths.config_file.exists());
        assert!(done.text.contains("not copied"), "{}", done.text);
        // It names what is missing, as iprune spells it.
        assert!(done.text.contains("jazz"), "{}", done.text);
    }
);

// Prevents: a scope that cannot be read (no index, a broken iprune config)
// turning a good import into an error, or being guessed: the playlists are
// imported, nothing is copied, and the text says why.
sandboxed_test!(
    a_scope_that_cannot_be_read_leaves_the_import_done_and_nothing_copied,
    |home| {
        let r = Rig::new(home);
        r.w.adopt();
        let fake = r.ipod();

        // iprune has no index of this iPod.
        let done = import_with_scope(&r, &fake, &[]);
        assert_eq!(done.failed, 0);
        assert!(r.lists().contains("Rock.m3u8") && r.lists().contains("Jazz.m3u8"));
        assert!(done.text.contains("not copied"), "{}", done.text);
        assert_eq!(r.own_scope(), None);
        assert!(!r.w.paths.config_file.exists());

        // iprune's config does not parse.
        r.iprune_index("playlists", &["Rock"]);
        std::fs::write(&r.ip.config_file, "this is [not toml").unwrap();
        let done = r.import(&ipod_file(&fake), &[], OnConflict::Replace, true);
        assert_eq!(done.failed, 0);
        assert!(done.text.contains("not copied"), "{}", done.text);
        assert_eq!(r.own_scope(), None);
        assert!(!r.w.paths.config_file.exists());
    }
);

// ------------------------------------------------------------ names_to_import

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn set(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(|s| s.to_string()).collect()
}

// Prevents: the import being asked for the wrong playlists: an empty list
// means "all" (so two iPod playlists of one name both come), and it must only
// be sent when every playlist offered is checked; a part is named once each.
#[test]
fn names_to_import_is_everything_only_when_everything_offered_is_checked() {
    let offered = strings(&["Rock", "Jazz", "Rock"]);
    assert_eq!(
        names_to_import(&offered, &set(&["Rock", "Jazz"])),
        Some(vec![]),
        "all checked: all, duplicates included"
    );
    assert_eq!(
        names_to_import(&offered, &set(&["Rock"])),
        Some(strings(&["Rock"])),
        "a part, each name once, not Rock twice"
    );
    assert_eq!(
        names_to_import(&offered, &set(&["Jazz"])),
        Some(strings(&["Jazz"]))
    );
    // A checked name that is not offered (a filter, a stale checkbox) counts
    // for nothing.
    assert_eq!(
        names_to_import(&offered, &set(&["Rock", "Jazz", "Ghost"])),
        Some(vec![])
    );
    assert_eq!(names_to_import(&offered, &set(&["Ghost"])), None);
    assert_eq!(names_to_import(&offered, &set(&[])), None);
    assert_eq!(names_to_import(&[], &set(&["Rock"])), None);
}
