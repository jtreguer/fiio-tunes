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
use lofty::config::ParseOptions;
use lofty::file::AudioFile;
use lofty::mpeg::MpegFile;
use lofty::prelude::Accessor;

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
        .map(|a| a.path.clone())
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
        fs::copy(fixture(Kind::Mp3, 1), &src).unwrap();
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

    /// Replaces the source with `bytes`.
    fn set_source(&self, bytes: &[u8]) {
        fs::write(&self.src, bytes).unwrap();
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
/// code under test. `entries` are `(#EXTINF title, location)`.
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
            s.push_str(&format!("#EXTINF:{secs},{title}{eol}"));
        }
        s.push_str(&format!("{loc}{eol}"));
    }
    s
}

// Prevents: the playlists the owner tests on the player differing, in a
// byte, from what section 9.4 says they are: the result of the probe fixes
// the format of every playlist the tool will write.
#[test]
fn the_plan_matches_the_fourteen_variants_byte_for_byte() {
    let secs = 213;
    let p = plan(secs);
    let loc = |n: u32| format!("Music/zz-probe/{n:02} Probe.mp3");
    // One entry `List NN <label>` pointing at `loc`.
    let exp = |bom, header, eol, n: u32, label: &str, loc: &str| {
        expected(
            bom,
            header,
            eol,
            secs,
            &[(&format!("List {n:02} {label}"), loc)],
        )
    };
    let backslashed = loc(2).replace('/', "\\");
    let table: Vec<(&str, String)> = vec![
        (
            "zz-probe-01.m3u8",
            exp(false, true, "\n", 1, "basic", &loc(1)),
        ),
        (
            "zz-probe-02.m3u8",
            exp(false, true, "\n", 2, "backslash", &backslashed),
        ),
        ("zz-probe-03.m3u8", exp(true, true, "\n", 3, "BOM", &loc(3))),
        (
            "zz-probe-04.m3u8",
            exp(false, true, "\r\n", 4, "CRLF", &loc(4)),
        ),
        (
            "zz-probe-05.m3u8",
            exp(false, false, "\n", 5, "no header", &loc(5)),
        ),
        (
            "zz-probe-06.m3u8",
            exp(
                false,
                true,
                "\n",
                6,
                "leading slash",
                &format!("/{}", loc(6)),
            ),
        ),
        (
            "zz-probe-07.m3u8",
            exp(false, true, "\n", 7, "TF1", &format!("TF1:/{}", loc(7))),
        ),
        (
            "Playlists/zz-probe-08.m3u8",
            exp(false, true, "\n", 8, "dot-dot", &format!("../{}", loc(8))),
        ),
        (
            "Playlists/zz-probe-09.m3u8",
            exp(false, true, "\n", 9, "root-relative", &loc(9)),
        ),
    ];
    for (i, (path, text)) in table.iter().enumerate() {
        assert_eq!(p.playlists[i].path, *path);
        assert_eq!(p.playlists[i].text, *text, "variant {}", i + 1);
    }
    // 10: accents, then CJK, two files.
    assert_eq!(p.playlists[9].path, "zz-probe-10.m3u8");
    assert_eq!(
        p.playlists[9].text,
        expected(
            false,
            true,
            "\n",
            secs,
            &[
                ("List 10 accents", "Music/zz-probe/10 Élan café.mp3"),
                ("List 10 CJK", "Music/zz-probe/10 日本語の歌.mp3"),
            ]
        )
    );
    // 11: one entry, the long path.
    assert_eq!(p.playlists[10].path, "zz-probe-11.m3u8");
    let lines: Vec<&str> = p.playlists[10].text.lines().collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], "#EXTM3U");
    assert_eq!(lines[1], format!("#EXTINF:{secs},List 11 long path"));
    assert!(lines[2].starts_with("Music/zz-probe/11 A folder with a long name"));
    assert!(lines[2].ends_with(".mp3"));
    assert!(p.playlists[10].text.ends_with(".mp3\n"));
    // 12: 2000 entries numbered #0001 to #2000: one file 1999 times, then
    // another.
    assert_eq!(p.playlists[11].path, "zz-probe-12.m3u8");
    let names: Vec<String> = (1..=2000).map(|i| format!("List 12 #{i:04}")).collect();
    let entries: Vec<(&str, &str)> = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let file = if i == 1999 {
                "Music/zz-probe/12 Last.mp3"
            } else {
                "Music/zz-probe/12 Probe.mp3"
            };
            (n.as_str(), file)
        })
        .collect();
    assert_eq!(
        p.playlists[11].text,
        expected(false, true, "\n", secs, &entries)
    );
    assert_eq!(p.playlists[11].entries, 2000);
    assert_eq!(
        p.playlists[11].titles,
        ["Probe 12 2000 entries", "Probe 12 last"]
    );
    // 13: the same as 1, with the extension .m3u.
    assert_eq!(p.playlists[12].path, "zz-probe-13.m3u");
    assert_eq!(
        p.playlists[12].text,
        exp(false, true, "\n", 13, "m3u", &loc(13))
    );
    // 14: in the music folder, next to its file, the bare file name.
    assert_eq!(p.playlists[13].path, "Music/zz-probe/zz-probe-14.m3u8");
    assert_eq!(
        p.playlists[13].text,
        exp(false, true, "\n", 14, "same folder", "14 Probe.mp3")
    );
    assert_eq!(p.playlists.len(), 14);
    // The files, in the order of the variants, each with the title the tag
    // gets.
    let audio: Vec<(&str, &str)> = p
        .audio
        .iter()
        .map(|a| (a.path.as_str(), a.title.as_str()))
        .collect();
    assert_eq!(audio.len(), 16);
    assert_eq!(audio[0], ("Music/zz-probe/01 Probe.mp3", "Probe 01 basic"));
    assert_eq!(
        audio[9],
        ("Music/zz-probe/10 Élan café.mp3", "Probe 10 accents")
    );
    assert_eq!(
        audio[10],
        ("Music/zz-probe/10 日本語の歌.mp3", "Probe 10 CJK")
    );
    assert_eq!(
        audio[12],
        ("Music/zz-probe/12 Probe.mp3", "Probe 12 2000 entries")
    );
    assert_eq!(audio[13], ("Music/zz-probe/12 Last.mp3", "Probe 12 last"));
    assert_eq!(
        audio[15],
        ("Music/zz-probe/14 Probe.mp3", "Probe 14 same folder")
    );
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
    assert!(p.audio.iter().any(|a| a.path == loc));
    for a in &p.audio {
        assert!(
            a.path
                .split('/')
                .all(|c| c.encode_utf16().count() <= 255 && !c.ends_with(['.', ' '])),
            "{}",
            a.path
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
            c => parts.push(c),
        }
    }
    if loc.starts_with('/') || playlist.contains("zz-probe-09") {
        // Absolute, or relative to the root: not to the playlist's folder.
        // (Variants 7 and 6 are absolute; 9 is root relative by design: the
        // point of that variant is to see if the player agrees.)
        return loc.trim_start_matches('/').to_owned();
    }
    parts.join("/")
}

/// The locations of a playlist's text, in order.
fn locations(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|l| !l.trim_start_matches('\u{feff}').starts_with('#'))
        .collect()
}

// Prevents: a playlist pointing at a file the probe did not write, so the
// owner would conclude the format fails when the file was simply missing.
#[test]
fn every_location_resolves_to_a_file_the_probe_writes() {
    let p = plan(10);
    let paths: Vec<&str> = p.audio.iter().map(|a| a.path.as_str()).collect();
    let mut seen = 0;
    for pl in &p.playlists {
        for l in locations(&pl.text) {
            let target = resolve(&pl.path, l);
            assert!(
                paths.contains(&target.as_str()),
                "{} points at {target:?} ({l:?})",
                pl.path
            );
            seen += 1;
        }
    }
    assert_eq!(seen, 9 + 2 + 1 + 2000 + 1 + 1);
}

// Prevents: two variants sharing a file (the first probe's flaw: what
// played told nothing about which variant worked), a copy nobody points at,
// and a playlist whose `titles` are not those of the files its entries
// reach.
#[test]
fn each_file_belongs_to_exactly_one_variant_and_the_titles_follow() {
    let p = plan(10);
    for a in &p.audio {
        let users: Vec<&str> = p
            .playlists
            .iter()
            .filter(|pl| {
                locations(&pl.text)
                    .iter()
                    .any(|l| resolve(&pl.path, l) == a.path)
            })
            .map(|pl| pl.path.as_str())
            .collect();
        assert_eq!(users.len(), 1, "{} is used by {users:?}", a.path);
    }
    for pl in &p.playlists {
        // The titles of the files, once each, in the order of first use.
        let mut want: Vec<&str> = Vec::new();
        for l in locations(&pl.text) {
            let target = resolve(&pl.path, l);
            let a = p.audio.iter().find(|a| a.path == target).unwrap();
            if !want.contains(&a.title.as_str()) {
                want.push(&a.title);
            }
        }
        assert_eq!(pl.titles, want, "{}", pl.path);
        assert_eq!(pl.entries, locations(&pl.text).len(), "{}", pl.path);
    }
    // Every title is unique, and names its variant number.
    let mut titles: Vec<&str> = p.audio.iter().map(|a| a.title.as_str()).collect();
    assert_eq!(titles.len(), 16);
    titles.sort();
    titles.dedup();
    assert_eq!(titles.len(), 16, "two files share a title");
    for (i, pl) in p.playlists.iter().enumerate() {
        let n = format!("{:02}", i + 1);
        for t in &pl.titles {
            assert!(t.starts_with(&format!("Probe {n} ")), "{t} in variant {n}");
        }
    }
    // What the playlist's own lines say is distinct from the file's title.
    for pl in p.playlists.iter().filter(|p| p.text.contains("#EXTINF")) {
        for l in pl.text.lines().filter(|l| l.starts_with("#EXTINF")) {
            let t = l.split_once(',').unwrap().1.trim_end_matches('\r');
            assert!(t.starts_with("List "), "{l}");
            assert!(!p.audio.iter().any(|a| a.title == t), "{l}");
        }
    }
}

// Prevents: the plan depending on anything but the duration it is given
// (it is documented pure), and the duration leaking into the wrong place.
#[test]
fn the_plan_is_pure_and_only_the_extinf_lines_depend_on_the_duration() {
    let a = plan(7);
    assert_eq!(a, plan(7));
    assert_eq!(probe::checklist(&a), probe::checklist(&plan(7)));
    let b = plan(8);
    assert_eq!((&a.dirs, &a.audio), (&b.dirs, &b.audio));
    for (x, y) in a.playlists.iter().zip(&b.playlists) {
        assert_eq!((&x.path, x.what), (&y.path, y.what));
        assert_eq!((&x.titles, x.entries), (&y.titles, y.entries));
        assert_eq!(x.text.replace(":7,", ":8,"), y.text, "{}", x.path);
    }
    // An unknown duration is written as the M3U convention `-1`.
    assert!(
        plan(-1).playlists[0]
            .text
            .contains("#EXTINF:-1,List 01 basic\n")
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
    for f in audio
        .iter()
        .map(|a| &a.path)
        .chain(playlists.iter().map(|p| &p.path))
    {
        if let Some((dir, _)) = f.rsplit_once('/') {
            assert!(dirs.iter().any(|d| d == dir), "no folder {dir} for {f}");
        }
    }
    let mut all: Vec<&String> = audio
        .iter()
        .map(|a| &a.path)
        .chain(playlists.iter().map(|p| &p.path))
        .collect();
    let n = all.len();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), n, "a path is written twice");
    assert_eq!(n, 16 + 14);
}

// Prevents: a checklist that leaves a playlist out, or tells the owner to
// expect another entry count or other titles than the playlist holds: the
// owner compares the player's screen to it.
#[test]
fn the_checklist_names_every_playlist_with_its_entry_count_and_titles() {
    let p = plan(213);
    let list = probe::checklist(&p);
    let lines: Vec<&str> = list.lines().collect();
    for pl in &p.playlists {
        let at: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.trim() == pl.path)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(at.len(), 1, "{} appears {} times", pl.path, at.len());
        // Its block: up to the next blank line.
        let block: Vec<&str> = lines[at[0]..]
            .iter()
            .take_while(|l| !l.trim().is_empty())
            .copied()
            .collect();
        let block = block.join("\n");
        let count = if pl.entries == 1 {
            "1 entry".to_owned()
        } else {
            format!("{} entries", pl.entries)
        };
        assert!(block.contains(&count), "{}: {block}", pl.path);
        assert!(block.contains(pl.what), "{}: {block}", pl.path);
        for t in &pl.titles {
            assert!(block.contains(&format!("\"{t}\"")), "{}: {block}", pl.path);
        }
        // And no other variant's title.
        for other in p.audio.iter().filter(|a| !pl.titles.contains(&a.title)) {
            assert!(!block.contains(&other.title), "{}: {block}", pl.path);
        }
    }
    assert!(list.contains("2000 entries"));
    assert!(list.contains("Probe 10 accents") && list.contains("Probe 10 CJK"));
    // It tells the owner how to undo the probe.
    assert!(list.contains("probe --clean"), "{list}");
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

// Prevents: a probe whose copies differ from what the tagging makes of the
// library file, whose playlists differ from the plan, or that reports other
// numbers than it wrote.
#[test]
fn write_copies_the_tagged_source_and_writes_the_plan() {
    let rig = Rig::new();
    let w = rig.write().unwrap();
    let t = rig.tree();
    let p = plan(200);
    let src = fs::read(&rig.src).unwrap();
    for a in &p.audio {
        let want = probe::tagged(&src, &a.title, &a.artist).unwrap();
        assert_eq!(t[&a.path].as_deref(), Some(&want[..]), "{}", a.path);
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
    assert_eq!(files, 30);
    assert_eq!(w.files, 30);
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
        for l in locations(&pl.text) {
            let target = resolve(&pl.path, l);
            assert!(rig.card.join(&target).is_file(), "{}: {target}", pl.path);
        }
    }
    // The library file is read, never written.
    assert_eq!(fs::read(&rig.src).unwrap(), src);
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
        "zz-probe-14.m3u8",
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

/// `bytes` followed by zeros up to `len`: a valid MP3 that takes a while to
/// write (the zeros trail the last frame).
fn padded(bytes: &[u8], len: usize) -> Vec<u8> {
    let mut v = bytes.to_vec();
    v.resize(len, 0);
    v
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
        rig.set_source(&padded(&fs::read(&rig.src).unwrap(), 24 << 20));
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
                    !t.keys().any(|k| k.contains("02 Probe")),
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
        ("zz-probe-13.m3u8", b"13 is .m3u"),
        ("Music/zz-probe/zz-probe-14.m3u", b"14 is .m3u8"),
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
    assert_eq!(c.files, 30, "{c:?}");
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
    assert_eq!(c.files, 30);
    assert_eq!(names(&b.tree()), ["Music", "Playlists"]);
    assert_eq!(
        fs::read(&a.record).unwrap(),
        record_a,
        "A's record was changed"
    );
    // A can still be cleaned completely with it.
    let ca = a.clean().unwrap();
    assert_eq!(ca.files, 30);
    assert!(a.tree().is_empty(), "{:?}", names(&a.tree()));
    assert!(!a.record.exists());
}

// Prevents: cleaning card B, while the record names card A whose probe is
// still there, deleting A's checklist (the owner still needs it); and a
// clean of A leaving the checklist behind.
#[test]
fn clean_keeps_the_checklist_of_another_cards_probe_and_deletes_its_own() {
    let a = Rig::new();
    let b = Rig::new();
    a.write().unwrap();
    let list = probe::checklist_path(&a.record);
    assert_eq!(list.parent(), a.record.parent());
    fs::write(&list, b"the list").unwrap();
    probe::write(&b.card, &b.src, 200, &b.record.with_file_name("other.toml")).unwrap();
    probe::clean(&b.card, &a.record).unwrap();
    assert_eq!(fs::read(&list).unwrap(), b"the list", "A's checklist went");
    assert!(a.record.is_file(), "A's record went");
    assert!(probe::already_there(&a.card));
    a.clean().unwrap();
    assert!(!list.exists(), "the checklist outlived its probe");
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
    assert_eq!(c.files, 30);
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
    assert_eq!(first.files, 30);
    assert_eq!(rig.tree(), before);
    let again = rig.clean().unwrap();
    assert_eq!(again, Cleaned::default());
    assert_eq!(rig.tree(), before);
    // The source file the probe copied is never touched.
    assert_eq!(
        fs::read(&rig.src).unwrap(),
        fs::read(fixture(Kind::Mp3, 1)).unwrap()
    );
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
    let gone = [
        ".fiio-tunes-abc123.tmp",
        "Playlists/.fiio-tunes-xyz.tmp",
        "Music/zz-probe/.fiio-tunes-cut.tmp",
    ];
    let stay = [
        ".fiio-tunes-abc.txt",
        ".other.tmp",
        "fiio-tunes-abc.tmp",
        ".fiio-tunes-abc.tmp.bak",
        "Playlists/.hidden",
        "Music/.fiio-tunes-deep.tmp",
        "Music/zz-probe/.fiio-tunes-cut.txt",
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

// ---- tags

/// The length of the ID3v2 tag `bytes` starts with, 0 when it has none.
fn id3v2_len(bytes: &[u8]) -> usize {
    if bytes.len() < 10 || &bytes[..3] != b"ID3" {
        return 0;
    }
    let size = bytes[6..10]
        .iter()
        .fold(0usize, |acc, b| (acc << 7) | usize::from(*b & 0x7f));
    10 + size + if bytes[5] & 0x10 != 0 { 10 } else { 0 }
}

/// The audio frames of the fixture MP3, with no tag at all.
fn bare_audio() -> Vec<u8> {
    let fx = fs::read(fixture(Kind::Mp3, 1)).unwrap();
    assert!(id3v2_len(&fx) > 0, "the fixture is expected to carry a tag");
    let audio = fx[id3v2_len(&fx)..].to_vec();
    assert_eq!(audio[0], 0xff, "the audio starts on a frame");
    audio
}

/// An ID3v1 tag, with a title no probe tag has.
fn id3v1() -> Vec<u8> {
    let mut v = b"TAG".to_vec();
    for (text, len) in [
        ("OLDV1TITLE", 30),
        ("OLDV1ARTIST", 30),
        ("OLDV1ALBUM", 30),
        ("1999", 4),
        ("OLDV1COMMENT", 30),
    ] {
        let mut field = text.as_bytes().to_vec();
        field.resize(len, 0);
        v.extend(field);
    }
    v.push(17);
    assert_eq!(v.len(), 128);
    v
}

/// An APEv2 tag (header, one item, footer) as most writers make them.
fn ape() -> Vec<u8> {
    let mut items = Vec::new();
    items.extend(11u32.to_le_bytes());
    items.extend(0u32.to_le_bytes());
    items.extend(b"Title\0OLDAPEVALUE");
    let tag = |flags: u32| {
        let mut v = b"APETAGEX".to_vec();
        v.extend(2000u32.to_le_bytes());
        // The size counts the items and the footer, not the header.
        v.extend((items.len() as u32 + 32).to_le_bytes());
        v.extend(1u32.to_le_bytes());
        v.extend(flags.to_le_bytes());
        v.extend([0u8; 8]);
        v
    };
    let mut v = tag(0xa000_0000);
    v.extend(&items);
    v.extend(tag(0x8000_0000));
    v
}

fn concat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// Sources with every combination of old tags, each with the audio they
/// hold and strings that must not survive into a copy.
fn sources() -> Vec<(&'static str, Vec<u8>)> {
    let audio = bare_audio();
    let fx = fs::read(fixture(Kind::Mp3, 1)).unwrap();
    vec![
        ("no tag", audio.clone()),
        ("id3v2 only", fx.clone()),
        ("id3v1 only", concat(&[&audio, &id3v1()])),
        ("id3v2 and id3v1", concat(&[&fx, &id3v1()])),
        ("ape only", concat(&[&audio, &ape()])),
        ("ape and id3v1", concat(&[&audio, &ape(), &id3v1()])),
        ("id3v2, ape and id3v1", concat(&[&fx, &ape(), &id3v1()])),
    ]
}

/// The text encoding byte of each frame of the ID3v2.3 tag `copy` starts
/// with, by frame id.
fn frame_encodings(copy: &[u8]) -> BTreeMap<String, u8> {
    let end = id3v2_len(copy);
    let mut at = 10;
    let mut out = BTreeMap::new();
    while at + 10 < end && copy[at] != 0 {
        let id = String::from_utf8(copy[at..at + 4].to_vec()).unwrap();
        let size = u32::from_be_bytes(copy[at + 4..at + 8].try_into().unwrap()) as usize;
        out.insert(id, copy[at + 10]);
        at += 10 + size;
    }
    out
}

/// What the encodings must be: Latin-1 (0) for ASCII, UTF-16 (1) for the
/// non-ASCII artists of variant 10.
fn want_encodings(a: &probe::ProbeAudio) -> BTreeMap<String, u8> {
    let enc = |t: &str| u8::from(!t.is_ascii());
    BTreeMap::from([
        ("TIT2".to_owned(), enc(&a.title)),
        ("TPE1".to_owned(), enc(&a.artist)),
        ("TALB".to_owned(), 0),
    ])
}

const OLD: &[&[u8]] = &[b"OLDV1", b"OLDAPE", b"Title1", b"Artist1"];

// Prevents: a copy carrying a leftover tag (ID3v1 at the end, an APE tag, a
// second ID3v2) the X3 might read instead of the one naming the variant, a
// tag of another version, wrong text in it, or audio frames altered by the
// tagging. Each source shape has its own way of going wrong.
#[test]
fn every_copy_has_one_id3v23_tag_and_the_sources_audio_whatever_tags_it_had() {
    for (name, source) in sources() {
        let rig = Rig::new();
        rig.set_source(&source);
        rig.write().unwrap_or_else(|e| panic!("{name}: {e}"));
        let audio = bare_audio();
        let p = plan(200);
        assert_eq!(p.audio.len(), 16);
        for a in &p.audio {
            let copy = fs::read(rig.card.join(&a.path)).unwrap();
            let ctx = format!("{name}: {}", a.path);
            // The file starts with an ID3v2.3 header...
            assert_eq!(&copy[..4], b"ID3\x03", "{ctx}");
            let tag_len = id3v2_len(&copy);
            // ...which is followed by the audio, exactly, and nothing else:
            // no second tag after it, no ID3v1 or APE at the end.
            assert_eq!(&copy[tag_len..], &audio[..], "{ctx}: frames differ");
            assert_ne!(&copy[tag_len..tag_len + 3], b"ID3", "{ctx}: two tags");
            // What lofty makes of it.
            let f = MpegFile::read_from(&mut std::io::Cursor::new(&copy), ParseOptions::new())
                .unwrap_or_else(|e| panic!("{ctx}: {e}"));
            let tag = f.id3v2().unwrap_or_else(|| panic!("{ctx}: no ID3v2 tag"));
            assert_eq!(tag.title().as_deref(), Some(a.title.as_str()), "{ctx}");
            assert_eq!(tag.artist().as_deref(), Some(a.artist.as_str()), "{ctx}");
            assert_eq!(frame_encodings(&copy), want_encodings(a), "{ctx}");
            assert_eq!(tag.album().as_deref(), Some("zz-probe"), "{ctx}");
            assert_eq!(tag.len(), 3, "{ctx}: more than title, artist, album");
            assert!(f.id3v1().is_none(), "{ctx}: an ID3v1 tag");
            assert!(f.ape().is_none(), "{ctx}: an APE tag");
            for old in OLD {
                assert!(
                    !copy.windows(old.len()).any(|w| w == *old),
                    "{ctx}: {} survives",
                    String::from_utf8_lossy(old)
                );
            }
        }
        // And the source is untouched.
        assert_eq!(fs::read(&rig.src).unwrap(), source, "{name}");
    }
}

// Prevents: the tagging of a source with no ID3v1 tag adding an empty one
// (lofty's own removal does), or of one with an ID3v1 tag leaving it.
#[test]
fn tagged_ends_with_the_last_audio_byte_with_or_without_an_id3v1_tag() {
    let audio = bare_audio();
    for with_v1 in [false, true] {
        let source = if with_v1 {
            concat(&[&audio, &id3v1()])
        } else {
            audio.clone()
        };
        let out = probe::tagged(&source, "Probe 99 x", "fiio-tunes").unwrap();
        assert_eq!(out.len() - id3v2_len(&out), audio.len(), "v1: {with_v1}");
        assert_eq!(out.last(), audio.last(), "v1: {with_v1}");
    }
}

// Prevents: a title being shared between the file and the plan, or the
// tagging being given the wrong title: each copy on the card carries the
// title the plan lists for its path, and no two copies are the same file.
#[test]
fn each_copy_carries_its_planned_title_and_no_two_copies_are_equal() {
    let rig = Rig::new();
    rig.write().unwrap();
    let p = plan(200);
    let mut seen: Vec<Vec<u8>> = Vec::new();
    for a in &p.audio {
        let copy = fs::read(rig.card.join(&a.path)).unwrap();
        let f = MpegFile::read_from(&mut std::io::Cursor::new(&copy), ParseOptions::new()).unwrap();
        assert_eq!(
            f.id3v2().unwrap().title().as_deref(),
            Some(a.title.as_str()),
            "{}",
            a.path
        );
        assert!(!seen.contains(&copy), "{} repeats another copy", a.path);
        seen.push(copy);
    }
    // The titles an owner reads in a playlist's block are those on the card.
    for pl in &p.playlists {
        for l in locations(&pl.text) {
            let file = rig.card.join(resolve(&pl.path, l));
            let f = MpegFile::read_from(
                &mut std::io::Cursor::new(fs::read(file).unwrap()),
                ParseOptions::new(),
            )
            .unwrap();
            let title = f.id3v2().unwrap().title().unwrap().into_owned();
            assert!(pl.titles.contains(&title), "{}: {title}", pl.path);
        }
    }
}

// Prevents: a source lofty cannot tag leaving a half probe on the card (the
// first copies written, then a failure on a later one), or a folder, or a
// record: the tags are made in memory first, so nothing is written.
#[test]
fn a_source_that_cannot_be_tagged_writes_nothing_at_all() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("text named mp3", b"this is not audio".to_vec()),
        (
            "a flac named mp3",
            fs::read(fixture(Kind::Flac, 1)).unwrap(),
        ),
        ("a tag header and nothing", b"ID3 not really audio".to_vec()),
    ];
    for (name, bytes) in cases {
        let rig = Rig::new();
        // Something already on the card, in the probe's folders.
        fs::create_dir_all(rig.card.join("Music")).unwrap();
        fs::write(rig.card.join("Music/a.mp3"), b"mine").unwrap();
        rig.set_source(&bytes);
        let before = rig.tree();
        match rig.write() {
            Err(Error::ProbeSourceTags(path, _)) => assert_eq!(path, rig.src, "{name}"),
            other => panic!("{name}: expected ProbeSourceTags, got {other:?}"),
        }
        assert_eq!(rig.tree(), before, "{name}: the card changed");
        assert!(!rig.record.exists(), "{name}: a record was written");
        assert!(!probe::already_there(&rig.card), "{name}");
    }
}

// Prevents: --clean leaving anything the probe or its cut-short writes
// made, in any of the three folders it writes playlists to, and removing
// anything of the owner's beside them: write then clean is the identity.
#[test]
fn write_then_clean_leaves_the_card_exactly_as_it_was_even_with_leftover_temp_files() {
    let rig = Rig::new();
    for (f, bytes) in [
        ("Music/Album/a.mp3", &b"a"[..]),
        ("Music/zz-probe-notes.txt", b"n"),
        ("best.m3u8", b"#EXTM3U\n"),
        ("Playlists/mine.m3u8", b"#EXTM3U\n"),
        (".fiio-tunes/manifest.json", b"{}"),
        ("Music/.fiio-tunes-theirs.tmp", b"not in a playlist folder"),
    ] {
        let p = rig.card.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }
    let before = rig.tree();
    rig.write().unwrap();
    // A playlist write cut short in each folder, and a copy that is not one.
    for f in [
        ".fiio-tunes-a1.tmp",
        "Playlists/.fiio-tunes-a2.tmp",
        "Music/zz-probe/.fiio-tunes-a3.tmp",
    ] {
        fs::write(rig.card.join(f), b"half").unwrap();
    }
    let c = rig.clean().unwrap();
    assert_eq!(c.files, 30 + 3, "{c:?}");
    assert_eq!(rig.tree(), before);
    assert!(!rig.record.exists());
    // And on an empty card.
    let rig = Rig::new();
    rig.write().unwrap();
    fs::write(rig.card.join("Music/zz-probe/.fiio-tunes-a3.tmp"), b"half").unwrap();
    rig.clean().unwrap();
    assert!(rig.tree().is_empty(), "{:?}", names(&rig.tree()));
}

// Prevents: the artists of variant 10 being written as Latin-1 (mangled) or
// every text frame being UTF-16 (a player that cannot read it shows nothing
// for the ASCII titles): exactly the two non-ASCII artists are UTF-16, the
// rest Latin-1, and every artist is `fiio-tunes` otherwise.
#[test]
fn only_the_two_variant_10_artists_are_utf16_and_the_rest_latin1() {
    let rig = Rig::new();
    rig.write().unwrap();
    let p = plan(200);
    let mut utf16 = Vec::new();
    for a in &p.audio {
        let copy = fs::read(rig.card.join(&a.path)).unwrap();
        let enc = frame_encodings(&copy);
        assert_eq!(enc.len(), 3, "{}", a.path);
        assert_eq!(enc["TIT2"], 0, "{}: title", a.path);
        assert_eq!(enc["TALB"], 0, "{}: album", a.path);
        if enc["TPE1"] == 1 {
            utf16.push(a.artist.clone());
        } else {
            assert_eq!(a.artist, "fiio-tunes", "{}", a.path);
        }
        // Read back by lofty, whatever the encoding.
        let f = MpegFile::read_from(&mut std::io::Cursor::new(&copy), ParseOptions::new()).unwrap();
        assert_eq!(
            f.id3v2().unwrap().artist().as_deref(),
            Some(a.artist.as_str())
        );
    }
    assert_eq!(utf16, ["Élan café", "日本語の歌"]);
    // The artist belongs to the plan's accents and CJK copies.
    assert_eq!(p.audio[9].artist, "Élan café");
    assert_eq!(p.audio[10].artist, "日本語の歌");
}

// Prevents: the hint lines (what else to look at for variants 5, 10 and
// 12) missing, under the wrong playlist, or spread over several lines.
#[test]
fn the_checklist_hints_appear_under_variants_5_10_and_12_only_on_one_line() {
    let p = plan(213);
    let list = probe::checklist(&p);
    let lines: Vec<&str> = list.lines().collect();
    let mut hinted = Vec::new();
    for pl in &p.playlists {
        let at = lines.iter().position(|l| l.trim() == pl.path).unwrap();
        let block: Vec<&str> = lines[at..]
            .iter()
            .take_while(|l| !l.trim().is_empty())
            .copied()
            .collect();
        let also: Vec<&&str> = block.iter().filter(|l| l.starts_with("  also: ")).collect();
        assert!(also.len() <= 1, "{}: {block:?}", pl.path);
        // Nothing but the three known lines in a block, so a hint cannot
        // spill onto a line of its own.
        assert_eq!(block.len(), 3 + also.len(), "{}: {block:?}", pl.path);
        if let Some(l) = also.first() {
            assert!(l.len() > "  also: ".len(), "{}: empty hint", pl.path);
            hinted.push(pl.path.as_str());
            assert_eq!(pl.hint, &l["  also: ".len()..], "{}", pl.path);
        } else {
            assert!(pl.hint.is_empty(), "{}", pl.path);
        }
    }
    assert_eq!(
        hinted,
        ["zz-probe-05.m3u8", "zz-probe-10.m3u8", "zz-probe-12.m3u8"]
    );
    assert_eq!(list.matches("  also: ").count(), 3);
}

// Prevents: --clean deleting a checklist it cannot tie to this card: with
// no record at all (or an unreadable one) the checklist may be that of a
// probe elsewhere whose record was lost. The record itself, when unusable,
// is still dropped, and a record of this card takes the checklist with it.
#[test]
fn clean_keeps_the_checklist_when_no_record_names_this_card() {
    for record_state in ["missing", "corrupt"] {
        let rig = Rig::new();
        rig.write().unwrap();
        let list = probe::checklist_path(&rig.record);
        fs::write(&list, b"the list").unwrap();
        if record_state == "missing" {
            fs::remove_file(&rig.record).unwrap();
        } else {
            fs::write(&rig.record, b"not [valid = toml").unwrap();
        }
        rig.clean().unwrap();
        assert_eq!(
            fs::read(&list).unwrap(),
            b"the list",
            "{record_state}: the checklist was deleted"
        );
        assert!(!rig.record.exists(), "{record_state}: the record stayed");
        assert!(!probe::already_there(&rig.card));
    }
    // With a record of this card, both go.
    let rig = Rig::new();
    rig.write().unwrap();
    let list = probe::checklist_path(&rig.record);
    fs::write(&list, b"the list").unwrap();
    rig.clean().unwrap();
    assert!(!list.exists() && !rig.record.exists());
}
