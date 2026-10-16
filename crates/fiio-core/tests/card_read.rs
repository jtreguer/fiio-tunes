//! `card::read` on card folders in temp dirs: what is listed, what is
//! skipped, what is reported, and that reading leaves access times alone.

#[path = "common/mod.rs"]
mod common;

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;

use fiio_core::card::{self, Contents, is_audio, is_playlist};

fn put(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

fn audio(c: &Contents) -> Vec<&str> {
    c.audio.iter().map(|f| f.path.as_str()).collect()
}

fn playlists(c: &Contents) -> Vec<&str> {
    c.playlists.iter().map(|f| f.path.as_str()).collect()
}

// Prevents: audio in a format the scan indexes or reports (`opus`, `wma`)
// being missing from the card listing, so the planner would copy it again;
// and files that are neither audio nor playlists being listed.
#[test]
fn audio_playlists_and_other_files_are_told_apart() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    for rel in [
        "a.mp3",
        "b.m4a",
        "c.wav",
        "d.aif",
        "e.aiff",
        "f.flac",
        "g.ogg",
        "h.oga",
        "i.ape",
        // reported, not copied, but still audio
        "j.opus",
        "k.wma",
        "l.mpc",
        "m.wv",
        "n.dsf",
        "o.dff",
        // not audio
        "cover.jpg",
        "Thumbs.db",
        "notes.txt",
        "list.m3u",
        "mp3",
        "song.mp3.txt",
        "song.",
        // playlists
        "p.m3u8",
        "Playlists/q.m3u8",
    ] {
        put(r, rel, b"x");
    }
    let c = card::read(r).unwrap();
    assert_eq!(
        audio(&c),
        [
            "a.mp3", "b.m4a", "c.wav", "d.aif", "e.aiff", "f.flac", "g.ogg", "h.oga", "i.ape",
            "j.opus", "k.wma", "l.mpc", "m.wv", "n.dsf", "o.dff"
        ]
    );
    assert_eq!(playlists(&c), ["Playlists/q.m3u8", "p.m3u8"]);
    assert!(c.problems.is_empty(), "{:?}", c.problems);
}

// Prevents: FAT cards, which keep names in whatever case they were written
// in, listing `SONG.MP3` as not audio.
#[test]
fn extensions_are_matched_without_case() {
    let t = tempfile::tempdir().unwrap();
    for rel in [
        "A.MP3",
        "b.Flac",
        "C.M4a",
        "d.OPUS",
        "E.M3U8",
        "f.m3U8",
        "g.Mp3.TXT",
    ] {
        put(t.path(), rel, b"x");
    }
    let c = card::read(t.path()).unwrap();
    assert_eq!(audio(&c), ["A.MP3", "C.M4a", "b.Flac", "d.OPUS"]);
    assert_eq!(playlists(&c), ["E.M3U8", "f.m3U8"]);
    for (name, a, p) in [
        ("x.MP3", true, false),
        (".mp3", false, false),
        ("mp3", false, false),
        ("x.m3u8", false, true),
        ("x.M3U8", false, true),
        ("x.m3u", false, false),
    ] {
        assert_eq!((is_audio(name), is_playlist(name)), (a, p), "{name}");
    }
}

// Prevents: macOS `._` files, trash, the manifest folder and Windows' system
// folder being listed (or descended into) at any depth.
#[test]
fn dot_entries_and_system_volume_information_are_skipped_at_any_depth() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    for rel in [
        "keep.mp3",
        "Music/A/keep.mp3",
        "Music/A/._keep.mp3",
        ".hidden.mp3",
        ".fiio-tunes/manifest.json",
        ".fiio-tunes/backups/old.m3u8",
        ".fiio-tunes/x.mp3",
        ".Trash-1000/files/gone.mp3",
        "Music/.Trash-1000/deep/gone.mp3",
        "Music/A/.cache/x.mp3",
        "System Volume Information/a.mp3",
        "system volume information/b.mp3",
        "SYSTEM VOLUME INFORMATION/c.m3u8",
        "Music/System Volume Information/d.mp3",
        "Music/A/System Volume Information/e.mp3",
        "Music/System Volume Information.mp3/f.mp3",
    ] {
        put(r, rel, b"x");
    }
    let c = card::read(r).unwrap();
    // The last entry is a folder merely starting with the system folder's
    // name plus an extension: it is not that folder, and is read.
    assert_eq!(
        audio(&c),
        [
            "Music/A/keep.mp3",
            "Music/System Volume Information.mp3/f.mp3",
            "keep.mp3"
        ]
    );
    assert!(c.playlists.is_empty(), "{:?}", playlists(&c));
    assert!(c.problems.is_empty(), "{:?}", c.problems);
}

// Prevents: a symlink on the card leading the walk out of the card (to the
// library, the home folder) or round in circles.
#[test]
fn symlinks_are_not_followed() {
    let t = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    put(outside.path(), "dir/secret.mp3", b"s");
    put(outside.path(), "file.mp3", b"f");
    put(outside.path(), "list.m3u8", b"l");
    let card = t.path();
    put(card, "real.mp3", b"r");
    symlink(outside.path().join("dir"), card.join("linked-dir")).unwrap();
    symlink(
        outside.path().join("file.mp3"),
        card.join("linked-file.mp3"),
    )
    .unwrap();
    symlink(outside.path().join("list.m3u8"), card.join("linked.m3u8")).unwrap();
    symlink(card, card.join("loop")).unwrap();
    symlink("/nonexistent/target.mp3", card.join("dangling.mp3")).unwrap();
    let c = card::read(card).unwrap();
    assert_eq!(audio(&c), ["real.mp3"]);
    assert!(c.playlists.is_empty());
    assert!(c.problems.is_empty(), "{:?}", c.problems);
}

// Prevents: a folder that cannot be read silently making the card look
// emptier than it is (the planner would then copy over, or delete from, a
// listing it believes complete).
#[test]
fn a_subfolder_that_cannot_be_read_is_reported_and_the_rest_is_listed() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    put(r, "Music/ok/a.mp3", b"a");
    put(r, "Music/locked/b.mp3", b"b");
    put(r, "Music/locked/inner/c.mp3", b"c");
    put(r, "Music/z.mp3", b"z");
    let locked = r.join("Music/locked");
    if !common::chmod_000_blocks_reads(&locked) {
        eprintln!("skipped: permission bits do not stop this user (root?)");
        return;
    }
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let c = card::read(r);
    // Restore before asserting, so a failure still lets the temp dir go.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    let c = c.unwrap();
    assert_eq!(audio(&c), ["Music/ok/a.mp3", "Music/z.mp3"]);
    assert_eq!(c.problems.len(), 1, "{:?}", c.problems);
    assert_eq!(c.problems[0].0, "Music/locked");
    assert!(!c.problems[0].1.is_empty());
}

// Prevents: a name that is not valid UTF-8 (FAT cannot hold one, but a
// folder copy can) vanishing without a trace, or aborting the walk.
#[test]
fn a_name_that_is_not_utf8_is_reported_and_skipped() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    put(r, "good.mp3", b"g");
    let bad = OsStr::from_bytes(b"bad\xff.mp3");
    if fs::write(r.join(bad), b"b").is_err() {
        eprintln!("skipped: the file system refuses non-UTF-8 names");
        return;
    }
    // A dot entry with a bad name is skipped like any other dot entry.
    fs::write(r.join(OsStr::from_bytes(b".bad\xfe.mp3")), b"d").unwrap();
    // Names that could not be audio or playlists do not make the listing
    // short: nothing is reported for them.
    fs::write(r.join(OsStr::from_bytes(b"notes\xff.txt")), b"n").unwrap();
    fs::write(r.join(OsStr::from_bytes(b"cover\xff.jpg")), b"j").unwrap();
    // A playlist with a bad name could be one: reported.
    fs::write(r.join(OsStr::from_bytes(b"list\xff.m3u8")), b"#EXTM3U\n").unwrap();
    // A folder with a bad name is reported and not descended into.
    let dir = r.join(OsStr::from_bytes(b"dir\xfd"));
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("inside.mp3"), b"i").unwrap();
    let c = card::read(r).unwrap();
    assert_eq!(audio(&c), ["good.mp3"]);
    let mut problems: Vec<&str> = c.problems.iter().map(|(p, _)| p.as_str()).collect();
    problems.sort();
    assert_eq!(
        problems,
        ["bad\u{fffd}.mp3", "dir\u{fffd}", "list\u{fffd}.m3u8"]
    );
}

// Prevents: names normalized on the way in: the planner compares card paths
// by `card_key`, but the listing must show what is on disk, byte for byte.
#[test]
fn nfd_and_nfc_names_are_kept_exactly_as_on_disk() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    let nfd = "Cafe\u{301}/Jo\u{301}hann.mp3";
    let nfc = "Caf\u{e9}/J\u{f3}hann2.mp3";
    put(r, nfd, b"d");
    put(r, nfc, b"c");
    let c = card::read(r).unwrap();
    // Byte order: `e` (0x65) sorts before the NFC `\u{e9}` (0xc3 0xa9).
    assert_eq!(audio(&c), [nfd, nfc]);
    assert_eq!(c.audio[0].path.as_bytes(), nfd.as_bytes());
    assert_eq!(c.audio[1].path.as_bytes(), nfc.as_bytes());
}

// Prevents: sizes and times taken from the wrong place (a symlink's, a
// rounded value): the planner compares them with what it copied.
#[test]
fn sizes_and_modification_times_are_those_of_the_files() {
    use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, utimensat};
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    put(r, "a/one.mp3", &[7; 1234]);
    put(r, "two.flac", b"");
    put(r, "list.m3u8", b"#EXTM3U\n");
    let set = |rel: &str, sec: i64, nsec: i64| {
        let ts = |tv_sec, tv_nsec| Timespec { tv_sec, tv_nsec };
        utimensat(
            CWD,
            r.join(rel),
            &Timestamps {
                last_access: ts(5, 0),
                last_modification: ts(sec, nsec),
            },
            AtFlags::empty(),
        )
        .unwrap();
    };
    set("a/one.mp3", 1_700_000_000, 123_456_789);
    set("two.flac", 1_000_000_000, 1);
    set("list.m3u8", 1_500_000_000, 999_999_999);
    let c = card::read(r).unwrap();
    let got: Vec<_> = c
        .audio
        .iter()
        .map(|f| (f.path.as_str(), f.size, f.mtime_ns))
        .collect();
    assert_eq!(
        got,
        [
            ("a/one.mp3", 1234, 1_700_000_000_123_456_789),
            ("two.flac", 0, 1_000_000_000_000_000_001)
        ]
    );
    assert_eq!(
        (c.playlists[0].size, c.playlists[0].mtime_ns),
        (8, 1_500_000_000_999_999_999)
    );
    assert_eq!(c.audio_bytes(), 1234);
}

// Prevents: a listing in directory order (which differs between file
// systems, and between runs on FAT) reaching the planner; the order is
// that of the whole path string, not folder by folder.
#[test]
fn the_listing_is_sorted_by_path_bytes() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    for rel in [
        "Music/b.mp3",
        "Music/a/z.mp3",
        "Music-x/a.mp3",
        "Music/B.mp3",
        "Music/a.mp3",
        "music/a.mp3",
        "Music/é.mp3",
        "Playlists/z.m3u8",
        "Playlists/a.m3u8",
        "Playlists-b/a.m3u8",
        "a.m3u8",
    ] {
        put(r, rel, b"x");
    }
    let c = card::read(r).unwrap();
    // '-' (0x2d) sorts before '/' (0x2f), so "Music-x" precedes "Music/".
    assert_eq!(
        audio(&c),
        [
            "Music-x/a.mp3",
            "Music/B.mp3",
            "Music/a.mp3",
            "Music/a/z.mp3",
            "Music/b.mp3",
            "Music/é.mp3",
            "music/a.mp3"
        ]
    );
    assert_eq!(
        playlists(&c),
        [
            "Playlists-b/a.m3u8",
            "Playlists/a.m3u8",
            "Playlists/z.m3u8",
            "a.m3u8"
        ]
    );
}

// Prevents: the manifest being taken for present when it is not there, or
// is something else: only a file at `.fiio-tunes/manifest.json` counts.
#[test]
fn the_manifest_is_a_file_at_one_place_only() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    assert!(!card::read(r).unwrap().manifest, "empty card");
    put(r, "Music/.fiio-tunes/manifest.json", b"{}");
    put(r, "manifest.json", b"{}");
    fs::create_dir(r.join(".fiio-tunes")).unwrap();
    assert!(
        !card::read(r).unwrap().manifest,
        "state folder, no manifest"
    );
    fs::create_dir(r.join(".fiio-tunes/manifest.json")).unwrap();
    assert!(!card::read(r).unwrap().manifest, "a folder of that name");
    fs::remove_dir(r.join(".fiio-tunes/manifest.json")).unwrap();
    put(r, ".fiio-tunes/manifest.json", b"{}");
    let c = card::read(r).unwrap();
    assert!(c.manifest);
    // Its presence adds nothing to the listing.
    assert!(c.audio.is_empty() && c.playlists.is_empty() && c.problems.is_empty());
}

// Prevents: an empty card or an unusable root being mistaken for each
// other: an empty folder is an empty listing, a file or a missing folder is
// an error.
#[test]
fn an_empty_card_reads_empty_and_a_bad_root_is_an_error() {
    let t = tempfile::tempdir().unwrap();
    assert_eq!(card::read(t.path()).unwrap(), Contents::default());
    let file = t.path().join("f.mp3");
    fs::write(&file, b"x").unwrap();
    assert!(card::read(&file).is_err(), "a file is not a card");
    assert!(card::read(&t.path().join("missing")).is_err());
    // A root that is a symlink to a folder is opened (that is how a
    // mountpoint link would be given).
    put(t.path(), "real/a.mp3", b"x");
    symlink(t.path().join("real"), t.path().join("link")).unwrap();
    assert_eq!(
        audio(&card::read(&t.path().join("link")).unwrap()),
        ["a.mp3"]
    );
}

// Prevents: reading the card rewriting the access dates of its folders (a
// FAT card has dates of one day's precision, and the listing is run often).
#[test]
fn reading_leaves_the_access_times_of_folders_and_files_alone() {
    use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, utimensat};
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    put(r, "Music/A/one.mp3", b"x");
    put(r, "Music/A/deep/two.flac", b"y");
    put(r, "list.m3u8", b"#EXTM3U\n");
    put(r, "control/c.txt", b"c");
    let ts = |s| Timespec {
        tv_sec: s,
        tv_nsec: 0,
    };
    let old = |rel: &str| {
        // Old access, newer modification: a plain read updates atime even
        // under relatime.
        utimensat(
            CWD,
            r.join(rel),
            &Timestamps {
                last_access: ts(1_000_000),
                last_modification: ts(2_000_000),
            },
            AtFlags::empty(),
        )
        .unwrap();
    };
    let watched = [
        "Music/A/deep/two.flac",
        "Music/A/deep",
        "Music/A/one.mp3",
        "Music/A",
        "Music",
        "list.m3u8",
        "",
    ];
    for rel in watched.iter().chain(&["control"]) {
        old(rel);
    }
    let atime = |rel: &str| fs::metadata(r.join(rel)).unwrap().atime();
    // A control folder, listed the plain way: if this file system does not
    // update atime on a read, the check below could not fail.
    let _ = fs::read_dir(r.join("control")).unwrap().count();
    if atime("control") == 1_000_000 {
        eprintln!("skipped: filesystem does not update atime");
        return;
    }
    let c = card::read(r).unwrap();
    assert_eq!(c.audio.len(), 2);
    for rel in watched {
        assert_eq!(atime(rel), 1_000_000, "{rel:?} was read with a plain open");
    }
}

// Prevents: a manifest that cannot be examined (a link loop, a folder that
// cannot be searched) being read as "no manifest": the card would look
// never synced. Only "not there" is no manifest; anything else is a
// problem, and the listing is then incomplete.
#[test]
fn a_manifest_that_cannot_be_examined_is_a_problem_not_an_absence() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    put(r, "a.mp3", b"x");
    fs::create_dir(r.join(".fiio-tunes")).unwrap();
    // A dangling link is simply not there.
    symlink("/nonexistent/manifest", r.join(".fiio-tunes/manifest.json")).unwrap();
    let c = card::read(r).unwrap();
    assert!(!c.manifest);
    assert!(c.problems.is_empty(), "{:?}", c.problems);
    // A link to itself cannot be followed (ELOOP).
    fs::remove_file(r.join(".fiio-tunes/manifest.json")).unwrap();
    symlink("manifest.json", r.join(".fiio-tunes/manifest.json")).unwrap();
    let c = card::read(r).unwrap();
    assert!(!c.manifest);
    assert_eq!(c.problems.len(), 1, "{:?}", c.problems);
    assert_eq!(c.problems[0].0, ".fiio-tunes/manifest.json");
    assert_eq!(audio(&c), ["a.mp3"]);

    // A state folder that cannot be searched (not as root).
    fs::remove_file(r.join(".fiio-tunes/manifest.json")).unwrap();
    put(r, ".fiio-tunes/manifest.json", b"{}");
    let state = r.join(".fiio-tunes");
    if common::chmod_000_blocks_reads(&state) {
        fs::set_permissions(&state, fs::Permissions::from_mode(0o000)).unwrap();
        let c = card::read(r);
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        let c = c.unwrap();
        assert!(!c.manifest);
        assert_eq!(c.problems.len(), 1, "{:?}", c.problems);
        assert_eq!(c.problems[0].0, ".fiio-tunes/manifest.json");
    }
    assert!(card::read(r).unwrap().manifest);
}

// Prevents: the read-back hashing something other than the file: it must
// equal the BLAKE3 of the bytes just written and synced, for any size.
#[test]
fn read_back_is_the_blake3_of_a_file_just_written_and_synced() {
    use std::io::Write;
    let t = tempfile::tempdir().unwrap();
    for len in [0usize, 1, 4095, 4096, 4097, 3 << 20] {
        let bytes: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        let p = t.path().join(format!("f{len}.bin"));
        let mut f = fs::File::create(&p).unwrap();
        f.write_all(&bytes).unwrap();
        f.sync_all().unwrap();
        drop(f);
        assert_eq!(
            card::read_back(&p).unwrap(),
            *blake3::hash(&bytes).as_bytes(),
            "{len} bytes"
        );
    }
    // A different file hashes differently, and a missing one is an error.
    fs::write(t.path().join("x"), b"x").unwrap();
    assert_ne!(
        card::read_back(&t.path().join("x")).unwrap(),
        card::read_back(&t.path().join("f1.bin")).unwrap()
    );
    let err = card::read_back(&t.path().join("missing")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

// Prevents: the read-back going through the page cache, which would compare
// the written bytes with themselves and never see a card that stored them
// wrong. Shown on a disk file system (a tmpfs has no pages to drop; the
// test then skips) with the process's `read_bytes` counter in
// /proc/self/io, which counts what is fetched from storage: reading a file
// that was just written and synced fetches nothing when its pages are
// cached (the control read), and all of it after `read_back`.
#[test]
fn read_back_fetches_the_file_from_storage_not_from_the_page_cache() {
    use std::io::{Read, Write};
    // `FIIO_TEST_DISK_DIR` names another folder to try (a tmpfs, to see the
    // skip); by default the target folder, which is on the project's disk.
    let base = std::env::var_os("FIIO_TEST_DISK_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_TARGET_TMPDIR").into());
    let dir = tempfile::tempdir_in(base).unwrap();
    let size = 16usize << 20;
    let write = |name: &str| {
        let p = dir.path().join(name);
        let mut f = fs::File::create(&p).unwrap();
        // Not compressible, so the storage layer really moves the bytes.
        let bytes: Vec<u8> = (0..size as u64)
            .map(|i| (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8)
            .collect();
        f.write_all(&bytes).unwrap();
        f.sync_all().unwrap();
        p
    };
    let stored = || -> Option<u64> {
        let io = fs::read_to_string("/proc/self/io").ok()?;
        io.lines()
            .find_map(|l| l.strip_prefix("read_bytes:"))?
            .trim()
            .parse()
            .ok()
    };
    let Some(start) = stored() else {
        eprintln!("skipped: no /proc/self/io");
        return;
    };
    // Control: a plain read of a file just written comes from the cache.
    let control = write("control.bin");
    let mut sink = Vec::new();
    fs::File::open(&control)
        .unwrap()
        .read_to_end(&mut sink)
        .unwrap();
    let plain = stored().unwrap() - start;
    if plain >= size as u64 / 2 {
        eprintln!("skipped: this file system does not cache what was just written");
        return;
    }
    // Second control: with the pages dropped by hand, a read must fetch the
    // file. When it does not, nothing here can be measured (a tmpfs or an
    // overlay has no cache to drop, or counts nothing) and the test skips.
    let advised = write("advised.bin");
    let f = fs::File::open(&advised).unwrap();
    rustix::fs::fadvise(&f, 0, None, rustix::fs::Advice::DontNeed).unwrap();
    let before = stored().unwrap();
    let mut sink = Vec::new();
    (&f).read_to_end(&mut sink).unwrap();
    let advised_fetch = stored().unwrap() - before;
    if advised_fetch < size as u64 / 2 {
        eprintln!(
            "skipped: dropping the cache by hand fetched {advised_fetch} bytes; nothing to measure here"
        );
        return;
    }
    let p = write("probe.bin");
    let before = stored().unwrap();
    card::read_back(&p).unwrap();
    let fetched = stored().unwrap() - before;
    assert!(
        fetched >= size as u64 / 2,
        "read_back fetched {fetched} of {size} bytes from storage (a plain read: {plain})"
    );
}

// Prevents: `leftover` taking a file that is not a sync's for one (the
// doctor would delete it or refuse the card), or missing one that is: the
// part name is `.fiio-tunes-<digits>.part`, a hop is
// `.fiio-tunes-move-<digits>.move` or `.m3u8`, nothing looser, and
// `part_name` makes names it recognises.
#[test]
fn leftover_names_are_recognised_exactly() {
    use card::Leftover::{Hop, Part};
    for (name, want) in [
        (".fiio-tunes-1.part", Some(Part)),
        (".fiio-tunes-123.part", Some(Part)),
        (".fiio-tunes-move-0.move", Some(Hop)),
        (".fiio-tunes-move-17.m3u8", Some(Hop)),
        (".fiio-tunes-.part", None),
        (".fiio-tunes-x.part", None),
        (".fiio-tunes-1x.part", None),
        (".fiio-tunes-1.part.bak", None),
        (".fiio-tunes-1.PART", None),
        (".fiio-tunes-move-.move", None),
        (".fiio-tunes-move-1.mp3", None),
        (".fiio-tunes-move-1", None),
        (".fiio-tunes-move-a.move", None),
        ("fiio-tunes-1.part", None),
        (".fiio-tunes", None),
        ("", None),
    ] {
        assert_eq!(card::leftover(name), want, "{name:?}");
    }
    assert_eq!(card::leftover(&card::part_name(42)), Some(Part));
}

// Prevents: the listing hiding a sync's leftovers (a `prepare` would plan
// over a half-done swap) or inventing them: parts and hops are listed
// sorted with their folder, a hidden folder of that name is not one, an
// ordinary hidden file is not one, and none of them is audio or a playlist.
#[test]
fn parts_and_hops_are_listed_apart_from_audio_and_playlists() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    put(r, "Music/b/.fiio-tunes-2.part", b"p");
    put(r, "Music/.fiio-tunes-1.part", b"p");
    put(r, "Music/.fiio-tunes-move-3.move", b"h");
    for d in ["a", "b", "c", "d", "e", "f"] {
        put(r, &format!("Music/{d}/.fiio-tunes-move-8.move"), b"h");
    }
    put(r, ".fiio-tunes-move-4.m3u8", b"h");
    put(r, "Music/.hidden", b"other");
    put(r, "Music/.fiio-tunes-9.part.txt", b"other");
    put(r, "Music/a.mp3", b"a");
    fs::create_dir_all(r.join("Music/.fiio-tunes-5.part")).unwrap();
    put(
        r,
        "Music/.fiio-tunes-5.part/inner.mp3",
        b"inside a folder of that name",
    );
    symlink("a.mp3", r.join("Music/.fiio-tunes-6.part")).unwrap();
    let c = card::read(r).unwrap();
    assert_eq!(
        c.parts,
        ["Music/.fiio-tunes-1.part", "Music/b/.fiio-tunes-2.part"]
    );
    let mut want: Vec<String> = [".fiio-tunes-move-4.m3u8", "Music/.fiio-tunes-move-3.move"]
        .map(String::from)
        .to_vec();
    want.extend(
        ["a", "b", "c", "d", "e", "f"].map(|d| format!("Music/{d}/.fiio-tunes-move-8.move")),
    );
    want.sort();
    assert_eq!(c.hops, want);
    assert_eq!(audio(&c), ["Music/a.mp3"]);
    assert!(playlists(&c).is_empty());
    assert!(c.problems.is_empty());
}
