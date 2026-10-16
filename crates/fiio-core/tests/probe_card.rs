//! The probe (technical.md 9.4): its plan, the choice of the source file,
//! `write` and `clean` on a card folder in a temp dir. Nothing here touches a
//! real card: the card, the record file and the library are temp folders.

#[path = "common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use common::{Env, Kind, fixture};
use fiio_core::Error;
use fiio_core::probe::{self, Cleaned, Plan, plan};

// ---- helpers

/// Every entry under `root`, relative: `None` for a folder, the bytes for a
/// file, `link -> target` for a symlink.
fn tree(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    fn walk(root: &Path, p: &Path, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
        for e in fs::read_dir(p).unwrap() {
            let e = e.unwrap();
            let path = e.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            let ft = e.file_type().unwrap();
            if ft.is_symlink() {
                let target = fs::read_link(&path).unwrap();
                out.insert(
                    rel,
                    Some(format!("link -> {}", target.display()).into_bytes()),
                );
            } else if ft.is_dir() {
                out.insert(rel, None);
                walk(root, &path, out);
            } else {
                out.insert(rel, Some(fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn names(t: &BTreeMap<String, Option<Vec<u8>>>) -> Vec<&str> {
    t.keys().map(String::as_str).collect()
}

/// Where the probe's files are: every audio copy and playlist of the plan.
fn probe_files() -> Vec<String> {
    let p = plan(0);
    p.audio
        .iter()
        .cloned()
        .chain(p.playlists.iter().map(|p| p.path.clone()))
        .collect()
}

struct Rig {
    _dir: tempfile::TempDir,
    card: PathBuf,
    record: PathBuf,
    src: PathBuf,
}

impl Rig {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let card = base.join("card");
        fs::create_dir(&card).unwrap();
        let src = base.join("src.mp3");
        fs::write(&src, b"ID3 not really audio").unwrap();
        Rig {
            record: base.join("data/probe.toml"),
            card,
            src,
            _dir: dir,
        }
    }

    fn write(&self) -> fiio_core::Result<probe::Written> {
        probe::write(&self.card, &self.src, 200, &self.record)
    }

    fn clean(&self) -> fiio_core::Result<Cleaned> {
        probe::clean(&self.card, &self.record)
    }

    fn tree(&self) -> BTreeMap<String, Option<Vec<u8>>> {
        tree(&self.card)
    }
}

/// Sets the flag when dropped.
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn is_io(r: &fiio_core::Result<probe::Written>) -> bool {
    matches!(r, Err(Error::Io { .. }))
}

// ---- plan

/// A playlist as the spec describes it, built here independently of the
/// code under test.
fn expected(bom: bool, header: bool, eol: &str, secs: i64, entries: &[(&str, &str)]) -> String {
    let mut s = String::new();
    if bom {
        s.push('\u{feff}');
    }
    if header {
        s.push_str(&format!("#EXTM3U{eol}"));
    }
    for (title, loc) in entries {
        if header {
            s.push_str(&format!("#EXTINF:{secs},fiio-tunes - {title}{eol}"));
        }
        s.push_str(&format!("{loc}{eol}"));
    }
    s
}

// Prevents: the playlists the owner tests on the player differing, in a
// byte, from what section 9.4 says they are: the result of the probe fixes
// the format of every playlist the tool will write.
#[test]
fn the_plan_matches_the_twelve_variants_byte_for_byte() {
    let secs = 213;
    let p = plan(secs);
    let one = "Music/zz-probe/01 Probe.mp3";
    let e = |n: u32, loc: &str| vec![(format!("Probe {n:02}"), loc.to_owned())];
    let exp = |bom, header, eol, n: u32, loc: &str| {
        let entries = e(n, loc);
        let refs: Vec<(&str, &str)> = entries
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        expected(bom, header, eol, secs, &refs)
    };
    let backslashed = one.replace('/', "\\");
    let table: Vec<(&str, String)> = vec![
        ("zz-probe-01.m3u8", exp(false, true, "\n", 1, one)),
        ("zz-probe-02.m3u8", exp(false, true, "\n", 2, &backslashed)),
        ("zz-probe-03.m3u8", exp(true, true, "\n", 3, one)),
        ("zz-probe-04.m3u8", exp(false, true, "\r\n", 4, one)),
        ("zz-probe-05.m3u8", exp(false, false, "\n", 5, one)),
        (
            "zz-probe-06.m3u8",
            exp(false, true, "\n", 6, &format!("/{one}")),
        ),
        (
            "zz-probe-07.m3u8",
            exp(false, true, "\n", 7, &format!("TF1:/{one}")),
        ),
        (
            "Playlists/zz-probe-08.m3u8",
            exp(false, true, "\n", 8, &format!("../{one}")),
        ),
        ("Playlists/zz-probe-09.m3u8", exp(false, true, "\n", 9, one)),
    ];
    for (i, (path, text)) in table.iter().enumerate() {
        assert_eq!(p.playlists[i].path, *path);
        assert_eq!(p.playlists[i].text, *text, "variant {}", i + 1);
    }
    // 10: accents, CJK, and the é / É pair, in this order.
    assert_eq!(p.playlists[9].path, "zz-probe-10.m3u8");
    assert_eq!(
        p.playlists[9].text,
        expected(
            false,
            true,
            "\n",
            secs,
            &[
                ("Probe 10 accents", "Music/zz-probe/02 Élan café.mp3"),
                ("Probe 10 CJK", "Music/zz-probe/03 日本語の歌.mp3"),
                ("Probe 10 é", "Music/zz-probe/04 é.mp3"),
                ("Probe 10 É", "Music/zz-probe/04 É.mp3"),
            ]
        )
    );
    // 11: one entry, the long path.
    assert_eq!(p.playlists[10].path, "zz-probe-11.m3u8");
    let lines: Vec<&str> = p.playlists[10].text.lines().collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], "#EXTM3U");
    assert_eq!(lines[1], format!("#EXTINF:{secs},fiio-tunes - Probe 11"));
    assert!(lines[2].starts_with("Music/zz-probe/05 A folder with a long name"));
    assert!(lines[2].ends_with(".mp3"));
    assert!(p.playlists[10].text.ends_with(".mp3\n"));
    // 12: 2000 entries, numbered in order, one location.
    assert_eq!(p.playlists[11].path, "zz-probe-12.m3u8");
    let entries: Vec<(String, &str)> = (1..=2000)
        .map(|i| (format!("Probe 12 #{i}"), one))
        .collect();
    let refs: Vec<(&str, &str)> = entries.iter().map(|(t, l)| (t.as_str(), *l)).collect();
    assert_eq!(
        p.playlists[11].text,
        expected(false, true, "\n", secs, &refs)
    );
    assert_eq!(p.playlists.len(), 12);
}

// Prevents: the BOM or CRLF leaking into variants that are meant to be
// plain (which would make the owner's report unreadable), and a variant
// that lacks the byte it exists to test.
#[test]
fn only_the_variants_that_say_so_have_a_bom_or_crlf() {
    let p = plan(10);
    for (i, pl) in p.playlists.iter().enumerate() {
        let n = i + 1;
        assert_eq!(pl.text.starts_with('\u{feff}'), n == 3, "BOM in {n}");
        assert_eq!(
            pl.text.matches('\u{feff}').count(),
            usize::from(n == 3),
            "BOM in {n}"
        );
        assert_eq!(pl.text.contains('\r'), n == 4, "CR in {n}");
        if n == 4 {
            assert!(
                !pl.text.replace("\r\n", "").contains('\n'),
                "a bare LF in 4"
            );
        }
        assert_eq!(pl.text.contains("#EXTM3U"), n != 5, "header in {n}");
        assert_eq!(pl.text.contains("#EXTINF"), n != 5, "EXTINF in {n}");
        assert_eq!(pl.text.contains('\\'), n == 2, "backslash in {n}");
        assert_eq!(pl.text.contains("TF1:"), n == 7, "drive prefix in {n}");
        assert!(
            pl.text.ends_with(if n == 4 { "\r\n" } else { "\n" }),
            "end of {n}"
        );
        assert!(!pl.what.is_empty());
    }
}

// Prevents: the long-path variant not being long (it must go past 200
// characters, the limit the owner wants to find), or past what FAT can hold.
#[test]
fn the_long_path_is_over_200_characters_and_every_name_fits_fat() {
    let p = plan(10);
    let loc = p.playlists[10].text.lines().last().unwrap();
    assert!(loc.chars().count() > 200, "{}", loc.chars().count());
    assert!(
        loc.split('/')
            .all(|c| (1..=255).contains(&c.encode_utf16().count()))
    );
    assert!(p.audio.iter().any(|a| a == loc));
    for a in &p.audio {
        assert!(
            a.split('/')
                .all(|c| c.encode_utf16().count() <= 255 && !c.ends_with(['.', ' ']))
        );
    }
}

/// The card file a location in a playlist at `playlist` points at, the way
/// the variants are meant to be read.
fn resolve(playlist: &str, location: &str) -> String {
    let loc = location
        .trim_start_matches('\u{feff}')
        .trim_start_matches("TF1:")
        .replace('\\', "/");
    let dir = playlist.rsplit_once('/').map_or("", |(d, _)| d);
    let mut parts: Vec<&str> = dir.split('/').filter(|d| !d.is_empty()).collect();
    for c in loc.split('/').filter(|c| !c.is_empty()) {
        match c {
            ".." => {
                parts.pop();
            }
            // Variants 7 and 6 are absolute; 9 is root relative by design
            // (the point of that variant is to see if the player agrees).
            c => parts.push(c),
        }
    }
    if loc.starts_with('/') || playlist.contains("zz-probe-09") {
        // Absolute, or relative to the root: not to the playlist's folder.
        return loc.trim_start_matches('/').to_owned();
    }
    parts.join("/")
}

// Prevents: a playlist pointing at a file the probe did not write, so the
// owner would conclude the format fails when the file was simply missing.
#[test]
fn every_location_resolves_to_a_file_the_probe_writes() {
    let p = plan(10);
    let mut seen = 0;
    for pl in &p.playlists {
        for l in pl
            .text
            .lines()
            .filter(|l| !l.trim_start_matches('\u{feff}').starts_with('#'))
        {
            let target = resolve(&pl.path, l);
            assert!(
                p.audio.contains(&target),
                "{} points at {target:?} ({l:?})",
                pl.path
            );
            seen += 1;
        }
    }
    assert_eq!(seen, 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 4 + 1 + 2000);
    // And the other way: every copy is used by some playlist.
    for a in &p.audio {
        let used = p.playlists.iter().any(|pl| {
            pl.text
                .lines()
                .any(|l| !l.starts_with('#') && resolve(&pl.path, l) == *a)
        });
        assert!(used, "{a} is written and never pointed at");
    }
}

// Prevents: the plan depending on anything but the duration it is given
// (it is documented pure), and the duration leaking into the wrong place.
#[test]
fn the_plan_is_pure_and_only_the_extinf_lines_depend_on_the_duration() {
    let a = plan(7);
    assert_eq!(a, plan(7));
    let b = plan(8);
    assert_eq!((&a.dirs, &a.audio), (&b.dirs, &b.audio));
    for (x, y) in a.playlists.iter().zip(&b.playlists) {
        assert_eq!((&x.path, x.what), (&y.path, y.what));
        assert_eq!(x.text.replace(":7,", ":8,"), y.text, "{}", x.path);
    }
    // An unknown duration is written as the M3U convention `-1`.
    assert!(
        plan(-1).playlists[0]
            .text
            .contains("#EXTINF:-1,fiio-tunes - Probe 01\n")
    );
    // Folders come parents first and cover every file.
    let Plan {
        dirs,
        audio,
        playlists,
    } = plan(0);
    for (i, d) in dirs.iter().enumerate() {
        if let Some((parent, _)) = d.rsplit_once('/') {
            assert!(
                dirs[..i].iter().any(|p| p == parent),
                "{d} before its parent"
            );
        }
    }
    for f in audio.iter().chain(playlists.iter().map(|p| &p.path)) {
        if let Some((dir, _)) = f.rsplit_once('/') {
            assert!(dirs.iter().any(|d| d == dir), "no folder {dir} for {f}");
        }
    }
    let mut all: Vec<&String> = audio
        .iter()
        .chain(playlists.iter().map(|p| &p.path))
        .collect();
    let n = all.len();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), n, "a path is written twice");
    assert_eq!(n, 6 + 12);
}

// ---- source

/// A library of MP3s (and a FLAC) named by `name`, with the durations and
/// sizes the index holds set directly: the choice of source is about the
/// index rows, not the audio.
struct Lib {
    env: Env,
    next: u32,
    /// `(name, duration_ms, size)` to write into the index after the scan.
    specs: Vec<(String, Option<u32>, u64)>,
}

impl Lib {
    fn new() -> Self {
        Lib {
            env: Env::new(),
            next: 1,
            specs: Vec::new(),
        }
    }

    fn add(&mut self, name: &str, kind: Kind, duration_ms: Option<u32>, size: u64) -> &mut Self {
        self.next += 1;
        self.env.add(name, kind, self.next);
        self.specs.push((name.to_owned(), duration_ms, size));
        self
    }

    /// Scans once, then sets the durations and sizes (a later scan would
    /// see the sizes differ from the files and read them again).
    fn build(&mut self) -> &mut Self {
        self.env.scan();
        for (name, duration_ms, size) in &self.specs {
            let path = self.env.music.join(name);
            self.env
                .store
                .conn()
                .execute(
                    "UPDATE tracks SET duration_ms = ?1, size = ?2 WHERE path = ?3",
                    rusqlite::params![duration_ms, *size as i64, path.to_str().unwrap()],
                )
                .unwrap();
        }
        self
    }

    fn set(&self, name: &str, column: &str, value: rusqlite::types::Value) {
        let path = self.env.music.join(name);
        let n = self
            .env
            .store
            .conn()
            .execute(
                &format!("UPDATE tracks SET {column} = ?1 WHERE path = ?2"),
                rusqlite::params![value, path.to_str().unwrap()],
            )
            .unwrap();
        assert_eq!(n, 1, "no track {name}");
    }

    fn source(&self) -> fiio_core::Result<String> {
        probe::source(self.env.store.conn())
            .map(|t| t.path.file_name().unwrap().to_str().unwrap().to_owned())
    }
}

use rusqlite::types::Value;

const MIN: u32 = 60_000;

// Prevents: the probe copying a track that is too short or too long to be a
// song (6 copies of a 2-hour mix would fill the card), a protected one, one
// that is gone from disk, or one that is not an MP3; and not taking the
// smallest of those left.
#[test]
fn the_source_is_the_smallest_present_non_drm_mp3_of_one_to_ten_minutes() {
    let mut lib = Lib::new();
    lib.add("short.mp3", Kind::Mp3, Some(30_000), 10)
        .add("big.mp3", Kind::Mp3, Some(2 * MIN), 500)
        .add("best.mp3", Kind::Mp3, Some(2 * MIN), 300)
        .add("long.mp3", Kind::Mp3, Some(11 * MIN), 5)
        .add("drm.mp3", Kind::Mp3, Some(2 * MIN), 4)
        .add("gone.mp3", Kind::Mp3, Some(2 * MIN), 3)
        .add("tiny.flac", Kind::Flac, Some(2 * MIN), 2)
        .add("broken-tags.mp3", Kind::Mp3, Some(2 * MIN), 100)
        .build();
    lib.set("drm.mp3", "drm", Value::Integer(1));
    lib.set("gone.mp3", "missing_since", Value::Integer(1_700_000_000));
    lib.set(
        "broken-tags.mp3",
        "tag_error",
        Value::Text("bad frame".into()),
    );
    assert_eq!(lib.source().unwrap(), "best.mp3");
    // Gone from the index like the rest: the next smallest.
    lib.set("best.mp3", "missing_since", Value::Integer(1));
    assert_eq!(lib.source().unwrap(), "big.mp3");
}

// Prevents: an off-by-one at the ends of the one-to-ten-minute window.
#[test]
fn the_duration_window_includes_one_and_ten_minutes_exactly() {
    for (ms, preferred) in [
        (MIN - 1, false),
        (MIN, true),
        (10 * MIN, true),
        (10 * MIN + 1, false),
    ] {
        let mut lib = Lib::new();
        // The reference is a bigger track that is in the window.
        lib.add("ref.mp3", Kind::Mp3, Some(2 * MIN), 1000)
            .add("edge.mp3", Kind::Mp3, Some(ms), 10)
            .build();
        let want = if preferred { "edge.mp3" } else { "ref.mp3" };
        assert_eq!(lib.source().unwrap(), want, "{ms} ms");
    }
}

// Prevents: the probe refusing to run on a library with no ideal track, and
// taking a bad one over a good one when one exists.
#[test]
fn without_a_track_in_the_window_the_smallest_mp3_is_used() {
    let mut lib = Lib::new();
    lib.add("short.mp3", Kind::Mp3, Some(30_000), 50)
        .add("long.mp3", Kind::Mp3, Some(20 * MIN), 40)
        .add("unknown.mp3", Kind::Mp3, None, 60)
        .add("tagless.mp3", Kind::Mp3, Some(2 * MIN), 45)
        .add("drm.mp3", Kind::Mp3, Some(2 * MIN), 1)
        .add("gone.mp3", Kind::Mp3, Some(2 * MIN), 1)
        .add("solo.flac", Kind::Flac, Some(2 * MIN), 1)
        .build();
    lib.set("tagless.mp3", "tag_error", Value::Text("unreadable".into()));
    lib.set("drm.mp3", "drm", Value::Integer(1));
    lib.set("gone.mp3", "missing_since", Value::Integer(5));
    // A track with a tag error is not a "song" but is still an MP3.
    assert_eq!(lib.source().unwrap(), "long.mp3");
    lib.set("long.mp3", "missing_since", Value::Integer(5));
    assert_eq!(lib.source().unwrap(), "tagless.mp3");
}

// Prevents: ties giving a different file from one run to the next.
#[test]
fn equal_sizes_are_broken_by_the_older_track() {
    let mut lib = Lib::new();
    lib.add("first.mp3", Kind::Mp3, Some(2 * MIN), 100)
        .add("second.mp3", Kind::Mp3, Some(2 * MIN), 100)
        .add("third.mp3", Kind::Mp3, Some(2 * MIN), 100)
        .build();
    let tracks = lib.env.tracks();
    let oldest = tracks.iter().min_by_key(|t| t.id).unwrap();
    assert_eq!(tracks.len(), 3);
    assert_eq!(
        lib.source().unwrap(),
        oldest.path.file_name().unwrap().to_str().unwrap()
    );
}

// Prevents: a library with nothing the probe can copy being given an
// arbitrary file (a FLAC, a protected or missing MP3), or an empty index
// panicking.
#[test]
fn no_usable_mp3_is_no_probe_source() {
    let mut lib = Lib::new();
    lib.build();
    assert!(matches!(lib.source(), Err(Error::NoProbeSource)));
    let mut lib = Lib::new();
    lib.add("a.flac", Kind::Flac, Some(2 * MIN), 1)
        .add("drm.mp3", Kind::Mp3, Some(2 * MIN), 1)
        .add("gone.mp3", Kind::Mp3, Some(2 * MIN), 1)
        .build();
    lib.set("drm.mp3", "drm", Value::Integer(1));
    lib.set("gone.mp3", "missing_since", Value::Integer(5));
    assert!(
        matches!(lib.source(), Err(Error::NoProbeSource)),
        "{:?}",
        lib.source()
    );
}

// ---- write

// Prevents: a probe whose copies differ from the library file, whose
// playlists differ from the plan, or that reports other numbers than it
// wrote.
#[test]
fn write_copies_the_source_and_writes_the_plan() {
    let rig = Rig::new();
    let w = rig.write().unwrap();
    assert!(
        w.case_pair_kept,
        "this file system keeps `04 é` and `04 É` apart"
    );
    let t = rig.tree();
    let p = plan(200);
    let src = fs::read(&rig.src).unwrap();
    for a in &p.audio {
        assert_eq!(t[a].as_deref(), Some(&src[..]), "{a}");
    }
    for pl in &p.playlists {
        assert_eq!(
            t[&pl.path].as_deref(),
            Some(pl.text.as_bytes()),
            "{}",
            pl.path
        );
    }
    let files = t.values().filter(|v| v.is_some()).count();
    assert_eq!(files, 18);
    assert_eq!(w.files, 18);
    let bytes: u64 = t.values().flatten().map(|b| b.len() as u64).sum();
    assert_eq!(w.bytes, bytes);
    // Nothing besides the plan's folders and files.
    let want: BTreeMap<String, ()> = p
        .dirs
        .iter()
        .cloned()
        .chain(probe_files())
        .map(|f| (f, ()))
        .collect();
    assert_eq!(
        names(&t),
        want.keys().map(String::as_str).collect::<Vec<_>>()
    );
    // The folders it made are on record, so --clean can find them.
    assert!(rig.record.is_file());
    // Every location resolves to a file now on the card.
    for pl in &p.playlists {
        for l in pl
            .text
            .lines()
            .filter(|l| !l.trim_start_matches('\u{feff}').starts_with('#'))
        {
            let target = resolve(&pl.path, l);
            assert!(rig.card.join(&target).is_file(), "{}: {target}", pl.path);
        }
    }
}

// Prevents: the probe overwriting a card file, or leaving a half probe on
// top of an older one, when any one of its files is already there; it
// refuses before changing anything, whichever file it is.
#[test]
fn write_refuses_when_any_probe_file_exists_and_writes_nothing() {
    for existing in probe_files() {
        let rig = Rig::new();
        let path = rig.card.join(&existing);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"precious").unwrap();
        let before = rig.tree();
        match rig.write() {
            Err(Error::ProbeThere(card)) => assert_eq!(card, rig.card, "{existing}"),
            other => panic!("{existing}: expected ProbeThere, got {other:?}"),
        }
        assert_eq!(rig.tree(), before, "{existing}: the card changed");
        assert!(!rig.record.exists(), "{existing}: a record was written");
        assert_eq!(fs::read(&path).unwrap(), b"precious");
        assert!(probe::already_there(&rig.card));
    }
}

// Prevents: a dangling symlink on the card at a probe path being treated as
// free, then written through to wherever it points.
#[test]
fn write_treats_a_dangling_symlink_as_a_probe_file_already_there() {
    let rig = Rig::new();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("target.m3u8");
    symlink(&target, rig.card.join("zz-probe-01.m3u8")).unwrap();
    assert!(probe::already_there(&rig.card));
    assert!(matches!(rig.write(), Err(Error::ProbeThere(_))));
    assert!(!target.exists(), "written through the symlink");
}

// Prevents: a card that holds nothing of the probe's being reported as
// holding some.
#[test]
fn already_there_is_false_for_a_card_with_only_other_files() {
    let rig = Rig::new();
    assert!(!probe::already_there(&rig.card));
    for f in [
        "Music/zz-probe/mine.mp3",
        "zz-probe-13.m3u8",
        "zz-probe-1.m3u8",
        "Playlists/zz-probe-01.m3u8",
        "Music/zz-probe/01 probe.mp3",
        "Music/other.mp3",
    ] {
        let p = rig.card.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"x").unwrap();
    }
    assert!(
        !probe::already_there(&rig.card),
        "none of these is a probe file"
    );
}

// Prevents: a source that cannot be read leaving folders, or a record,
// behind on the card.
#[test]
fn an_unreadable_source_fails_before_anything_is_written() {
    let rig = Rig::new();
    fs::create_dir(rig.card.join("Music")).unwrap();
    let before = rig.tree();
    let r = probe::write(
        &rig.card,
        &rig.src.with_extension("missing"),
        200,
        &rig.record,
    );
    assert!(is_io(&r), "{r:?}");
    assert_eq!(rig.tree(), before);
    assert!(!rig.record.exists());
}

// Prevents: folders being created before they are on record: a probe that
// dies after creating `Music` would leave a folder `--clean` is not
// allowed to remove. When the record cannot be written, no folder may
// have been made.
#[test]
fn the_record_is_written_before_any_folder_is_created() {
    let rig = Rig::new();
    // The record cannot be written: a folder stands where the file goes.
    let record = rig.card.parent().unwrap().join("record-is-a-folder");
    fs::create_dir(&record).unwrap();
    let before = rig.tree();
    let r = probe::write(&rig.card, &rig.src, 200, &record);
    assert!(is_io(&r), "{r:?}");
    assert_eq!(rig.tree(), before, "folders were created without a record");
}

// Prevents: a probe cut short (here the `Playlists` folder is not
// writable, so the first playlist in it fails) leaving folders that
// `--clean` can neither find nor remove; and `--clean` removing the
// owner's `Playlists` folder, which the probe did not make.
#[test]
fn a_probe_cut_short_can_still_be_cleaned() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new();
    let playlists = rig.card.join("Playlists");
    fs::create_dir(&playlists).unwrap();
    if !common::chmod_000_blocks_reads(&playlists) {
        eprintln!("skipped: permission bits do not stop this user (root?)");
        return;
    }
    fs::set_permissions(&playlists, fs::Permissions::from_mode(0o555)).unwrap();
    let r = rig.write();
    let state = (
        rig.card.join("Music/zz-probe/01 Probe.mp3").is_file(),
        rig.card.join("zz-probe-07.m3u8").is_file(),
        probe::already_there(&rig.card),
        rig.record.is_file(),
    );
    let cleaned = rig.clean();
    // Restored before asserting, so a failure still lets the temp dir go.
    fs::set_permissions(&playlists, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(is_io(&r), "{r:?}");
    // The audio copies and the root playlists were made, and recorded.
    assert_eq!(state, (true, true, true, true));
    cleaned.unwrap();
    assert_eq!(
        names(&rig.tree()),
        ["Playlists"],
        "clean left the probe's folders, or removed the owner's"
    );
    assert!(!probe::already_there(&rig.card));
    assert!(!rig.record.exists());
}

// Prevents: `--clean` failing with "Not a directory" when the card has a
// regular file named `Playlists` (`remove_file("Playlists/zz-probe-08.m3u8")`
// gives ENOTDIR, which means "not there"): it used to stop after the root
// files, leaving `Music/zz-probe`, the record and `Music` for ever.
#[test]
fn clean_copes_with_a_file_where_the_playlists_folder_should_be() {
    let rig = Rig::new();
    fs::write(rig.card.join("Playlists"), b"not a folder").unwrap();
    assert!(is_io(&rig.write()));
    rig.clean().expect("clean must finish");
    assert_eq!(names(&rig.tree()), ["Playlists"]);
    assert_eq!(
        fs::read(rig.card.join("Playlists")).unwrap(),
        b"not a folder"
    );
    assert!(!rig.record.exists());
}

// Prevents: the read-back being skipped or comparing the wrong thing: a
// copy damaged after it is written must be reported as VerifyFailed (which
// the CLI turns into exit code 3) and the playlists must not be written
// after it. A second thread cuts the first copy down as soon as it
// exists, so the read-back sees a short file; the source is big so the
// write takes long enough for that to be certain in practice, and the
// attempt is repeated if the thread was ever too slow.
#[test]
fn a_copy_that_reads_back_different_is_a_verify_failure() {
    let mut last = String::new();
    for attempt in 0..8 {
        let rig = Rig::new();
        fs::write(&rig.src, vec![0x5a; 96 << 20]).unwrap();
        let target = rig.card.join("Music/zz-probe/01 Probe.mp3");
        let stop = AtomicBool::new(false);
        let r = std::thread::scope(|s| {
            // Set on any exit from this closure, a panic included: the
            // scope waits for the thread, which would spin for ever.
            let _stop = StopOnDrop(&stop);
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(f) = fs::OpenOptions::new().write(true).open(&target) {
                        let _ = f.set_len(1);
                    }
                }
            });
            rig.write()
        });
        match r {
            Err(Error::VerifyFailed(msg)) => {
                assert!(msg.contains("01 Probe.mp3"), "{msg}");
                let t = rig.tree();
                assert!(
                    !t.keys().any(|k| k.ends_with(".m3u8")),
                    "playlists written after a failed copy: {:?}",
                    names(&t)
                );
                assert!(
                    !t.keys().any(|k| k.contains("02 Élan")),
                    "went on after the failure"
                );
                // And it can be cleaned.
                rig.clean().unwrap();
                assert!(rig.tree().is_empty(), "{:?}", names(&rig.tree()));
                return;
            }
            other => last = format!("attempt {attempt}: {:?}", other.map(|w| w.files)),
        }
    }
    panic!("the damage was never seen by the read-back: {last}");
}

// ---- clean

// Prevents: --clean removing a file that is not the probe's: a user file
// beside it, a lookalike name, or the target of a symlink at a probe path.
#[test]
fn clean_removes_exactly_the_probes_files() {
    let rig = Rig::new();
    let outside = tempfile::tempdir().unwrap();
    let theirs = outside.path().join("theirs.m3u8");
    fs::write(&theirs, b"not yours").unwrap();
    let keep = [
        ("Music/zz-probe/mine.mp3", &b"m"[..]),
        ("Music/other.mp3", b"o"),
        ("Playlists/mine.m3u8", b"p"),
        ("zz-probe-13.m3u8", b"13"),
        ("zz-probe-01.m3u8.bak", b"bak"),
        ("Playlists/zz-probe-10.m3u8", b"not ours: 10 is at the root"),
        ("Music/zz-probe/05 other/x.mp3", b"x"),
    ];
    for (f, bytes) in keep {
        let p = rig.card.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }
    rig.write().unwrap();
    // A probe path that has since become a symlink to a file elsewhere.
    fs::remove_file(rig.card.join("zz-probe-02.m3u8")).unwrap();
    symlink(&theirs, rig.card.join("zz-probe-02.m3u8")).unwrap();

    let c = rig.clean().unwrap();
    assert_eq!(c.files, 18, "{c:?}");
    assert_eq!(
        fs::read(&theirs).unwrap(),
        b"not yours",
        "removed through the symlink"
    );
    let t = rig.tree();
    let mut want: Vec<&str> = vec!["Music", "Music/zz-probe", "Music/zz-probe/05 other"];
    want.extend(keep.iter().map(|(f, _)| *f));
    want.extend(["Playlists"]);
    want.sort();
    assert_eq!(names(&t), want);
    for (f, bytes) in keep {
        assert_eq!(t[f].as_deref(), Some(bytes), "{f}");
    }
    // The probe's own folder now holds other files: kept, and said to be.
    // (`Music` and `Playlists` were there before and are not its to remove.)
    assert_eq!(c.kept, ["Music/zz-probe"], "{c:?}");
    assert!(!c.dirs.contains(&"Music/zz-probe".to_owned()));
}

// Prevents: --clean removing `Music` or `Playlists` that were the owner's
// (empty or not), and leaving the ones the probe made. An empty `Music`
// that was already there stays; one the probe made goes.
#[test]
fn clean_removes_music_and_playlists_only_if_the_probe_made_them() {
    // Neither existed: both go, the card is as it was.
    let rig = Rig::new();
    let before = rig.tree();
    rig.write().unwrap();
    let c = rig.clean().unwrap();
    assert_eq!(rig.tree(), before);
    assert!(c.dirs.contains(&"Music".to_owned()) && c.dirs.contains(&"Playlists".to_owned()));
    assert!(c.kept.is_empty(), "{c:?}");
    assert!(!rig.record.exists());

    // Both existed, empty: both stay.
    let rig = Rig::new();
    fs::create_dir(rig.card.join("Music")).unwrap();
    fs::create_dir(rig.card.join("Playlists")).unwrap();
    let before = rig.tree();
    rig.write().unwrap();
    let c = rig.clean().unwrap();
    assert_eq!(rig.tree(), before, "{c:?}");
    assert!(
        c.dirs.iter().all(|d| d != "Music" && d != "Playlists"),
        "{c:?}"
    );

    // The probe made both, then the owner put files in them: kept, and said so.
    let rig = Rig::new();
    rig.write().unwrap();
    fs::write(rig.card.join("Music/new.mp3"), b"n").unwrap();
    fs::write(rig.card.join("Playlists/new.m3u8"), b"n").unwrap();
    let c = rig.clean().unwrap();
    assert!(
        c.kept.contains(&"Music".to_owned()) && c.kept.contains(&"Playlists".to_owned()),
        "{c:?}"
    );
    assert_eq!(
        names(&rig.tree()),
        ["Music", "Music/new.mp3", "Playlists", "Playlists/new.m3u8"]
    );

    // Only Music existed.
    let rig = Rig::new();
    fs::create_dir(rig.card.join("Music")).unwrap();
    rig.write().unwrap();
    rig.clean().unwrap();
    assert_eq!(names(&rig.tree()), ["Music"]);
}

// Prevents: a record left by a probe on another card (a second player,
// the same data folder) being used for, or erased by, the cleaning of this
// one: its folders stay, its record stays.
#[test]
fn clean_leaves_the_record_of_another_card_alone() {
    let a = Rig::new();
    let b = Rig::new();
    a.write().unwrap();
    let record_a = fs::read(&a.record).unwrap();
    let rec_b = b.record.with_file_name("probe-b.toml");
    probe::write(&b.card, &b.src, 200, &rec_b).unwrap();
    // Cleaning B with A's record: B's files and its own folders go, but
    // `Music` and `Playlists` are not proven to be the probe's.
    let c = probe::clean(&b.card, &a.record).unwrap();
    assert_eq!(c.files, 18);
    assert_eq!(names(&b.tree()), ["Music", "Playlists"]);
    assert_eq!(
        fs::read(&a.record).unwrap(),
        record_a,
        "A's record was changed"
    );
    // A can still be cleaned completely with it.
    let ca = a.clean().unwrap();
    assert_eq!(ca.files, 18);
    assert!(a.tree().is_empty(), "{:?}", names(&a.tree()));
    assert!(!a.record.exists());
}

// Prevents: a record that cannot be read (truncated, hand-edited) blocking
// the cleaning, or making it remove folders it cannot prove it made.
#[test]
fn clean_with_a_corrupt_record_removes_files_but_not_shared_folders() {
    let rig = Rig::new();
    rig.write().unwrap();
    fs::write(&rig.record, b"this is = not [valid toml").unwrap();
    let c = rig.clean().unwrap();
    assert_eq!(c.files, 18);
    assert_eq!(names(&rig.tree()), ["Music", "Playlists"]);
    assert!(!rig.record.exists(), "the unusable record is dropped");
}

// Prevents: --clean failing, or touching the card, when there is nothing to
// clean; and a second --clean doing anything.
#[test]
fn clean_is_idempotent_and_a_no_op_on_a_card_without_a_probe() {
    let rig = Rig::new();
    let p = rig.card.join("Music/a.mp3");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, b"keep").unwrap();
    let before = rig.tree();
    assert_eq!(rig.clean().unwrap(), Cleaned::default());
    assert_eq!(rig.tree(), before);
    assert!(!rig.record.exists());

    rig.write().unwrap();
    let first = rig.clean().unwrap();
    assert_eq!(first.files, 18);
    assert_eq!(rig.tree(), before);
    let again = rig.clean().unwrap();
    assert_eq!(again, Cleaned::default());
    assert_eq!(rig.tree(), before);
    // The source file the probe copied is never touched.
    assert_eq!(fs::read(&rig.src).unwrap(), b"ID3 not really audio");
}

// Prevents: write -> clean -> write failing on the second round because the
// first left a record or files behind.
#[test]
fn the_probe_can_be_written_again_after_a_clean() {
    let rig = Rig::new();
    let before = rig.tree();
    for _ in 0..2 {
        rig.write().unwrap();
        assert!(probe::already_there(&rig.card));
        rig.clean().unwrap();
        assert_eq!(rig.tree(), before);
    }
}

// Prevents: a real fixture MP3 failing to copy as a whole (sizes, binary
// bytes) the way the toy source above passes.
#[test]
fn a_real_mp3_is_copied_bit_for_bit() {
    let rig = Rig::new();
    fs::copy(fixture(Kind::Mp3, 1), &rig.src).unwrap();
    rig.write().unwrap();
    let src = fs::read(&rig.src).unwrap();
    for a in &plan(200).audio {
        assert_eq!(fs::read(rig.card.join(a)).unwrap(), src, "{a}");
    }
}

// ---- the probe of another card, and temp files

// Prevents: a second card's probe taking over the single record, so the
// first card's created folders are forgotten: `check` and `write` refuse
// while the record names another card, and change nothing.
#[test]
fn a_probe_is_refused_while_the_record_names_another_card() {
    let a = Rig::new();
    let b = Rig::new();
    a.write().unwrap();
    let record = fs::read(&a.record).unwrap();
    let before = b.tree();
    match probe::check(&b.card, &a.record) {
        Err(Error::ProbeElsewhere(card)) => assert_eq!(card, a.card),
        other => panic!("expected ProbeElsewhere, got {other:?}"),
    }
    match probe::write(&b.card, &b.src, 200, &a.record) {
        Err(Error::ProbeElsewhere(card)) => assert_eq!(card, a.card),
        other => panic!("expected ProbeElsewhere, got {other:?}"),
    }
    assert_eq!(b.tree(), before, "the other card was written");
    assert_eq!(
        fs::read(&a.record).unwrap(),
        record,
        "the record was overwritten"
    );
    // A's own check says its files are there; cleaning A frees the record,
    // and then B may be probed.
    assert!(matches!(
        probe::check(&a.card, &a.record),
        Err(Error::ProbeThere(_))
    ));
    a.clean().unwrap();
    probe::check(&b.card, &a.record).unwrap();
    probe::write(&b.card, &b.src, 200, &a.record).unwrap();
    assert!(probe::already_there(&b.card));
}

// Prevents: a record that no longer means anything (the other card cleaned
// by hand, gone, or this card now mounted at another path) blocking every
// later probe for ever: it is replaced, and then names this card.
#[test]
fn a_stale_record_of_another_card_is_replaced_by_the_next_probe() {
    for gone_entirely in [false, true] {
        let a = Rig::new();
        let b = Rig::new();
        a.write().unwrap();
        // The probe files of A are removed by hand; the record stays.
        if gone_entirely {
            fs::remove_dir_all(&a.card).unwrap();
        } else {
            for f in probe_files() {
                fs::remove_file(a.card.join(f)).unwrap();
            }
        }
        assert!(a.record.is_file());
        probe::check(&b.card, &a.record).unwrap();
        probe::write(&b.card, &b.src, 200, &a.record).unwrap();
        // The record now names B: B's own folders are on it, so cleaning B
        // leaves B as it was, and a probe of A is refused because of B.
        match probe::check(&a.card, &a.record) {
            Err(Error::ProbeElsewhere(card)) => assert_eq!(card, b.card),
            other => panic!("gone={gone_entirely}: expected ProbeElsewhere(B), got {other:?}"),
        }
        probe::clean(&b.card, &a.record).unwrap();
        assert!(b.tree().is_empty(), "{:?}", names(&b.tree()));
        assert!(!a.record.exists());
    }
}

// Prevents: `check` refusing when it should not: no record, a record of
// this card (a probe cut short and cleaned by hand), or one that cannot be
// read.
#[test]
fn check_accepts_a_card_with_no_usable_record_of_another() {
    let rig = Rig::new();
    probe::check(&rig.card, &rig.record).unwrap();
    fs::create_dir_all(rig.record.parent().unwrap()).unwrap();
    fs::write(&rig.record, b"not [valid = toml").unwrap();
    probe::check(&rig.card, &rig.record).unwrap();
    // A record of this very card, files gone.
    rig.write().unwrap();
    for f in probe_files() {
        fs::remove_file(rig.card.join(f)).unwrap();
    }
    assert!(rig.record.is_file());
    probe::check(&rig.card, &rig.record).unwrap();
    // Files there: that refusal comes first.
    fs::write(rig.card.join("zz-probe-03.m3u8"), b"x").unwrap();
    assert!(matches!(
        probe::check(&rig.card, &rig.record),
        Err(Error::ProbeThere(_))
    ));
}

// Prevents: `.fiio-tunes-*.tmp` files of a playlist write cut short being
// left on the card by --clean (they sit where the probe writes playlists:
// the root and `Playlists/`); and anything else being taken for them.
#[test]
fn clean_removes_temp_files_of_cut_short_writes_and_only_those() {
    let rig = Rig::new();
    rig.write().unwrap();
    let gone = [".fiio-tunes-abc123.tmp", "Playlists/.fiio-tunes-xyz.tmp"];
    let stay = [
        ".fiio-tunes-abc.txt",
        ".other.tmp",
        "fiio-tunes-abc.tmp",
        ".fiio-tunes-abc.tmp.bak",
        "Playlists/.hidden",
        "Music/.fiio-tunes-deep.tmp",
        ".fiio-tunes/manifest.json",
    ];
    for f in gone.iter().chain(&stay) {
        let p = rig.card.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"t").unwrap();
    }
    rig.clean().unwrap();
    let t = rig.tree();
    for f in gone {
        assert!(!t.contains_key(f), "{f} was left");
    }
    for f in stay {
        assert_eq!(t[f].as_deref(), Some(&b"t"[..]), "{f} changed");
        assert!(t.contains_key(f), "{f} was removed");
    }
    assert!(!probe::already_there(&rig.card));
    // Temp files with no probe at all are removed too (a cut short run
    // whose files were already cleaned by hand), and the rest stays.
    let rig = Rig::new();
    fs::write(rig.card.join(".fiio-tunes-q.tmp"), b"t").unwrap();
    fs::write(rig.card.join(".keep"), b"k").unwrap();
    rig.clean().unwrap();
    assert_eq!(names(&rig.tree()), [".keep"]);
}
