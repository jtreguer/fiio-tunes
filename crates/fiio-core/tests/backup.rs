//! `backup.rs` (technical document, 11.2): what a backup holds, how stamps
//! are made and sorted, listing, finding, pruning and reading backups back.
//! The card and the backups folder are temp folders.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use fiio_core::Error;
use fiio_core::backup::{self, Backup, Saved, SavedPlaylist, SavedTrack};
use fiio_core::card;

struct Fx {
    _dir: tempfile::TempDir,
    card: PathBuf,
    root: PathBuf,
}

impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let card = base.join("card");
        let root = base.join("backups");
        fs::create_dir(&card).unwrap();
        Fx {
            _dir: dir,
            card,
            root,
        }
    }

    fn put(&self, rel: &str, bytes: &[u8]) {
        let p = self.card.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }

    fn create(&self, key: &str, now: i64) -> Backup {
        self.create_with(key, now, &[("mappings.json", b"{}".as_slice())])
    }

    fn create_with(&self, key: &str, now: i64, extra: &[(&str, &[u8])]) -> Backup {
        let contents = card::read(&self.card).unwrap();
        backup::create(&self.root, key, &self.card, &contents, extra, now).unwrap()
    }

    fn stamps(&self, key: &str) -> Vec<String> {
        backup::list(&self.root, key)
            .unwrap()
            .into_iter()
            .map(|b| b.stamp)
            .collect()
    }
}

/// Every file under `dir`, relative, sorted.
fn files(dir: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.insert(p.strip_prefix(dir).unwrap().to_str().unwrap().to_owned());
            }
        }
    }
    out
}

const KEY: &str = "usb:323820474077";
/// 2026-10-16 09:30:45 UTC.
const T0: i64 = 1_792_143_045;

// ---------------------------------------------------------------- create

// Prevents: a backup that misses part of the card's state, or holds what it
// should not: every .m3u8 anywhere under its card path byte for byte, the
// manifest when there is one, a listing of audio and playlists with sizes,
// the extra files byte for byte, no audio, no hidden folder left behind.
#[test]
fn a_backup_holds_the_playlists_the_manifest_a_listing_and_the_extra_files() {
    let fx = Fx::new();
    fx.put("Rock.m3u8", b"#EXTM3U\nMusic/a.mp3\n");
    fx.put(
        "Playlists/Pop \u{e9}.m3u8",
        "\u{feff}#EXTM3U\r\nx\r\n".as_bytes(),
    );
    fx.put("deep/er/Misc.m3u8", &[0, 159, 146, 150, 10]);
    fx.put("Music/a.mp3", b"audio");
    fx.put("Music/b.FLAC", b"more audio");
    fx.put("Music/cover.jpg", b"jpg");
    fx.put(".fiio-tunes/manifest.json", b"{\"v\": 1}");
    let b = fx.create_with(
        KEY,
        T0,
        &[
            ("mappings.json", b"{\"m\":1}"),
            ("plan.json", b"[1]"),
            ("work.json", b"[]"),
        ],
    );
    assert_eq!(b.stamp, "20261016-093045");
    assert_eq!(b.dir, fx.root.join("usb_323820474077/20261016-093045"));

    assert_eq!(
        files(&b.dir),
        BTreeSet::from(
            [
                "listing.txt",
                "manifest.json",
                "mappings.json",
                "plan.json",
                "playlists/Playlists/Pop \u{e9}.m3u8",
                "playlists/Rock.m3u8",
                "playlists/deep/er/Misc.m3u8",
                "work.json",
            ]
            .map(String::from)
        )
    );
    for (rel, bytes) in [
        ("playlists/Rock.m3u8", &b"#EXTM3U\nMusic/a.mp3\n"[..]),
        ("playlists/deep/er/Misc.m3u8", &[0, 159, 146, 150, 10][..]),
        ("manifest.json", b"{\"v\": 1}"),
        ("mappings.json", b"{\"m\":1}"),
        ("plan.json", b"[1]"),
        ("work.json", b"[]"),
    ] {
        assert_eq!(fs::read(b.dir.join(rel)).unwrap(), bytes, "{rel}");
    }
    assert_eq!(
        fs::read(b.dir.join("playlists/Playlists/Pop \u{e9}.m3u8")).unwrap(),
        "\u{feff}#EXTM3U\r\nx\r\n".as_bytes()
    );
    // Listing: path and size of every audio file, then every playlist.
    let listing = fs::read_to_string(b.dir.join("listing.txt")).unwrap();
    let mut lines: Vec<&str> = listing.lines().collect();
    lines.sort();
    assert_eq!(
        lines,
        [
            "Music/a.mp3\t5",
            "Music/b.FLAC\t10",
            "Playlists/Pop \u{e9}.m3u8\t15",
            "Rock.m3u8\t20",
            "deep/er/Misc.m3u8\t5",
        ]
    );
    assert_eq!(backup::manifest(&b).unwrap().unwrap(), b"{\"v\": 1}");
    // No hidden folder left, and the backup is the only entry.
    let entries: Vec<String> = fs::read_dir(b.dir.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(entries, ["20261016-093045"]);
}

// Prevents: a stale manifest being invented for a card with none.
#[test]
fn a_card_without_a_manifest_gives_a_backup_without_one() {
    let fx = Fx::new();
    fx.put("a.m3u8", b"x");
    let b = fx.create(KEY, T0);
    assert!(!b.dir.join("manifest.json").exists());
    assert_eq!(backup::manifest(&b).unwrap(), None);
    // An empty card is a backup too, with an empty listing.
    let fx = Fx::new();
    let b = fx.create(KEY, T0);
    assert_eq!(fs::read_to_string(b.dir.join("listing.txt")).unwrap(), "");
    assert!(backup::playlists(&b).unwrap().is_empty());
}

// Prevents: a backup interrupted part way being taken for a complete one
// or piling up: hidden `.partial-` folders are removed by the next backup;
// nothing else in the folder is.
#[test]
fn a_leftover_partial_backup_is_removed_and_nothing_else() {
    let fx = Fx::new();
    let base = fx.root.join("usb_323820474077");
    fs::create_dir_all(base.join(".partial-20200101-000000/playlists")).unwrap();
    fs::write(base.join(".partial-20200101-000000/playlists/x.m3u8"), b"x").unwrap();
    fs::create_dir_all(base.join(".partial-20300101-000000")).unwrap();
    fs::create_dir_all(base.join("notes")).unwrap();
    fx.put("a.m3u8", b"x");
    // Not listed as a backup while it is partial.
    assert!(fx.stamps(KEY).is_empty());
    let b = fx.create(KEY, T0);
    let mut entries: Vec<String> = fs::read_dir(&base)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    entries.sort();
    assert_eq!(entries, ["20261016-093045", "notes"]);
    // A partial folder whose date is in the future did not push the stamp.
    assert_eq!(b.stamp, "20261016-093045");
}

// Prevents: the key's folder leaking into another's, or a hostile key (a
// path in a manifest) writing outside the backups folder.
#[test]
fn folders_are_made_from_keys_without_leaving_the_backups_folder() {
    let r = Path::new("/b");
    assert_eq!(
        backup::folder(r, "usb:323820474077"),
        Path::new("/b/usb_323820474077")
    );
    assert_eq!(
        backup::folder(r, "folder:/tmp/x y"),
        Path::new("/b/folder__tmp_x_y")
    );
    assert_eq!(backup::folder(r, "a.b-c_d"), Path::new("/b/a.b-c_d"));
    assert_eq!(backup::folder(r, "\u{e9}t\u{e9}"), Path::new("/b/_t_"));
    for key in [
        "../x",
        "..",
        "a/../../b",
        "/etc/passwd",
        ".hidden",
        "x\0y",
        "a\\b",
    ] {
        let f = backup::folder(r, key);
        assert!(f.starts_with(r), "{key}: {f:?}");
        let rel = f.strip_prefix(r).unwrap();
        assert!(
            rel.components().count() <= 1
                && rel
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "{key}: {f:?}"
        );
    }
    assert_eq!(backup::folder(r, ".hidden"), Path::new("/b/hidden"));
    // Never the backups folder itself: it would mix every device's backups.
    for key in ["..", ".", "", "...", "/", "\0"] {
        let f = backup::folder(r, key);
        assert_ne!(f, r, "{key:?}");
        assert_eq!(f.parent(), Some(r), "{key:?}: {f:?}");
    }
}

// Prevents: backups of two players mixing: each key has its own folder.
#[test]
fn backups_of_different_keys_are_apart() {
    let fx = Fx::new();
    fx.put("a.m3u8", b"x");
    let a = fx.create("usb:A", T0);
    let b = fx.create("usb:B", T0);
    assert_ne!(a.dir, b.dir);
    // Same second, different devices: both are the plain stamp.
    assert_eq!(a.stamp, b.stamp);
    assert_eq!(fx.stamps("usb:A"), [a.stamp]);
    assert_eq!(fx.stamps("usb:C"), Vec::<String>::new());
    assert!(!fx.root.join("usb_C").exists(), "listing created a folder");
}

// ---------------------------------------------------------------- stamps

// Prevents: stamps in local time or the wrong calendar: UTC, zero padded,
// across a leap day and a century.
#[test]
fn stamps_are_utc_dates() {
    assert_eq!(backup::stamp(0), "19700101-000000");
    assert_eq!(backup::stamp(86_399), "19700101-235959");
    assert_eq!(backup::stamp(951_782_400), "20000229-000000");
    assert_eq!(backup::stamp(1_709_251_199), "20240229-235959");
    assert_eq!(backup::stamp(1_709_251_200), "20240301-000000");
    assert_eq!(backup::stamp(T0), "20261016-093045");
    assert_eq!(backup::stamp(4_102_444_799), "20991231-235959");
}

// Prevents: two backups in the same second overwriting one another, and the
// numbered ones sorting as text (-10 before -2).
#[test]
fn two_backups_in_one_second_get_numbered_stamps_that_sort_by_number() {
    let fx = Fx::new();
    fx.put("a.m3u8", b"1");
    let mut made = Vec::new();
    for i in 0..12 {
        fx.put("a.m3u8", format!("{i}").as_bytes());
        made.push(fx.create(KEY, T0));
    }
    let stamps: Vec<String> = made.iter().map(|b| b.stamp.clone()).collect();
    let unique: BTreeSet<&String> = stamps.iter().collect();
    assert_eq!(unique.len(), 12, "{stamps:?}");
    assert_eq!(stamps[0], "20261016-093045");
    assert!(
        stamps[1..]
            .iter()
            .all(|s| s.starts_with("20261016-093045-"))
    );
    // Listed in the order made, and each holds the card as it was then.
    assert_eq!(fx.stamps(KEY), stamps);
    for (i, b) in made.iter().enumerate() {
        let held = backup::playlists(b).unwrap();
        assert_eq!(held, [("a.m3u8".to_owned(), format!("{i}").into_bytes())]);
    }
}

// Prevents: a backup taken while the clock is behind the earlier ones
// sorting before them (it would be pruned first, and `restore` listings
// would lie about which is newest).
#[test]
fn a_clock_that_runs_behind_still_gives_the_newest_backup_the_last_stamp() {
    let fx = Fx::new();
    fx.put("a.m3u8", b"x");
    let a = fx.create(KEY, T0);
    let b = fx.create(KEY, T0 - 86_400);
    let c = fx.create(KEY, T0 - 10_000_000);
    let d = fx.create(KEY, T0 + 5);
    let stamps = fx.stamps(KEY);
    assert_eq!(stamps.len(), 4);
    assert_eq!(stamps.first(), Some(&a.stamp));
    assert_eq!(&stamps[1], &b.stamp);
    assert_eq!(&stamps[2], &c.stamp);
    assert_eq!(stamps.last(), Some(&d.stamp));
    assert_eq!(d.stamp, backup::stamp(T0 + 5));
    assert!(b.stamp.starts_with(&a.stamp) && c.stamp.starts_with(&a.stamp));
}

// ------------------------------------------------------------ list, find

// Prevents: a folder that is not a complete backup being listed (and then
// offered to restore): only valid stamps holding mappings.json count, in
// the order of time then number.
#[test]
fn list_shows_only_complete_backups_oldest_first() {
    let fx = Fx::new();
    fx.put("a.m3u8", b"x");
    let early = fx.create(KEY, T0);
    let late = fx.create(KEY, T0 + 100);
    let base = late.dir.parent().unwrap();
    // Not backups: no mappings.json, a name that is not a stamp, a hidden
    // folder, a plain file with a stamp's name.
    fs::create_dir(base.join("20200101-000000")).unwrap();
    fs::create_dir_all(base.join("20200102-000000-x")).unwrap();
    fs::write(base.join("20200102-000000-x/mappings.json"), b"{}").unwrap();
    fs::create_dir_all(base.join("20200103-000000-")).unwrap();
    fs::write(base.join("20200103-000000-/mappings.json"), b"{}").unwrap();
    fs::create_dir_all(base.join(".partial-20200104-000000")).unwrap();
    fs::write(base.join(".partial-20200104-000000/mappings.json"), b"{}").unwrap();
    fs::create_dir_all(base.join("2020010-000000")).unwrap();
    fs::write(base.join("2020010-000000/mappings.json"), b"{}").unwrap();
    fs::write(base.join("20200105-000000"), b"file").unwrap();
    let got = fx.stamps(KEY);
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got, [early.stamp, late.stamp]);
}

// Prevents: `find` returning a backup that is not there, or the wrong error:
// it must be NoSuchBackup, with the stamp asked for; and an incomplete
// folder is not found.
#[test]
fn find_returns_the_backup_or_no_such_backup() {
    let fx = Fx::new();
    fx.put("a.m3u8", b"x");
    let a = fx.create(KEY, T0);
    let b = fx.create(KEY, T0);
    assert_eq!(backup::find(&fx.root, KEY, &a.stamp).unwrap(), a);
    assert_eq!(backup::find(&fx.root, KEY, &b.stamp).unwrap(), b);
    fs::create_dir(a.dir.parent().unwrap().join("20200101-000000")).unwrap();
    for stamp in [
        "20200101-000000",
        "nope",
        "",
        "../x",
        &format!("{}-9", a.stamp),
    ] {
        match backup::find(&fx.root, KEY, stamp) {
            Err(Error::NoSuchBackup(s)) => assert_eq!(s, stamp),
            other => panic!("{stamp:?}: {other:?}"),
        }
    }
    // Another device's backup is not this device's.
    assert!(matches!(
        backup::find(&fx.root, "usb:other", &a.stamp),
        Err(Error::NoSuchBackup(_))
    ));
}

// ----------------------------------------------------------------- prune

// Prevents: more backups kept than asked, the newest or the wrong ones
// deleted, and the removed folders not reported.
#[test]
fn prune_keeps_the_newest_and_never_the_new_one_last() {
    let fx = Fx::new();
    fx.put("a.m3u8", b"x");
    let made: Vec<Backup> = (0..5).map(|i| fx.create(KEY, T0 + i * 10)).collect();
    // Another device's backups are not pruned.
    let other = fx.create("usb:other", T0);
    let removed = backup::prune(&fx.root, KEY, 3, &made[4]).unwrap();
    assert_eq!(removed, [made[0].dir.clone(), made[1].dir.clone()]);
    assert_eq!(
        fx.stamps(KEY),
        [&made[2], &made[3], &made[4]].map(|b| b.stamp.clone())
    );
    assert!(!made[0].dir.exists() && made[2].dir.is_dir());
    assert!(other.dir.is_dir());
    // Nothing to prune.
    assert!(
        backup::prune(&fx.root, KEY, 3, &made[4])
            .unwrap()
            .is_empty()
    );
    assert!(
        backup::prune(&fx.root, KEY, 20, &made[4])
            .unwrap()
            .is_empty()
    );
    assert_eq!(fx.stamps(KEY).len(), 3);
}

// Prevents: the backup just made being deleted: even `keep` of 0 keeps it,
// and a new backup that is not the newest by name (clock behind) survives.
#[test]
fn prune_never_removes_the_new_backup_whatever_keep_says() {
    let fx = Fx::new();
    fx.put("a.m3u8", b"x");
    let made: Vec<Backup> = (0..4).map(|i| fx.create(KEY, T0 + i * 10)).collect();
    let removed = backup::prune(&fx.root, KEY, 0, &made[3]).unwrap();
    assert_eq!(removed.len(), 3);
    assert_eq!(fx.stamps(KEY), [made[3].stamp.clone()]);

    // The "new" one is the oldest by name: the others go, it stays.
    let fx = Fx::new();
    fx.put("a.m3u8", b"x");
    let made: Vec<Backup> = (0..4).map(|i| fx.create(KEY, T0 + i * 10)).collect();
    let removed = backup::prune(&fx.root, KEY, 1, &made[0]).unwrap();
    assert_eq!(removed.len(), 3);
    assert_eq!(fx.stamps(KEY), [made[0].stamp.clone()]);

    // keep 2 with the oldest as new: one other is kept, the newest of them.
    let fx = Fx::new();
    fx.put("a.m3u8", b"x");
    let made: Vec<Backup> = (0..4).map(|i| fx.create(KEY, T0 + i * 10)).collect();
    backup::prune(&fx.root, KEY, 2, &made[0]).unwrap();
    assert_eq!(
        fx.stamps(KEY),
        [made[0].stamp.clone(), made[3].stamp.clone()]
    );
}

// --------------------------------------------------------------- readers

fn saved() -> Saved {
    Saved {
        playlists_dir: None,
        device_key: KEY.into(),
        card_id: Some("abcd".into()),
        tracks: vec![SavedTrack {
            track_id: 7,
            card_path: "Music/a.mp3".into(),
            hash: "ab".repeat(32),
            size: 99,
        }],
        playlists: vec![
            SavedPlaylist {
                playlist_id: 3,
                card_name: "Rock.m3u8".into(),
                name: "Rock".into(),
            },
            SavedPlaylist {
                playlist_id: 4,
                card_name: "Pop (2).m3u8".into(),
                name: "Pop".into(),
            },
        ],
    }
}

// Prevents: the record of the card changing on its way through a backup,
// the mappings built from it losing the hash, the size or the names.
#[test]
fn the_saved_record_round_trips_and_gives_back_the_mappings() {
    let fx = Fx::new();
    let s = saved();
    let json = serde_json::to_vec_pretty(&s).unwrap();
    let b = fx.create_with(KEY, T0, &[("mappings.json", &json)]);
    let back = backup::saved(&b).unwrap();
    assert_eq!(back, s);
    let tracks = back.track_mappings().unwrap();
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].track_id, 7);
    assert_eq!(tracks[0].card_path, "Music/a.mp3");
    assert_eq!(tracks[0].synced_hash, [0xab; 32]);
    assert_eq!(tracks[0].synced_size, 99);
    let lists = backup::playlist_mappings(&back);
    assert_eq!(
        lists
            .iter()
            .map(|l| (l.playlist_id, l.card_name.as_str()))
            .collect::<Vec<_>>(),
        [(3, "Rock.m3u8"), (4, "Pop (2).m3u8")]
    );
}

// Prevents: a damaged backup being read as an empty one (and restored as
// such, erasing the record): bad JSON and a bad hash are BadBackup errors.
#[test]
fn a_damaged_record_is_a_bad_backup_not_an_empty_one() {
    let fx = Fx::new();
    let b = fx.create_with(KEY, T0, &[("mappings.json", b"{not json")]);
    assert!(matches!(backup::saved(&b), Err(Error::BadBackup(_))));
    let mut s = saved();
    for hash in [
        "xyz",
        &"ab".repeat(31),
        &"zz".repeat(32),
        &"é".repeat(32),
        "",
    ] {
        s.tracks[0].hash = hash.to_string();
        assert!(
            matches!(s.track_mappings(), Err(Error::BadBackup(m)) if m.contains("Music/a.mp3")),
            "{hash:?}"
        );
    }
    // A backup whose record was deleted is not listed at all.
    fs::remove_file(b.dir.join("mappings.json")).unwrap();
    assert!(fx.stamps(KEY).is_empty());
    assert!(matches!(backup::saved(&b), Err(Error::Io { .. })));
}

// Prevents: the playlists of a backup coming back in an unstable order or
// under the wrong path.
#[test]
fn playlists_come_back_by_card_path_sorted() {
    let fx = Fx::new();
    fx.put("b.m3u8", b"B");
    fx.put("a.m3u8", b"A");
    fx.put("z/c.m3u8", b"C");
    fx.put("Playlists/d.m3u8", b"D");
    let b = fx.create(KEY, T0);
    assert_eq!(
        backup::playlists(&b).unwrap(),
        [
            ("Playlists/d.m3u8".to_owned(), b"D".to_vec()),
            ("a.m3u8".to_owned(), b"A".to_vec()),
            ("b.m3u8".to_owned(), b"B".to_vec()),
            ("z/c.m3u8".to_owned(), b"C".to_vec()),
        ]
    );
}
