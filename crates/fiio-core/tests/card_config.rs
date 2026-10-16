//! The `[card]` section of the config: defaults, what validation accepts and
//! refuses, and that unknown keys are an error. Temp folders only.

use std::fs;
use std::path::Path;

use fiio_core::Error;
use fiio_core::config::Config;

/// A config file with the library and playlists folders as `dir` and
/// `extra` appended.
fn load(dir: &Path, extra: &str) -> fiio_core::Result<Config> {
    let file = dir.join("config.toml");
    let d = dir.to_str().unwrap();
    fs::write(
        &file,
        format!("[library]\nroots = [{d:?}]\n[playlists]\ndir = {d:?}\n{extra}"),
    )
    .unwrap();
    Config::load(&file)
}

fn card(dir: &Path, music: &str, playlists: &str) -> fiio_core::Result<Config> {
    // Strings quoted the way TOML wants (`\u00A0`, not Rust's `\u{a0}`).
    let quote = |s: &str| toml::Value::String(s.to_owned()).to_string();
    load(
        dir,
        &format!(
            "[card]\nmusic_dir = {}\nplaylists_dir = {}\n",
            quote(music),
            quote(playlists)
        ),
    )
}

fn refused(r: fiio_core::Result<Config>, field: &str) {
    match r {
        Err(Error::Config { message, .. }) => {
            assert!(message.contains(field), "{message}");
        }
        other => panic!("expected a config error about {field}, got {other:?}"),
    }
}

// Prevents: the defaults drifting from the spec (Music, and the card root
// for playlists), or a config with no [card] table failing to load.
#[test]
fn the_defaults_are_music_and_the_card_root() {
    let t = tempfile::tempdir().unwrap();
    for cfg in [Config::default(), load(t.path(), "").unwrap()] {
        assert_eq!(cfg.card.music_dir, "Music");
        assert_eq!(cfg.card.playlists_dir, "");
    }
    // A table with one key keeps the default of the other.
    let c = load(t.path(), "[card]\nmusic_dir = \"Audio\"\n").unwrap();
    assert_eq!(
        (c.card.music_dir.as_str(), c.card.playlists_dir.as_str()),
        ("Audio", "")
    );
    let c = load(t.path(), "[card]\nplaylists_dir = \"Playlists\"\n").unwrap();
    assert_eq!(
        (c.card.music_dir.as_str(), c.card.playlists_dir.as_str()),
        ("Music", "Playlists")
    );
    let c = load(t.path(), "[card]\n").unwrap();
    assert_eq!(c.card, Config::default().card);
}

// Prevents: legitimate folders on the card being refused.
#[test]
fn plain_relative_folders_are_accepted() {
    let t = tempfile::tempdir().unwrap();
    for (music, playlists) in [
        ("Music", ""),
        ("Audio/Library", "Playlists"),
        ("Musique \u{e9}t\u{e9}", "Listes/Mes listes"),
        ("a", "b/c/d"),
        ("日本語", ""),
        ("Music.old", "x y"),
    ] {
        let c = card(t.path(), music, playlists)
            .unwrap_or_else(|e| panic!("{music:?}, {playlists:?}: {e}"));
        assert_eq!(
            (c.card.music_dir.as_str(), c.card.playlists_dir.as_str()),
            (music, playlists)
        );
    }
}

// Prevents: a music folder that would write outside the card, or to its
// root, or to a folder no FAT card can hold: absolute, empty, `.`/`..`,
// backslashes, empty segments, padded segments.
#[test]
fn music_dir_refuses_what_is_not_a_plain_relative_folder() {
    let t = tempfile::tempdir().unwrap();
    for bad in [
        "",
        "/",
        "/Music",
        "Music/",
        "/Music/",
        ".",
        "..",
        "./Music",
        "Music/.",
        "../Music",
        "Music/..",
        "a/../b",
        "a/./b",
        "a//b",
        "//a",
        "Music\\Rock",
        "\\Music",
        "a\\",
        " Music",
        "Music ",
        "a/ b",
        "a/b /c",
        "\tMusic",
        "Music\n",
        "\u{a0}Music",
    ] {
        refused(card(t.path(), bad, ""), "card.music_dir");
    }
}

// Prevents: the same, for the playlists folder, where the empty string is
// allowed (and means the card root).
#[test]
fn playlists_dir_refuses_the_same_but_may_be_empty() {
    let t = tempfile::tempdir().unwrap();
    card(t.path(), "Music", "").unwrap();
    for bad in [
        "/",
        "/Playlists",
        "Playlists/",
        ".",
        "..",
        "./P",
        "P/..",
        "a/../b",
        "a//b",
        "P\\Q",
        " P",
        "P ",
        "a/ b",
        "a/b /c",
        "\tP",
    ] {
        refused(card(t.path(), "Music", bad), "card.playlists_dir");
    }
}

// Prevents: a mistyped key (`music-dir`, `playlist_dir`) silently ignored,
// so the defaults apply and the files go somewhere the owner did not choose.
#[test]
fn unknown_keys_in_the_card_section_are_refused() {
    let t = tempfile::tempdir().unwrap();
    for extra in [
        "[card]\nmusic_directory = \"Music\"\n",
        "[card]\nplaylist_dir = \"P\"\n",
        "[card]\nmusic_dir = \"Music\"\nextra = 1\n",
    ] {
        match load(t.path(), extra) {
            Err(Error::Config { message, .. }) => {
                assert!(message.contains("unknown field"), "{message}")
            }
            other => panic!("expected a config error for {extra:?}, got {other:?}"),
        }
    }
}

// Prevents: the section being dropped or altered by a save/load round trip
// (`init --force`, the window), which would silently reset the folders.
#[test]
fn the_card_section_survives_a_save_and_load() {
    let t = tempfile::tempdir().unwrap();
    let mut cfg = load(t.path(), "").unwrap();
    cfg.card.music_dir = "Audio/Library".into();
    cfg.card.playlists_dir = "Listes".into();
    let file = t.path().join("saved.toml");
    cfg.save(&file).unwrap();
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.contains("[card]"), "{text}");
    let back = Config::load(&file).unwrap();
    assert_eq!(back, cfg);
    assert_eq!(back.card.music_dir, "Audio/Library");
}

// Prevents: a folder name FAT cannot hold, or would rewrite, passing
// validation (and then every file written there ending up elsewhere than
// the plan says): each segment must be what `naming::clean` makes of it,
// and at most 255 UTF-16 units.
#[test]
fn segments_must_be_names_fat_holds_as_they_are() {
    let t = tempfile::tempdir().unwrap();
    let a255 = "a".repeat(255);
    let a256 = "a".repeat(256);
    let astral127 = "\u{1d11e}".repeat(127); // 254 units
    let astral128 = "\u{1d11e}".repeat(128); // 256 units
    for ok in [
        a255.as_str(),
        astral127.as_str(),
        "Music/Ab",
        "x.y",
        ".hidden",
    ] {
        card(t.path(), ok, ok).unwrap_or_else(|e| panic!("{:.20}: {e}", ok));
    }
    for bad in [
        "Mu:sic",
        "Music.",
        "Mus?c",
        "Mu*sic",
        "Mu\"sic",
        "Mu<sic",
        "Mu>sic",
        "Mu|sic",
        "Mu\u{1}sic",
        "Mu\tsic",
        "a/Mu:sic",
        "Music./a",
        "a/Music.",
        "Cafe\u{301}",
        a256.as_str(),
        astral128.as_str(),
    ] {
        refused(card(t.path(), bad, ""), "card.music_dir");
        refused(card(t.path(), "Music", bad), "card.playlists_dir");
    }
}
