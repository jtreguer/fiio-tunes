//! Adoption (technical document, section 10) on card folders in temp dirs:
//! what is matched, what is read, what is saved, whose card it is, and that
//! the card is never written. The matcher itself (`adopt.rs`) has its own
//! tests; these cover the adoption around it.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use common::{Env, FakeCard, Kind, fixture, id3v24_text, mp3_with_id3v2};
use fiio_core::adopt::{Proposal, Reason};
use fiio_core::adoption::{self, Run};
use fiio_core::device::{Found, UsbDevice};
use fiio_core::manifest::Identity;
use fiio_core::mounts::Mount;
use fiio_core::store::devices::{self, PlaylistMapping, TrackMapping};
use fiio_core::store::{self};
use fiio_core::{Error, playlists};

const KEY: &str = "folder:test";

struct World {
    env: Env,
    card: FakeCard,
    _dir: tempfile::TempDir,
}

impl World {
    /// An empty library and an empty card.
    fn empty() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("card");
        fs::create_dir(&root).unwrap();
        World {
            env: Env::new(),
            card: FakeCard::new(root),
            _dir: dir,
        }
    }

    /// A library of MP3 fixtures `1..=n` at `t<i>.mp3`, scanned.
    fn with_library(n: u32) -> Self {
        let mut w = Self::empty();
        for i in 1..=n {
            w.env.add(&format!("t{i}.mp3"), Kind::Mp3, i);
        }
        w.env.scan();
        w
    }

    fn conn(&self) -> &rusqlite::Connection {
        self.env.store.conn()
    }

    fn run_with(&self, key: &str, accept: bool) -> fiio_core::Result<Run> {
        adoption::run(
            self.conn(),
            &self.env.cfg,
            &self.card.root,
            key,
            accept,
            &|_, _| {},
        )
    }

    fn run(&self, accept: bool) -> Run {
        self.run_with(KEY, accept).expect("adoption runs")
    }

    fn lib_id(&self, rel: &str) -> i64 {
        self.env.track(&self.env.music.join(rel)).id
    }

    fn manifest(&mut self, device_key: &str, card_id: &str) {
        self.card.put(
            ".fiio-tunes/manifest.json",
            format!(r#"{{"device_key": "{device_key}", "card_id": "{card_id}"}}"#).as_bytes(),
        );
    }

    fn load_playlists(&self, names: &[&str]) {
        for n in names {
            self.env.write_playlist(n, "#EXTM3U\n");
        }
        playlists::sync_from_disk(self.conn(), &self.env.cfg, true).unwrap();
    }

    fn playlist_id(&self, name: &str) -> i64 {
        self.env.playlist_id(name).unwrap()
    }
}

/// `card path -> library path relative to the music folder` of the final
/// mapping.
fn mapped(w: &World, r: &Run) -> BTreeMap<String, String> {
    r.mapping
        .iter()
        .map(|&(k, id)| {
            (
                r.tracks[k as usize].file.path.clone(),
                r.library[&id]
                    .path
                    .strip_prefix(&w.env.music)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
            )
        })
        .collect()
}

fn map_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect()
}

fn track<'a>(r: &'a Run, rel: &str) -> &'a adoption::CardTrack {
    r.tracks
        .iter()
        .find(|t| t.file.path == rel)
        .unwrap_or_else(|| panic!("{rel} is not among the card tracks"))
}

fn key_of(r: &Run, rel: &str) -> u64 {
    r.tracks.iter().position(|t| t.file.path == rel).unwrap() as u64
}

fn proposal<'a>(r: &'a Run, rel: &str) -> &'a Proposal {
    let k = key_of(r, rel);
    r.adoption
        .proposals
        .iter()
        .find(|p| p.key == k)
        .unwrap_or_else(|| panic!("{rel} has no proposal: {:?}", r.adoption))
}

fn is_unmatched(r: &Run, rel: &str) -> bool {
    r.adoption.unmatched_device.contains(&key_of(r, rel))
}

fn unreadable(r: &Run) -> Vec<&str> {
    r.unreadable.iter().map(|(p, _)| p.as_str()).collect()
}

fn blake(path: &std::path::Path) -> [u8; 32] {
    *blake3::hash(&fs::read(path).unwrap()).as_bytes()
}

fn count(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// The same MP3 audio as fixture `variant`, with the given ID3 frames.
fn retagged(variant: u32, frames: &[Vec<u8>]) -> Vec<u8> {
    mp3_with_id3v2(variant, frames)
}

fn fiio(serial: Option<&str>) -> Found {
    found_with_usb("2972", serial, "/mnt/x3".into())
}

fn found_with_usb(vendor: &str, serial: Option<&str>, mountpoint: PathBuf) -> Found {
    Found {
        mount: Mount {
            mountpoint,
            fstype: "vfat".into(),
            read_only: false,
            root: "/".into(),
        },
        source: Some("/dev/sdc1".into()),
        usb: Some(UsbDevice {
            vendor: vendor.into(),
            product: "0003".into(),
            serial: serial.map(str::to_owned),
            manufacturer: Some("FiiO".into()),
            name: Some("X3 2nd gen".into()),
        }),
    }
}

fn folder_found(path: &str) -> Found {
    Found {
        mount: Mount {
            mountpoint: path.into(),
            fstype: String::new(),
            read_only: false,
            root: PathBuf::new(),
        },
        source: None,
        usb: None,
    }
}

// ---- 1. matching

// Prevents: matching by path or name instead of content: files moved and
// renamed on the card, in any layout, must still be taken as the library's.
#[test]
fn identical_files_are_accepted_whatever_their_folder_and_name() {
    let mut w = World::with_library(3);
    w.card.copy("Whatever/Deep/renamed one.mp3", Kind::Mp3, 1);
    w.card.copy("zzz.mp3", Kind::Mp3, 2);
    w.card.copy("Music/t3.MP3", Kind::Mp3, 3);
    w.card.age();
    let r = w.run(false);
    assert_eq!(
        mapped(&w, &r),
        map_of(&[
            ("Whatever/Deep/renamed one.mp3", "t1.mp3"),
            ("zzz.mp3", "t2.mp3"),
            ("Music/t3.MP3", "t3.mp3"),
        ])
    );
    assert!(r.adoption.proposals.is_empty(), "{:?}", r.adoption);
    assert_eq!(r.adoption.identical.len(), 3);
    assert!(r.left.is_empty());
    w.card.assert_untouched();
}

// Prevents: a name or a folder the card has by chance being taken for the
// library's: the same name with other content is not the track.
#[test]
fn a_card_file_named_like_a_library_file_but_with_other_content_is_not_matched_by_name() {
    let mut w = World::with_library(1);
    // Other audio and length: nothing in common with t1.mp3 but its name.
    w.card.put(
        "t1.mp3",
        &fs::read(common::fixture_secs(Kind::Mp3, 9, 3)).unwrap(),
    );
    let r = w.run(true);
    assert!(r.mapping.is_empty(), "{:?}", r.mapping);
    assert!(is_unmatched(&r, "t1.mp3"));
}

// Prevents: identical copies in the library collapsing to one, or two card
// files both taking the same copy.
#[test]
fn identical_library_copies_are_proposed_and_accepting_takes_the_next_free_copy() {
    let mut w = World::empty();
    w.env.add("a.mp3", Kind::Mp3, 1);
    w.env.add("b.mp3", Kind::Mp3, 1);
    w.env.scan();
    let (a, b) = (w.lib_id("a.mp3"), w.lib_id("b.mp3"));
    let (first, second) = (a.min(b), a.max(b));
    w.card.copy("x/one.mp3", Kind::Mp3, 1);
    w.card.copy("y/two.mp3", Kind::Mp3, 1);
    w.card.age();

    let r = w.run(false);
    for rel in ["x/one.mp3", "y/two.mp3"] {
        let p = proposal(&r, rel);
        assert_eq!(p.reason, Reason::SeveralCandidates);
        assert_eq!(p.candidates, [first, second]);
        assert_eq!(p.default, Some(first));
    }
    assert!(r.mapping.is_empty(), "nothing is mapped before accepting");
    assert_eq!(r.left.len(), 2);

    let r = w.run(true);
    let ids: BTreeMap<String, i64> = r
        .mapping
        .iter()
        .map(|&(k, id)| (r.tracks[k as usize].file.path.clone(), id))
        .collect();
    assert_eq!(ids["x/one.mp3"], first);
    assert_eq!(ids["y/two.mp3"], second);
    assert!(r.left.is_empty());
    w.card.assert_untouched();
}

// Prevents: a file whose tags were changed after the copy being managed as a
// stranger (and copied again) instead of proposed.
#[test]
fn a_retagged_copy_is_proposed_by_tags_and_accepting_maps_it() {
    let mut w = World::with_library(2);
    // Same audio as t1.mp3 and the same title and artist, plus a genre: not
    // the same bytes, not the same size.
    let bytes = retagged(
        1,
        &[
            id3v24_text("TIT2", 3, b"Title1"),
            id3v24_text("TPE1", 3, b"Artist1"),
            id3v24_text("TCON", 3, b"Rock"),
        ],
    );
    assert_ne!(
        bytes.len() as u64,
        w.env.track(&w.env.music.join("t1.mp3")).size
    );
    w.card.put("old/retagged.mp3", &bytes);
    w.card.age();

    let r = w.run(false);
    let p = proposal(&r, "old/retagged.mp3");
    assert_eq!(p.reason, Reason::TagsOnly);
    assert_eq!(p.candidates, [w.lib_id("t1.mp3")]);
    assert!(r.mapping.is_empty());

    let r = w.run(true);
    assert_eq!(mapped(&w, &r), map_of(&[("old/retagged.mp3", "t1.mp3")]));
    w.card.assert_untouched();
}

// Prevents: same size and duration, with other tags, being taken as the
// same track silently, or being mapped by --accept-proposals when there is
// no evidence at all.
#[test]
fn same_size_and_duration_with_other_tags_is_a_weak_proposal_with_no_default() {
    let mut w = World::empty();
    let lib = retagged(
        1,
        &[
            id3v24_text("TIT2", 3, b"AAAAAA"),
            id3v24_text("TPE1", 3, b"BBBBBB"),
        ],
    );
    w.env.add_bytes("w.mp3", &lib);
    w.env.scan();
    let card = retagged(
        1,
        &[
            id3v24_text("TIT2", 3, b"CCCCCC"),
            id3v24_text("TPE1", 3, b"DDDDDD"),
        ],
    );
    assert_eq!(card.len(), lib.len());
    assert_ne!(card, lib);
    w.card.put("weak.mp3", &card);
    w.card.age();

    for accept in [false, true] {
        let r = w.run(accept);
        let p = proposal(&r, "weak.mp3");
        assert_eq!(p.reason, Reason::WeakTags);
        assert_eq!(p.candidates, [w.lib_id("w.mp3")]);
        assert_eq!(p.default, None);
        assert!(r.mapping.is_empty(), "accept={accept}: {:?}", r.mapping);
        assert_eq!(r.left.len(), 1);
    }
    w.card.assert_untouched();
}

// Prevents: two card copies of one track both mapped to it (the second
// would be taken for the first on the next sync), or both dropped.
#[test]
fn two_identical_card_files_claiming_one_library_file_are_proposals_and_one_wins() {
    let mut w = World::with_library(1);
    w.card.copy("a/first.mp3", Kind::Mp3, 1);
    w.card.copy("b/second.mp3", Kind::Mp3, 1);
    w.card.age();

    let r = w.run(false);
    for rel in ["a/first.mp3", "b/second.mp3"] {
        let p = proposal(&r, rel);
        assert_eq!(p.reason, Reason::SharedWithAnotherDeviceTrack);
        assert_eq!(p.candidates, [w.lib_id("t1.mp3")]);
    }
    assert!(r.mapping.is_empty());

    let r = w.run(true);
    assert_eq!(mapped(&w, &r), map_of(&[("a/first.mp3", "t1.mp3")]));
    assert_eq!(r.left.len(), 1);
    assert_eq!(r.left[0].key, key_of(&r, "b/second.mp3"));
    w.card.assert_untouched();
}

// Prevents: a file with no library counterpart being mapped or proposed
// because of a shared size alone, or being touched.
#[test]
fn a_card_file_with_no_library_file_is_unmanaged() {
    let mut w = World::with_library(2);
    w.card.copy("mine.mp3", Kind::Mp3, 1);
    // Another length, so another size: nothing in the library resembles it.
    w.card.put(
        "stranger.mp3",
        &fs::read(common::fixture_secs(Kind::Mp3, 9, 3)).unwrap(),
    );
    w.card.age();
    let r = w.run(true);
    assert_eq!(mapped(&w, &r), map_of(&[("mine.mp3", "t1.mp3")]));
    assert!(is_unmatched(&r, "stranger.mp3"));
    assert!(r.adoption.proposals.is_empty());
    assert!(unreadable(&r).is_empty(), "{:?}", r.unreadable);
    w.card.assert_untouched();
}

// Prevents: formats fiio-tunes does not copy (opus) cluttering the report as
// unreadable files, or being matched.
#[test]
fn unsupported_formats_stay_unmanaged_without_being_reported_unreadable() {
    let mut w = World::with_library(1);
    w.card.copy("song.opus", Kind::Opus, 2);
    w.card.age();
    let r = w.run(true);
    assert!(r.mapping.is_empty(), "{:?}", r.mapping);
    assert!(is_unmatched(&r, "song.opus"));
    assert_eq!(unreadable(&r), Vec::<&str>::new());
    w.card.assert_untouched();
}

// Prevents (was a bug): the scan skips .wma and the like by extension,
// but adoption sends them to the tag reader, which cannot tell what an ASF
// file is and fails: the file is listed as unreadable instead of staying
// unmanaged silently (section 10: "Unsupported formats stay unmanaged").
#[test]
fn a_wma_file_stays_unmanaged_without_being_reported_unreadable() {
    let mut w = World::with_library(1);
    // The header of an ASF file, which is what a .wma is.
    let mut wma = vec![
        0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE,
        0x6C,
    ];
    wma.extend([0u8; 200]);
    w.card.put("song.wma", &wma);
    let r = w.run(true);
    assert!(is_unmatched(&r, "song.wma"));
    assert_eq!(unreadable(&r), Vec::<&str>::new());
}

// Prevents: a corrupt file silently disappearing from the report, or
// stopping the adoption of the files next to it.
#[test]
fn a_corrupt_file_is_reported_and_the_others_still_match() {
    let mut w = World::with_library(1);
    w.card.copy("good.mp3", Kind::Mp3, 1);
    w.card.put("broken.mp3", &[0x5A; 3001]);
    w.card.age();
    let r = w.run(true);
    assert_eq!(mapped(&w, &r), map_of(&[("good.mp3", "t1.mp3")]));
    assert_eq!(unreadable(&r), ["broken.mp3"]);
    assert!(
        r.mapping
            .iter()
            .all(|&(k, _)| k != key_of(&r, "broken.mp3"))
    );
    let report = adoption::report(&r);
    assert!(report.contains("could not be read"), "{report}");
    assert!(report.contains("broken.mp3"), "{report}");
    w.card.assert_untouched();
}

// Prevents: files without a title tag never matching, since the library
// titles them by file name.
#[test]
fn a_card_file_without_a_title_tag_is_matched_by_its_file_name() {
    let mut w = World::empty();
    // The library file has an artist but no title: its title is "Song One".
    let lib = retagged(1, &[id3v24_text("TPE1", 3, b"Artist1")]);
    w.env.add_bytes("Song One.mp3", &lib);
    w.env.scan();
    // The card file: same audio and artist, a genre more, same file name.
    let card = retagged(
        1,
        &[
            id3v24_text("TPE1", 3, b"Artist1"),
            id3v24_text("TCON", 3, b"Rock"),
        ],
    );
    w.card.put("Old/Song One.mp3", &card);
    w.card.age();
    let r = w.run(true);
    assert_eq!(
        track(&r, "Old/Song One.mp3").title.as_deref(),
        Some("Song One")
    );
    assert_eq!(
        mapped(&w, &r),
        map_of(&[("Old/Song One.mp3", "Song One.mp3")])
    );
    w.card.assert_untouched();
}

// Prevents: a track whose file is gone from the library being the target of
// a card file (the next sync would then expect it on the library side).
#[test]
fn missing_library_tracks_are_never_targets() {
    let mut w = World::empty();
    w.env.add("here.flac", Kind::Flac, 1);
    w.env.add("gone.mp3", Kind::Mp3, 2);
    w.env.scan();
    store::set_missing(w.conn(), w.lib_id("gone.mp3"), Some(1)).unwrap();
    w.card.copy("here.flac", Kind::Flac, 1);
    w.card.copy("gone.mp3", Kind::Mp3, 2);
    w.card.age();
    let r = w.run(true);
    assert_eq!(mapped(&w, &r), map_of(&[("here.flac", "here.flac")]));
    assert!(is_unmatched(&r, "gone.mp3"));
    assert!(r.adoption.proposals.is_empty());
    assert!(!r.library.contains_key(&w.lib_id("gone.mp3")));
    w.card.assert_untouched();
}

// Prevents: adopting against an index that has nothing present, which would
// leave every card file unmanaged and look like success.
#[test]
fn an_empty_index_or_one_with_only_missing_tracks_is_refused() {
    let mut w = World::empty();
    w.card.copy("a.mp3", Kind::Mp3, 1);
    assert!(matches!(w.run_with(KEY, false), Err(Error::EmptyIndex)));

    w.env.add("a.mp3", Kind::Mp3, 1);
    w.env.scan();
    store::set_missing(w.conn(), w.lib_id("a.mp3"), Some(1)).unwrap();
    assert!(matches!(w.run_with(KEY, true), Err(Error::EmptyIndex)));
    assert_eq!(count(w.conn(), "SELECT COUNT(*) FROM devices"), 0);
}

// ---- 2. what is read

// Prevents: the whole card being read in full (a 64 GB card over USB takes
// an hour), instead of the files that could be a library file.
#[test]
fn only_files_whose_size_a_library_file_has_are_hashed() {
    let mut w = World::with_library(2);
    w.card.copy("same size/a.mp3", Kind::Mp3, 1);
    w.card.put(
        "other size.mp3",
        &fs::read(common::fixture_secs(Kind::Mp3, 9, 3)).unwrap(),
    );
    w.card.put("junk.mp3", &[7; 123]);
    w.card.age();
    let r = w.run(false);
    assert!(track(&r, "same size/a.mp3").hash.is_some());
    assert_eq!(
        track(&r, "same size/a.mp3").hash,
        Some(blake(&fixture(Kind::Mp3, 1)))
    );
    assert_eq!(track(&r, "other size.mp3").hash, None);
    assert_eq!(track(&r, "junk.mp3").hash, None);
    assert_eq!(r.hashed, 1);
    w.card.assert_untouched();
}

// Prevents: tags being read from files already proven identical to a
// library file (the slow part of a large card), or not read from the rest.
#[test]
fn tags_are_read_only_from_files_not_identical_to_a_library_file() {
    let mut w = World::with_library(2);
    w.card.copy("identical.mp3", Kind::Mp3, 1);
    w.card.put(
        "other.mp3",
        &fs::read(common::fixture_secs(Kind::Mp3, 9, 3)).unwrap(),
    );
    w.card.age();
    let r = w.run(false);
    let same = track(&r, "identical.mp3");
    assert_eq!((same.title.as_deref(), same.duration_ms), (None, None));
    let other = track(&r, "other.mp3");
    assert_eq!(other.title.as_deref(), Some("Title9"));
    assert_eq!(other.artist.as_deref(), Some("Artist9"));
    assert!(other.duration_ms.is_some_and(|d| d > 2500), "{other:?}");
    w.card.assert_untouched();
}

// Prevents: a mapping recorded without the card file's own hash (the next
// sync would trust a hash it never saw), for files of another size than any
// library file's.
#[test]
fn every_proposed_or_mapped_file_is_hashed_even_when_its_size_is_new() {
    let mut w = World::with_library(1);
    let bytes = retagged(
        1,
        &[
            id3v24_text("TIT2", 3, b"Title1"),
            id3v24_text("TPE1", 3, b"Artist1"),
            id3v24_text("TCON", 3, b"Rock"),
        ],
    );
    w.card.put("retagged.mp3", &bytes);
    w.card.age();
    let lib_size = w.env.track(&w.env.music.join("t1.mp3")).size;
    let r = w.run(false);
    let t = track(&r, "retagged.mp3");
    assert_ne!(t.file.size, lib_size);
    assert_eq!(t.hash, Some(*blake3::hash(&bytes).as_bytes()));
    assert_eq!(r.hashed, 1);
    w.card.assert_untouched();
}

// Prevents: reading the card rewriting access dates (reading 20 GB would
// otherwise write to the card): hashing, tag reading and the manifest are
// all opened with O_NOATIME.
#[test]
fn reading_the_card_leaves_access_times_modification_times_and_bytes_alone() {
    let mut w = World::with_library(3);
    w.card.copy("Music/A/identical.mp3", Kind::Mp3, 1);
    w.card.put(
        "Music/B/retagged.mp3",
        &retagged(
            3,
            &[
                id3v24_text("TIT2", 3, b"Title3"),
                id3v24_text("TPE1", 3, b"Artist3"),
                id3v24_text("TCON", 3, b"Rock"),
            ],
        ),
    );
    w.card.put("Music/B/broken.mp3", &[1; 999]);
    w.card.copy("deep/er/other.mp3", Kind::Mp3, 2);
    w.card.put("list.m3u8", b"#EXTM3U\n");
    w.manifest(KEY, "card-1");
    w.card.age();
    let r = w.run(true);
    assert_eq!(r.mapping.len(), 3, "{:?}", r.mapping);
    w.card.assert_untouched();
}

// ---- 3. saving

fn device_rows(w: &World) -> (Vec<TrackMapping>, Vec<PlaylistMapping>) {
    let d = devices::device(w.conn(), KEY).unwrap().unwrap();
    (
        devices::track_mappings(w.conn(), d.id).unwrap(),
        devices::playlist_mappings(w.conn(), d.id).unwrap(),
    )
}

// Prevents: the library's path, hash or size being recorded for a card file
// that differs from it: the next sync would believe the card holds the
// library's bytes.
#[test]
fn saving_records_the_card_files_own_path_hash_and_size() {
    let mut w = World::with_library(2);
    let bytes = retagged(
        1,
        &[
            id3v24_text("TIT2", 3, b"Title1"),
            id3v24_text("TPE1", 3, b"Artist1"),
            id3v24_text("TCON", 3, b"Rock"),
        ],
    );
    w.card.put("Elsewhere/Retagged.mp3", &bytes);
    w.card.copy("Elsewhere/Same.mp3", Kind::Mp3, 2);
    w.card.age();
    let r = w.run(true);
    adoption::save(w.conn(), &r, Some("My X3"), Some("FiiO X3 2nd gen"), false).unwrap();

    let d = devices::device(w.conn(), KEY).unwrap().unwrap();
    assert_eq!(d.name.as_deref(), Some("My X3"));
    assert_eq!(d.model.as_deref(), Some("FiiO X3 2nd gen"));
    let (tracks, _) = device_rows(&w);
    assert_eq!(
        tracks,
        [
            TrackMapping {
                track_id: w.lib_id("t1.mp3"),
                card_path: "Elsewhere/Retagged.mp3".into(),
                synced_hash: *blake3::hash(&bytes).as_bytes(),
                synced_size: bytes.len() as u64,
            },
            TrackMapping {
                track_id: w.lib_id("t2.mp3"),
                card_path: "Elsewhere/Same.mp3".into(),
                synced_hash: blake(&fixture(Kind::Mp3, 2)),
                synced_size: fs::metadata(fixture(Kind::Mp3, 2)).unwrap().len(),
            },
        ]
    );
    // The card key is the FAT form of the path.
    let key: String = w
        .conn()
        .query_row(
            "SELECT card_key FROM device_tracks WHERE card_path = 'Elsewhere/Same.mp3'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(key, fiio_core::paths::card_key("Elsewhere/Same.mp3"));
    w.card.assert_untouched();
}

// Prevents: playlists mapped under the library's name instead of the card
// file's, or without the key FAT compares by.
#[test]
fn saving_records_playlists_with_the_card_name_and_key() {
    let mut w = World::with_library(1);
    w.load_playlists(&["Jazz"]);
    w.card.put("jazz.m3u8", b"#EXTM3U\n");
    let r = w.run(false);
    adoption::save(w.conn(), &r, None, None, false).unwrap();
    let (_, pls) = device_rows(&w);
    assert_eq!(
        pls,
        [PlaylistMapping {
            playlist_id: w.playlist_id("Jazz"),
            card_name: "jazz.m3u8".into()
        }]
    );
    let key: String = w
        .conn()
        .query_row("SELECT card_key FROM device_playlists", [], |r| r.get(0))
        .unwrap();
    assert_eq!(key, fiio_core::paths::card_key("jazz.m3u8"));
}

// Prevents: proposals the user has not accepted being saved as mappings.
#[test]
fn proposals_not_accepted_are_not_saved() {
    let mut w = World::with_library(1);
    w.card.copy("one.mp3", Kind::Mp3, 1);
    w.card.copy("two.mp3", Kind::Mp3, 1);
    let r = w.run(false);
    assert_eq!(r.adoption.proposals.len(), 2);
    adoption::save(w.conn(), &r, None, None, false).unwrap();
    assert_eq!(device_rows(&w).0, []);
    assert_eq!(count(w.conn(), "SELECT COUNT(*) FROM device_tracks"), 0);
}

// Prevents: a second adoption silently replacing the mappings a sync has
// since built on, and --force failing to replace them when asked.
#[test]
fn an_adopted_device_is_refused_without_force_and_replaced_with_it() {
    let mut w = World::with_library(2);
    w.card.copy("first.mp3", Kind::Mp3, 1);
    let r = w.run(false);
    adoption::save(w.conn(), &r, Some("one"), Some("m1"), false).unwrap();
    let before = device_rows(&w);
    assert_eq!(before.0.len(), 1);

    // The card changed since: another file, the first one gone.
    w.card.remove("first.mp3");
    w.card.copy("second.mp3", Kind::Mp3, 2);
    let r = w.run(false);
    let err = adoption::save(w.conn(), &r, Some("two"), Some("m2"), false).unwrap_err();
    assert!(
        matches!(&err, Error::AlreadyAdopted(k) if k == KEY),
        "{err:?}"
    );
    assert_eq!(device_rows(&w), before, "a refused adoption changed rows");
    let d = devices::device(w.conn(), KEY).unwrap().unwrap();
    assert_eq!(d.name.as_deref(), Some("one"));
    assert_eq!(d.model.as_deref(), Some("m1"));

    adoption::save(w.conn(), &r, Some("two"), Some("m2"), true).unwrap();
    let (tracks, _) = device_rows(&w);
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].card_path, "second.mp3");
    assert_eq!(tracks[0].track_id, w.lib_id("t2.mp3"));
    let d = devices::device(w.conn(), KEY).unwrap().unwrap();
    assert_eq!(d.name.as_deref(), Some("two"));
    assert_eq!(count(w.conn(), "SELECT COUNT(*) FROM devices"), 1);
}

// Prevents: a device with only playlists mapped being taken for a fresh one
// (mappings are tracks and playlists).
#[test]
fn a_device_with_only_playlists_mapped_counts_as_adopted() {
    let mut w = World::with_library(1);
    w.load_playlists(&["Jazz"]);
    w.card.put("Jazz.m3u8", b"#EXTM3U\n");
    let r = w.run(false);
    adoption::save(w.conn(), &r, None, None, false).unwrap();
    assert!(matches!(
        adoption::save(w.conn(), &r, None, None, false),
        Err(Error::AlreadyAdopted(_))
    ));
}

// Prevents: an empty card (nothing to map) locking its device out of a
// second adoption once files are on it.
#[test]
fn an_empty_card_adopts_with_nothing_mapped_and_can_be_adopted_again_without_force() {
    let mut w = World::with_library(1);
    let r = w.run(false);
    assert!(r.tracks.is_empty() && r.mapping.is_empty());
    adoption::save(w.conn(), &r, Some("n"), None, false).unwrap();
    assert!(devices::device(w.conn(), KEY).unwrap().is_some());
    assert_eq!(device_rows(&w), (vec![], vec![]));

    w.card.copy("later.mp3", Kind::Mp3, 1);
    let r = w.run(false);
    adoption::save(w.conn(), &r, Some("n"), None, false).unwrap();
    assert_eq!(device_rows(&w).0.len(), 1);
    assert_eq!(count(w.conn(), "SELECT COUNT(*) FROM devices"), 1);
}

// Prevents: tombstones (deleted playlists still mapped, kept so a sync can
// delete the card file) outliving the last mapping, or one device dropping
// a tombstone another still needs.
#[test]
fn replacing_mappings_drops_tombstones_no_device_maps_any_more() {
    let w = World::with_library(1);
    w.load_playlists(&["P1", "P2", "Live"]);
    let (p1, p2, live) = (
        w.playlist_id("P1"),
        w.playlist_id("P2"),
        w.playlist_id("Live"),
    );
    let a = devices::upsert_device(w.conn(), "usb:A", None, None).unwrap();
    let b = devices::upsert_device(w.conn(), "usb:B", None, None).unwrap();
    let map = |id, name: &str| PlaylistMapping {
        playlist_id: id,
        card_name: name.into(),
    };
    devices::replace_mappings(w.conn(), a, &[], &[map(p1, "P1.m3u8"), map(p2, "P2.m3u8")]).unwrap();
    devices::replace_mappings(w.conn(), b, &[], &[map(p2, "P2.m3u8")]).unwrap();
    store::delete_playlist(w.conn(), "P1").unwrap();
    store::delete_playlist(w.conn(), "P2").unwrap();
    let tombstones = |w: &World| {
        store::removed_playlists(w.conn())
            .unwrap()
            .into_iter()
            .map(|(_, n)| n)
            .collect::<Vec<_>>()
    };
    assert_eq!(tombstones(&w), ["P1", "P2"]);

    devices::replace_mappings(w.conn(), a, &[], &[]).unwrap();
    // P1 is mapped by no one now; P2 still by B; the live playlist, mapped
    // by no one, is not a tombstone and stays.
    assert_eq!(tombstones(&w), ["P2"]);
    assert_eq!(w.env.playlist_id("Live"), Some(live));
    assert_eq!(devices::playlist_mappings(w.conn(), b).unwrap().len(), 1);

    devices::replace_mappings(w.conn(), b, &[], &[]).unwrap();
    assert!(tombstones(&w).is_empty());
}

// Prevents: two card files FAT takes for one both being mapped (the unique
// card key would make the save fail, or the sync would write one over the
// other).
#[test]
fn two_card_files_fat_takes_for_one_are_mapped_once() {
    let mut w = World::with_library(2);
    w.card.copy("Music/A.mp3", Kind::Mp3, 1);
    w.card.copy("Music/a.mp3", Kind::Mp3, 2);
    w.card.age();
    let r = w.run(false);
    assert_eq!(mapped(&w, &r), map_of(&[("Music/A.mp3", "t1.mp3")]));
    assert!(
        unreadable(&r).contains(&"Music/a.mp3"),
        "{:?}",
        r.unreadable
    );
    adoption::save(w.conn(), &r, None, None, false).unwrap();
    let (tracks, _) = device_rows(&w);
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].card_path, "Music/A.mp3");
    w.card.assert_untouched();
}

// ---- 4. identity

// Prevents: an X3 being keyed by anything but its serial, so the same player
// would get a new device row at each mount.
#[test]
fn an_x3_is_keyed_by_its_usb_serial_even_with_a_manifest_and_allow_folder() {
    let id = Identity {
        device_key: "usb:OTHER".into(),
        card_id: "c".into(),
    };
    for allow in [false, true] {
        assert_eq!(
            adoption::device_key(&fiio(Some("ABC123")), None, allow).unwrap(),
            "usb:ABC123"
        );
        assert_eq!(
            adoption::device_key(&fiio(Some("ABC123")), Some(&id), allow).unwrap(),
            "usb:ABC123"
        );
    }
}

// Prevents: a folder, another vendor's player, or an X3 with no serial
// being adopted as an X3 without allow_folder.
#[test]
fn anything_but_an_x3_is_refused_without_allow_folder() {
    let foreign = found_with_usb("0781", Some("S"), "/mnt/stick".into());
    for f in [folder_found("/tmp/card"), foreign.clone(), fiio(None)] {
        let err = adoption::device_key(&f, None, false).unwrap_err();
        assert!(
            matches!(&err, Error::NotAnX3(p) if *p == f.mount.mountpoint),
            "{err:?}"
        );
    }
    // With allow_folder these are folders, whatever USB device they sit on.
    assert_eq!(
        adoption::device_key(&foreign, None, true).unwrap(),
        "folder:/mnt/stick"
    );
}

// Prevents: a test folder getting a new key at each run (losing its
// mappings), or ignoring the key a manifest gave it.
#[test]
fn a_folder_card_takes_its_manifest_key_else_folder_and_its_path() {
    let f = folder_found("/tmp/card");
    assert_eq!(
        adoption::device_key(&f, None, true).unwrap(),
        "folder:/tmp/card"
    );
    let id = Identity {
        device_key: "folder:earlier".into(),
        card_id: "c".into(),
    };
    assert_eq!(
        adoption::device_key(&f, Some(&id), true).unwrap(),
        "folder:earlier"
    );
}

// Prevents: the index losing track of the card's id when it is rebuilt, so
// the first sync would refuse the card as not the one adopted.
#[test]
fn a_manifest_of_the_same_key_gives_its_card_id_to_the_device() {
    let mut w = World::with_library(1);
    w.manifest(KEY, "card-77");
    w.card.copy("a.mp3", Kind::Mp3, 1);
    w.card.age();
    let r = w.run(false);
    assert_eq!(
        r.manifest,
        Some(Identity {
            device_key: KEY.into(),
            card_id: "card-77".into()
        })
    );
    adoption::save(w.conn(), &r, None, None, false).unwrap();
    let d = devices::device(w.conn(), KEY).unwrap().unwrap();
    assert_eq!(d.card_id.as_deref(), Some("card-77"));
    // The manifest file is not a card track, and was not written.
    assert_eq!(r.tracks.len(), 1);
    w.card.assert_untouched();
}

// Prevents: a card_id left by an earlier adoption or sync surviving onto a
// card that no longer has the manifest (a reformatted card would pass the
// identity check of the first sync as the old one).
#[test]
fn no_manifest_clears_the_card_id_an_earlier_adoption_or_sync_left() {
    let mut w = World::with_library(1);
    w.manifest(KEY, "card-1");
    w.card.copy("a.mp3", Kind::Mp3, 1);
    let r = w.run(false);
    adoption::save(w.conn(), &r, None, None, false).unwrap();
    assert_eq!(
        devices::device(w.conn(), KEY)
            .unwrap()
            .unwrap()
            .card_id
            .as_deref(),
        Some("card-1")
    );

    // Card reformatted: files back, no manifest.
    w.card.remove(".fiio-tunes/manifest.json");
    let r = w.run(false);
    assert_eq!(r.manifest, None);
    adoption::save(w.conn(), &r, None, None, true).unwrap();
    let d = devices::device(w.conn(), KEY).unwrap().unwrap();
    assert_eq!(d.card_id, None);

    // A sync set one meanwhile; an adoption of a card without manifest
    // clears it too.
    devices::set_card_id(w.conn(), d.id, Some("from-a-sync")).unwrap();
    adoption::save(w.conn(), &w.run(false), None, None, true).unwrap();
    assert_eq!(
        devices::device(w.conn(), KEY).unwrap().unwrap().card_id,
        None
    );
}

// Prevents: adopting a card that another player's sync made, which would
// map its files to this player.
#[test]
fn a_manifest_of_another_key_refuses_and_saves_nothing() {
    let mut w = World::with_library(1);
    w.manifest("usb:SOMEONE-ELSE", "card-9");
    w.card.copy("a.mp3", Kind::Mp3, 1);
    w.card.age();
    let err = w.run_with("usb:MINE", true).err().expect("refused");
    assert!(
        matches!(&err, Error::ForeignManifest { found, key }
            if found == "usb:SOMEONE-ELSE" && key == "usb:MINE"),
        "{err:?}"
    );
    assert_eq!(count(w.conn(), "SELECT COUNT(*) FROM devices"), 0);
    assert_eq!(count(w.conn(), "SELECT COUNT(*) FROM device_tracks"), 0);
    w.card.assert_untouched();
}

// Prevents: a damaged manifest being read as "no manifest", after which the
// first sync would give the card a new identity over it.
#[test]
fn a_manifest_that_does_not_parse_refuses() {
    let mut w = World::with_library(1);
    for bad in [&b"{"[..], b"", br#"{"device_key": "folder:test"}"#, b"[]"] {
        w.card.put(".fiio-tunes/manifest.json", bad);
        let err = w.run_with(KEY, false).err().expect("refused");
        assert!(matches!(err, Error::BadManifest(..)), "{bad:?}: {err:?}");
    }
    assert_eq!(count(w.conn(), "SELECT COUNT(*) FROM devices"), 0);
}

// ---- 5. playlists

// Prevents: card playlists matched to the wrong library playlist, or
// matched twice, by any of the three name passes.
#[test]
fn playlists_match_exactly_without_case_and_with_file_safe_names() {
    let mut w = World::with_library(1);
    // 'Dance:Trance' is a legal library name on Linux; the card has the
    // file-safe form of it.
    w.load_playlists(&["Rock", "Jazz", "Dance:Trance", "Only Here"]);
    for n in ["Rock.m3u8", "jazz.m3u8", "Dance_Trance.m3u8", "Gone.m3u8"] {
        w.card.put(n, b"#EXTM3U\n");
    }
    // Other cards' sub folders are not the playlists folder.
    w.card.put("Sub/Only Here.m3u8", b"#EXTM3U\n");
    w.card.age();
    let r = w.run(false);
    let got: Vec<(&str, &str)> = r
        .playlists
        .matched
        .iter()
        .map(|m| (m.card_name.as_str(), m.library_name.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("Dance_Trance.m3u8", "Dance:Trance"),
            ("Rock.m3u8", "Rock"),
            ("jazz.m3u8", "Jazz"),
        ]
    );
    for m in &r.playlists.matched {
        assert_eq!(m.playlist_id, w.playlist_id(&m.library_name));
    }
    assert_eq!(r.playlists.unmatched_card, ["Gone.m3u8"]);
    assert_eq!(r.playlists.unmatched_library, ["Only Here"]);
    w.card.assert_untouched();
}

// Prevents: one library playlist being claimed by two card files, and the
// stricter pass losing to the looser one.
#[test]
fn each_library_playlist_is_matched_once_and_the_exact_name_wins() {
    let mut w = World::with_library(1);
    w.load_playlists(&["Jazz"]);
    // Listed in this order, "jazz" would come first in a single loose pass.
    w.card.put("jazz.m3u8", b"#EXTM3U\n");
    w.card.put("Jazz.m3u8", b"#EXTM3U\n");
    let r = w.run(false);
    assert_eq!(r.playlists.matched.len(), 1);
    assert_eq!(r.playlists.matched[0].card_name, "Jazz.m3u8");
    assert_eq!(r.playlists.unmatched_card, ["jazz.m3u8"]);
    assert!(r.playlists.unmatched_library.is_empty());
}

// Prevents: playlists elsewhere on the card being adopted (a sync would
// then write the library's playlist over them), or the configured folder
// being compared case-sensitively where FAT is not.
#[test]
fn only_playlists_in_the_playlists_folder_are_matched() {
    let mut w = World::with_library(1);
    w.load_playlists(&["Rock", "Jazz"]);
    w.card.put("Rock.m3u8", b"#EXTM3U\n");
    w.card.put("Playlists/Jazz.m3u8", b"#EXTM3U\n");
    w.card.put("Playlists/Sub/Rock.m3u8", b"#EXTM3U\n");
    // Default: the card root.
    let r = w.run(false);
    let names: Vec<_> = r
        .playlists
        .matched
        .iter()
        .map(|m| &m.card_name[..])
        .collect();
    assert_eq!(names, ["Rock.m3u8"]);
    assert_eq!(r.playlists.unmatched_library, ["Jazz"]);

    w.env.cfg.card.playlists_dir = "playlists".into();
    let r = w.run(false);
    let names: Vec<_> = r
        .playlists
        .matched
        .iter()
        .map(|m| &m.card_name[..])
        .collect();
    assert_eq!(names, ["Jazz.m3u8"]);
    assert_eq!(r.playlists.unmatched_library, ["Rock"]);
    assert!(r.playlists.unmatched_card.is_empty(), "{:?}", r.playlists);
}

// Prevents: two card playlists FAT takes for one, matched to two library
// playlists, making the save fail on the unique card key after the device
// row was written.
#[test]
fn card_playlists_fat_takes_for_one_do_not_break_the_save() {
    let mut w = World::with_library(1);
    // Two library playlists differing only by case, which Linux allows.
    w.load_playlists(&["Rock", "rock"]);
    w.card.put("Rock.m3u8", b"#EXTM3U\n");
    w.card.put("rock.m3u8", b"#EXTM3U\n");
    let r = w.run(false);
    let matched: Vec<_> = r
        .playlists
        .matched
        .iter()
        .map(|m| (&m.card_name[..], &m.library_name[..]))
        .collect();
    assert_eq!(matched, [("Rock.m3u8", "Rock")]);
    assert_eq!(r.playlists.unmatched_card, ["rock.m3u8"]);
    assert_eq!(r.playlists.unmatched_library, ["rock"]);
    let saved = adoption::save(w.conn(), &r, None, None, false);
    assert!(saved.is_ok(), "{saved:?}");
    let (_, pls) = device_rows(&w);
    let keys: std::collections::HashSet<_> = pls
        .iter()
        .map(|p| fiio_core::paths::card_key(&p.card_name))
        .collect();
    assert_eq!(keys.len(), pls.len());
}

// ---- 6. the report

// Prevents: the report promised to the user lacking a section.
#[test]
fn the_report_has_every_section() {
    let mut w = World::with_library(2);
    w.load_playlists(&["Rock", "Lib only"]);
    w.card.copy("a.mp3", Kind::Mp3, 1);
    w.card.copy("dup.mp3", Kind::Mp3, 1);
    w.card.put("junk.mp3", &[1; 77]);
    w.card.put("Rock.m3u8", b"#EXTM3U\n");
    w.card.put("Card only.m3u8", b"#EXTM3U\n");
    let r = w.run(false);
    let report = adoption::report(&r);
    for section in [
        "device: folder:test",
        "== Proposals (2)",
        "== Card files with no library file (1)",
        "== Card files that could not be read (1)",
        "== Playlists matched (1)",
        "== Card playlists with no library playlist (1)",
        "== Library playlists not on the card (1)",
    ] {
        assert!(report.contains(section), "missing {section:?} in\n{report}");
    }
    // The proposals name both sides.
    assert!(
        report.contains("a.mp3") && report.contains("dup.mp3"),
        "{report}"
    );
    assert!(report.contains("t1.mp3"), "{report}");
    assert!(report.contains("Rock.m3u8 = Rock"), "{report}");
    assert!(report.contains("Card only.m3u8"), "{report}");
    assert!(report.contains("Lib only"), "{report}");
}

// ---- follow-up: fixes after review

// Prevents: a file in a format the scan does not index being hashed (it has
// the size of a library file here), read for tags, matched or proposed.
#[test]
fn unsupported_extensions_are_skipped_even_with_a_library_files_bytes() {
    let mut w = World::with_library(1);
    for name in ["a.wma", "B.WMA", "dir/c.dsf", "d.opus"] {
        w.card.copy(name, Kind::Mp3, 1);
    }
    w.card.age();
    let r = w.run(true);
    for name in ["a.wma", "B.WMA", "dir/c.dsf", "d.opus"] {
        let t = track(&r, name);
        assert_eq!(t.hash, None, "{name} was hashed");
        assert_eq!(t.title, None, "{name} had its tags read");
        assert!(is_unmatched(&r, name), "{name}");
    }
    assert_eq!(r.hashed, 0);
    assert!(r.mapping.is_empty() && r.adoption.proposals.is_empty());
    assert!(r.unreadable.is_empty(), "{:?}", r.unreadable);
    w.card.assert_untouched();
}

// Prevents: the recorded size being the listing's, which a file that grew
// since the listing no longer has (the next sync would compare it to bytes
// that were not hashed). The progress callback, called after each file,
// appends to the second file once the first is done.
#[test]
fn the_recorded_size_is_the_number_of_bytes_hashed() {
    let mut w = World::with_library(2);
    w.card.copy("a.mp3", Kind::Mp3, 1);
    w.card.copy("b.mp3", Kind::Mp3, 2);
    let path = w.card.root.join("b.mp3");
    let listed = fs::metadata(&path).unwrap().len();
    let grow = |done: usize, _: usize| {
        if done == 1 {
            use std::io::Write;
            let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&[0u8; 100]).unwrap();
        }
    };
    let r = adoption::run(w.conn(), &w.env.cfg, &w.card.root, KEY, true, &grow).unwrap();
    let on_disk = fs::read(&path).unwrap();
    assert_eq!(on_disk.len() as u64, listed + 100);
    assert_eq!(
        r.contents.audio[1].size, listed,
        "the listing is the old one"
    );
    adoption::save(w.conn(), &r, None, None, false).unwrap();
    let (tracks, _) = device_rows(&w);
    assert_eq!(tracks.len(), 2, "{:?}", r.adoption);
    let b = tracks.iter().find(|t| t.card_path == "b.mp3").unwrap();
    assert_eq!(b.synced_size, on_disk.len() as u64);
    assert_eq!(b.synced_hash, *blake3::hash(&on_disk).as_bytes());
}

// Prevents: a file that cannot be opened being listed twice in the report
// (once for the hash, once for the tags).
#[test]
fn a_size_matching_file_that_cannot_be_opened_is_listed_once() {
    use std::os::unix::fs::PermissionsExt;
    let mut w = World::with_library(1);
    w.card.copy("locked.mp3", Kind::Mp3, 1);
    let p = w.card.root.join("locked.mp3");
    fs::set_permissions(&p, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::File::open(&p).is_ok() {
        eprintln!(
            "SKIPPED: permissions do not stop this user (running as root?); this test checked nothing"
        );
        return;
    }
    let r = w.run(true);
    fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(unreadable(&r), ["locked.mp3"], "{:?}", r.unreadable);
    assert!(r.mapping.is_empty());
}

// Prevents: a later adoption that passes no name or model (a card seen
// without USB details) erasing the ones saved, and one that passes values
// failing to update them.
#[test]
fn upsert_device_keeps_name_and_model_when_none_and_updates_them_when_given() {
    let w = World::empty();
    let id = devices::upsert_device(w.conn(), KEY, Some("X3"), Some("FiiO X3")).unwrap();
    assert_eq!(
        devices::upsert_device(w.conn(), KEY, None, None).unwrap(),
        id
    );
    let d = devices::device(w.conn(), KEY).unwrap().unwrap();
    assert_eq!(
        (d.name.as_deref(), d.model.as_deref()),
        (Some("X3"), Some("FiiO X3"))
    );
    devices::upsert_device(w.conn(), KEY, Some("New"), None).unwrap();
    let d = devices::device(w.conn(), KEY).unwrap().unwrap();
    assert_eq!(
        (d.name.as_deref(), d.model.as_deref()),
        (Some("New"), Some("FiiO X3"))
    );
    devices::upsert_device(w.conn(), KEY, None, Some("M2")).unwrap();
    let d = devices::device(w.conn(), KEY).unwrap().unwrap();
    assert_eq!(
        (d.name.as_deref(), d.model.as_deref()),
        (Some("New"), Some("M2"))
    );
}
