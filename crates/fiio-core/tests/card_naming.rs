//! `naming::clean`, `card_path` and `card_paths`: table-driven cases for the
//! rules of technical.md 9.3, then properties over random collections of
//! paths built to collide.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use fiio_core::naming::{card_path, card_paths, clean};
use fiio_core::paths::card_key;
use proptest::prelude::*;

const REFUSED: [char; 8] = ['"', '*', ':', '<', '>', '?', '\\', '|'];

fn units(s: &str) -> usize {
    s.encode_utf16().count()
}

fn p(s: &str) -> &Path {
    Path::new(s)
}

/// `card_paths` for `/m`-rooted files, as plain strings.
fn map(paths: &[&str]) -> Vec<String> {
    let files: Vec<(&Path, &Path)> = paths.iter().map(|f| (p("/m"), p(f))).collect();
    card_paths("Music", &files)
        .into_iter()
        .map(|o| o.expect("every file is under /m"))
        .collect()
}

// ---- clean

// Prevents: a character FAT refuses reaching the card, where the write
// would fail or the file would be unreadable on Windows.
#[test]
fn every_refused_character_becomes_an_underscore() {
    for c in REFUSED {
        assert_eq!(clean(&format!("a{c}b")), "a_b", "{c:?}");
        assert_eq!(clean(&format!("{c}{c}")), "__", "{c:?}");
    }
    assert_eq!(clean("a\"*:<>?\\|b"), "a________b");
}

// Prevents: control characters (a tab, a newline, DEL, C1) kept in a name;
// and ordinary characters over-cleaned.
#[test]
fn control_characters_are_replaced_and_other_characters_kept() {
    for c in [
        '\u{0}', '\u{1}', '\t', '\n', '\r', '\u{1f}', '\u{7f}', '\u{85}', '\u{9f}',
    ] {
        assert_eq!(clean(&format!("a{c}b")), "a_b", "{c:?}");
    }
    for keep in [
        "a b",
        "a-b",
        "a_b",
        "a'b",
        "a,b",
        "a;b",
        "a&b",
        "a#b",
        "a%b",
        "a+b",
        "a=b",
        "a[b]",
        "a(b)",
        "a!b",
        "a@b",
        "a$b",
        "a~b",
        "a\u{a0}b",
        "a\u{200b}b",
        "日本語",
        "🎵",
        "ß",
        "ı",
        ".hidden",
        " lead",
    ] {
        assert_eq!(clean(keep), keep);
    }
}

// Prevents: names FAT would silently rewrite (it drops trailing dots and
// spaces) differing from what the plan says is on the card.
#[test]
fn trailing_dots_and_spaces_are_removed_and_nothing_else_is_trimmed() {
    for (input, want) in [
        ("Vol. 2...", "Vol. 2"),
        ("name ", "name"),
        ("name. . .", "name"),
        ("name \t", "name _"),
        ("a. b", "a. b"),
        ("a .b", "a .b"),
        ("end?.", "end_"),
        ("end.?", "end._"),
    ] {
        assert_eq!(clean(input), want, "{input:?}");
    }
}

// Prevents: an empty name (which cannot exist) or a name of dots; both
// become `_`.
#[test]
fn names_that_clean_to_nothing_become_an_underscore() {
    for input in ["", ".", "..", "...", " ", ". .", "  .  ", "?", "\u{1}", ":"] {
        let got = clean(input);
        assert!(
            !got.is_empty() && got.chars().all(|c| c == '_'),
            "{input:?} -> {got:?}"
        );
    }
    for input in ["", ".", "..", "...", " ", ". .", "  .  "] {
        assert_eq!(clean(input), "_", "{input:?}");
    }
    assert_eq!(clean("\u{1}"), "_");
    assert_eq!(clean("?"), "_");
}

// Prevents: NFD names reaching the card unnormalized, so the same title
// spelled two ways would be two files.
#[test]
fn names_are_written_in_nfc() {
    assert_eq!(clean("Cafe\u{301}"), "Caf\u{e9}");
    assert_eq!(clean("Jo\u{301}hann"), "J\u{f3}hann");
    assert_eq!(clean("Caf\u{e9}"), "Caf\u{e9}");
    // Hangul jamo compose too.
    assert_eq!(clean("\u{1112}\u{1161}\u{11ab}"), "\u{d55c}");
}

// ---- card_path

// Prevents: the music folder, the relative path or the cleaning of folder
// names (not just the file name) being wrong.
#[test]
fn card_path_cleans_every_component_under_the_music_folder() {
    let root = p("/lib");
    for (path, want) in [
        ("/lib/a.mp3", "Music/a.mp3"),
        (
            "/lib/Artist/Album/01 Song.mp3",
            "Music/Artist/Album/01 Song.mp3",
        ),
        (
            "/lib/AC:DC/Who Made Who?/01.mp3",
            "Music/AC_DC/Who Made Who_/01.mp3",
        ),
        ("/lib/Vol. 2.../x.mp3", "Music/Vol. 2/x.mp3"),
        ("/lib/Album /x.mp3", "Music/Album/x.mp3"),
        ("/lib/a/Jo\u{301}hann.mp3", "Music/a/J\u{f3}hann.mp3"),
        ("/lib/a/tab\there.mp3", "Music/a/tab_here.mp3"),
        ("/lib/.../x.mp3", "Music/_/x.mp3"),
        ("/lib/a/./b.mp3", "Music/a/b.mp3"),
        ("/lib//a.mp3", "Music/a.mp3"),
        ("/lib/a/日本語.mp3", "Music/a/日本語.mp3"),
    ] {
        assert_eq!(
            card_path("Music", root, p(path)).as_deref(),
            Some(want),
            "{path}"
        );
    }
    assert_eq!(
        card_path("Audio/Lib", root, p("/lib/a.mp3")).as_deref(),
        Some("Audio/Lib/a.mp3")
    );
    // A root given with a trailing slash is the same root.
    assert_eq!(
        card_path("Music", p("/lib/"), p("/lib/a.mp3")).as_deref(),
        Some("Music/a.mp3")
    );
}

// Prevents: a file the planner cannot place being given a made-up place
// (the root itself, a file outside it, one reaching out with `..`, one with
// a name that is not UTF-8).
#[test]
fn files_that_cannot_be_placed_have_no_card_path() {
    let root = p("/lib");
    for path in [
        "/lib",
        "/lib/",
        "/other/a.mp3",
        "/libx/a.mp3",
        "/li/a.mp3",
        "a.mp3",
        "/lib/../etc/a.mp3",
        "/lib/a/../b.mp3",
        "/lib/..",
    ] {
        assert_eq!(card_path("Music", root, p(path)), None, "{path}");
    }
    let bad = Path::new(OsStr::from_bytes(b"/lib/bad\xff.mp3"));
    assert_eq!(card_path("Music", root, bad), None);
    let bad_dir = Path::new(OsStr::from_bytes(b"/lib/d\xfe/ok.mp3"));
    assert_eq!(card_path("Music", root, bad_dir), None);
}

/// The names of a card path past the music folder.
fn parts(path: &str) -> Vec<&str> {
    path.split('/').skip(1).collect()
}

// Prevents: names over FAT's 255 UTF-16 units reaching the card (the write
// fails halfway through a sync), or a cut that splits a character, drops the
// extension, or leaves a trailing dot/space.
#[test]
fn long_names_are_cut_to_255_utf16_units_keeping_the_extension() {
    let bmp = "é".repeat(300);
    let astral = "𝄞".repeat(200);
    let cases: Vec<(String, usize)> = vec![
        // (file name, units expected in the result)
        (format!("{}.mp3", "a".repeat(251)), 255),
        (format!("{}.mp3", "a".repeat(252)), 255),
        (format!("{}.mp3", "a".repeat(400)), 255),
        (format!("{bmp}.flac"), 255),
        // 250 units of 𝄞 + ".mp3"; a 126th would make 256.
        (format!("{astral}.mp3"), 254),
        // A cut landing on a space or dot is trimmed: "aaa... .mp3".
        (format!("{} xx.mp3", "a".repeat(250)), 254),
        (format!("{}...x.mp3", "a".repeat(250)), 254),
        // No extension: the whole name is cut.
        ("b".repeat(300), 255),
        ("𝄞".repeat(200), 254),
    ];
    for (name, want) in cases {
        let got = card_path("Music", p("/m"), &Path::new("/m").join(&name)).unwrap();
        let file = parts(&got).pop().unwrap().to_owned();
        assert_eq!(
            units(&file),
            want,
            "{} units for {:?}...",
            units(&name),
            &name[..8]
        );
        if let Some((_, ext)) = name.rsplit_once('.') {
            assert!(file.ends_with(&format!(".{ext}")), "{file:?}");
        }
        assert!(!file.ends_with(['.', ' ']), "{file:?}");
    }
    // Exactly 255 units is left alone.
    let exact = format!("{}.mp3", "a".repeat(251));
    assert_eq!(
        card_path("Music", p("/m"), &Path::new("/m").join(&exact)).unwrap(),
        format!("Music/{exact}")
    );
    // Folders are cut too, and have no extension to keep.
    let dir = "a.b".repeat(100);
    let got = card_path("Music", p("/m"), &Path::new("/m").join(&dir).join("x.mp3")).unwrap();
    assert_eq!(units(parts(&got)[0]), 255, "{got}");
    assert_eq!(parts(&got)[1], "x.mp3");
}

// Prevents: a very long "extension" (a name with a late dot) either being
// kept whole (leaving no room for the stem, or over 255) or panicking.
#[test]
fn an_extension_too_long_to_keep_is_cut_with_the_rest() {
    for ext_len in [10, 200, 252, 253, 254, 255, 300] {
        for stem in ["a", "ab", "𝄞𝄞"] {
            let name = format!("{stem}.{}", "x".repeat(ext_len));
            let got = card_path("Music", p("/m"), &Path::new("/m").join(&name)).unwrap();
            let file = parts(&got).pop().unwrap().to_owned();
            assert!(units(&file) <= 255, "ext {ext_len}: {} units", units(&file));
            assert!(!file.is_empty() && !file.ends_with(['.', ' ']));
            if ext_len + 1 + units(stem) <= 255 {
                assert_eq!(file, name, "fits as it is");
            } else if ext_len < 254 {
                // The extension is kept and the stem gives way.
                assert!(
                    file.ends_with(&format!(".{}", "x".repeat(ext_len))),
                    "{file}"
                );
            }
        }
    }
}

// ---- card_paths

// Prevents: ` (n)` pushing a name over 255 units, or eating its extension
// (also with two digits, and with no extension at all).
#[test]
fn the_collision_suffix_still_fits_in_255_units() {
    for ext in [".mp3", ""] {
        // All of these are cut to the same 251 (or 255) characters.
        let names: Vec<String> = (0..12)
            .map(|i| format!("/m/{}{i}{ext}", "a".repeat(300)))
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let got = map(&refs);
        let mut keys = BTreeSet::new();
        for g in &got {
            let file = parts(g).pop().unwrap();
            assert!(units(file) <= 255, "{} units: {file:?}", units(file));
            assert!(file.ends_with(ext), "{file:?}");
            assert!(keys.insert(card_key(g)), "duplicate {g}");
        }
        // Library path order is the order of the strings, not of `i`: the
        // k-th in that order keeps the plain name (k = 1) or gets ` (k)`.
        let mut order: Vec<usize> = (0..names.len()).collect();
        order.sort_by_key(|&i| &names[i]);
        let rank = |i: usize| order.iter().position(|&o| o == i).unwrap() + 1;
        // Only the suffix is compared (the stems are all `a`s), so a
        // failure prints something readable.
        let tail = |g: &str| g.replace('a', "");
        let want: Vec<String> = (0..names.len())
            .map(|i| match rank(i) {
                1 => format!("Music/{ext}"),
                k => format!("Music/ ({k}){ext}"),
            })
            .collect();
        assert_eq!(
            got.iter().map(|g| tail(g)).collect::<Vec<_>>(),
            want,
            "ext {ext:?}"
        );
        assert!(
            got.iter().all(|g| units(parts(g)[0]) == 255),
            "every name is cut to the limit"
        );
    }
}

// Prevents: two library files FAT holds as one overwriting each other on the
// card. Library path order decides who keeps the plain name.
#[test]
fn paths_that_come_to_one_key_are_told_apart_in_library_path_order() {
    // Case, ASCII.
    assert_eq!(
        map(&["/m/song.mp3", "/m/Song.mp3", "/m/SONG.MP3"]),
        ["Music/song (3).mp3", "Music/Song (2).mp3", "Music/SONG.MP3"]
    );
    // Case, accented.
    assert_eq!(
        map(&["/m/Café.mp3", "/m/CAFÉ.mp3"]),
        ["Music/Café (2).mp3", "Music/CAFÉ.mp3"]
    );
    // NFC against NFD.
    assert_eq!(
        map(&["/m/Caf\u{e9}.mp3", "/m/Cafe\u{301}.mp3"]),
        ["Music/Café (2).mp3", "Music/Café.mp3"]
    );
    // Cleaning: `a?` and `a_` both come to `a_`.
    assert_eq!(
        map(&["/m/a?.mp3", "/m/a_.mp3"]),
        ["Music/a_.mp3", "Music/a_ (2).mp3"]
    );
    // Trailing dots: `a.` and `a..` and `a` (no extension).
    assert_eq!(
        map(&["/m/a", "/m/a.", "/m/a.."]),
        ["Music/a", "Music/a (2)", "Music/a (3)"]
    );
    // The same library file twice still gets two distinct places.
    let twice = map(&["/m/x.mp3", "/m/x.mp3"]);
    assert_ne!(twice[0], twice[1]);
}

// Prevents: a literal `a (2).mp3` in the library clashing with the
// suffix given to a collision.
#[test]
fn a_suffix_never_lands_on_a_name_already_taken() {
    // Library order: "/m/A.mp3" < "/m/a (2).mp3" < "/m/a.mp3".
    assert_eq!(
        map(&["/m/a.mp3", "/m/A.mp3", "/m/a (2).mp3"]),
        ["Music/a (3).mp3", "Music/A.mp3", "Music/a (2).mp3"]
    );
    let upper_lower = ["/m/x.mp3", "/m/X.mp3", "/m/x (2).mp3", "/m/X (3).mp3"];
    let got = map(&upper_lower);
    let keys: BTreeSet<String> = got.iter().map(|g| card_key(g)).collect();
    assert_eq!(keys.len(), 4, "{got:?}");
}

// Prevents: one library folder spelled two ways becoming two card folders
// (or one file overwriting another in the merged folder).
#[test]
fn a_folder_spelled_two_ways_takes_the_spelling_of_the_first_file() {
    assert_eq!(
        map(&[
            "/m/rock/b.mp3",
            "/m/Rock/a.mp3",
            "/m/ROCK/sub/c.mp3",
            "/m/rock/sub/d.mp3"
        ]),
        [
            "Music/ROCK/b.mp3",
            "Music/ROCK/a.mp3",
            "Music/ROCK/sub/c.mp3",
            "Music/ROCK/sub/d.mp3"
        ]
    );
    // NFC and NFD folders are one folder.
    assert_eq!(
        map(&["/m/Cafe\u{301}/a.mp3", "/m/Caf\u{e9}/b.mp3"]),
        ["Music/Café/a.mp3", "Music/Café/b.mp3"]
    );
    // Same file name in the merged folder: told apart.
    assert_eq!(
        map(&["/m/Rock/a.mp3", "/m/rock/a.mp3"]),
        ["Music/Rock/a.mp3", "Music/Rock/a (2).mp3"]
    );
}

// Prevents: a file and a folder sharing a name on the card (impossible on
// FAT: one of the writes would fail).
#[test]
fn a_file_and_a_folder_with_one_name_are_told_apart() {
    // Two roots: a file `a` in /m1 and a folder `A` in /m2.
    let files = [
        (p("/m1"), p("/m1/a")),
        (p("/m2"), p("/m2/A/b.mp3")),
        (p("/m2"), p("/m2/A/c.mp3")),
    ];
    let got: Vec<String> = card_paths("Music", &files)
        .into_iter()
        .map(Option::unwrap)
        .collect();
    assert_eq!(
        got[1].rsplit_once('/').unwrap().0,
        got[2].rsplit_once('/').unwrap().0
    );
    let dir = got[1].rsplit_once('/').unwrap().0;
    assert_ne!(card_key(dir), card_key(&got[0]), "{got:?}");
    // The other order of path names: the folder is seen first.
    let files = [(p("/m1"), p("/m1/z")), (p("/m2"), p("/m2/Z/b.mp3"))];
    let got: Vec<String> = card_paths("Music", &files)
        .into_iter()
        .map(Option::unwrap)
        .collect();
    let dir = got[1].rsplit_once('/').unwrap().0;
    assert_ne!(card_key(dir), card_key(&got[0]), "{got:?}");
}

// Prevents: roots being merged wrongly, or a file from one root changing
// another's place: both go under the one music folder, collisions told
// apart by library path.
#[test]
fn several_roots_share_the_music_folder() {
    let files = [
        (p("/m2"), p("/m2/A.mp3")),
        (p("/m1"), p("/m1/a.mp3")),
        (p("/m1"), p("/m1/sub/x.mp3")),
        (p("/m2"), p("/m2/sub/X.mp3")),
    ];
    let got: Vec<String> = card_paths("Music", &files)
        .into_iter()
        .map(Option::unwrap)
        .collect();
    // Library path order: /m1/a.mp3, /m1/sub/x.mp3, /m2/A.mp3, /m2/sub/X.mp3.
    assert_eq!(
        got,
        [
            "Music/A (2).mp3",
            "Music/a.mp3",
            "Music/sub/x.mp3",
            "Music/sub/X (2).mp3"
        ]
    );
}

// Prevents: one unplaceable file shifting the others' results, or being
// given a path.
#[test]
fn unplaceable_files_keep_their_slot_and_leave_the_others_alone() {
    let bad = Path::new(OsStr::from_bytes(b"/m/bad\xff.mp3"));
    let files = [
        (p("/m"), p("/m/a.mp3")),
        (p("/m"), bad),
        (p("/elsewhere"), p("/m/b.mp3")),
        (p("/m"), p("/m")),
        (p("/m"), p("/m/A.mp3")),
    ];
    let got = card_paths("Music", &files);
    assert_eq!(
        got,
        [
            Some("Music/a (2).mp3".to_owned()),
            None,
            None,
            None,
            Some("Music/A.mp3".to_owned())
        ]
    );
    // The same two files without the others give the same two answers.
    assert_eq!(
        card_paths("Music", &[files[0], files[4]]),
        [
            Some("Music/a (2).mp3".to_owned()),
            Some("Music/A.mp3".to_owned())
        ]
    );
    assert!(card_paths("Music", &[]).is_empty());
}

// Prevents: a nested music folder being mangled or taking part in the
// collisions as if it were a library folder.
#[test]
fn a_nested_music_folder_is_used_as_given() {
    let files = [(p("/m"), p("/m/a.mp3")), (p("/m"), p("/m/A.mp3"))];
    assert_eq!(
        card_paths("Audio/Library", &files),
        [
            Some("Audio/Library/a (2).mp3".to_owned()),
            Some("Audio/Library/A.mp3".to_owned())
        ]
    );
}

// ---- properties

/// A small alphabet that collides a lot: case, accents, NFC against NFD,
/// refused characters, dots, spaces, astral and CJK characters.
fn atom() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        "a", "A", "b", "é", "É", "e\u{301}", "E\u{301}", "?", "_", ".", " ", "1", "𝄞", "日", "ß",
        "\u{1}", "x y",
    ])
}

fn name() -> impl Strategy<Value = String> {
    name_any().prop_filter("`.` and `..` are not file names", |n| n != "." && n != "..")
}

fn name_any() -> impl Strategy<Value = String> {
    prop_oneof![
        8 => prop::collection::vec(atom(), 1..4).prop_map(|v| v.concat()),
        // Long names, to meet the 255 limit and the suffix.
        1 => (atom(), 120..300usize).prop_map(|(a, n)| a.repeat(n)),
    ]
}

fn file_name() -> impl Strategy<Value = String> {
    (
        name(),
        prop::sample::select(vec!["", ".mp3", ".MP3", ".flac", ".x.mp3"]),
    )
        .prop_map(|(n, e)| format!("{n}{e}"))
}

/// `(root, path)` pairs: one to three levels, one of two roots.
fn files() -> impl Strategy<Value = Vec<(PathBuf, PathBuf)>> {
    let one = (
        prop::sample::select(vec!["/m1", "/m2"]),
        prop::collection::vec(name(), 0..3),
        file_name(),
    )
        .prop_map(|(root, dirs, file)| {
            let mut path = PathBuf::from(root);
            path.extend(dirs);
            path.push(file);
            (PathBuf::from(root), path)
        });
    prop::collection::vec(one, 1..25).prop_map(|v| {
        // Distinct pairs only: the same file listed twice is another case.
        v.into_iter().collect::<BTreeSet<_>>().into_iter().collect()
    })
}

fn place(files: &[(PathBuf, PathBuf)]) -> Vec<Option<String>> {
    let refs: Vec<(&Path, &Path)> = files
        .iter()
        .map(|(r, p)| (r.as_path(), p.as_path()))
        .collect();
    card_paths("Music", &refs)
}

proptest! {
    // Prevents: any of the rules breaking only on inputs nobody thought to
    // write down.
    #[test]
    fn every_card_path_is_valid_unique_and_consistent(files in files()) {
        let got: Vec<String> = place(&files).into_iter().map(|o| o.expect("placeable")).collect();
        let mut file_keys = BTreeSet::new();
        for g in &got {
            prop_assert!(g.starts_with("Music/"), "{g}");
            for part in parts(g) {
                prop_assert!(!part.is_empty(), "{g}");
                prop_assert!(units(part) <= 255, "{} units in {g:?}", units(part));
                prop_assert!(!part.contains(REFUSED), "{g:?}");
                prop_assert!(!part.chars().any(char::is_control), "{g:?}");
                prop_assert!(!part.ends_with(['.', ' ']), "{g:?}");
                let cleaned = clean(part);
                prop_assert_eq!(part, cleaned.as_str());
            }
            prop_assert!(file_keys.insert(card_key(g)), "two files share a key: {g}");
        }
        // One spelling per folder key, and no folder shares a key with a file.
        let mut dirs: HashMap<String, String> = HashMap::new();
        for g in &got {
            let comps: Vec<&str> = g.split('/').collect();
            for n in 1..comps.len() {
                let dir = comps[..n].join("/");
                let key = card_key(&dir);
                prop_assert!(!file_keys.contains(&key), "{dir} is a file and a folder: {got:?}");
                let first = dirs.entry(key).or_insert_with(|| dir.clone());
                prop_assert_eq!(&*first, &dir, "a folder spelled two ways");
            }
        }
    }

    // Prevents: a result that depends on the order the planner happened to
    // list the library in.
    #[test]
    fn the_mapping_does_not_depend_on_the_order_of_the_input(
        files in files().prop_flat_map(|f| (Just(f.clone()), Just(f).prop_shuffle()))
    ) {
        let (a, b) = files;
        let pairs = |files: &[(PathBuf, PathBuf)]| -> BTreeSet<(PathBuf, Option<String>)> {
            files.iter().map(|f| f.1.clone()).zip(place(files)).collect()
        };
        prop_assert_eq!(pairs(&a), pairs(&b));
        // And the same input twice gives the same answer.
        prop_assert_eq!(place(&a), place(&a));
    }

    // Prevents: `card_path` and `card_paths` disagreeing for a file nothing
    // collides with.
    #[test]
    fn a_file_alone_gets_its_plain_card_path(files in files()) {
        let (root, path) = &files[0];
        let alone = place(&files[..1]);
        prop_assert_eq!(alone[0].clone(), card_path("Music", root, path));
    }

    // Prevents: cleaning not being stable (a name cleaned twice changing again).
    #[test]
    fn clean_is_idempotent_and_never_empty(s in ".{0,40}") {
        let once = clean(&s);
        prop_assert!(!once.is_empty());
        prop_assert!(!once.ends_with(['.', ' ']));
        prop_assert!(!once.contains(REFUSED));
        prop_assert_eq!(clean(&once), once);
    }
}

// Prevents: a trailing dot or space of a stem, which FAT keeps before an
// extension, being trimmed (`Track .mp3` becoming `Track.mp3`): the card
// would not hold the name the library has. Names with no extension (a
// folder) are still trimmed, and so is a stem the 255-unit cut left ending
// in one.
#[test]
fn a_stem_keeps_its_trailing_dots_and_spaces_before_the_extension() {
    for (path, want) in [
        ("/m/Track .mp3", "Music/Track .mp3"),
        ("/m/Vol..mp3", "Music/Vol..mp3"),
        ("/m/a . .flac", "Music/a . .flac"),
        ("/m/Dir/Track 1 .mp3", "Music/Dir/Track 1 .mp3"),
        // No extension: trimmed.
        ("/m/Track ", "Music/Track"),
        ("/m/Vol..", "Music/Vol"),
        ("/m/Vol. /x.mp3", "Music/Vol/x.mp3"),
    ] {
        assert_eq!(
            card_path("Music", p("/m"), p(path)).as_deref(),
            Some(want),
            "{path}"
        );
    }
    // They are different names from the trimmed ones, so they do not
    // collide; and they still collide with their own case variants, the
    // suffix going before the space-ended stem's extension.
    assert_eq!(
        map(&["/m/Track .mp3", "/m/Track.mp3", "/m/TRACK .mp3"]),
        [
            "Music/Track  (2).mp3",
            "Music/Track.mp3",
            "Music/TRACK .mp3"
        ]
    );
    // A cut that lands on a space or dot still trims it (stem 250 `a`, a
    // space, then more), with or without an extension.
    for name in [
        format!("{} xx.mp3", "a".repeat(250)),
        format!("{}...x.mp3", "a".repeat(250)),
        format!("{} xx", "a".repeat(253)),
    ] {
        let got = card_path("Music", p("/m"), &Path::new("/m").join(&name)).unwrap();
        let file = parts(&got).pop().unwrap().to_owned();
        let stem = file.strip_suffix(".mp3").unwrap_or(&file);
        assert!(!stem.ends_with(['.', ' ']), "{file:?}");
        assert!(units(&file) <= 255);
    }
}
