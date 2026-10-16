//! `manifest.rs` as F5 extended it: the manifest the executor writes and
//! reads back, its removal, the card id and the hex of hashes. Cards are
//! temp folders.

use std::fs;

use fiio_core::Error;
use fiio_core::manifest::{self, Manifest, ManifestTrack};

fn sample() -> Manifest {
    Manifest {
        version: 1,
        device_key: "usb:1".into(),
        card_id: "ab".repeat(16),
        written_at: 1_792_143_045,
        tracks: vec![ManifestTrack {
            path: "Music/\u{e9}.mp3".into(),
            size: 12,
            hash: manifest::hex(&[0x0f; 32]),
        }],
        playlists: vec!["Rock.m3u8".into(), "Playlists/Pop.m3u8".into()],
    }
}

// Prevents: the manifest changing on its way to the card and back: the
// identity read from it is the one written, the bytes are whole JSON with
// every field, a second write replaces the first, and `raw` gives the bytes
// on the card.
#[test]
fn a_written_manifest_reads_back() {
    let card = tempfile::tempdir().unwrap();
    assert_eq!(manifest::raw(card.path()).unwrap(), None);
    let m = sample();
    manifest::write(card.path(), &m).unwrap();
    let bytes = manifest::raw(card.path()).unwrap().unwrap();
    assert_eq!(bytes, fs::read(manifest::path(card.path())).unwrap());
    let back: Manifest = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(back, m);
    let id = manifest::identity(card.path()).unwrap().unwrap();
    assert_eq!(
        (id.device_key.as_str(), id.card_id.as_str()),
        ("usb:1", m.card_id.as_str())
    );

    let mut m2 = sample();
    m2.card_id = "cd".repeat(16);
    m2.playlists.clear();
    manifest::write(card.path(), &m2).unwrap();
    let back: Manifest =
        serde_json::from_slice(&manifest::raw(card.path()).unwrap().unwrap()).unwrap();
    assert_eq!(back, m2);
    // Only the manifest in its folder: no temp file left.
    let names: Vec<_> = fs::read_dir(card.path().join(".fiio-tunes"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["manifest.json"]);
}

// Prevents: raw bytes that are not text being written as a manifest (the
// restore would plant garbage on a card), and a failed write leaving a file.
#[test]
fn write_raw_refuses_bytes_that_are_not_utf8_and_writes_nothing() {
    let card = tempfile::tempdir().unwrap();
    assert!(matches!(
        manifest::write_raw(card.path(), &[0xff, 0xfe, 0x00]),
        Err(Error::BadManifest(..))
    ));
    assert_eq!(manifest::raw(card.path()).unwrap(), None);
    manifest::write_raw(card.path(), b"{\"x\": 1}").unwrap();
    assert_eq!(manifest::raw(card.path()).unwrap().unwrap(), b"{\"x\": 1}");
}

// Prevents: remove failing on a card with no manifest (a restore of a state
// without one), or leaving the file.
#[test]
fn remove_deletes_the_manifest_and_is_idempotent() {
    let card = tempfile::tempdir().unwrap();
    manifest::remove(card.path()).unwrap();
    manifest::write(card.path(), &sample()).unwrap();
    manifest::remove(card.path()).unwrap();
    assert_eq!(manifest::raw(card.path()).unwrap(), None);
    assert_eq!(manifest::identity(card.path()).unwrap(), None);
    manifest::remove(card.path()).unwrap();
}

// Prevents: a card id of the wrong shape (the kernel's uuid with dashes),
// or the same id twice: 32 lowercase hex digits, new each time.
#[test]
fn card_ids_are_32_hex_digits_and_differ() {
    let a = manifest::new_card_id().unwrap();
    let b = manifest::new_card_id().unwrap();
    for id in [&a, &b] {
        assert_eq!(id.len(), 32, "{id}");
        assert!(
            id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
            "{id}"
        );
    }
    assert_ne!(a, b);
}

// Prevents: hashes mangled on the way into a manifest or a backup: hex is
// lowercase and 64 digits, and reads back exactly; anything else (wrong
// length, not hex, not ASCII) is None and never a panic.
#[test]
fn hex_and_unhex_round_trip_and_refuse_anything_else() {
    let mut h = [0u8; 32];
    for (i, b) in h.iter_mut().enumerate() {
        *b = (i * 8 + 3) as u8;
    }
    let text = manifest::hex(&h);
    assert_eq!(text.len(), 64);
    assert!(text.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
    assert!(text.starts_with("03") && text.ends_with("fb"), "{text}");
    assert_eq!(manifest::unhex(&text), Some(h));
    assert_eq!(manifest::unhex(&text.to_uppercase()), Some(h));
    assert_eq!(manifest::hex(&[0; 32]), "0".repeat(64));
    assert_eq!(manifest::hex(&[255; 32]), "f".repeat(64));
    for bad in [
        "",
        &text[..62],
        &format!("{text}00"),
        &"zz".repeat(32),
        &"\u{e9}".repeat(32),
        &"0".repeat(63),
        // 64 bytes with a multi-byte character straddling a pair: slicing
        // it would panic.
        &format!("a{}a", "\u{e9}".repeat(31)),
    ] {
        assert_eq!(manifest::unhex(bad), None, "{bad:?}");
    }
}

// BUG (reported, not fixed): `u8::from_str_radix` accepts a leading plus
// sign, so a damaged backup whose hash is "+1" repeated reads as a valid
// hash instead of being refused as bad.
#[test]
fn unhex_refuses_a_plus_sign() {
    assert_eq!(manifest::unhex(&"+1".repeat(32)), None);
}

// Prevents: a removed manifest leaving its empty folder behind (the card is
// then not as it was before the first sync), and removing a folder that
// holds something else.
#[test]
fn remove_deletes_the_empty_state_folder_but_keeps_one_with_other_files() {
    let card = tempfile::tempdir().unwrap();
    manifest::write(card.path(), &sample()).unwrap();
    manifest::remove(card.path()).unwrap();
    assert!(!card.path().join(".fiio-tunes").exists());

    manifest::write(card.path(), &sample()).unwrap();
    fs::write(card.path().join(".fiio-tunes/orphans.txt"), b"keep").unwrap();
    manifest::remove(card.path()).unwrap();
    assert_eq!(
        fs::read(card.path().join(".fiio-tunes/orphans.txt")).unwrap(),
        b"keep"
    );
    assert!(!manifest::path(card.path()).exists());
}
