mod common;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use common::{
    Env, Kind, chmod_000_blocks_reads, fairplay_m4a, fixture, fixture_secs, id3v24_text,
    mp3_with_id3v2, rich_fixture,
};
use fiio_core::Error;
use fiio_core::tags::{self, FileKind};

const NFC: &str = "J\u{f3}hann.mp3";
const NFD: &str = "Jo\u{301}hann.mp3";

fn bump_mtime(p: &std::path::Path) {
    let f = fs::File::options().write(true).open(p).unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(120))
        .unwrap();
}

fn only_track_ids(env: &Env) -> Vec<(i64, PathBuf, bool)> {
    env.tracks()
        .into_iter()
        .map(|t| (t.id, t.path, t.missing_since.is_some()))
        .collect()
}

#[test]
fn first_scan_adds_everything_and_second_finds_nothing_changed() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.add("Album/b.m4a", Kind::Aac, 2);

    let r = env.scan();
    assert_eq!((r.added, r.unchanged, r.updated), (2, 0, 0));
    let t = env.track(&a);
    assert_eq!(t.tags.title.as_deref(), Some("Title1"));
    assert_eq!(t.tags.artist.as_deref(), Some("Artist1"));
    assert_eq!(t.info.kind, FileKind::Mp3);
    assert_eq!(t.size, fs::metadata(&a).unwrap().len());
    assert_eq!(t.hash, *blake3::hash(&fs::read(&a).unwrap()).as_bytes());

    let ids = only_track_ids(&env);
    let r = env.scan();
    assert_eq!((r.added, r.unchanged, r.updated), (0, 2, 0));
    assert!(r.moved.is_empty() && r.missing.is_empty() && r.unreadable.is_empty());
    assert_eq!(only_track_ids(&env), ids);
}

#[test]
fn each_supported_format_gets_the_right_kind() {
    let mut env = Env::new();
    let cases = [
        ("a.mp3", Kind::Mp3, FileKind::Mp3),
        ("b.m4a", Kind::Aac, FileKind::Aac),
        ("c.m4a", Kind::Alac, FileKind::Alac),
        ("d.wav", Kind::Wav, FileKind::Wav),
        ("e.aiff", Kind::Aiff, FileKind::Aiff),
    ];
    for (i, (name, kind, _)) in cases.iter().enumerate() {
        env.add(name, *kind, i as u32);
    }
    let r = env.scan();
    assert_eq!(r.added, 5);
    assert!(r.unsupported.is_empty() && r.unreadable.is_empty());
    for (name, _, expected) in cases {
        assert_eq!(
            env.track(&env.music.join(name)).info.kind,
            expected,
            "{name}"
        );
    }
}

#[test]
fn uppercase_extension_is_indexed() {
    let mut env = Env::new();
    env.add("LOUD.MP3", Kind::Mp3, 1);
    assert_eq!(env.scan().added, 1);
}

#[test]
fn rewritten_file_is_updated_with_same_id() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    let before = env.track(&a);

    fs::copy(fixture(Kind::Mp3, 2), &a).unwrap();
    let r = env.scan();
    assert_eq!((r.updated, r.added, r.unchanged), (1, 0, 0));
    let after = env.track(&a);
    assert_eq!(after.id, before.id);
    assert_ne!(after.hash, before.hash);
    assert_eq!(after.tags.title.as_deref(), Some("Title2"));
    assert_eq!(env.tracks().len(), 1);
}

#[test]
fn touched_file_with_same_content_is_reread_not_added() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    let before = env.track(&a);
    bump_mtime(&a);
    let r = env.scan();
    assert_eq!((r.updated, r.added, r.moved.len()), (1, 0, 0));
    let after = env.track(&a);
    assert_eq!(after.id, before.id);
    assert_ne!(after.mtime_ns, before.mtime_ns);
    // and it settles again
    assert_eq!(env.scan().unchanged, 1);
}

#[test]
fn renamed_file_keeps_its_track_id_and_playlists_follow() {
    let mut env = Env::new();
    let old = env.add("a.mp3", Kind::Mp3, 1);
    let other = env.add("b.mp3", Kind::Mp3, 2);
    env.scan();
    let id = env.track(&old).id;
    env.write_playlist(
        "mix",
        &format!(
            "#EXTM3U\n#EXTINF:1,Artist1 - Title1\n{}\n{}\n/elsewhere/ghost.mp3\n",
            old.display(),
            other.display()
        ),
    );
    env.write_playlist("untouched", &format!("#EXTM3U\n{}\n", other.display()));
    let untouched_before = env.read_playlist("untouched");
    env.scan();

    let new = env.music.join("Renamed/a2.mp3");
    fs::create_dir(env.music.join("Renamed")).unwrap();
    fs::rename(&old, &new).unwrap();
    let r = env.scan();

    assert_eq!(r.moved, vec![(old.clone(), new.clone())]);
    assert_eq!((r.added, r.missing.len()), (0, 0));
    assert_eq!(r.rewritten_playlists, vec!["mix".to_string()]);
    let t = env.track(&new);
    assert_eq!(t.id, id);
    assert!(t.missing_since.is_none());
    assert_eq!(env.tracks().len(), 2, "no duplicate row for the old path");
    assert_eq!(
        env.locations("mix"),
        vec![
            new.to_str().unwrap().to_owned(),
            other.to_str().unwrap().to_owned(),
            "/elsewhere/ghost.mp3".to_owned()
        ]
    );
    assert!(!env.read_playlist("mix").contains("a.mp3"));
    assert_eq!(env.read_playlist("untouched"), untouched_before);
    // The index agrees: first entry now resolves to the same track id.
    let items = env.indexed_items("mix");
    assert_eq!(items[0].1, Some(id));
    assert_eq!(items[2].1, None);
}

#[test]
fn moved_file_rewrites_relative_playlist_entries() {
    let mut env = Env::new().relative_playlists();
    let old = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    env.write_playlist("rel", "#EXTM3U\n../music/a.mp3\n");
    env.scan();
    let new = env.music.join("sub/a.mp3");
    fs::create_dir(env.music.join("sub")).unwrap();
    fs::rename(&old, &new).unwrap();
    let r = env.scan();
    assert_eq!(r.rewritten_playlists, vec!["rel".to_string()]);
    assert_eq!(env.locations("rel"), vec!["../music/sub/a.mp3".to_string()]);
}

#[test]
fn deleted_file_is_marked_missing_and_comes_back_with_same_id() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.add("b.mp3", Kind::Mp3, 2);
    env.scan();
    let id = env.track(&a).id;
    env.write_playlist("p", &format!("#EXTM3U\n{}\n", a.display()));
    env.scan();
    let saved = fs::read(&a).unwrap();
    fs::remove_file(&a).unwrap();

    let r = env.scan();
    assert_eq!(r.missing, vec![a.clone()]);
    assert_eq!(r.unchanged, 1);
    let t = env.track(&a);
    assert_eq!(t.id, id);
    assert!(t.missing_since.is_some());
    // The playlist file keeps the line, the index no longer resolves it.
    assert_eq!(env.locations("p"), vec![a.to_str().unwrap().to_owned()]);
    assert_eq!(env.indexed_items("p")[0].1, None);
    assert_eq!(r.playlists.unresolved.len(), 1);

    // A second scan does not report it missing again.
    assert!(env.scan().missing.is_empty());

    fs::write(&a, saved).unwrap();
    let r = env.scan();
    assert_eq!((r.returned, r.added, r.updated), (1, 0, 0));
    let t = env.track(&a);
    assert_eq!(t.id, id);
    assert!(t.missing_since.is_none());
    assert_eq!(env.indexed_items("p")[0].1, Some(id));
    assert_eq!(env.tracks().len(), 2);
}

#[test]
fn two_new_copies_of_a_vanished_file_are_not_a_move() {
    let mut env = Env::new();
    let old = env.add("a.mp3", Kind::Mp3, 1);
    env.add("keep.mp3", Kind::Mp3, 9);
    env.scan();
    let id = env.track(&old).id;
    env.write_playlist("p", &format!("#EXTM3U\n{}\n", old.display()));
    env.scan();
    let playlist_before = env.read_playlist("p");

    fs::remove_file(&old).unwrap();
    let c1 = env.add("copy1.mp3", Kind::Mp3, 1);
    let c2 = env.add("copy2.mp3", Kind::Mp3, 1);
    let r = env.scan();

    assert!(r.moved.is_empty());
    assert_eq!((r.added, r.missing.clone()), (2, vec![old.clone()]));
    assert!(r.rewritten_playlists.is_empty());
    assert!(env.track(&old).missing_since.is_some());
    assert_eq!(env.track(&old).id, id);
    assert_ne!(env.track(&c1).id, id);
    assert_ne!(env.track(&c2).id, id);
    assert_eq!(env.read_playlist("p"), playlist_before);
}

#[test]
fn two_vanished_duplicates_and_one_new_copy_is_not_a_move() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    let b = env.add("b.mp3", Kind::Mp3, 1);
    env.add("keep.mp3", Kind::Mp3, 9);
    env.scan();
    fs::remove_file(&a).unwrap();
    fs::remove_file(&b).unwrap();
    let c = env.add("c.mp3", Kind::Mp3, 1);
    let r = env.scan();
    assert!(r.moved.is_empty());
    assert_eq!(r.added, 1);
    assert_eq!(r.missing.len(), 2);
    assert!(env.track(&a).missing_since.is_some());
    assert!(env.track(&b).missing_since.is_some());
    assert!(env.track(&c).missing_since.is_none());
}

#[test]
fn garbage_mp3_is_reported_and_not_added() {
    let mut env = Env::new();
    env.add("ok.mp3", Kind::Mp3, 1);
    let bad = env.music.join("bad.mp3");
    fs::write(&bad, b"this is not audio at all, just text").unwrap();
    let r = env.scan();
    assert_eq!(r.added, 1);
    assert_eq!(r.unreadable.len(), 1);
    assert_eq!(r.unreadable[0].0, bad);
    assert!(env.tracks().iter().all(|t| t.path != bad));
}

#[test]
fn indexed_file_that_turns_unreadable_is_not_marked_missing() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.add("b.mp3", Kind::Mp3, 2);
    env.scan();
    let before = env.track(&a);
    fs::write(&a, b"corrupted").unwrap();

    let r = env.scan();
    assert_eq!(r.unreadable.len(), 1);
    assert_eq!(r.unreadable[0].0, a);
    assert!(r.missing.is_empty(), "a corrupt file is not a missing file");
    let after = env.track(&a);
    assert!(after.missing_since.is_none());
    assert_eq!(after.id, before.id);
    assert_eq!(after.hash, before.hash, "old record kept until readable");
}

#[test]
fn unsupported_formats_are_reported_and_not_indexed() {
    let mut env = Env::new();
    env.add("ok.mp3", Kind::Mp3, 1);
    env.add("song.flac", Kind::Flac, 2);
    env.add("hidden-flac.m4a", Kind::FlacInM4a, 3);
    env.add("opus.m4a", Kind::OpusInM4a, 4);
    let r = env.scan();
    assert_eq!(r.added, 2, "the X3 plays FLAC");
    let mut names: Vec<_> = r
        .unsupported
        .iter()
        .map(|(p, _)| p.file_name().unwrap().to_str().unwrap().to_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["hidden-flac.m4a", "opus.m4a"]);
    assert!(r.unreadable.is_empty(), "{:?}", r.unreadable);
    assert_eq!(env.tracks().len(), 2);
}

#[test]
fn indexed_m4a_replaced_by_unsupported_codec_is_not_marked_missing() {
    let mut env = Env::new();
    let a = env.add("a.m4a", Kind::Aac, 1);
    env.add("b.mp3", Kind::Mp3, 2);
    env.scan();
    fs::copy(fixture(Kind::FlacInM4a, 1), &a).unwrap();
    let r = env.scan();
    assert_eq!(r.unsupported.len(), 1);
    assert!(r.missing.is_empty());
    assert!(env.track(&a).missing_since.is_none());
}

#[test]
fn nfd_file_resolves_from_nfc_playlist_entry_and_back() {
    let mut env = Env::new();
    let nfd_file = env.add(NFD, Kind::Mp3, 1);
    let nfc_file = env.add("Caf\u{e9}.mp3", Kind::Mp3, 2);
    assert_ne!(NFC, NFD);
    // The on-disk spelling is kept exactly.
    let r = env.scan();
    assert_eq!(r.added, 2);
    assert!(env.tracks().iter().any(|t| t.path == nfd_file));

    let dir = env.music.display().to_string();
    env.write_playlist(
        "unicode",
        &format!("#EXTM3U\n{dir}/{NFC}\n{dir}/Cafe\u{301}.mp3\n"),
    );
    let r = env.scan();
    assert!(
        r.playlists.unresolved.is_empty(),
        "{:?}",
        r.playlists.unresolved
    );
    let items = env.indexed_items("unicode");
    assert_eq!(items[0].1, Some(env.track(&nfd_file).id));
    assert_eq!(items[1].1, Some(env.track(&nfc_file).id));
    // Entries are kept as the user wrote them until the playlist is edited.
    assert!(
        env.read_playlist("unicode")
            .contains(&format!("{dir}/{NFC}"))
    );
}

#[test]
fn hidden_folders_and_files_are_skipped() {
    let mut env = Env::new();
    env.add("shown.mp3", Kind::Mp3, 1);
    env.add(".Trash/gone.mp3", Kind::Mp3, 2);
    env.add("Album/.hidden.mp3", Kind::Mp3, 3);
    env.add(".a/.b/deep.mp3", Kind::Mp3, 4);
    let r = env.scan();
    assert_eq!(r.added, 1);
    assert_eq!(env.tracks().len(), 1);
}

#[test]
fn excluded_extensions_are_skipped_case_insensitively() {
    let mut env = Env::new();
    env.cfg.library.exclude_extensions = vec!["WAV".into()];
    env.add("a.mp3", Kind::Mp3, 1);
    env.add("b.wav", Kind::Wav, 2);
    env.add("c.flac", Kind::Flac, 3);
    env.cfg.library.exclude_extensions.push("flac".into());
    let r = env.scan();
    assert_eq!(r.added, 1);
    assert!(r.unsupported.is_empty(), "excluded beats unsupported");
    assert_eq!(env.tracks().len(), 1);
}

#[test]
fn non_audio_files_are_ignored_silently() {
    let mut env = Env::new();
    env.add("a.mp3", Kind::Mp3, 1);
    fs::write(env.music.join("cover.jpg"), b"x").unwrap();
    fs::write(env.music.join("notes.txt"), b"x").unwrap();
    let r = env.scan();
    assert_eq!(r.added, 1);
    assert!(r.unsupported.is_empty() && r.unreadable.is_empty());
}

// Safety guards.

#[test]
fn missing_root_is_an_error_and_changes_nothing() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    let before = only_track_ids(&env);
    let moved_away = env.music.with_file_name("music-gone");
    fs::rename(&env.music, &moved_away).unwrap();

    let err = env.try_scan().unwrap_err();
    assert!(
        matches!(&err, Error::RootMissing(p) if *p == env.music),
        "{err:?}"
    );
    assert_eq!(only_track_ids(&env), before);
    assert!(env.track(&a).missing_since.is_none());
}

#[test]
fn one_missing_root_among_several_aborts_the_whole_scan() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    let second = env.music.with_file_name("second");
    env.cfg.library.roots.push(second.clone());
    let err = env.try_scan().unwrap_err();
    assert!(
        matches!(&err, Error::RootMissing(p) if *p == second),
        "{err:?}"
    );
    assert!(env.track(&a).missing_since.is_none());
}

#[test]
fn empty_library_with_indexed_tracks_is_refused_and_nothing_is_marked_missing() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.add("b.mp3", Kind::Mp3, 2);
    env.scan();
    fs::remove_file(&a).unwrap();
    fs::remove_file(env.music.join("b.mp3")).unwrap();

    let err = env.try_scan().unwrap_err();
    assert!(matches!(err, Error::EmptyRoot { tracks: 2, .. }), "{err:?}");
    assert!(env.tracks().iter().all(|t| t.missing_since.is_none()));
}

#[test]
fn empty_library_with_nothing_indexed_is_fine() {
    let mut env = Env::new();
    let r = env.scan();
    assert_eq!((r.added, r.unchanged, r.missing.len()), (0, 0, 0));
}

#[test]
fn empty_library_when_everything_is_already_missing_is_fine() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    fiio_core::store::set_missing(env.store.conn(), env.track(&a).id, Some(5)).unwrap();
    fs::remove_file(&a).unwrap();
    let r = env.scan();
    assert!(r.missing.is_empty() && r.added == 0);
    assert_eq!(env.track(&a).missing_since, Some(5), "timestamp kept");
}

#[test]
fn unreadable_subfolder_does_not_mark_its_tracks_missing() {
    let mut env = Env::new();
    let inside = env.add("locked/a.mp3", Kind::Mp3, 1);
    let outside = env.add("open/b.mp3", Kind::Mp3, 2);
    env.add("locked/deep/c.mp3", Kind::Mp3, 3);
    env.scan();
    let locked = env.music.join("locked");
    if !chmod_000_blocks_reads(&locked) {
        eprintln!("skipping: permissions are not enforced (running as root?)");
        return;
    }
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let result = env.try_scan();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let r = result.unwrap();
    assert!(r.missing.is_empty(), "{:?}", r.missing);
    assert!(r.unreadable.iter().any(|(p, _)| *p == locked));
    assert!(env.track(&inside).missing_since.is_none());
    assert!(
        env.track(&env.music.join("locked/deep/c.mp3"))
            .missing_since
            .is_none()
    );
    assert!(env.track(&outside).missing_since.is_none());
    // Once readable again, nothing has changed.
    let r = env.scan();
    assert_eq!((r.unchanged, r.added, r.updated), (3, 0, 0));
}

#[test]
fn files_deleted_elsewhere_are_still_missing_when_another_folder_is_unreadable() {
    let mut env = Env::new();
    env.add("locked/a.mp3", Kind::Mp3, 1);
    let gone = env.add("open/b.mp3", Kind::Mp3, 2);
    env.add("open/c.mp3", Kind::Mp3, 3);
    env.scan();
    let locked = env.music.join("locked");
    if !chmod_000_blocks_reads(&locked) {
        return;
    }
    fs::remove_file(&gone).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let result = env.try_scan();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(result.unwrap().missing, vec![gone]);
}

// Move detection across a missing period.

#[test]
fn file_that_was_missing_and_reappears_elsewhere_is_a_move() {
    let mut env = Env::new();
    let old = env.add("a.mp3", Kind::Mp3, 1);
    env.add("keep.mp3", Kind::Mp3, 9);
    env.scan();
    let id = env.track(&old).id;
    env.write_playlist("p", &format!("#EXTM3U\n{}\n", old.display()));
    env.scan();
    fs::remove_file(&old).unwrap();
    assert_eq!(env.scan().missing, vec![old.clone()]);
    assert!(env.track(&old).missing_since.is_some());

    let new = env.add("Elsewhere/a.mp3", Kind::Mp3, 1);
    let r = env.scan();
    assert_eq!(r.moved, vec![(old.clone(), new.clone())]);
    assert_eq!((r.added, r.returned, r.missing.len()), (0, 0, 0));
    assert_eq!(r.rewritten_playlists, vec!["p".to_string()]);
    let t = env.track(&new);
    assert_eq!(t.id, id);
    assert!(t.missing_since.is_none());
    assert_eq!(env.tracks().len(), 2);
    assert_eq!(env.locations("p"), vec![s_path(&new)]);
    assert_eq!(env.indexed_items("p")[0].1, Some(id));
}

fn s_path(p: &std::path::Path) -> String {
    p.to_str().unwrap().to_owned()
}

#[test]
fn missing_track_and_two_new_copies_is_not_a_move() {
    let mut env = Env::new();
    let old = env.add("a.mp3", Kind::Mp3, 1);
    env.add("keep.mp3", Kind::Mp3, 9);
    env.scan();
    fs::remove_file(&old).unwrap();
    env.scan();
    let id = env.track(&old).id;
    let c1 = env.add("c1.mp3", Kind::Mp3, 1);
    let c2 = env.add("c2.mp3", Kind::Mp3, 1);
    let r = env.scan();
    assert!(r.moved.is_empty());
    assert_eq!(r.added, 2);
    assert!(env.track(&old).missing_since.is_some());
    assert_eq!(env.track(&old).id, id);
    assert_ne!(env.track(&c1).id, id);
    assert_ne!(env.track(&c2).id, id);
}

#[test]
fn two_missing_duplicates_and_one_new_copy_is_not_a_move() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    let b = env.add("b.mp3", Kind::Mp3, 1);
    env.add("keep.mp3", Kind::Mp3, 9);
    env.scan();
    fs::remove_file(&a).unwrap();
    fs::remove_file(&b).unwrap();
    env.scan();
    let c = env.add("c.mp3", Kind::Mp3, 1);
    let r = env.scan();
    assert!(r.moved.is_empty());
    assert_eq!(r.added, 1);
    assert!(env.track(&a).missing_since.is_some());
    assert!(env.track(&b).missing_since.is_some());
    assert!(env.track(&c).missing_since.is_none());
}

#[test]
fn newly_vanished_and_already_missing_with_same_content_are_ambiguous() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    let b = env.add("b.mp3", Kind::Mp3, 1);
    env.add("keep.mp3", Kind::Mp3, 9);
    env.scan();
    fs::remove_file(&a).unwrap();
    env.scan();
    fs::remove_file(&b).unwrap();
    let c = env.add("c.mp3", Kind::Mp3, 1);
    let r = env.scan();
    assert!(r.moved.is_empty());
    assert_eq!((r.added, r.missing.clone()), (1, vec![b.clone()]));
    assert!(env.track(&a).missing_since.is_some());
    assert!(env.track(&c).missing_since.is_none());
}

// Per-root guard.

#[test]
fn one_emptied_root_is_refused_even_when_another_root_has_files() {
    let mut env = Env::new();
    let second = env.music.with_file_name("second");
    fs::create_dir(&second).unwrap();
    env.cfg.library.roots.push(second.clone());
    let a = env.add("a.mp3", Kind::Mp3, 1);
    let b = env.add_at(&second, "b.mp3", Kind::Mp3, 2);
    env.add_at(&second, "c.mp3", Kind::Mp3, 3);
    env.scan();
    // The second disk "mounts" as an empty folder; the first loses a file.
    fs::remove_file(&b).unwrap();
    fs::remove_file(second.join("c.mp3")).unwrap();
    env.add("a2.mp3", Kind::Mp3, 4);
    fs::remove_file(&a).unwrap();

    let err = env.try_scan().unwrap_err();
    assert!(
        matches!(&err, Error::EmptyRoot { root, tracks: 2 } if *root == second),
        "{err:?}"
    );
    assert!(env.tracks().iter().all(|t| t.missing_since.is_none()));
    assert_eq!(env.tracks().len(), 3, "a2.mp3 was not indexed either");
}

#[test]
fn empty_root_with_nothing_indexed_under_it_is_fine() {
    let mut env = Env::new();
    let second = env.music.with_file_name("second");
    fs::create_dir(&second).unwrap();
    env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    env.cfg.library.roots.push(second);
    let r = env.scan();
    assert_eq!((r.unchanged, r.added, r.missing.len()), (1, 0, 0));
}

fn tit2(t: &str) -> Vec<u8> {
    id3v24_text("TIT2", 3, t.as_bytes())
}

/// UTF-16 text with an invalid BOM: not even Relaxed parsing accepts the tag.
fn unparseable_tag() -> Vec<Vec<u8>> {
    vec![
        tit2("Lost"),
        id3v24_text("TRCK", 1, &[0x31, 0x00, 0x32, 0x00]),
    ]
}

// Prevents: valid MP3s with a quirky tag landing in "unreadable" and never
// being indexed.
#[test]
fn file_with_unparseable_tags_is_indexed_without_tags_and_reported() {
    let mut env = Env::new();
    let good = env.add("good.mp3", Kind::Mp3, 1);
    let bad = env.add_bytes("bad-tag.mp3", &mp3_with_id3v2(2, &unparseable_tag()));
    let garbage = env.music.join("garbage.mp3");
    fs::write(&garbage, b"this is not audio at all, just text").unwrap();

    let r = env.scan();
    assert_eq!(r.added, 2);
    assert_eq!(r.untagged.len(), 1, "{:?}", r.untagged);
    assert_eq!(r.untagged[0].0, bad);
    assert!(!r.untagged[0].1.is_empty());
    assert_eq!(r.unreadable.len(), 1, "{:?}", r.unreadable);
    assert_eq!(r.unreadable[0].0, garbage, "garbage stays unreadable");
    assert!(r.drm.is_empty() && r.unsupported.is_empty());

    let row = env.track(&bad);
    assert_eq!(row.tags, fiio_core::tags::Tags::default());
    assert_eq!(row.info.kind, FileKind::Mp3);
    assert!(
        row.info.duration_ms.is_some_and(|ms| ms > 900),
        "{:?}",
        row.info
    );
    assert!(row.missing_since.is_none());
    assert_eq!(row.hash, *blake3::hash(&fs::read(&bad).unwrap()).as_bytes());
    assert!(env.tracks().iter().all(|t| t.path != garbage));
    assert!(!r.untagged.iter().any(|(p, _)| *p == good));

    // A file with a tag error is never "unchanged": it is read again.
    let r = env.scan();
    assert_eq!((r.added, r.unchanged, r.updated), (0, 1, 1));
    assert!(r.unreadable.len() == 1 && r.missing.is_empty());
}

// Prevents: a skippable frame being reported as a tag failure, or its file
// losing the tags that did parse.
#[test]
fn file_with_one_skippable_frame_is_indexed_with_its_tags_and_not_reported() {
    let mut env = Env::new();
    let p = env.add_bytes(
        "odd.mp3",
        &mp3_with_id3v2(1, &[tit2("Survivor"), id3v24_text("TDRC", 3, b"notadate")]),
    );
    let r = env.scan();
    assert_eq!(r.added, 1);
    assert!(r.untagged.is_empty() && r.unreadable.is_empty(), "{r:?}");
    assert_eq!(env.track(&p).tags.title.as_deref(), Some("Survivor"));
}

#[test]
fn file_whose_tags_break_later_is_updated_not_lost() {
    let mut env = Env::new();
    let p = env.add_bytes("a.mp3", &mp3_with_id3v2(1, &[tit2("Fine")]));
    env.add("b.mp3", Kind::Mp3, 2);
    env.scan();
    let before = env.track(&p);
    assert_eq!(before.tags.title.as_deref(), Some("Fine"));

    fs::write(&p, mp3_with_id3v2(1, &unparseable_tag())).unwrap();
    let r = env.scan();
    assert_eq!(r.updated, 1);
    assert_eq!(r.untagged.len(), 1);
    assert!(r.unreadable.is_empty() && r.missing.is_empty());
    let after = env.track(&p);
    assert_eq!(after.id, before.id);
    assert_eq!(after.tags.title, None);
    assert!(after.missing_since.is_none());
}

// Prevents: iTunes Store purchases (`drms` sample entry) being rejected as an
// unsupported codec.
#[test]
fn fairplay_m4a_is_indexed_as_aac_and_reported() {
    let mut env = Env::new();
    let store = env.add_bytes("Store/song.m4a", &fairplay_m4a(1));
    let plain = env.add("plain.m4a", Kind::Aac, 2);

    let r = env.scan();
    assert_eq!(r.added, 2);
    assert_eq!(r.drm, vec![store.clone()]);
    assert!(r.unsupported.is_empty() && r.unreadable.is_empty(), "{r:?}");
    assert!(r.untagged.is_empty());
    let row = env.track(&store);
    assert_eq!(row.info.kind, FileKind::Aac);
    assert_eq!(row.tags.title.as_deref(), Some("Title1"));
    assert_eq!(env.track(&plain).info.kind, FileKind::Aac);
}

const GOOD_TRCK_ENC3: &[u8] = b"1/23";

/// Same byte length as the bad frame: UTF-16 text with a bad BOM is 1 + 4 bytes.
fn good_track() -> Vec<u8> {
    id3v24_text("TRCK", 3, GOOD_TRCK_ENC3)
}

fn set_mtime(p: &std::path::Path, t: SystemTime) {
    fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

// Prevents: a file with unparseable tags being treated as unchanged (and so
// never retried or reported again) once its size and mtime match the index.
#[test]
fn untagged_file_is_listed_and_retried_on_every_scan() {
    let mut env = Env::new();
    let bad = env.add_bytes("bad.mp3", &mp3_with_id3v2(2, &unparseable_tag()));
    env.add("ok.mp3", Kind::Mp3, 1);
    env.scan();
    let id = env.track(&bad).id;
    for round in 1..=3 {
        let r = env.scan();
        assert_eq!(
            (r.added, r.unchanged, r.updated),
            (0, 1, 1),
            "round {round}"
        );
        assert_eq!(r.untagged.len(), 1, "round {round}: {:?}", r.untagged);
        assert_eq!(r.untagged[0].0, bad);
        assert!(!r.untagged[0].1.is_empty());
        assert!(r.unreadable.is_empty() && r.missing.is_empty());
        assert_eq!(env.track(&bad).id, id, "same row each time");
    }
}

// Prevents: a repaired file staying flagged forever. The repaired file has the
// same size and mtime as the broken one, so only the retry rule can heal it.
#[test]
fn repairing_the_tags_clears_the_flag_even_with_same_size_and_mtime() {
    let mut env = Env::new();
    let bad_frames = unparseable_tag();
    let mut good_frames = bad_frames.clone();
    good_frames[1] = good_track();
    let broken = mp3_with_id3v2(2, &bad_frames);
    let fixed = mp3_with_id3v2(2, &good_frames);
    assert_eq!(broken.len(), fixed.len(), "fixture precondition");

    let p = env.add_bytes("a.mp3", &broken);
    env.add("b.mp3", Kind::Mp3, 1);
    let when = SystemTime::now() - Duration::from_secs(3600);
    set_mtime(&p, when);
    let r = env.scan();
    assert_eq!(r.untagged.len(), 1);
    let before = env.track(&p);
    assert!(before.tag_error.is_some());
    assert_eq!(before.tags.title, None);

    fs::write(&p, &fixed).unwrap();
    set_mtime(&p, when);
    let r = env.scan();
    assert!(r.untagged.is_empty(), "{:?}", r.untagged);
    assert!(r.unreadable.is_empty());
    let after = env.track(&p);
    assert_eq!(after.id, before.id);
    assert_eq!(after.tag_error, None);
    assert_eq!(after.tags.title.as_deref(), Some("Lost"));
    assert_eq!(after.tags.track_no, Some(1));

    // Healed means unchanged from now on.
    let r = env.scan();
    assert_eq!((r.unchanged, r.updated), (2, 0));
    assert!(r.untagged.is_empty());
}

#[test]
fn repairing_the_tags_with_a_new_mtime_also_clears_the_flag() {
    let mut env = Env::new();
    let p = env.add_bytes("a.mp3", &mp3_with_id3v2(2, &unparseable_tag()));
    assert_eq!(env.scan().untagged.len(), 1);
    fs::write(&p, mp3_with_id3v2(2, &[tit2("Fixed")])).unwrap();
    bump_mtime(&p);
    let r = env.scan();
    assert!(r.untagged.is_empty());
    let t = env.track(&p);
    assert_eq!(t.tag_error, None);
    assert_eq!(t.tags.title.as_deref(), Some("Fixed"));
}

#[test]
fn a_deleted_untagged_file_drops_out_of_the_list() {
    let mut env = Env::new();
    let p = env.add_bytes("a.mp3", &mp3_with_id3v2(2, &unparseable_tag()));
    env.add("b.mp3", Kind::Mp3, 1);
    assert_eq!(env.scan().untagged.len(), 1);
    fs::remove_file(&p).unwrap();
    let r = env.scan();
    assert_eq!(r.missing, vec![p.clone()]);
    assert!(r.untagged.is_empty(), "{:?}", r.untagged);
    // It comes back flagged if the file returns.
    fs::write(&p, mp3_with_id3v2(2, &unparseable_tag())).unwrap();
    let r = env.scan();
    assert_eq!(r.untagged.len(), 1);
    assert!(env.track(&p).missing_since.is_none());
}

// Prevents: the FairPlay list only showing on the scan that first read the file.
#[test]
fn fairplay_file_stays_listed_on_rescans_and_leaves_when_deleted() {
    let mut env = Env::new();
    let store = env.add_bytes("Store/song.m4a", &fairplay_m4a(1));
    let plain = env.add("plain.m4a", Kind::Aac, 2);
    assert_eq!(env.scan().drm, vec![store.clone()]);
    for round in 1..=2 {
        let r = env.scan();
        assert_eq!((r.added, r.unchanged), (0, 2), "round {round}");
        assert_eq!(r.drm, vec![store.clone()], "round {round}");
    }
    assert!(env.track(&store).drm);
    assert!(!env.track(&plain).drm);

    fs::remove_file(&store).unwrap();
    let r = env.scan();
    assert_eq!(r.missing, vec![store.clone()]);
    assert!(r.drm.is_empty(), "{:?}", r.drm);
    assert!(
        env.track(&store).drm,
        "the row remembers, the report skips it"
    );
}

#[test]
fn replacing_a_fairplay_file_with_an_ordinary_one_clears_the_flag() {
    let mut env = Env::new();
    let p = env.add_bytes("a.m4a", &fairplay_m4a(1));
    env.add("b.mp3", Kind::Mp3, 2);
    assert_eq!(env.scan().drm.len(), 1);
    fs::copy(fixture(Kind::Aac, 3), &p).unwrap();
    bump_mtime(&p);
    let r = env.scan();
    assert!(r.drm.is_empty());
    assert!(!env.track(&p).drm);
}

// ---- tags read by an older reader (audit M15)

fn versions(env: &Env) -> Vec<(String, i64)> {
    let mut stmt = env
        .store
        .conn()
        .prepare("SELECT path, tags_version FROM tracks ORDER BY path")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Marks every row as read by the previous tag reader.
fn mark_read_by_older_reader(env: &mut Env) {
    env.store
        .conn()
        .execute(
            "UPDATE tracks SET tags_version = ?1",
            [tags::READER_VERSION - 1],
        )
        .unwrap();
}

fn multi_value_mp3() -> Vec<u8> {
    mp3_with_id3v2(
        1,
        &[
            id3v24_text("TIT2", 3, b"Multi"),
            id3v24_text("TPE1", 3, b"Artist A\0Artist B"),
            id3v24_text("TCON", 3, b"Rock\0Pop"),
        ],
    )
}

// Prevents: a changed tag reader never reaching files already indexed (the
// scan skips them by size and mtime), and a tags-only re-read replacing the
// content hash that move detection depends on.
#[test]
fn rows_read_by_an_older_reader_get_their_tags_read_again_but_keep_their_hash_once() {
    let mut env = Env::new();
    let multi = env.add_bytes("multi.mp3", &multi_value_mp3());
    let plain = env.add("plain.mp3", Kind::Mp3, 2);
    let current = env.add("current.mp3", Kind::Mp3, 3);
    let r = env.scan();
    assert_eq!((r.added, r.updated, r.unchanged), (3, 0, 0));
    assert!(
        versions(&env)
            .iter()
            .all(|(_, v)| *v == tags::READER_VERSION),
        "a new row records the reader that read it: {:?}",
        versions(&env)
    );
    let ids: Vec<i64> = [&multi, &plain, &current]
        .iter()
        .map(|p| env.track(p).id)
        .collect();

    // As the previous reader left them: the multi-value tags were cut to the
    // first value, and the hashes are not the files' (so a re-hash shows).
    mark_read_by_older_reader(&mut env);
    let conn = env.store.conn();
    let mut cut = env.track(&multi).tags;
    cut.artist = Some("Artist A".into());
    cut.genre = Some("Rock".into());
    conn.execute(
        "UPDATE tracks SET artist = 'Artist A', genre = 'Rock', tags_hash = ?1 WHERE path = ?2",
        rusqlite::params![&cut.hash()[..], multi.to_str().unwrap()],
    )
    .unwrap();
    for (k, p) in [&multi, &plain, &current].iter().enumerate() {
        conn.execute(
            "UPDATE tracks SET hash = ?1 WHERE path = ?2",
            rusqlite::params![&[k as u8 + 1; 32][..], p.to_str().unwrap()],
        )
        .unwrap();
    }
    // One row was already read by the current reader, whatever its tags say.
    conn.execute(
        "UPDATE tracks SET tags_version = ?1, title = 'Marker' WHERE path = ?2",
        rusqlite::params![tags::READER_VERSION, current.to_str().unwrap()],
    )
    .unwrap();
    let marker_hash = env.track(&current).tags_hash;
    let versions_before: Vec<i64> = versions(&env).into_iter().map(|(_, v)| v).collect();
    assert_eq!(
        versions_before
            .iter()
            .filter(|v| **v == tags::READER_VERSION - 1)
            .count(),
        2,
        "set up"
    );

    let r = env.scan();
    assert_eq!(
        (r.updated, r.unchanged, r.added, r.returned),
        (2, 1, 0, 0),
        "{r:?}"
    );
    let t = env.track(&multi);
    assert_eq!(t.tags.artist.as_deref(), Some("Artist A/Artist B"));
    assert_eq!(t.tags.genre.as_deref(), Some("Rock/Pop"));
    assert_eq!(t.tags_hash, t.tags.hash(), "the tag hash follows the tags");
    assert_ne!(t.tags_hash, cut.hash());
    assert_eq!(t.hash, [1; 32], "the content hash is kept");
    assert_eq!(env.track(&plain).hash, [2; 32], "the content hash is kept");
    assert_eq!(env.track(&plain).tags.title.as_deref(), Some("Title2"));
    // The row of the current reader was not read again.
    let c = env.track(&current);
    assert_eq!(c.tags.title.as_deref(), Some("Marker"));
    assert_eq!(c.tags_hash, marker_hash);
    assert_eq!(c.hash, [3; 32]);
    // Same rows, all at the current version.
    assert_eq!(
        [&multi, &plain, &current].map(|p| env.track(p).id).to_vec(),
        ids
    );
    assert!(
        versions(&env)
            .iter()
            .all(|(_, v)| *v == tags::READER_VERSION),
        "{:?}",
        versions(&env)
    );

    // Once: the next scan finds everything unchanged.
    let r = env.scan();
    assert_eq!((r.updated, r.unchanged, r.added), (0, 3, 0), "{r:?}");
}

// Prevents: a stale row whose file also changed being read tags-only, which
// would keep the hash of the old content.
#[test]
fn a_stale_row_whose_file_changed_is_read_in_full_and_gets_the_new_hash() {
    let mut env = Env::new();
    let p = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    mark_read_by_older_reader(&mut env);
    env.store
        .conn()
        .execute("UPDATE tracks SET hash = ?1", [&[9u8; 32][..]])
        .unwrap();
    fs::copy(fixture(Kind::Mp3, 5), &p).unwrap();
    bump_mtime(&p);
    let r = env.scan();
    assert_eq!((r.updated, r.unchanged, r.added), (1, 0, 0), "{r:?}");
    let t = env.track(&p);
    assert_eq!(t.tags.title.as_deref(), Some("Title5"));
    assert_ne!(t.hash, [9; 32], "the content changed, so did its hash");
    // The hash is the new file's: the same file in a fresh index.
    let mut other = Env::new();
    let q = other.add("a.mp3", Kind::Mp3, 5);
    other.scan();
    assert_eq!(t.hash, other.track(&q).hash);
    assert_eq!(versions(&env)[0].1, tags::READER_VERSION);
}

// Prevents: FLAC and Ogg Vorbis files being dropped or read as another kind,
// or indexed without the tags the X3's playlists are matched on.
#[test]
fn flac_and_ogg_vorbis_are_indexed_with_their_kind_and_tags() {
    let mut env = Env::new();
    let flac = env.add("a.flac", Kind::Flac, 1);
    let ogg = env.add("Album/b.ogg", Kind::Ogg, 2);
    let r = env.scan();
    assert_eq!(r.added, 2);
    assert!(r.unsupported.is_empty() && r.unreadable.is_empty(), "{r:?}");
    let f = env.track(&flac);
    assert_eq!(f.info.kind, FileKind::Flac);
    assert_eq!(f.tags.title.as_deref(), Some("Title1"));
    assert_eq!(f.tags.artist.as_deref(), Some("Artist1"));
    assert_eq!(f.hash, *blake3::hash(&fs::read(&flac).unwrap()).as_bytes());
    let o = env.track(&ogg);
    assert_eq!(o.info.kind, FileKind::Ogg);
    assert_eq!(o.tags.title.as_deref(), Some("Title2"));
    assert_eq!(o.tags.artist.as_deref(), Some("Artist2"));
    // The second scan finds both unchanged.
    let r = env.scan();
    assert_eq!((r.added, r.unchanged, r.updated), (0, 2, 0));
}

// Prevents: Opus (and the other formats the X3 does not play) being indexed,
// or silently ignored instead of reported.
#[test]
fn opus_and_other_unplayable_extensions_are_reported_not_indexed() {
    let mut env = Env::new();
    env.add("ok.flac", Kind::Flac, 1);
    env.add("song.opus", Kind::Opus, 2);
    for name in ["a.wma", "b.mpc", "c.wv", "d.dsf", "e.dff"] {
        env.add_bytes(name, b"not read, only the extension counts");
    }
    let r = env.scan();
    assert_eq!(r.added, 1);
    let mut got: Vec<_> = r
        .unsupported
        .iter()
        .map(|(p, ext)| {
            (
                p.file_name().unwrap().to_str().unwrap().to_owned(),
                ext.clone(),
            )
        })
        .collect();
    got.sort();
    let want: Vec<(String, String)> = [
        ("a.wma", "wma"),
        ("b.mpc", "mpc"),
        ("c.wv", "wv"),
        ("d.dsf", "dsf"),
        ("e.dff", "dff"),
        ("song.opus", "opus"),
    ]
    .iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect();
    assert_eq!(got, want);
    assert_eq!(env.tracks().len(), 1);
}

// Prevents: a card fiio-tunes wrote, under a library root, being indexed as
// library tracks (its copies would then be treated as the library's files).
#[test]
fn folder_holding_a_fiio_tunes_manifest_is_skipped_and_reported() {
    let mut env = Env::new();
    let keep = env.add("Album/keep.mp3", Kind::Mp3, 1);
    env.add("card/Music/copy.mp3", Kind::Mp3, 2);
    env.add_bytes("card/.fiio-tunes/manifest.json", b"{}");
    let r = env.scan();
    assert_eq!(r.added, 1);
    assert_eq!(r.devices, [env.music.join("card")]);
    assert_eq!(
        env.tracks().iter().map(|t| &t.path).collect::<Vec<_>>(),
        [&keep]
    );
}

// Prevents: tracks indexed before a card appeared under the root being marked
// missing (and their playlist entries lost) once the folder is skipped.
#[test]
fn skipped_card_folder_does_not_mark_earlier_indexed_tracks_missing() {
    let mut env = Env::new();
    let under = env.add("card/old.mp3", Kind::Mp3, 1);
    env.add("other.mp3", Kind::Mp3, 2);
    env.scan();
    let before = env.track(&under);
    assert!(before.missing_since.is_none());

    env.add_bytes("card/.fiio-tunes/manifest.json", b"{}");
    let r = env.scan();
    assert_eq!(r.devices, [env.music.join("card")]);
    assert!(r.missing.is_empty(), "{:?}", r.missing);
    assert_eq!(r.added, 0);
    let after = env.track(&under);
    assert!(after.missing_since.is_none());
    assert_eq!(after.id, before.id);
}

// Prevents: the iPod rule being lost when the manifest rule was added.
#[test]
fn folder_holding_an_itunesdb_is_still_skipped_and_reported() {
    let mut env = Env::new();
    env.add("a.mp3", Kind::Mp3, 1);
    env.add("ipod/iPod_Control/Music/F00/X.mp3", Kind::Mp3, 2);
    env.add_bytes("ipod/iPod_Control/iTunes/iTunesDB", b"x");
    let r = env.scan();
    assert_eq!(r.added, 1);
    assert_eq!(r.devices, [env.music.join("ipod")]);
}

// Prevents: lookalike markers hiding a folder: the manifest must be a file
// named exactly `.fiio-tunes/manifest.json` in the folder itself.
#[test]
fn lookalike_markers_do_not_hide_a_folder() {
    let mut env = Env::new();
    env.add_bytes("a/manifest.json", b"{}"); // not inside .fiio-tunes
    env.add("a/song.mp3", Kind::Mp3, 1);
    fs::create_dir_all(env.music.join("c/.fiio-tunes/manifest.json")).unwrap(); // a directory
    env.add("c/song.mp3", Kind::Mp3, 2);
    env.add_bytes("d/.fiio-tunes/other.json", b"{}"); // another file
    env.add("d/song.mp3", Kind::Mp3, 3);
    let r = env.scan();
    assert_eq!(r.added, 3);
    assert!(r.devices.is_empty(), "{:?}", r.devices);
}

// Prevents: FLAC and Ogg Vorbis tags other than title and artist (album,
// numbers, genre, compilation flag) being dropped on the way into the index.
#[test]
fn flac_and_ogg_vorbis_keep_every_tag_the_index_stores() {
    let mut env = Env::new();
    for (rel, kind, kind_expected) in [
        ("a.flac", Kind::Flac, FileKind::Flac),
        ("b.ogg", Kind::Ogg, FileKind::Ogg),
    ] {
        let dest = env.music.join(rel);
        fs::copy(rich_fixture(kind, 5), &dest).unwrap();
        env.scan();
        let t = env.track(&dest);
        assert_eq!(t.info.kind, kind_expected, "{rel}");
        assert_eq!(t.tags.title.as_deref(), Some("Title5"), "{rel}");
        assert_eq!(t.tags.artist.as_deref(), Some("Artist5"), "{rel}");
        assert_eq!(t.tags.album.as_deref(), Some("Album"), "{rel}");
        assert_eq!(t.tags.album_artist.as_deref(), Some("AlbumArtist"), "{rel}");
        assert_eq!(t.tags.genre.as_deref(), Some("Genre"), "{rel}");
        assert_eq!(t.tags.composer.as_deref(), Some("Composer"), "{rel}");
        assert_eq!(t.tags.year, Some(2001), "{rel}");
        assert_eq!(
            (t.tags.track_no, t.tags.track_total),
            (Some(3), Some(12)),
            "{rel}"
        );
        assert_eq!(
            (t.tags.disc_no, t.tags.disc_total),
            (Some(2), Some(4)),
            "{rel}"
        );
        assert!(t.tags.compilation, "{rel}");
        assert!(t.tag_error.is_none(), "{rel}");
    }
}

// ---- a file moved and retagged in one go

/// Mp3 fixture `variant`'s audio under hand-written tags.
fn tagged(variant: u32, title: &str, artist: &str, album: Option<&str>) -> Vec<u8> {
    let mut frames = vec![
        id3v24_text("TIT2", 3, title.as_bytes()),
        id3v24_text("TPE1", 3, artist.as_bytes()),
    ];
    if let Some(a) = album {
        frames.push(id3v24_text("TALB", 3, a.as_bytes()));
    }
    mp3_with_id3v2(variant, &frames)
}

/// The old file (a/old.mp3) is replaced by `new` at b/new.mp3. Returns
/// (env, old id, old row hash, scan report, new track).
fn replace_file(
    old: Vec<u8>,
    new: Vec<u8>,
) -> (
    Env,
    i64,
    [u8; 32],
    fiio_core::scan::ScanReport,
    fiio_core::store::TrackRow,
) {
    let mut env = Env::new();
    env.add("keep.mp3", Kind::Mp3, 9);
    let old_path = env.add_bytes("a/old.mp3", &old);
    env.scan();
    let (id, hash) = {
        let t = env.track(&old_path);
        (t.id, t.hash)
    };
    fs::remove_file(&old_path).unwrap();
    let new_path = env.add_bytes("b/new.mp3", &new);
    let r = env.scan();
    let t = env.track(&new_path);
    (env, id, hash, r, t)
}

// Prevents: a retagged, moved file becoming a new track (losing its playlist
// slots) when its title, or its artist and album, still say it is the same
// song; and unrelated files being merged into one track.
#[test]
fn pairing_by_audio_needs_the_same_title_or_the_same_artist_and_album() {
    let old = || tagged(1, "Song", "Band", Some("LP"));
    // (new title, artist, album, paired)
    let cases: [(&str, &str, Option<&str>, bool); 8] = [
        ("Renamed", "Band", Some("LP"), true),
        ("Song", "Other", Some("Other LP"), true),
        ("  SONG ", "x", None, true),
        ("Renamed", "BAND", Some(" lp "), true),
        ("Renamed", "Band", None, false),
        ("Renamed", "Band", Some("Other LP"), false),
        ("Renamed", "Other", Some("LP"), false),
        ("Another", "Other", Some("Other LP"), false),
    ];
    for (title, artist, album, paired) in cases {
        let new_bytes = tagged(1, title, artist, album);
        let new_hash = *blake3::hash(&new_bytes).as_bytes();
        let (env, id, old_hash, r, new) = replace_file(old(), new_bytes);
        let what = format!("{title:?} {artist:?} {album:?}: {r:?}");
        let old_row = env.tracks().into_iter().find(|t| t.id == id).unwrap();
        if paired {
            assert_eq!(r.moved.len(), 1, "{what}");
            assert_eq!((r.added, r.missing.len()), (0, 0), "{what}");
            assert_eq!(new.id, id, "{what}");
            assert!(old_row.missing_since.is_none(), "{what}");
            assert_eq!(old_row.path, new.path, "{what}");
            // Tags are stored trimmed.
            assert_eq!(old_row.tags.title.as_deref(), Some(title.trim()), "{what}");
            // The row now carries the new file's hash, which is what makes
            // the copy on a card be replaced by the next sync.
            assert_eq!(old_row.hash, new_hash, "{what}");
            assert_ne!(old_row.hash, old_hash, "{what}");
        } else {
            assert!(r.moved.is_empty(), "{what}");
            assert_eq!((r.added, r.missing.len()), (1, 1), "{what}");
            assert_ne!(new.id, id, "{what}");
            assert!(old_row.missing_since.is_some(), "{what}");
            assert_eq!(old_row.hash, old_hash, "{what}");
        }
    }
}

// Prevents: pairing a deleted track with an unrelated file that happens to
// have the same length and bitrate.
#[test]
fn a_delete_and_an_unrelated_add_with_the_same_audio_are_not_paired() {
    let (env, id, _, r, new) = replace_file(
        tagged(1, "Song", "Band", Some("LP")),
        tagged(1, "Different", "Someone Else", Some("Another LP")),
    );
    assert!(r.moved.is_empty(), "{r:?}");
    assert_eq!((r.added, r.missing.len()), (1, 1));
    assert_ne!(new.id, id);
    let old = env.tracks().into_iter().find(|t| t.id == id).unwrap();
    assert!(old.missing_since.is_some());
    assert_eq!(old.tags.title.as_deref(), Some("Song"));
}

// Prevents: audio of another duration being paired on its tags alone. Wav
// has a constant bitrate, so the duration is the only thing that differs.
#[test]
fn audio_of_another_length_is_not_paired_even_with_the_same_tags() {
    for (kind, ext) in [(Kind::Wav, "wav"), (Kind::Mp3, "mp3")] {
        let mut env = Env::new();
        env.add("keep.mp3", Kind::Mp3, 9);
        let old = env.add(&format!("old.{ext}"), kind, 1);
        env.scan();
        let row = env.track(&old);
        assert_eq!(
            row.tags.title.as_deref(),
            Some("Title1"),
            "{ext}: precondition"
        );
        fs::remove_file(&old).unwrap();
        // Same title and artist ("Title1", "Artist1"), twice as long.
        let new = env.music.join(format!("new.{ext}"));
        fs::copy(fixture_secs(kind, 1, 2), &new).unwrap();
        let r = env.scan();
        assert!(r.moved.is_empty(), "{ext}: {r:?}");
        assert_eq!((r.added, r.missing.len()), (1, 1), "{ext}");
        let after = env.tracks().into_iter().find(|t| t.id == row.id).unwrap();
        assert!(after.missing_since.is_some(), "{ext}");
        assert_ne!(env.track(&new).id, row.id, "{ext}");
    }
}

// Prevents: guessing between two tracks that could both be the old one.
#[test]
fn two_vanished_candidates_for_one_new_file_are_not_paired() {
    let mut env = Env::new();
    env.add("keep.mp3", Kind::Mp3, 9);
    // Two files with the same audio, told apart by their album.
    let a = env.add_bytes("a.mp3", &tagged(1, "Song", "Band", Some("LP")));
    let b = env.add_bytes("b.mp3", &tagged(1, "Song", "Band", Some("LP2")));
    env.scan();
    let (ida, idb) = (env.track(&a).id, env.track(&b).id);
    fs::remove_file(&a).unwrap();
    fs::remove_file(&b).unwrap();
    let new = env.add_bytes("c.mp3", &tagged(1, "Song", "Band", Some("Zed")));
    let r = env.scan();
    assert!(r.moved.is_empty(), "{r:?}");
    assert_eq!((r.added, r.missing.len()), (1, 2));
    let t = env.track(&new);
    assert!(t.id != ida && t.id != idb);
    for id in [ida, idb] {
        let old = env.tracks().into_iter().find(|t| t.id == id).unwrap();
        assert!(old.missing_since.is_some());
    }
}

// Prevents: guessing between two new files that could both be the old track.
#[test]
fn two_new_candidates_for_one_vanished_file_are_not_paired() {
    let mut env = Env::new();
    env.add("keep.mp3", Kind::Mp3, 9);
    let a = env.add_bytes("a.mp3", &tagged(1, "Song", "Band", Some("LP")));
    env.scan();
    let ida = env.track(&a).id;
    fs::remove_file(&a).unwrap();
    env.add_bytes("c.mp3", &tagged(1, "Song", "Band", Some("X")));
    env.add_bytes("d.mp3", &tagged(1, "Song", "Band", Some("Y")));
    let r = env.scan();
    assert!(r.moved.is_empty(), "{r:?}");
    assert_eq!((r.added, r.missing.len()), (2, 1));
    let old = env.tracks().into_iter().find(|t| t.id == ida).unwrap();
    assert!(old.missing_since.is_some());
}

// Prevents: a track missing since an earlier scan, whose file comes back
// retagged under another name, getting a new id.
#[test]
fn a_track_already_missing_is_paired_with_the_retagged_file_that_returns() {
    let mut env = Env::new();
    env.add("keep.mp3", Kind::Mp3, 9);
    let a = env.add_bytes("a.mp3", &tagged(1, "Song", "Band", Some("LP")));
    env.scan();
    let (id, old_hash) = {
        let t = env.track(&a);
        (t.id, t.hash)
    };
    fs::remove_file(&a).unwrap();
    assert_eq!(env.scan().missing.len(), 1);
    let new_bytes = tagged(1, "Song 2", "Band", Some("LP"));
    let new = env.add_bytes("z/b.mp3", &new_bytes);
    let r = env.scan();
    assert_eq!(r.moved.len(), 1, "{r:?}");
    assert_eq!(r.added, 0);
    let t = env.track(&new);
    assert_eq!(t.id, id);
    assert!(t.missing_since.is_none());
    assert_eq!(t.hash, *blake3::hash(&new_bytes).as_bytes());
    assert_ne!(t.hash, old_hash);
}

// ---- symbolic links

fn elsewhere(env: &Env) -> PathBuf {
    let d = env.music.parent().unwrap().join("elsewhere");
    fs::create_dir_all(&d).unwrap();
    d
}

fn has_reason(r: &[(PathBuf, String)], path: &std::path::Path, fragment: &str) -> bool {
    r.iter().any(|(p, why)| p == path && why.contains(fragment))
}

// Prevents: a folder swapped for a link (to a copy, say) making its whole
// album go missing, and the tracks' playlist slots with it.
#[test]
fn an_indexed_folder_replaced_by_a_link_to_a_copy_marks_nothing_missing_and_is_reported() {
    let mut env = Env::new();
    env.add("keep.mp3", Kind::Mp3, 9);
    env.add("Album/a.mp3", Kind::Mp3, 1);
    env.add("Album/b.mp3", Kind::Mp3, 2);
    env.scan();
    let ids: Vec<i64> = env.tracks().iter().map(|t| t.id).collect();
    let copy = elsewhere(&env).join("Album");
    fs::rename(env.music.join("Album"), &copy).unwrap();
    symlink(&copy, env.music.join("Album")).unwrap();

    let r = env.scan();
    assert!(r.missing.is_empty(), "{:?}", r.missing);
    assert_eq!((r.added, r.moved.len()), (0, 0), "{r:?}");
    assert!(
        has_reason(&r.unreadable, &env.music.join("Album"), "symbolic link"),
        "{:?}",
        r.unreadable
    );
    let tracks = env.tracks();
    assert_eq!(tracks.len(), 3);
    assert!(tracks.iter().all(|t| t.missing_since.is_none()));
    assert_eq!(tracks.iter().map(|t| t.id).collect::<Vec<_>>(), ids);

    // Removing the link really deletes them.
    fs::remove_file(env.music.join("Album")).unwrap();
    let r = env.scan();
    assert_eq!(r.missing.len(), 2, "{r:?}");
}

// Prevents: a file swapped for a link being dropped from the index.
#[test]
fn an_indexed_file_replaced_by_a_link_is_not_missing_and_is_reported() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.add("keep.mp3", Kind::Mp3, 9);
    env.scan();
    let target = elsewhere(&env).join("a.mp3");
    fs::rename(&a, &target).unwrap();
    symlink(&target, &a).unwrap();

    let r = env.scan();
    assert!(r.missing.is_empty(), "{:?}", r.missing);
    assert!(has_reason(&r.unreadable, &a, "symbolic link"), "{r:?}");
    assert!(env.track(&a).missing_since.is_none());
    assert_eq!(env.tracks().len(), 2);
}

// Prevents: links being followed out of the library on a first scan, and
// dangling ones going unreported.
#[test]
fn links_on_a_first_scan_are_reported_and_nothing_under_them_is_indexed() {
    let mut env = Env::new();
    env.add("real.mp3", Kind::Mp3, 1);
    let out = elsewhere(&env);
    env.add_at(&out, "Album/x.mp3", Kind::Mp3, 2);
    env.add_at(&out, "y.mp3", Kind::Mp3, 3);
    symlink(out.join("Album"), env.music.join("linked-dir")).unwrap();
    symlink(out.join("y.mp3"), env.music.join("linked-file.mp3")).unwrap();
    symlink(out.join("nowhere.mp3"), env.music.join("dangling.mp3")).unwrap();

    let r = env.scan();
    assert_eq!(r.added, 1, "{r:?}");
    assert_eq!(env.tracks().len(), 1);
    let links: Vec<_> = r
        .unreadable
        .iter()
        .filter(|(_, why)| why.contains("symbolic link"))
        .map(|(p, _)| p.clone())
        .collect();
    assert_eq!(links.len(), 3, "{:?}", r.unreadable);
    for name in ["linked-dir", "linked-file.mp3", "dangling.mp3"] {
        assert!(links.contains(&env.music.join(name)), "{name}: {links:?}");
    }
}

// Prevents: a library root that is a link (a mount shortcut) being rejected.
#[test]
fn a_root_that_is_a_link_is_still_walked() {
    let mut env = Env::new();
    env.add("a.mp3", Kind::Mp3, 1);
    let link = env.music.parent().unwrap().join("music-link");
    symlink(&env.music, &link).unwrap();
    env.cfg.library.roots = vec![link.clone()];
    let r = env.scan();
    assert_eq!(r.added, 1, "{r:?}");
    assert!(r.unreadable.is_empty(), "{:?}", r.unreadable);
    assert_eq!(env.tracks()[0].path, link.join("a.mp3"));
}
