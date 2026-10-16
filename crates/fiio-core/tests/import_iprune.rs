//! Importing playlists from iprune's playlists folder
//! (`import::iprune_folder` and `import::import_iprune`).

mod common;
#[path = "common/ipod.rs"]
mod fake;

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use common::{Env, Kind};
use fake::{Iprune, snapshot};
use fiio_core::import::{self, IpruneFolder, IpruneReport, OnConflict, Outcome};
use fiio_core::{Error, playlist};

struct Rig {
    env: Env,
    ip: Iprune,
    work: PathBuf,
    _dir: tempfile::TempDir,
    /// Library files a.mp3, b.mp3.
    a: PathBuf,
    b: PathBuf,
}

fn rig_with(env: Env) -> Rig {
    let mut env = env;
    let a = env.add("a.mp3", Kind::Mp3, 1);
    let b = env.add("b.mp3", Kind::Mp3, 2);
    env.scan();
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().canonicalize().unwrap();
    let ip = Iprune::new(
        &work.join("config"),
        &work.join("data"),
        // Deeper than fiio-tunes' folder, so a relative entry means something
        // else from there.
        &work.join("deep/er/iprune-playlists"),
        "",
    );
    Rig {
        env,
        ip,
        work,
        _dir: dir,
        a,
        b,
    }
}

fn rig() -> Rig {
    rig_with(Env::new())
}

impl Rig {
    fn folder(&self) -> IpruneFolder {
        import::iprune_folder(&self.env.cfg, &self.ip.config_file).unwrap()
    }

    fn import(&self, names: &[&str], mode: OnConflict) -> IpruneReport {
        self.try_import(names, mode).unwrap()
    }

    fn try_import(&self, names: &[&str], mode: OnConflict) -> fiio_core::Result<IpruneReport> {
        let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        import::import_iprune(
            self.env.store.conn(),
            &self.env.cfg,
            &self.folder(),
            &names,
            mode,
        )
    }

    /// Everything under iprune's config, data and playlists folders.
    fn iprune_tree(&self) -> Vec<std::collections::BTreeMap<PathBuf, Option<fake::Stat>>> {
        [
            self.ip.config_file.parent().unwrap(),
            self.ip.database.parent().unwrap(),
            &self.ip.playlists,
        ]
        .iter()
        .map(|p| snapshot(p, false))
        .collect()
    }
}

fn s(p: &Path) -> String {
    p.to_str().unwrap().to_owned()
}

#[test]
fn absolute_and_relative_entries_resolve_against_iprunes_folder() {
    let rig = rig();
    // Relative to iprune's folder, which is not fiio-tunes' folder: read
    // from there, this goes nowhere.
    let rel_b = playlist::relative_to(&rig.b, &rig.ip.playlists);
    assert!(rel_b.starts_with(".."));
    rig.ip.write_playlist(
        "Road",
        &format!("#EXTM3U\n{}\n{}\n", rig.a.display(), rel_b.display()),
    );
    let report = rig.import(&[], OnConflict::Skip);
    assert_eq!(report.playlists.len(), 1);
    assert_eq!(report.playlists[0].outcome, Outcome::Created);
    assert_eq!(report.playlists[0].entries, 2);
    assert!(report.unresolved.is_empty(), "{:?}", report.unresolved);
    assert_eq!(rig.env.locations("Road"), [s(&rig.a), s(&rig.b)]);
    let ids: Vec<_> = rig
        .env
        .indexed_items("Road")
        .into_iter()
        .map(|r| r.1)
        .collect();
    assert_eq!(
        ids,
        [
            Some(rig.env.track(&rig.a).id),
            Some(rig.env.track(&rig.b).id)
        ]
    );
}

#[test]
fn resolved_entries_are_written_in_the_relative_style_when_configured() {
    let rig = rig_with(Env::new().relative_playlists());
    rig.ip
        .write_playlist("Road", &format!("#EXTM3U\n{}\n", rig.a.display()));
    rig.import(&[], OnConflict::Skip);
    assert_eq!(rig.env.locations("Road"), ["../music/a.mp3"]);
}

#[test]
fn unresolved_entries_and_the_lines_around_them_are_kept_verbatim() {
    let rig = rig();
    rig.ip.write_playlist(
        "Road",
        &format!(
            "#EXTM3U\n#PLAYLIST:Road trip\n#EXTINF:215,Artist1 - Title1\n{}\n\
             #EXTINF:99,Ghost - Gone\n/nowhere/ghost.mp3\n# a note\nrelative/ghost.mp3\n",
            rig.a.display()
        ),
    );
    let report = rig.import(&[], OnConflict::Skip);
    assert_eq!(
        report.unresolved,
        [
            ("Road".to_owned(), "/nowhere/ghost.mp3".to_owned()),
            ("Road".to_owned(), "relative/ghost.mp3".to_owned())
        ]
    );
    let text = rig.env.read_playlist("Road");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "#EXTM3U");
    assert_eq!(lines[1], "#PLAYLIST:Road trip");
    assert!(lines[2].starts_with("#EXTINF:"), "{lines:?}");
    assert_eq!(
        lines[3..],
        [
            s(&rig.a).as_str(),
            "#EXTINF:99,Ghost - Gone",
            "/nowhere/ghost.mp3",
            "# a note",
            "relative/ghost.mp3"
        ]
    );
    // The index has the unresolved ones as entries with no track.
    let rows = rig.env.indexed_items("Road");
    let tracks: Vec<bool> = rows.iter().map(|r| r.1.is_some()).collect();
    assert_eq!(tracks, [true, false, false]);
}

#[test]
fn a_playlist_of_comments_only_keeps_them() {
    let rig = rig();
    rig.ip
        .write_playlist("Notes", "#EXTM3U\n# only a note\n#PLAYLIST:Notes\n");
    let report = rig.import(&[], OnConflict::Skip);
    assert_eq!(report.playlists[0].entries, 0);
    assert_eq!(
        rig.env.read_playlist("Notes"),
        "#EXTM3U\n# only a note\n#PLAYLIST:Notes\n"
    );
}

#[test]
fn file_names_resolve_whatever_the_unicode_form_on_either_side() {
    let mut env = Env::new();
    // A library file stored decomposed, and another stored composed.
    let nfd = env.add("Cafe\u{301}.mp3", Kind::Mp3, 3);
    let nfc = env.add("G\u{e9}.mp3", Kind::Mp3, 4);
    env.scan();
    let rig = {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().canonicalize().unwrap();
        let ip = Iprune::new(
            &work.join("config"),
            &work.join("data"),
            &work.join("pl"),
            "",
        );
        Rig {
            a: nfd.clone(),
            b: nfc.clone(),
            env,
            ip,
            work,
            _dir: dir,
        }
    };
    let music = rig.env.music.clone();
    // iprune wrote the other form of each name.
    rig.ip.write_playlist(
        "Forms",
        &format!(
            "#EXTM3U\n{}/Caf\u{e9}.mp3\n{}/Ge\u{301}.mp3\n",
            music.display(),
            music.display()
        ),
    );
    let report = rig.import(&[], OnConflict::Skip);
    assert!(report.unresolved.is_empty(), "{:?}", report.unresolved);
    // Written as the files are on disk, byte for byte.
    let got: Vec<Vec<u8>> = rig
        .env
        .locations("Forms")
        .iter()
        .map(|l| l.as_bytes().to_vec())
        .collect();
    assert_eq!(
        got,
        [
            nfd.as_os_str().as_bytes().to_vec(),
            nfc.as_os_str().as_bytes().to_vec()
        ]
    );
    assert_ne!(got[0], got[1]);
}

#[test]
fn iprunes_folder_being_fiio_tunes_own_is_refused_and_nothing_is_written() {
    let rig = rig();
    rig.env
        .write_playlist("Mine", &format!("#EXTM3U\n{}\n", rig.a.display()));
    let before = snapshot(&rig.env.lists, false);
    let link = rig.work.join("link-to-lists");
    std::os::unix::fs::symlink(&rig.env.lists, &link).unwrap();
    let spellings = [
        rig.env.lists.clone(),
        link,
        rig.env.lists.join("..").join("lists"),
    ];
    for dir in spellings {
        fs::write(
            &rig.ip.config_file,
            format!("[playlists]\ndir = {:?}\n", dir),
        )
        .unwrap();
        let err = import::iprune_folder(&rig.env.cfg, &rig.ip.config_file).unwrap_err();
        assert!(
            matches!(err, Error::Iprune(_)),
            "{}: {err:?}",
            dir.display()
        );
    }
    assert_eq!(snapshot(&rig.env.lists, false), before);
}

#[test]
fn an_iprune_config_that_does_not_name_a_folder_is_an_error() {
    let rig = rig();
    let cfg = &rig.ip.config_file;
    for text in [
        "",
        "[sync]\nscope = \"all\"\n",
        "[playlists]\ndir = \"relative/dir\"\n",
    ] {
        fs::write(cfg, text).unwrap();
        let err = import::iprune_folder(&rig.env.cfg, cfg).unwrap_err();
        assert!(matches!(err, Error::Config { .. }), "{text:?}: {err:?}");
    }
    fs::write(cfg, "[playlists\n").unwrap();
    assert!(matches!(
        import::iprune_folder(&rig.env.cfg, cfg),
        Err(Error::Config { .. })
    ));
    fs::remove_file(cfg).unwrap();
    assert!(matches!(
        import::iprune_folder(&rig.env.cfg, cfg),
        Err(Error::Io { .. })
    ));
}

#[test]
fn the_folder_lists_playlists_by_name_and_ignores_hidden_and_other_files() {
    let rig = rig();
    for n in ["Zed", "Alpha", "Mid"] {
        rig.ip.write_playlist(n, "#EXTM3U\n");
    }
    fs::write(rig.ip.playlists.join(".hidden.m3u8"), "#EXTM3U\n").unwrap();
    fs::write(rig.ip.playlists.join("notes.txt"), "x").unwrap();
    fs::write(rig.ip.playlists.join("old.m3u"), "x").unwrap();
    fs::create_dir(rig.ip.playlists.join("Dir.m3u8")).unwrap();
    let folder = rig.folder();
    let names: Vec<_> = folder.playlists.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["Alpha", "Mid", "Zed"]);
    assert!(folder.problems.is_empty(), "{:?}", folder.problems);
    let report = rig.import(&[], OnConflict::Skip);
    assert_eq!(report.playlists.len(), 3);
    assert!(!rig.env.playlist_file(".hidden").exists());
}

#[test]
fn a_playlist_that_is_not_utf8_is_reported_and_the_others_still_import() {
    let rig = rig();
    rig.ip
        .write_playlist("Good", &format!("#EXTM3U\n{}\n", rig.a.display()));
    let bad = rig.ip.playlists.join("Bad.m3u8");
    fs::write(&bad, b"#EXTM3U\n/music/caf\xe9.mp3\n").unwrap();
    // A file name that is not UTF-8 is a problem of the folder, not a crash.
    let odd = rig
        .ip
        .playlists
        .join(std::ffi::OsStr::from_bytes(b"Odd\xff.m3u8"));
    fs::write(&odd, "#EXTM3U\n").unwrap();

    assert_eq!(rig.folder().problems.len(), 1);
    let report = rig.import(&[], OnConflict::Skip);
    let imported: Vec<_> = report.playlists.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(imported, ["Good"]);
    assert_eq!(report.unreadable.len(), 1, "{:?}", report.unreadable);
    assert_eq!(report.unreadable[0].0, bad);
    assert!(!rig.env.playlist_file("Bad").exists());
    assert!(rig.env.playlist_id("Bad").is_none());
    assert!(rig.env.playlist_file("Good").is_file());

    // Asked for by name, it is just as unreadable, and nothing is written.
    let again = rig.import(&["Bad"], OnConflict::Skip);
    assert!(again.playlists.is_empty());
    assert_eq!(again.unreadable.len(), 1);
}

#[test]
fn skip_keeps_the_existing_playlist_and_reports_nothing_unresolved_for_it() {
    let rig = rig();
    let mine = "#EXTM3U\n/mine.mp3\n";
    rig.env.write_playlist("Road", mine);
    rig.ip.write_playlist("Road", "#EXTM3U\n/ghost.mp3\n");
    let before = snapshot(&rig.env.lists, false);
    let report = rig.import(&[], OnConflict::Skip);
    assert_eq!(report.playlists[0].outcome, Outcome::Skipped);
    assert_eq!(report.playlists[0].name, "Road");
    assert!(report.unresolved.is_empty(), "{:?}", report.unresolved);
    assert_eq!(rig.env.read_playlist("Road"), mine);
    assert_eq!(snapshot(&rig.env.lists, false), before);
}

#[test]
fn replace_overwrites_and_rename_keeps_both() {
    let rig = rig();
    let mine = "#EXTM3U\n/mine.mp3\n";
    rig.env.write_playlist("Road", mine);
    rig.ip.write_playlist(
        "Road",
        &format!("#EXTM3U\n{}\n/ghost.mp3\n", rig.b.display()),
    );

    let renamed = rig.import(&[], OnConflict::Rename);
    assert_eq!(renamed.playlists[0].name, "Road (iPod)");
    assert_eq!(renamed.playlists[0].outcome, Outcome::Renamed);
    assert_eq!(
        renamed.unresolved,
        [("Road (iPod)".to_owned(), "/ghost.mp3".to_owned())]
    );
    assert_eq!(rig.env.read_playlist("Road"), mine);
    assert_eq!(
        rig.env.locations("Road (iPod)"),
        [s(&rig.b), "/ghost.mp3".to_owned()]
    );

    let replaced = rig.import(&[], OnConflict::Replace);
    assert_eq!(replaced.playlists[0].outcome, Outcome::Replaced);
    assert_eq!(
        rig.env.locations("Road"),
        [s(&rig.b), "/ghost.mp3".to_owned()]
    );
    let ids: Vec<_> = rig
        .env
        .indexed_items("Road")
        .into_iter()
        .map(|r| r.1)
        .collect();
    assert_eq!(ids, [Some(rig.env.track(&rig.b).id), None]);
}

#[test]
fn names_are_found_loosely_and_ambiguity_or_an_unknown_name_writes_nothing() {
    let rig = rig();
    for n in ["Jazz", "jazz", "Rock", "Dance_Trance"] {
        rig.ip.write_playlist(n, "#EXTM3U\n");
    }
    let before = snapshot(&rig.env.lists, false);
    let fails = |names: &[&str]| rig.try_import(names, OnConflict::Skip).unwrap_err();
    assert!(matches!(fails(&["JAZZ"]), Error::AmbiguousName(_)));
    assert!(matches!(fails(&["Rock", "Nope"]), Error::NoSuchPlaylist(_)));
    assert!(matches!(fails(&["Rock", "JAZZ"]), Error::AmbiguousName(_)));
    assert_eq!(snapshot(&rig.env.lists, false), before);
    assert!(rig.env.playlist_id("Rock").is_none());

    let report = rig.import(&["rock", "jazz", "Dance/Trance", "ROCK"], OnConflict::Skip);
    let names: Vec<_> = report.playlists.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["Rock", "jazz", "Dance_Trance"]);
}

#[test]
fn importing_changes_nothing_of_iprunes() {
    let rig = rig();
    rig.ip
        .write_config("[sync]\nscope = \"playlists\"\nplaylists = [\"Road\"]");
    rig.ip.write_playlist(
        "Road",
        &format!("#EXTM3U\n{}\n/ghost.mp3\n", rig.a.display()),
    );
    rig.ip.write_playlist("Other", "#EXTM3U\n");
    let idx = rig.ip.index();
    idx.device("1", "p", None);
    drop(idx);
    let before = rig.iprune_tree();
    assert!(before.iter().all(|t| t.len() > 1), "{before:?}");
    for mode in [OnConflict::Skip, OnConflict::Replace, OnConflict::Rename] {
        rig.import(&[], mode);
        assert_eq!(rig.iprune_tree(), before, "{mode:?}");
    }
}

// ---- a playlist that cannot be written

// Prevents one playlist that cannot be written from ending the import: it is
// recorded in `failed` and the others still come in. Here the name of
// iprune's file is the longest a file can have and is taken, so the rename
// must add to it, and the file system refuses the result.
#[test]
fn a_playlist_that_cannot_be_written_is_reported_and_the_others_still_are() {
    let rig = rig();
    let long = "L".repeat(250); // 255 bytes with ".m3u8"
    let text = format!("#EXTM3U\n{}\n/ghost.mp3\n", rig.a.display());
    rig.ip.write_playlist("First", &text);
    rig.ip.write_playlist(&long, &text);
    rig.ip.write_playlist("Last", &text);
    let mine = "#EXTM3U\n/mine.mp3\n";
    rig.env.write_playlist(&long, mine);
    let report = rig.import(&[], OnConflict::Rename);

    let mut names: Vec<_> = report.playlists.iter().map(|i| i.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["First", "Last"]);
    assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
    assert_eq!(report.failed[0].0, long);
    assert!(!report.failed[0].1.is_empty());
    assert_eq!(
        rig.env.locations("First"),
        [s(&rig.a), "/ghost.mp3".to_owned()]
    );
    assert_eq!(
        rig.env.locations("Last"),
        [s(&rig.a), "/ghost.mp3".to_owned()]
    );
    // What was there is as it was, nothing else came in under another name,
    // and the failed playlist's unresolved entry is not reported.
    assert_eq!(rig.env.read_playlist(&long), mine);
    let mut files: Vec<_> = fs::read_dir(&rig.env.lists)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    assert_eq!(
        files,
        ["First.m3u8", format!("{long}.m3u8").as_str(), "Last.m3u8"]
    );
    assert_eq!(
        report.unresolved,
        [
            ("First".to_owned(), "/ghost.mp3".to_owned()),
            ("Last".to_owned(), "/ghost.mp3".to_owned())
        ]
    );
}

#[test]
fn a_run_where_nothing_fails_has_an_empty_failed_list() {
    let rig = rig();
    rig.ip.write_playlist("Road", "#EXTM3U\n");
    let report = rig.import(&[], OnConflict::Skip);
    assert_eq!(report.playlists.len(), 1);
    assert!(report.failed.is_empty(), "{:?}", report.failed);
}
