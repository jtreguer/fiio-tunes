//! `verify::differences` (technical document, 11.3) called directly on a
//! card that is a temp folder and a hand-built `Expected`.

mod common;

use std::fs;
use std::path::PathBuf;

use fiio_core::plan::{Expected, ExpectedFile, ExpectedPlaylist};
use fiio_core::verify::differences;

struct Card {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Card {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        Card { _dir: dir, root }
    }

    fn put(&self, rel: &str, bytes: &[u8]) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }

    fn diffs(&self, e: &Expected) -> Vec<String> {
        differences(&self.root, e).unwrap()
    }
}

fn file(path: &str, size: u64) -> ExpectedFile {
    ExpectedFile {
        path: path.into(),
        size,
        track_id: 1,
    }
}

fn list(path: &str, text: &str) -> ExpectedPlaylist {
    ExpectedPlaylist {
        playlist_id: 1,
        path: path.into(),
        text: text.into(),
    }
}

/// A card and the expectation it satisfies: one managed file, one managed
/// playlist, an unmanaged audio file and an unmanaged playlist.
fn good() -> (Card, Expected) {
    let c = Card::new();
    c.put("Music/a.mp3", b"12345");
    c.put("Rock.m3u8", b"#EXTM3U\n");
    c.put("Stray/s.flac", b"abc");
    c.put("Other.m3u8", b"mine");
    let e = Expected {
        files: vec![file("Music/a.mp3", 5)],
        playlists: vec![list("Rock.m3u8", "#EXTM3U\n")],
        unmanaged: vec![("Stray/s.flac".into(), 3), ("Other.m3u8".into(), 4)],
    };
    (c, e)
}

// Prevents: a card that is exactly as planned being reported, which would
// fail every sync; files the walk skips (covers, hidden entries, the
// manifest, other file types) are not differences either.
#[test]
fn a_card_as_planned_has_no_difference() {
    let (c, e) = good();
    assert_eq!(c.diffs(&e), Vec::<String>::new());
    c.put("Music/cover.jpg", b"jpg");
    c.put("Music/Thumbs.db", b"db");
    c.put("notes.txt", b"t");
    c.put(".fiio-tunes/manifest.json", b"{}");
    c.put(".Trash-1000/files/x.mp3", b"trash");
    c.put("System Volume Information/x.m3u8", b"s");
    c.put("Music/._a.mp3", b"mac");
    assert_eq!(c.diffs(&e), Vec::<String>::new());
    assert_eq!(c.diffs(&Expected::default()).len(), 4);
}

// Prevents: a missing managed file, or one of another size, passing; the
// report names the path and both sizes.
#[test]
fn a_managed_file_missing_or_of_another_size_is_a_difference() {
    let (c, mut e) = good();
    e.files.push(file("Music/gone.mp3", 9));
    assert_eq!(c.diffs(&e), ["Music/gone.mp3: missing"]);
    e.files.pop();
    c.put("Music/a.mp3", b"123456");
    assert_eq!(c.diffs(&e), ["Music/a.mp3: 6 bytes, 5 expected"]);
    c.put("Music/a.mp3", b"");
    assert_eq!(c.diffs(&e), ["Music/a.mp3: 0 bytes, 5 expected"]);
}

// Prevents: a managed playlist passing when its bytes are not the ones
// written (same length included), when it is missing, or is an empty file.
#[test]
fn a_managed_playlist_missing_or_with_other_bytes_is_a_difference() {
    let (c, mut e) = good();
    c.put("Rock.m3u8", b"#EXTM3U\r");
    assert_eq!(c.diffs(&e), ["Rock.m3u8: not the playlist written"]);
    c.put("Rock.m3u8", b"");
    assert_eq!(c.diffs(&e), ["Rock.m3u8: not the playlist written"]);
    fs::remove_file(c.root.join("Rock.m3u8")).unwrap();
    assert_eq!(c.diffs(&e), ["Rock.m3u8: missing"]);
    // Expected empty text against an empty file is fine.
    e.playlists = vec![list("Rock.m3u8", "")];
    c.put("Rock.m3u8", b"");
    assert!(c.diffs(&e).is_empty());
}

// Prevents: an unmanaged file lost or damaged by a sync passing: gone, or
// of another size, audio or playlist.
#[test]
fn an_unmanaged_file_gone_or_resized_is_a_difference() {
    let (c, e) = good();
    fs::remove_file(c.root.join("Stray/s.flac")).unwrap();
    assert_eq!(c.diffs(&e), ["Stray/s.flac: an unmanaged file is gone"]);
    c.put("Stray/s.flac", b"abc");
    c.put("Stray/s.flac", b"abcd");
    let d = c.diffs(&e);
    assert_eq!(d.len(), 1, "{d:?}");
    assert!(
        d[0].starts_with("Stray/s.flac: an unmanaged file changed size"),
        "{d:?}"
    );
    assert!(d[0].contains('4') && d[0].contains('3'), "{d:?}");
    c.put("Stray/s.flac", b"abc");
    fs::remove_file(c.root.join("Other.m3u8")).unwrap();
    assert_eq!(c.diffs(&e), ["Other.m3u8: an unmanaged file is gone"]);
    c.put("Other.m3u8", b"mine, longer");
    let d = c.diffs(&e);
    assert!(
        d.len() == 1 && d[0].starts_with("Other.m3u8: an unmanaged file changed size"),
        "{d:?}"
    );
}

// Prevents: a file the plan does not account for passing: an audio file or
// a playlist that should have been removed, or that appeared, anywhere on
// the card.
#[test]
fn an_audio_or_playlist_file_not_in_the_plan_is_a_difference() {
    let (c, e) = good();
    c.put("Music/old.mp3", b"o");
    c.put("deep/er/old.OGG", b"o");
    c.put("Dead.m3u8", b"d");
    c.put("sub/Dead2.m3u8", b"d");
    let mut d = c.diffs(&e);
    d.sort();
    assert_eq!(
        d,
        [
            "Dead.m3u8: on the card, not in the plan",
            "Music/old.mp3: on the card, not in the plan",
            "deep/er/old.OGG: on the card, not in the plan",
            "sub/Dead2.m3u8: on the card, not in the plan",
        ]
    );
}

// Prevents: paths compared the way Rust strings compare rather than the way
// FAT does: a managed or unmanaged file the card lists in another case or
// in decomposed Unicode is the same file.
#[test]
fn paths_match_as_fat_compares_them() {
    let c = Card::new();
    // Card: NFD name and upper case; plan: NFC name and lower case.
    c.put("Music/e\u{301}.mp3", b"12");
    c.put("MUSIC/B.MP3", b"123");
    c.put("rock.M3U8", b"x");
    c.put("Stray/S.FLAC", b"ab");
    let e = Expected {
        files: vec![file("Music/\u{e9}.mp3", 2), file("Music/b.mp3", 3)],
        playlists: vec![list("Rock.m3u8", "x")],
        unmanaged: vec![("stray/s.flac".into(), 2)],
    };
    assert_eq!(c.diffs(&e), Vec::<String>::new());
}

// Prevents: a card that could not be read completely passing (an
// unreadable folder hides files that should have been checked).
#[test]
fn problems_in_the_listing_are_differences() {
    let (c, e) = good();
    let dir = c.root.join("Locked");
    fs::create_dir(&dir).unwrap();
    if !common::chmod_000_blocks_reads(&dir) {
        eprintln!(
            "SKIPPED: permissions do not stop this user (running as root?); this test checked nothing"
        );
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
    let d = c.diffs(&e);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(d.len(), 1, "{d:?}");
    assert!(d[0].starts_with("Locked: could not be read"), "{d:?}");
}

// Prevents: a card root that cannot be listed passing as empty.
#[test]
fn a_card_that_cannot_be_opened_is_an_error() {
    let c = Card::new();
    assert!(differences(&c.root.join("absent"), &Expected::default()).is_err());
}
