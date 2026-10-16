//! Where a library file goes on the card. Pure: no I/O, and the same input
//! always gives the same output in the same order.
//!
//! A file goes to `<music_dir>/<its path relative to its library root>`.
//! Each name is made acceptable to FAT32: NFC, the characters it refuses
//! (`" * : < > ? \ |` and control characters) replaced with `_`, trailing
//! dots and spaces removed, and cut to 255 UTF-16 units, the extension kept.
//! FAT compares names without case, Unicode included ([`card_key`]), so
//! paths that come to the same key are told apart: a file gets ` (2)`,
//! ` (3)` before its extension, in library path order, and a folder spelled
//! two ways takes the spelling of the first file under it.

use std::collections::HashMap;
use std::path::Path;

use unicode_normalization::UnicodeNormalization;

use crate::paths::card_key;

/// FAT's limit for one name, in UTF-16 units.
const MAX_UNITS: usize = 255;

fn units(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `s` cut to at most `max` UTF-16 units, at a character boundary.
fn cut(s: &str, max: usize) -> &str {
    let mut n = 0;
    for (i, c) in s.char_indices() {
        n += c.len_utf16();
        if n > max {
            return &s[..i];
        }
    }
    s
}

fn trim_end(s: &str) -> &str {
    s.trim_end_matches(['.', ' '])
}

/// One name made acceptable to FAT32: NFC, refused characters replaced
/// with `_`, trailing dots and spaces removed. Never empty.
pub fn clean(name: &str) -> String {
    let s: String = name
        .nfc()
        .map(|c| match c {
            '"' | '*' | ':' | '<' | '>' | '?' | '\\' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    match trim_end(&s) {
        "" => "_".into(),
        t => t.to_owned(),
    }
}

/// `stem` (with `suffix`, such as ` (2)`) and `.ext`, the stem cut so the
/// whole fits in 255 UTF-16 units. Without an extension the whole name is
/// cut.
fn fit(stem: &str, suffix: &str, ext: Option<&str>) -> String {
    let tail = ext.map_or(0, |e| 1 + units(e)) + units(suffix);
    let (stem, ext) = match ext {
        // An extension too long to keep is cut with the rest.
        Some(e) if tail >= MAX_UNITS => (format!("{stem}.{e}"), None),
        e => (stem.to_owned(), e),
    };
    let room = MAX_UNITS.saturating_sub(ext.map_or(0, |e| 1 + units(e)) + units(suffix));
    let short = cut(&stem, room);
    // Trailing dots and spaces matter at the end of a name only: a stem
    // keeps them before its extension, unless the cut left them last.
    let mut s = match ext.is_none() || short.len() < stem.len() {
        true => trim_end(short).to_owned(),
        false => short.to_owned(),
    };
    if s.is_empty() {
        s.push('_');
    }
    s.push_str(suffix);
    if let Some(e) = ext {
        s.push('.');
        s.push_str(e);
    }
    s
}

/// A file name split at its extension, if it has one.
fn split_ext(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => (stem, Some(ext)),
        _ => (name, None),
    }
}

/// `n`-th spelling of a name: itself for 1, then ` (n)` before the
/// extension (a folder has none).
fn nth(name: &str, n: usize, is_file: bool) -> String {
    let (stem, ext) = match is_file {
        true => split_ext(name),
        false => (name, None),
    };
    let suffix = match n {
        1 => String::new(),
        n => format!(" ({n})"),
    };
    fit(stem, &suffix, ext)
}

/// The `n`-th spelling of the file name `name`: itself for 1, then ` (n)`
/// before its extension, fitted in 255 UTF-16 units.
pub fn numbered(name: &str, n: usize) -> String {
    nth(name, n, true)
}

/// The cleaned names of `path` relative to `root`, or `None` when it is not
/// under it, is the root itself, or is not UTF-8.
fn components(root: &Path, path: &Path) -> Option<Vec<String>> {
    let rel = path.strip_prefix(root).ok()?;
    let names: Option<Vec<String>> = rel
        .components()
        .map(|c| match c {
            std::path::Component::Normal(s) => s.to_str().map(clean),
            _ => None,
        })
        .collect();
    names.filter(|n| !n.is_empty())
}

/// The card path of one file, without telling it apart from others:
/// `<music_dir>/<relative path>`, each name cleaned and fitted.
pub fn card_path(music_dir: &str, root: &Path, path: &Path) -> Option<String> {
    let names = components(root, path)?;
    let last = names.len() - 1;
    let mut out = music_dir.to_owned();
    for (i, n) in names.iter().enumerate() {
        out.push('/');
        out.push_str(&nth(n, 1, i == last));
    }
    Some(out)
}

/// The card paths of library files given as `(root, path)`, in the same
/// order, `None` for a file [`card_path`] cannot place. Paths that would be
/// one file on FAT are told apart, the first in library path order keeping
/// its name.
pub fn card_paths(music_dir: &str, files: &[(&Path, &Path)]) -> Vec<Option<String>> {
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by(|&a, &b| {
        let key = |i: usize| (files[i].1.as_os_str(), files[i].0.as_os_str());
        key(a).cmp(&key(b))
    });
    // Every path handed out so far, by key: its spelling and whether it
    // is a folder.
    let mut taken: HashMap<String, (String, bool)> = HashMap::new();
    let mut out = vec![None; files.len()];
    for i in order {
        let (root, path) = files[i];
        let Some(names) = components(root, path) else {
            continue;
        };
        let last = names.len() - 1;
        let mut at = music_dir.to_owned();
        for (j, name) in names.iter().enumerate() {
            let is_file = j == last;
            for n in 1.. {
                let candidate = format!("{at}/{}", nth(name, n, is_file));
                match taken.get(&card_key(&candidate)) {
                    None => {
                        taken.insert(card_key(&candidate), (candidate.clone(), !is_file));
                        at = candidate;
                        break;
                    }
                    // The same folder, maybe spelled otherwise: the first
                    // spelling.
                    Some((spelling, true)) if !is_file => {
                        at = spelling.clone();
                        break;
                    }
                    Some(_) => {}
                }
            }
        }
        out[i] = Some(at);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    #[test]
    fn a_path_keeps_its_folders_under_the_music_folder() {
        assert_eq!(
            card_path("Music", p("/m"), p("/m/Jeff Buckley/Grace/02 Grace.mp3")).as_deref(),
            Some("Music/Jeff Buckley/Grace/02 Grace.mp3")
        );
        assert_eq!(card_path("Music", p("/m"), p("/other/a.mp3")), None);
        assert_eq!(card_path("Music", p("/m"), p("/m")), None);
    }

    #[test]
    fn names_are_cleaned_for_fat() {
        assert_eq!(clean("AC/DC? \"Live\": <1>|*\\"), "AC/DC_ _Live__ _1____");
        assert_eq!(clean("Vol. 2..."), "Vol. 2");
        assert_eq!(clean("Tab\there "), "Tab_here");
        assert_eq!(clean("..."), "_");
        assert_eq!(clean("Jo\u{301}hann"), "J\u{f3}hann");
    }

    #[test]
    fn long_names_are_cut_to_255_utf16_units_keeping_the_extension() {
        let long = format!("{}.flac", "é".repeat(300));
        let n = nth(&long, 1, true);
        assert_eq!(units(&n), 255);
        assert!(n.ends_with("é.flac"));
        let n = nth(&long, 12, true);
        assert_eq!(units(&n), 255);
        assert!(n.ends_with("é (12).flac"), "{n}");
        // A character outside the BMP is two units and is never split.
        let wide = format!("{}.mp3", "𝄞".repeat(200));
        let n = nth(&wide, 1, true);
        assert!(units(&n) <= 255 && n.ends_with("𝄞.mp3"));
        // A folder name has no extension to keep.
        assert_eq!(units(&nth(&"a.b".repeat(100), 1, false)), 255);
    }

    #[test]
    fn names_fat_holds_as_one_are_told_apart_in_library_path_order() {
        let files = [
            (p("/m"), p("/m/b/Song.mp3")),
            (p("/m"), p("/m/a/Café.mp3")),
            (p("/m"), p("/m/a/CAFÉ.mp3")),
            (p("/m"), p("/m/a/Cafe\u{301}.mp3")),
            (p("/m"), p("/m/a/x?.mp3")),
            (p("/m"), p("/m/a/x_.mp3")),
        ];
        let got = card_paths("Music", &files);
        // Library path order: "CAFÉ" < "Cafe\u{301}" < "Café" (bytes).
        assert_eq!(
            got,
            [
                Some("Music/b/Song.mp3".into()),
                Some("Music/a/Café (3).mp3".into()),
                Some("Music/a/CAFÉ.mp3".into()),
                Some("Music/a/Café (2).mp3".into()),
                Some("Music/a/x_.mp3".into()),
                Some("Music/a/x_ (2).mp3".into()),
            ]
        );
    }

    #[test]
    fn a_folder_spelled_two_ways_takes_the_first_spelling() {
        let files = [(p("/m"), p("/m/rock/b.mp3")), (p("/m"), p("/m/Rock/a.mp3"))];
        assert_eq!(
            card_paths("Music", &files),
            [
                Some("Music/Rock/b.mp3".into()),
                Some("Music/Rock/a.mp3".into())
            ]
        );
    }

    #[test]
    fn several_roots_share_the_music_folder() {
        let files = [(p("/m1"), p("/m1/a.mp3")), (p("/m2"), p("/m2/A.mp3"))];
        assert_eq!(
            card_paths("Music", &files),
            [Some("Music/a.mp3".into()), Some("Music/A (2).mp3".into())]
        );
    }

    #[test]
    fn the_same_input_gives_the_same_output_in_any_order() {
        let a = [(p("/m"), p("/m/x/A.mp3")), (p("/m"), p("/m/X/a.mp3"))];
        let b = [a[1], a[0]];
        let ra = card_paths("Music", &a);
        let rb = card_paths("Music", &b);
        assert_eq!(ra[0], rb[1]);
        assert_eq!(ra[1], rb[0]);
    }
}
