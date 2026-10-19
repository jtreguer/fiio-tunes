//! `tags::read` on fixtures built from scratch: ID3v2 tags assembled byte by
//! byte over ffmpeg-generated audio, and an AAC file patched to look FairPlay
//! protected. No real music is used.

mod common;

use std::fs;
use std::path::PathBuf;

use common::{
    Kind, fairplay_m4a, fixture, id3v2_tag, id3v22_title, id3v23_title, id3v24_text, mp3_audio,
    mp3_with_id3v2,
};
use fiio_core::tags::{self, FileKind, FileTags, ReadError, Tags};

fn put(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.path().join(name);
    fs::write(&p, bytes).unwrap();
    p
}

fn title(t: &str) -> Vec<u8> {
    id3v24_text("TIT2", 3, t.as_bytes())
}

/// A TDRC (recording date) frame lofty's default mode rejects and Relaxed skips.
fn bad_date() -> Vec<u8> {
    id3v24_text("TDRC", 3, b"notadate")
}

/// A TRCK frame in UTF-16 with a BOM that is neither FF FE nor FE FF. Even
/// Relaxed mode cannot parse the tag.
fn bad_bom_track() -> Vec<u8> {
    id3v24_text("TRCK", 1, &[0x31, 0x00, 0x32, 0x00])
}

fn read_ok(p: &std::path::Path) -> FileTags {
    tags::read(p).unwrap_or_else(|e| panic!("read failed: {e:?}"))
}

#[test]
fn clean_mp3_reports_no_tag_error_and_no_drm() {
    let d = tempfile::tempdir().unwrap();
    let r = read_ok(&put(&d, "a.mp3", &mp3_with_id3v2(1, &[title("Plain")])));
    assert_eq!(r.info.kind, FileKind::Mp3);
    assert_eq!(r.tags.title.as_deref(), Some("Plain"));
    assert_eq!(r.tag_error, None);
    assert!(!r.drm);
}

// Prevents: a frame lofty's default mode rejects ("failed to parse Mpeg file")
// making a valid MP3 unreadable.
#[test]
fn bad_frame_is_skipped_and_the_other_tags_survive() {
    let d = tempfile::tempdir().unwrap();
    let bytes = mp3_with_id3v2(
        1,
        &[
            title("Survivor"),
            id3v24_text("TPE1", 3, b"Some Artist"),
            bad_date(),
        ],
    );
    let p = put(&d, "a.mp3", &bytes);

    // Guard: lofty's default mode really does reject this file, so the test
    // is about the Relaxed fallback and not about a fixture lofty accepts.
    assert!(
        lofty::probe::Probe::open(&p)
            .unwrap()
            .guess_file_type()
            .unwrap()
            .read()
            .is_err(),
        "fixture must fail in the default parsing mode"
    );

    let r = read_ok(&p);
    assert_eq!(r.info.kind, FileKind::Mp3);
    assert_eq!(r.tags.title.as_deref(), Some("Survivor"));
    assert_eq!(r.tags.artist.as_deref(), Some("Some Artist"));
    assert_eq!(r.tags.year, None, "the rejected frame is dropped");
    assert_eq!(
        r.tag_error, None,
        "Relaxed parsing succeeded, tags are real"
    );
}

// Prevents: a tag that cannot be parsed at all hiding a playable file.
#[test]
fn unparseable_tag_gives_empty_tags_an_error_and_real_audio_info() {
    let d = tempfile::tempdir().unwrap();
    let p = put(
        &d,
        "a.mp3",
        &mp3_with_id3v2(1, &[title("Lost"), bad_bom_track()]),
    );
    let clean = read_ok(&put(&d, "clean.mp3", &mp3_with_id3v2(1, &[title("x")])));

    let r = read_ok(&p);
    assert_eq!(r.tags, Tags::default(), "nothing half-parsed is kept");
    let why = r.tag_error.expect("tag_error must be set");
    assert!(!why.is_empty());
    assert_eq!(r.info.kind, FileKind::Mp3);
    assert!(
        r.info.duration_ms.is_some_and(|ms| ms > 900),
        "{:?}",
        r.info
    );
    assert_eq!(r.info, clean.info, "same audio, same properties");
    assert!(!r.drm);
}

#[test]
fn garbage_is_still_unreadable() {
    let d = tempfile::tempdir().unwrap();
    let cases: [(&str, Vec<u8>); 5] = [
        ("text.mp3", b"this is not audio at all, just text".to_vec()),
        ("empty.mp3", Vec::new()),
        (
            "junk.m4a",
            b"\0\0\0\x18ftypM4A \0\0\0\0garbage after".to_vec(),
        ),
        // A valid-looking tag header followed by nothing usable.
        ("tagonly.mp3", {
            let mut v = mp3_with_id3v2(1, &[bad_bom_track()]);
            v.truncate(10 + 14);
            v
        }),
        (
            "noise.mp3",
            (0..4096u32).map(|i| (i * 7 + 13) as u8).collect(),
        ),
    ];
    for (name, bytes) in cases {
        let p = put(&d, name, &bytes);
        match tags::read(&p) {
            Err(ReadError::Unreadable(msg)) => assert!(!msg.is_empty(), "{name}"),
            other => panic!("{name}: expected Unreadable, got {other:?}"),
        }
    }
}

#[test]
fn fairplay_m4a_is_aac_and_flagged() {
    let d = tempfile::tempdir().unwrap();
    let plain = read_ok(&fixture(Kind::Aac, 1));
    let p = put(&d, "store.m4a", &fairplay_m4a(1));

    let r = read_ok(&p);
    assert_eq!(r.info.kind, FileKind::Aac);
    assert!(r.drm);
    assert_eq!(r.tag_error, None);
    assert_eq!(r.tags.title.as_deref(), Some("Title1"), "tags still read");
    assert!(
        r.info.duration_ms.is_some_and(|ms| ms > 900),
        "{:?}",
        r.info
    );
    assert!(!plain.drm, "an ordinary AAC file is not flagged");
    assert_eq!(plain.info.kind, FileKind::Aac);
}

#[test]
fn ordinary_and_unsupported_m4a_are_not_mistaken_for_fairplay() {
    assert!(!read_ok(&fixture(Kind::Alac, 1)).drm);
    assert_eq!(read_ok(&fixture(Kind::Alac, 1)).info.kind, FileKind::Alac);
    // Opus and FLAC in MP4 have no recognised codec and are not protected: they
    // must stay unsupported rather than be swept up by the FairPlay rule.
    for k in [Kind::OpusInM4a, Kind::FlacInM4a] {
        match tags::read(&fixture(k, 1)) {
            Err(ReadError::Unsupported(_)) => {}
            other => panic!("{k:?}: expected Unsupported, got {other:?}"),
        }
    }
}

// ---- multi-value text tags (audit M15)

fn mp3_tags(frames: &[Vec<u8>]) -> Tags {
    let d = tempfile::tempdir().unwrap();
    read_ok(&put(&d, "a.mp3", &mp3_with_id3v2(1, frames))).tags
}

// Prevents: ID3v2.4 frames holding several NUL-separated values shown cut to
// their first value.
#[test]
fn nul_separated_values_of_every_text_tag_are_joined_with_a_slash() {
    let t = mp3_tags(&[
        id3v24_text("TIT2", 3, b"One\0Two"),
        id3v24_text("TPE1", 3, b"Artist A\0Artist B"),
        id3v24_text("TALB", 3, b"Album\0Second"),
        id3v24_text("TPE2", 3, b"Band\0Guest"),
        id3v24_text("TCOM", 3, b"Writer 1\0Writer 2\0Writer 3"),
        id3v24_text("TCON", 3, b"Rock\0Pop"),
    ]);
    assert_eq!(t.title.as_deref(), Some("One/Two"));
    assert_eq!(t.artist.as_deref(), Some("Artist A/Artist B"));
    assert_eq!(t.album.as_deref(), Some("Album/Second"));
    assert_eq!(t.album_artist.as_deref(), Some("Band/Guest"));
    assert_eq!(t.composer.as_deref(), Some("Writer 1/Writer 2/Writer 3"));
    assert_eq!(t.genre.as_deref(), Some("Rock/Pop"));
}

#[test]
fn a_single_value_is_unchanged_and_the_join_changes_the_tag_hash() {
    let one = mp3_tags(&[id3v24_text("TPE1", 3, b"Artist A")]);
    let two = mp3_tags(&[id3v24_text("TPE1", 3, b"Artist A\0Artist B")]);
    assert_eq!(one.artist.as_deref(), Some("Artist A"));
    assert_ne!(one.hash(), two.hash());
}

// Prevents: blanks and empty values leaking into the joined string ("A//B",
// "A/ ") or a blank-only tag counting as a value.
#[test]
fn values_are_trimmed_and_empty_ones_dropped() {
    let t = mp3_tags(&[
        id3v24_text("TPE1", 3, b"  Artist A \0\0 \0Artist B  "),
        id3v24_text("TALB", 3, b"  Padded  "),
        id3v24_text("TCOM", 3, b"   \0  "),
        id3v24_text("TCON", 3, b"\0Rock"),
    ]);
    assert_eq!(t.artist.as_deref(), Some("Artist A/Artist B"));
    assert_eq!(t.album.as_deref(), Some("Padded"), "stored trimmed");
    assert_eq!(t.composer, None, "only blanks is no value");
    assert_eq!(t.genre.as_deref(), Some("Rock"));
}

// A numeric genre of ID3v1 style still names the genre.
#[test]
fn a_numeric_genre_still_maps_to_its_name() {
    let t = mp3_tags(&[id3v24_text("TCON", 3, b"(17)")]);
    assert_eq!(t.genre.as_deref(), Some("Rock"));
}

// An MP4 atom holding two values is the other multi-value case.
#[test]
fn an_m4a_atom_with_two_values_is_joined() {
    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt;
    use lofty::prelude::{ItemKey, TagExt};
    use lofty::tag::{ItemValue, TagItem};
    let d = tempfile::tempdir().unwrap();
    let p = put(&d, "a.m4a", &fs::read(fixture(Kind::Aac, 1)).unwrap());
    let mut f = lofty::read_from_path(&p).unwrap();
    let tag = f.primary_tag_mut().expect("the fixture has an ilst");
    tag.remove_key(ItemKey::TrackArtist);
    for a in ["Artist A", "Artist B"] {
        tag.push(TagItem::new(
            ItemKey::TrackArtist,
            ItemValue::Text(a.to_owned()),
        ));
    }
    tag.save_to_path(&p, WriteOptions::default()).unwrap();
    let r = read_ok(&p);
    assert_eq!(r.info.kind, FileKind::Aac);
    assert_eq!(r.tags.artist.as_deref(), Some("Artist A/Artist B"));
    assert_eq!(r.tags.title.as_deref(), Some("Title1"));
}

// Prevents: lofty reading a genre "(17)Rock" as "Rock" twice and the join
// showing "Rock/Rock".
#[test]
fn a_numeric_genre_followed_by_its_name_is_not_repeated() {
    let t = mp3_tags(&[id3v24_text("TCON", 3, b"(17)Rock")]);
    assert_eq!(t.genre.as_deref(), Some("Rock"));
}

#[test]
fn two_numeric_genres_give_two_names() {
    let t = mp3_tags(&[id3v24_text("TCON", 3, b"(17)(13)")]);
    assert_eq!(t.genre.as_deref(), Some("Rock/Pop"));
}

#[test]
fn repeated_values_differing_only_in_case_are_kept_once_the_first_spelling_wins() {
    let t = mp3_tags(&[
        id3v24_text("TPE1", 3, b"A\0a\0B"),
        id3v24_text("TALB", 3, b"b\0B\0a"),
    ]);
    assert_eq!(t.artist.as_deref(), Some("A/B"));
    assert_eq!(t.album.as_deref(), Some("b/a"));
}

// ---- MPEG frames far from the start (lofty's content guess gives up after
// 1024 bytes of junk; the extension is the fallback)

/// What the encoder of some real files left in front of the audio.
fn ffmpeg_banner() -> Vec<u8> {
    let mut v = b" \nffmpeg version git-2016-01-01 Copyright (c) 2000-2016 the FFmpeg developers\n"
        .to_vec();
    while v.len() < 1360 {
        v.extend(b"  configuration: --enable-gpl --enable-libmp3lame --enable-version3\n");
    }
    v.truncate(1360);
    v
}

/// The duration of the plain fixture, for the duration check.
fn plain_duration_ms() -> u32 {
    read_ok(&fixture(Kind::Mp3, 1))
        .info
        .duration_ms
        .expect("fixture has a duration")
}

/// Layouts of valid MP3s whose first frame is out of the content guess's reach.
/// Each tuple is a name and the whole file; the first tag's title is "First".
fn far_frame_layouts() -> Vec<(&'static str, Vec<u8>)> {
    let audio = mp3_audio(1);
    let first = id3v2_tag(4, &title("First"));
    let build = |parts: &[&[u8]]| -> Vec<u8> { [parts.concat(), audio.clone()].concat() };
    vec![
        // 1. padding beyond the declared tag size
        ("padding.mp3", build(&[&first, &[0u8; 1044]])),
        // 2. encoder text after the tag
        ("banner_after_tag.mp3", build(&[&first, &ffmpeg_banner()])),
        // 3. no tag at all, the text first
        ("banner_only.mp3", build(&[&ffmpeg_banner()])),
        // 4. v2.3 tag, a stray byte, a second ID3 tag
        (
            "stray_byte_second_tag.mp3",
            build(&[
                &id3v2_tag(3, &id3v23_title("First")),
                &[0x20],
                &id3v2_tag(3, &[0u8; 2049 - 11]),
            ]),
        ),
        // 5. v2.2 tag, a UTF-8 BOM, then a v2.3 tag of about 4 KiB
        (
            "bom_second_tag.mp3",
            build(&[
                &id3v2_tag(2, &id3v22_title("First")),
                &[0xEF, 0xBB, 0xBF],
                &id3v2_tag(3, &[0u8; 4089]),
            ]),
        ),
        // 6. two tags back to back, the frame sync exactly at the 1024 limit
        (
            "two_tags_at_limit.mp3",
            build(&[
                &id3v2_tag(3, &id3v23_title("First")),
                &id3v2_tag(4, &[0u8; 1014]),
            ]),
        ),
    ]
}

// Prevents: valid MP3s with junk between the tag and the first frame being
// dropped by the scan as unreadable (22 files in the real library).
#[test]
fn mp3_with_junk_before_the_first_frame_is_read_by_extension() {
    let d = tempfile::tempdir().unwrap();
    let want = plain_duration_ms();
    for (name, bytes) in far_frame_layouts() {
        let r = tags::read(&put(&d, name, &bytes))
            .unwrap_or_else(|e| panic!("{name}: read failed: {e:?}"));
        assert_eq!(r.info.kind, FileKind::Mp3, "{name}");
        let ms = r.info.duration_ms.unwrap_or(0);
        assert!(ms.abs_diff(want) < 200, "{name}: {ms} ms, want ~{want}");
        assert_eq!(r.tag_error, None, "{name}");
        // Layout 3 has no tag; every other keeps the first tag's title.
        let expect = (name != "banner_only.mp3").then_some("First");
        assert_eq!(r.tags.title.as_deref(), expect, "{name}");
    }
}

// Prevents: the extension fallback missing card files read in adoption.
#[test]
fn read_with_uses_the_extension_fallback_too() {
    let d = tempfile::tempdir().unwrap();
    let (name, bytes) = far_frame_layouts().remove(0);
    let p = put(&d, name, &bytes);
    let opened = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = opened.clone();
    let r = tags::read_with(&p, &move |p: &std::path::Path| {
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        fs::File::open(p)
    })
    .unwrap_or_else(|e| panic!("read_with failed: {e:?}"));
    assert!(
        opened.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "the given opener must be used"
    );
    assert_eq!(r.info.kind, FileKind::Mp3);
    assert_eq!(r.tags.title.as_deref(), Some("First"));
}

// Prevents: the extension fallback overriding what the content says.
#[test]
fn the_content_wins_over_a_misleading_extension() {
    let d = tempfile::tempdir().unwrap();
    let flac = fs::read(fixture(Kind::Flac, 1)).unwrap();
    let r = read_ok(&put(&d, "really_flac.mp3", &flac));
    assert_eq!(r.info.kind, FileKind::Flac);
    // And the reverse: an MP3 named .flac is still an MP3.
    let mp3 = fs::read(fixture(Kind::Mp3, 1)).unwrap();
    let r = read_ok(&put(&d, "really_mp3.flac", &mp3));
    assert_eq!(r.info.kind, FileKind::Mp3);
}

// Prevents: the extension fallback handing junk to the reader of another
// format and having it accepted.
#[test]
fn junk_under_every_other_audio_extension_is_still_unreadable() {
    let d = tempfile::tempdir().unwrap();
    let junk: [(&str, Vec<u8>); 2] = [("text", ffmpeg_banner()), ("zeros", vec![0; 4096])];
    for ext in ["m4a", "wav", "aif", "aiff", "flac", "ogg", "oga", "ape"] {
        for (what, bytes) in &junk {
            let p = put(&d, &format!("{what}.{ext}"), bytes);
            match tags::read(&p) {
                Err(ReadError::Unreadable(_) | ReadError::Unsupported(_)) => {}
                other => panic!("{what}.{ext}: expected an error, got {other:?}"),
            }
        }
    }
}

// Prevents: the extension making any file with a music extension readable.
#[test]
fn lots_of_text_and_no_frames_is_still_unreadable() {
    let d = tempfile::tempdir().unwrap();
    let mut text = ffmpeg_banner();
    text.extend(ffmpeg_banner());
    text.extend(ffmpeg_banner());
    let mut tagged = id3v2_tag(4, &title("Only a tag"));
    tagged.extend(&text);
    for (name, bytes) in [("text.mp3", text), ("tag_and_text.mp3", tagged)] {
        match tags::read(&put(&d, name, &bytes)) {
            Err(ReadError::Unreadable(msg)) => assert!(!msg.is_empty(), "{name}"),
            other => panic!("{name}: expected Unreadable, got {other:?}"),
        }
    }
}
