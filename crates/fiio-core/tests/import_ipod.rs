//! Reading a fake iPod and tracing its tracks to library files
//! (`import::read_ipod`), then writing its playlists (`import::import_ipod`).
//! The `iTunesDB` files are written by libgpod's C code, the iprune index by
//! rusqlite, all in temp folders.

mod common;
#[path = "common/ipod.rs"]
mod fake;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use common::{Env, Kind};
use fake::{FakeIpod, Index, IpodTrack, Iprune, Spec, snapshot};
use fiio_core::import::{self, How, Ipod, IpodSource, OnConflict, Outcome, Trace};
use fiio_core::store::TrackRow;
use fiio_core::{Error, playlist, playlists};

const A: u64 = 0xF000_0000_0000_0001; // high bit set: iprune stores dbids as i64
const B: u64 = 0x0000_0000_0000_00B2;
const C: u64 = 0x0000_0000_0000_00C3;
const D: u64 = 0x0000_0000_0000_00D4;
const X: u64 = 0x0000_0000_0000_00E5;

/// A library of four mp3 files, a.mp3 to d.mp3, indexed.
fn library() -> (Env, [TrackRow; 4]) {
    let mut env = Env::new();
    let paths = [
        env.add("a.mp3", Kind::Mp3, 1),
        env.add("b.mp3", Kind::Mp3, 2),
        env.add("c.mp3", Kind::Mp3, 3),
        env.add("d.mp3", Kind::Mp3, 4),
    ];
    env.scan();
    let rows = paths.map(|p| env.track(&p));
    (env, rows)
}

fn open(fake: &FakeIpod, mounted: bool) -> gpod::Database {
    let source = match mounted {
        true => IpodSource::Mounted(fake.root()),
        false => IpodSource::File(fake.itunesdb()),
    };
    import::open(&source).unwrap()
}

fn read(env: &Env, fake: &FakeIpod, mounted: bool, iprune_db: Option<&Path>) -> Ipod {
    try_read(env, fake, mounted, iprune_db).unwrap()
}

fn try_read(
    env: &Env,
    fake: &FakeIpod,
    mounted: bool,
    iprune_db: Option<&Path>,
) -> fiio_core::Result<Ipod> {
    let db = open(fake, mounted);
    import::read_ipod(env.store.conn(), &db, iprune_db, &|_, _| {})
}

fn trace(track_id: i64, how: How) -> Trace {
    Trace { track_id, how }
}

/// A scratch folder for the fake iprune of one test.
fn iprune_in(dir: &Path) -> Iprune {
    Iprune::new(
        &dir.join("config"),
        &dir.join("data"),
        &dir.join("iprune-playlists"),
        "",
    )
}

/// An index whose rows live only in its `-wal` file, installed at `dest`.
fn wal_index(dest: &Path, fill: impl FnOnce(&Index)) {
    let scratch = tempfile::tempdir().unwrap();
    let seed = Index::create_wal(&scratch.path().join("seed.sqlite"));
    fill(&seed);
    seed.copy_with_wal(dest);
}

/// The files under `dir` with their bytes and mtimes: what a failed write
/// must leave as it was (the folder's own mtime moves when a temporary file
/// comes and goes, which says nothing).
fn files_only(dir: &Path) -> Vec<(PathBuf, Option<fake::Stat>)> {
    snapshot(dir, false)
        .into_iter()
        .filter(|(_, s)| s.as_ref().is_some_and(|s| s.kind == 'f'))
        .collect()
}

/// The library file paths a playlist file in fiio-tunes' folder lists.
fn files_in(env: &Env, name: &str) -> Vec<PathBuf> {
    env.locations(name).into_iter().map(PathBuf::from).collect()
}

// ---- what is read from the iPod

#[test]
fn regular_and_smart_playlists_are_read_but_not_the_master_nor_podcasts() {
    let (env, t) = library();
    let spec = Spec::new("Julien's iPod")
        .track(IpodTrack::of(A, &t[0]))
        .track(IpodTrack::of(B, &t[1]))
        .track(IpodTrack::of(C, &t[2]))
        .track(IpodTrack::of(D, &t[3]))
        .playlist("Rock", &[A, B, A])
        .smart("Recent", &[C])
        .podcasts("Podcasts", &[D]);
    let fake = spec.mounted();
    let ipod = read(&env, &fake, true, None);
    let c = &ipod.contents;
    let seen: Vec<(&str, bool, &[u64])> = c
        .playlists
        .iter()
        .map(|p| (p.name.as_str(), p.smart, &p.members[..]))
        .collect();
    assert_eq!(
        seen,
        [("Rock", false, &[A, B, A][..]), ("Recent", true, &[C][..]),]
    );
    assert_eq!(c.name.as_deref(), Some("Julien's iPod"));
    assert_eq!(c.tracks.len(), 4);
}

#[test]
fn the_guid_comes_from_sysinfo_of_a_mounted_ipod_and_is_unknown_for_a_bare_file() {
    let (env, t) = library();
    let spec = Spec::new("P")
        .track(IpodTrack::of(A, &t[0]))
        .playlist("L", &[A]);
    let mounted = spec.mounted();
    assert_eq!(
        read(&env, &mounted, true, None)
            .contents
            .firewire_guid
            .as_deref(),
        Some("000A27001234ABCD")
    );
    let bare = spec.file_only();
    assert_eq!(read(&env, &bare, false, None).contents.firewire_guid, None);
}

#[test]
fn the_guid_is_found_in_sysinfo_extended_when_libgpod_has_none() {
    let (env, t) = library();
    let spec = Spec::new("P")
        .guid(None)
        .track(IpodTrack::of(A, &t[0]))
        .playlist("L", &[A]);
    let fake = spec.mounted();
    assert_eq!(read(&env, &fake, true, None).contents.firewire_guid, None);
    fs::write(
        fake.root().join("iPod_Control/Device/SysInfoExtended"),
        "<dict><key>FireWireGUID</key>\n<string>000a27ffeedd0011</string></dict>",
    )
    .unwrap();
    assert_eq!(
        read(&env, &fake, true, None)
            .contents
            .firewire_guid
            .as_deref(),
        Some("000A27FFEEDD0011")
    );
}

#[test]
fn an_empty_library_is_refused_rather_than_tracing_nothing() {
    let env = Env::new();
    let spec = Spec::new("P")
        .track(IpodTrack::stranger(A))
        .playlist("L", &[A]);
    let fake = spec.file_only();
    assert!(matches!(
        try_read(&env, &fake, false, None),
        Err(Error::EmptyIndex)
    ));
}

// ---- tracing through iprune's index

#[test]
fn tracks_are_traced_through_iprunes_record_of_the_ipod_found_by_guid() {
    let (env, t) = library();
    // Tags that match nothing: only iprune's record can say what they are.
    let spec = Spec::new("P")
        .track(IpodTrack::stranger(A))
        .track(IpodTrack::stranger(B))
        .track(IpodTrack::stranger(C))
        .playlist("Mix", &[A, B, C]);
    let fake = spec.mounted();
    let work = tempfile::tempdir().unwrap();
    let ip = iprune_in(work.path());
    let idx = ip.index();
    // Stored with a prefix and in lower case, as another tool may.
    let dev = idx.device("0x000a27001234abcd", "Julien", None);
    idx.map(dev, A, &t[0].path);
    idx.map(dev, B, &t[1].path);
    // A file the library does not have, and a dbid not on this iPod.
    idx.map(dev, C, Path::new("/nowhere/gone.mp3"));
    idx.map(dev, 0x99, &t[3].path);

    let ipod = read(&env, &fake, true, Some(&ip.database));
    assert_eq!(ipod.iprune_error, None);
    let d = ipod.iprune_device.as_ref().expect("device found");
    assert_eq!((d.id, d.firewire_guid.as_str()), (dev, "000A27001234ABCD"));
    assert_eq!(ipod.traced.len(), 2, "{:?}", ipod.traced);
    assert_eq!(ipod.traced[&A], trace(t[0].id, How::Iprune));
    assert_eq!(ipod.traced[&B], trace(t[1].id, How::Iprune));
    let mix = ipod.traced_playlist(&ipod.contents.playlists[0]);
    assert_eq!(mix.tracks, [t[0].id, t[1].id]);
    assert_eq!(mix.left_out, [C]);

    let report =
        import::import_ipod(env.store.conn(), &env.cfg, &ipod, &[], OnConflict::Skip).unwrap();
    assert_eq!(report.playlists.len(), 1);
    assert_eq!(
        files_in(&env, "Mix"),
        [t[0].path.clone(), t[1].path.clone()]
    );
}

#[test]
fn iprunes_path_is_looked_up_whatever_the_unicode_form_of_either_name() {
    let mut env = Env::new();
    // One file is on disk with a decomposed name, the other with a composed
    // one. The index looks files up by the composed form.
    let nfd = env.add("Cafe\u{301}.mp3", Kind::Mp3, 1);
    let nfc = env.add("G\u{e9}.mp3", Kind::Mp3, 2);
    env.scan();
    let (row_nfd, row_nfc) = (env.track(&nfd), env.track(&nfc));
    let spec = Spec::new("P")
        .track(IpodTrack::stranger(A))
        .track(IpodTrack::stranger(B))
        .playlist("L", &[A, B]);
    let fake = spec.file_only();
    let work = tempfile::tempdir().unwrap();
    let ip = iprune_in(work.path());
    let idx = ip.index();
    let dev = idx.device("111", "x", None);
    // iprune has the first as it is on disk, the second in the other form.
    idx.map(dev, A, &env.music.join("Cafe\u{301}.mp3"));
    idx.map(dev, B, &env.music.join("Ge\u{301}.mp3"));

    let ipod = read(&env, &fake, false, Some(&ip.database));
    assert_eq!(ipod.traced[&A], trace(row_nfd.id, How::Iprune));
    assert_eq!(ipod.traced[&B], trace(row_nfc.id, How::Iprune));
}

#[test]
fn the_device_with_the_guid_wins_over_the_one_with_more_dbids_in_common() {
    let (env, t) = library();
    let spec = Spec::new("P")
        .track(IpodTrack::stranger(A))
        .track(IpodTrack::stranger(B))
        .track(IpodTrack::stranger(C))
        .playlist("L", &[A, B, C]);
    let fake = spec.mounted();
    let work = tempfile::tempdir().unwrap();
    let ip = iprune_in(work.path());
    let idx = ip.index();
    let other = idx.device("0000000000000001", "other", None);
    idx.map(other, A, &t[3].path);
    idx.map(other, B, &t[3].path);
    idx.map(other, C, &t[3].path);
    let ours = idx.device("000A27001234ABCD", "ours", None);
    idx.map(ours, A, &t[0].path);

    let ipod = read(&env, &fake, true, Some(&ip.database));
    assert_eq!(ipod.iprune_device.as_ref().map(|d| d.id), Some(ours));
    assert_eq!(ipod.traced.len(), 1, "{:?}", ipod.traced);
    assert_eq!(ipod.traced[&A], trace(t[0].id, How::Iprune));
}

#[test]
fn with_no_guid_the_device_naming_most_of_the_dbids_is_taken() {
    let (env, t) = library();
    let spec = Spec::new("P")
        .track(IpodTrack::stranger(A))
        .track(IpodTrack::stranger(B))
        .track(IpodTrack::stranger(C))
        .playlist("L", &[A, B, C]);
    let fake = spec.file_only();
    let work = tempfile::tempdir().unwrap();
    let ip = iprune_in(work.path());
    let idx = ip.index();
    let few = idx.device("0000000000000001", "few", None);
    idx.map(few, A, &t[3].path);
    let many = idx.device("0000000000000002", "many", None);
    idx.map(many, B, &t[1].path);
    idx.map(many, C, &t[2].path);
    idx.map(many, 0x77, &t[3].path); // not on the iPod: does not count

    let ipod = read(&env, &fake, false, Some(&ip.database));
    assert_eq!(ipod.iprune_device.as_ref().map(|d| d.id), Some(many));
    assert_eq!(ipod.traced.len(), 2, "{:?}", ipod.traced);
    assert_eq!(ipod.traced[&B], trace(t[1].id, How::Iprune));
    assert_eq!(ipod.traced[&C], trace(t[2].id, How::Iprune));
}

#[test]
fn an_ipod_iprune_has_no_record_of_is_traced_by_the_matcher() {
    let (env, t) = library();
    let spec = Spec::new("P")
        .track(IpodTrack::of(A, &t[0]))
        .track(IpodTrack::of(B, &t[1]))
        .track(IpodTrack::of(C, &t[2]))
        .track(IpodTrack::stranger(X))
        .playlist("L", &[A, B, C, X]);
    let fake = spec.file_only();
    let work = tempfile::tempdir().unwrap();
    let ip = iprune_in(work.path());
    let idx = ip.index();
    // iprune knows an iPod, but not this one: no dbid in common.
    let dev = idx.device("0000000000000009", "else", None);
    idx.map(dev, 0x1234, &t[0].path);
    let from_index = read(&env, &fake, false, Some(&ip.database));
    let without_index = read(&env, &fake, false, None);
    // An index path that does not exist is no index either.
    let missing = read(&env, &fake, false, Some(&work.path().join("none.sqlite")));

    for ipod in [&from_index, &without_index, &missing] {
        assert!(ipod.iprune_device.is_none());
        assert_eq!(ipod.iprune_error, None);
        assert_eq!(ipod.traced.len(), 3, "{:?}", ipod.traced);
        assert_eq!(ipod.traced[&A], trace(t[0].id, How::Tags));
        assert_eq!(ipod.traced[&B], trace(t[1].id, How::Tags));
        assert_eq!(ipod.traced[&C], trace(t[2].id, How::Tags));
        assert!(!ipod.traced.contains_key(&X));
        assert_eq!(
            ipod.counts(),
            [(None, 1), (Some(How::Tags), 3)],
            "{:?}",
            ipod.counts()
        );
    }
}

#[test]
fn iprune_traces_what_it_knows_and_the_matcher_the_rest() {
    let (env, t) = library();
    let spec = Spec::new("P")
        .track(IpodTrack::stranger(A))
        .track(IpodTrack::of(B, &t[1]))
        .track(IpodTrack::stranger(X))
        .playlist("L", &[A, B, X]);
    let fake = spec.file_only();
    let work = tempfile::tempdir().unwrap();
    let ip = iprune_in(work.path());
    let idx = ip.index();
    let dev = idx.device("1", "p", None);
    idx.map(dev, A, &t[0].path);

    let ipod = read(&env, &fake, false, Some(&ip.database));
    assert_eq!(ipod.traced[&A], trace(t[0].id, How::Iprune));
    assert_eq!(ipod.traced[&B], trace(t[1].id, How::Tags));
    assert!(!ipod.traced.contains_key(&X));
    assert_eq!(
        ipod.counts(),
        [(None, 1), (Some(How::Iprune), 1), (Some(How::Tags), 1)]
    );
}

#[test]
fn an_index_that_cannot_be_read_leaves_the_matcher_and_says_so() {
    let (env, t) = library();
    let spec = Spec::new("P")
        .track(IpodTrack::of(A, &t[0]))
        .playlist("L", &[A]);
    let fake = spec.file_only();
    let work = tempfile::tempdir().unwrap();

    let garbage = work.path().join("garbage.sqlite");
    fs::write(&garbage, b"this is not a database, not even close, no").unwrap();
    // A real database of some other program.
    let foreign = work.path().join("foreign.sqlite");
    rusqlite::Connection::open(&foreign)
        .unwrap()
        .execute_batch("CREATE TABLE unrelated (x)")
        .unwrap();
    // A scope iprune's schema does not have.
    let odd = work.path().join("odd.sqlite");
    let idx = Index::create(&odd);
    idx.device("000A27001234ABCD", "p", Some("sideways"));
    drop(idx);

    for db in [&garbage, &foreign] {
        let ipod = read(&env, &fake, false, Some(db));
        assert!(
            ipod.iprune_error.is_some(),
            "{}: no error reported",
            db.display()
        );
        assert!(ipod.iprune_device.is_none());
        assert_eq!(ipod.traced.len(), 1, "{:?}", ipod.traced);
        assert_eq!(ipod.traced[&A], trace(t[0].id, How::Tags));
    }
    // The unknown scope is only met when the scope is read; tracing is fine.
    let ipod = read(&env, &fake, false, Some(&odd));
    assert_eq!(ipod.iprune_error, None);
}

#[test]
fn rows_living_only_in_the_wal_are_read_and_the_original_stays_as_it_was() {
    let (env, t) = library();
    let spec = Spec::new("P")
        .track(IpodTrack::stranger(A))
        .playlist("L", &[A]);
    let fake = spec.mounted();
    let work = tempfile::tempdir().unwrap();
    let ip = iprune_in(work.path());
    wal_index(&ip.database, |idx| {
        let dev = idx.device("000A27001234ABCD", "p", None);
        idx.map(dev, A, &t[0].path);
    });
    // The point of the fixture: the main file alone holds nothing.
    let bare = work.path().join("bare.sqlite");
    fs::copy(&ip.database, &bare).unwrap();
    let nothing = rusqlite::Connection::open(&bare).unwrap().query_row(
        "SELECT count(*) FROM devices",
        [],
        |r| r.get::<_, i64>(0),
    );
    assert!(nothing.is_err(), "the rows are not in the main file");

    let before = (
        snapshot(&work.path().join("data"), false),
        snapshot(&work.path().join("config"), false),
        snapshot(&fake.root(), false),
    );
    assert!(
        before
            .0
            .keys()
            .any(|p| p.to_string_lossy().ends_with("-wal")),
        "the fixture has a wal"
    );
    let ipod = read(&env, &fake, true, Some(&ip.database));
    let report =
        import::import_ipod(env.store.conn(), &env.cfg, &ipod, &[], OnConflict::Skip).unwrap();
    assert_eq!(report.playlists.len(), 1);

    assert_eq!(ipod.traced[&A], trace(t[0].id, How::Iprune));
    let after = (
        snapshot(&work.path().join("data"), false),
        snapshot(&work.path().join("config"), false),
        snapshot(&fake.root(), false),
    );
    assert_eq!(before.0, after.0, "iprune's data folder changed");
    assert_eq!(before.1, after.1, "iprune's config folder changed");
    assert_eq!(before.2, after.2, "the iPod changed");
}

// ---- tracing by the matcher

#[test]
fn an_identical_file_on_a_mounted_ipod_is_traced_where_tags_and_duration_disagree() {
    let (env, t) = library();
    let spec = Spec::new("P")
        .track(
            IpodTrack::of(A, &t[0])
                .title("Wrong title")
                .artist("Nobody")
                .len_ms(t[0].info.duration_ms.unwrap() + 30_000),
        )
        .playlist("L", &[A]);
    let mounted = spec.mounted();
    let ipod = read(&env, &mounted, true, None);
    assert_eq!(ipod.traced[&A], trace(t[0].id, How::Identical));
    assert_eq!((ipod.compared, ipod.unreadable.len()), (1, 0));

    // Without the files to compare, nothing says it is the same track.
    let bare = spec.file_only();
    let ipod = read(&env, &bare, false, None);
    assert!(ipod.traced.is_empty(), "{:?}", ipod.traced);
    assert_eq!(ipod.compared, 0);
}

#[test]
fn a_different_file_of_the_same_size_is_not_taken_for_the_library_file() {
    let (env, t) = library();
    let scratch = tempfile::tempdir().unwrap();
    let zeros = scratch.path().join("zeros.mp3");
    fs::write(&zeros, vec![0u8; t[0].size as usize]).unwrap();
    let mut track = IpodTrack::of(A, &t[0])
        .title("Wrong title")
        .artist("Nobody")
        .len_ms(t[0].info.duration_ms.unwrap() + 30_000);
    track.file = Some(zeros);
    let spec = Spec::new("P").track(track).playlist("L", &[A]);
    let fake = spec.mounted();
    let ipod = read(&env, &fake, true, None);
    assert_eq!(ipod.compared, 1, "the file was read and compared");
    assert!(ipod.traced.is_empty(), "{:?}", ipod.traced);
}

#[test]
fn an_ipod_file_that_cannot_be_read_is_reported_and_the_others_still_compared() {
    let (env, t) = library();
    let off = |t: &TrackRow| t.info.duration_ms.unwrap() + 30_000;
    let spec = Spec::new("P")
        .track(
            IpodTrack::of(A, &t[0])
                .title("x")
                .artist("y")
                .len_ms(off(&t[0])),
        )
        .track(
            IpodTrack::of(B, &t[1])
                .title("x")
                .artist("y")
                .len_ms(off(&t[1])),
        )
        .playlist("L", &[A, B]);
    let fake = spec.mounted();
    fs::remove_file(fake.file_of(0)).unwrap(); // A's file is gone
    let ipod = read(&env, &fake, true, None);
    assert_eq!(ipod.unreadable.len(), 1, "{:?}", ipod.unreadable);
    assert_eq!(ipod.unreadable[0].0, A);
    assert_eq!(ipod.compared, 1);
    assert_eq!(ipod.traced[&B], trace(t[1].id, How::Identical));
    assert!(!ipod.traced.contains_key(&A));
}

#[test]
fn reading_a_mounted_ipod_does_not_touch_the_access_time_of_its_files() {
    use rustix::fs::Timespec;
    use rustix::fs::{AtFlags, CWD, Timestamps, utimensat};
    let (env, t) = library();
    let spec = Spec::new("P")
        .track(IpodTrack::of(A, &t[0]).title("x").artist("y").len_ms(1))
        .playlist("L", &[A]);
    let fake = spec.mounted();
    let file = fake.file_of(0);
    // A control in the same folder, read the plain way below.
    let control = fake.root().join("control.bin");
    std::fs::write(&control, b"control").unwrap();
    // Old access, newer modification: a plain read would update atime even
    // under relatime.
    let ts = |s| Timespec {
        tv_sec: s,
        tv_nsec: 0,
    };
    for f in [&file, &control] {
        utimensat(
            CWD,
            f,
            &Timestamps {
                last_access: ts(1_000_000),
                last_modification: ts(2_000_000),
            },
            AtFlags::empty(),
        )
        .unwrap();
    }
    // Stat only: reading the bytes here would update the time itself.
    let atime = |p: &Path| std::fs::metadata(p).unwrap().atime();
    assert_eq!(atime(&file), 1_000_000);
    // Does this file system update atime on a plain read at all? If not (a
    // noatime mount), the test below could not fail.
    {
        use std::io::Read;
        let mut sink = Vec::new();
        std::fs::File::open(&control)
            .unwrap()
            .read_to_end(&mut sink)
            .unwrap();
    }
    if atime(&control) == 1_000_000 {
        eprintln!("skipped: filesystem does not update atime");
        return;
    }
    let ipod = read(&env, &fake, true, None);
    assert_eq!(ipod.compared, 1, "the file was read");
    assert_eq!(
        atime(&file),
        1_000_000,
        "the iPod file was read with a plain open"
    );
}

#[test]
fn duplicates_on_the_ipod_are_all_traced_to_the_one_library_file() {
    let (env, t) = library();
    let spec = Spec::new("P")
        .track(IpodTrack::of(A, &t[0]))
        .track(IpodTrack::of(B, &t[0]))
        .playlist("Both", &[A, B])
        .playlist("Second", &[B]);
    let fake = spec.file_only();
    let ipod = read(&env, &fake, false, None);
    let both = ipod.traced_playlist(&ipod.contents.playlists[0]);
    assert_eq!(both.tracks, [t[0].id, t[0].id]);
    assert!(both.left_out.is_empty());
    let second = ipod.traced_playlist(&ipod.contents.playlists[1]);
    assert_eq!(second.tracks, [t[0].id]);

    let report =
        import::import_ipod(env.store.conn(), &env.cfg, &ipod, &[], OnConflict::Skip).unwrap();
    assert_eq!(report.left_out, []);
    assert_eq!(
        files_in(&env, "Both"),
        [t[0].path.clone(), t[0].path.clone()]
    );
    assert_eq!(files_in(&env, "Second"), [t[0].path.clone()]);
}

#[test]
fn members_with_no_library_file_are_left_out_and_reported_once() {
    let (env, t) = library();
    let spec = Spec::new("P")
        .track(IpodTrack::of(A, &t[0]))
        .track(IpodTrack::of(B, &t[1]))
        .track(IpodTrack::stranger(X))
        .playlist("Mix", &[A, X, B, X]);
    let fake = spec.file_only();
    let ipod = read(&env, &fake, false, None);
    let mix = ipod.traced_playlist(&ipod.contents.playlists[0]);
    assert_eq!(mix.tracks, [t[0].id, t[1].id]);
    assert_eq!(mix.left_out, [X, X]);

    let report =
        import::import_ipod(env.store.conn(), &env.cfg, &ipod, &[], OnConflict::Skip).unwrap();
    assert_eq!(report.left_out, [("Mix".to_owned(), vec![X])]);
    assert_eq!(report.playlists[0].entries, 2);
    assert_eq!(
        files_in(&env, "Mix"),
        [t[0].path.clone(), t[1].path.clone()]
    );
    assert_eq!(ipod.track(X).unwrap().adopt.key, X);
}

#[test]
fn a_library_file_that_went_missing_is_not_a_place_to_send_a_playlist_to() {
    let (mut env, t) = library();
    fs::remove_file(&t[1].path).unwrap();
    env.scan();
    let spec = Spec::new("P")
        .track(IpodTrack::of(A, &t[0]))
        .track(IpodTrack::of(B, &t[1])) // its file is gone from the library
        .track(IpodTrack::stranger(C)) // iprune says: the gone file
        .playlist("Mix", &[A, B, C]);
    let fake = spec.file_only();
    let work = tempfile::tempdir().unwrap();
    let ip = iprune_in(work.path());
    let idx = ip.index();
    let dev = idx.device("1", "p", None);
    idx.map(dev, C, &t[1].path);

    let ipod = read(&env, &fake, false, Some(&ip.database));
    assert!(!ipod.library.contains_key(&t[1].id));
    assert_eq!(ipod.traced.len(), 1, "{:?}", ipod.traced);
    assert_eq!(ipod.traced[&A], trace(t[0].id, How::Tags));
    let report =
        import::import_ipod(env.store.conn(), &env.cfg, &ipod, &[], OnConflict::Skip).unwrap();
    assert_eq!(files_in(&env, "Mix"), [t[0].path.clone()]);
    assert_eq!(report.left_out, [("Mix".to_owned(), vec![B, C])]);
}

// ---- writing

fn two_playlists() -> (Env, [TrackRow; 4], FakeIpod) {
    let (env, t) = library();
    let spec = Spec::new("P")
        .track(IpodTrack::of(A, &t[0]))
        .track(IpodTrack::of(B, &t[1]))
        .track(IpodTrack::of(C, &t[2]))
        .playlist("Mix", &[A, B])
        .playlist("Other", &[C]);
    (env, t, spec.file_only())
}

fn import_all(env: &Env, ipod: &Ipod, names: &[&str], mode: OnConflict) -> import::IpodReport {
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    import::import_ipod(env.store.conn(), &env.cfg, ipod, &names, mode).unwrap()
}

#[test]
fn imported_playlists_hold_the_traced_files_in_order_and_the_index_follows() {
    let (env, t, fake) = two_playlists();
    let ipod = read(&env, &fake, false, None);
    let report = import_all(&env, &ipod, &[], OnConflict::Skip);
    let got: Vec<_> = report
        .playlists
        .iter()
        .map(|i| {
            (
                i.source.as_str(),
                i.name.as_str(),
                i.outcome.clone(),
                i.entries,
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("Mix", "Mix", Outcome::Created, 2),
            ("Other", "Other", Outcome::Created, 1)
        ]
    );
    assert_eq!(
        files_in(&env, "Mix"),
        [t[0].path.clone(), t[1].path.clone()]
    );
    assert_eq!(files_in(&env, "Other"), [t[2].path.clone()]);
    let ids: Vec<_> = env.indexed_items("Mix").into_iter().map(|r| r.1).collect();
    assert_eq!(ids, [Some(t[0].id), Some(t[1].id)]);
    // The persistent id of the iPod playlist is carried for the scope.
    let want: Vec<u64> = ipod.contents.playlists.iter().map(|p| p.id).collect();
    let have: Vec<u64> = report
        .playlists
        .iter()
        .map(|i| i.source_id.unwrap())
        .collect();
    assert_eq!(have, want);
}

#[test]
fn entries_are_written_in_the_configured_path_style() {
    let (env, t) = library();
    let env = env.relative_playlists();
    let spec = Spec::new("P")
        .track(IpodTrack::of(A, &t[0]))
        .playlist("Mix", &[A]);
    let fake = spec.file_only();
    let ipod = read(&env, &fake, false, None);
    import_all(&env, &ipod, &[], OnConflict::Skip);
    assert_eq!(env.locations("Mix"), ["../music/a.mp3"]);
}

#[test]
fn names_a_file_cannot_hold_are_replaced_in_the_file_name_only() {
    let (env, t) = library();
    let spec = Spec::new("P")
        .track(IpodTrack::of(A, &t[0]))
        .playlist("Dance/Trance", &[A])
        .playlist(".Hidden", &[A])
        .playlist("Why?", &[A]);
    let fake = spec.file_only();
    let ipod = read(&env, &fake, false, None);
    let report = import_all(&env, &ipod, &[], OnConflict::Skip);
    let pairs: Vec<_> = report
        .playlists
        .iter()
        .map(|i| (i.source.as_str(), i.name.as_str()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("Dance/Trance", "Dance_Trance"),
            (".Hidden", "_Hidden"),
            ("Why?", "Why_")
        ]
    );
    for f in ["Dance_Trance", "_Hidden", "Why_"] {
        assert!(env.playlist_file(f).is_file(), "{f}");
    }
}

#[test]
fn skip_leaves_an_existing_playlist_byte_for_byte() {
    let (env, _t, fake) = two_playlists();
    let before = "#EXTM3U\n#EXTINF:1,mine\n/somewhere/else.mp3\n";
    let file = env.write_playlist("Mix", before);
    let snap = snapshot(&env.lists, false);
    let ipod = read(&env, &fake, false, None);
    let report = import_all(&env, &ipod, &["Mix"], OnConflict::Skip);
    assert_eq!(report.playlists.len(), 1);
    assert_eq!(report.playlists[0].outcome, Outcome::Skipped);
    assert_eq!(report.playlists[0].name, "Mix");
    assert_eq!(fs::read_to_string(&file).unwrap(), before);
    assert_eq!(
        snapshot(&env.lists, false),
        snap,
        "nothing else written either"
    );
    assert!(report.left_out.is_empty());
    // Not indexed either: the file was not written, so nothing re-read it.
    assert!(env.indexed_items("Mix").is_empty());
}

#[test]
fn replace_overwrites_the_existing_playlist_and_the_index_follows() {
    let (env, t, fake) = two_playlists();
    env.write_playlist("Mix", "#EXTM3U\n/somewhere/else.mp3\n");
    let ipod = read(&env, &fake, false, None);
    let report = import_all(&env, &ipod, &["Mix"], OnConflict::Replace);
    assert_eq!(report.playlists[0].outcome, Outcome::Replaced);
    assert_eq!(
        files_in(&env, "Mix"),
        [t[0].path.clone(), t[1].path.clone()]
    );
    let ids: Vec<_> = env.indexed_items("Mix").into_iter().map(|r| r.1).collect();
    assert_eq!(ids, [Some(t[0].id), Some(t[1].id)]);
}

#[test]
fn rename_numbers_the_new_name_and_never_touches_what_is_there() {
    let (env, t, fake) = two_playlists();
    let mine = "#EXTM3U\n/somewhere/else.mp3\n";
    env.write_playlist("Mix", mine);
    let ipod = read(&env, &fake, false, None);

    let first = import_all(&env, &ipod, &["Mix"], OnConflict::Rename);
    assert_eq!(
        (
            first.playlists[0].name.as_str(),
            first.playlists[0].outcome.clone()
        ),
        ("Mix (iPod)", Outcome::Renamed)
    );
    let second = import_all(&env, &ipod, &["Mix"], OnConflict::Rename);
    assert_eq!(second.playlists[0].name, "Mix (iPod 2)");
    let third = import_all(&env, &ipod, &["Mix"], OnConflict::Rename);
    assert_eq!(third.playlists[0].name, "Mix (iPod 3)");

    assert_eq!(env.read_playlist("Mix"), mine);
    for n in ["Mix (iPod)", "Mix (iPod 2)", "Mix (iPod 3)"] {
        assert_eq!(
            files_in(&env, n),
            [t[0].path.clone(), t[1].path.clone()],
            "{n}"
        );
    }
}

#[test]
fn a_free_name_is_simply_created_whatever_the_conflict_mode() {
    for mode in [OnConflict::Skip, OnConflict::Replace, OnConflict::Rename] {
        let (env, t, fake) = two_playlists();
        let ipod = read(&env, &fake, false, None);
        let report = import_all(&env, &ipod, &["Other"], mode);
        assert_eq!(report.playlists[0].outcome, Outcome::Created, "{mode:?}");
        assert_eq!(report.playlists[0].name, "Other", "{mode:?}");
        assert_eq!(files_in(&env, "Other"), [t[2].path.clone()], "{mode:?}");
    }
}

#[test]
fn a_skipped_playlist_in_the_batch_does_not_stop_the_others() {
    let (env, t, fake) = two_playlists();
    env.write_playlist("Mix", "#EXTM3U\n");
    let ipod = read(&env, &fake, false, None);
    let report = import_all(&env, &ipod, &[], OnConflict::Skip);
    let outcomes: Vec<_> = report.playlists.iter().map(|i| i.outcome.clone()).collect();
    assert_eq!(outcomes, [Outcome::Skipped, Outcome::Created]);
    assert_eq!(files_in(&env, "Other"), [t[2].path.clone()]);
}

#[test]
fn write_imported_refuses_a_file_changed_since_it_was_read() {
    let (env, t) = library();
    let file = env.write_playlist("Mix", "#EXTM3U\n/old.mp3\n");
    let read_at = playlist::mtime_ns(&file).unwrap();
    // Someone saves a newer version after the playlist was read.
    fs::write(&file, "#EXTM3U\n/newer.mp3\n").unwrap();
    let newer = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(newer)
        .unwrap();
    let before = files_only(&env.lists);
    let items = vec![playlists::Item {
        entry: playlist::Entry::default(),
        track: Some(t[0].clone()),
    }];
    let err = playlists::write_imported(
        env.store.conn(),
        &env.cfg,
        "Mix",
        items.clone(),
        &[],
        playlist::WriteMode::Replace { mtime_ns: read_at },
    )
    .unwrap_err();
    assert!(matches!(err, Error::ChangedOnDisk(_)), "{err:?}");
    assert_eq!(files_only(&env.lists), before, "file and folder untouched");

    // With the current mtime it goes through.
    let now = playlist::mtime_ns(&file).unwrap();
    playlists::write_imported(
        env.store.conn(),
        &env.cfg,
        "Mix",
        items,
        &[],
        playlist::WriteMode::Replace { mtime_ns: now },
    )
    .unwrap();
    assert_eq!(files_in(&env, "Mix"), [t[0].path.clone()]);
}

#[test]
fn write_imported_as_new_refuses_a_name_that_appeared_meanwhile() {
    let (env, t) = library();
    env.write_playlist("Mix", "#EXTM3U\n/mine.mp3\n");
    let before = files_only(&env.lists);
    let items = vec![playlists::Item {
        entry: playlist::Entry::default(),
        track: Some(t[0].clone()),
    }];
    let err = playlists::write_imported(
        env.store.conn(),
        &env.cfg,
        "Mix",
        items,
        &[],
        playlist::WriteMode::CreateNew,
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::PlaylistExists(ref n) if n == "Mix"),
        "{err:?}"
    );
    assert_eq!(files_only(&env.lists), before);
}

// ---- choosing playlists by name

#[test]
fn names_are_found_exactly_then_without_case_then_file_safe() {
    let (env, t) = library();
    let spec = Spec::new("P")
        .track(IpodTrack::of(A, &t[0]))
        .playlist("Rock", &[A])
        .playlist("Dance/Trance", &[A])
        .playlist("Café", &[A]);
    let fake = spec.file_only();
    let ipod = read(&env, &fake, false, None);
    for (asked, found) in [
        ("Rock", "Rock"),
        ("rOCK", "Rock"),
        ("Dance_Trance", "Dance_Trance"),
        ("dance/trance", "Dance_Trance"),
        ("Cafe\u{301}", "Café"), // decomposed spelling of a composed name
    ] {
        let report = import_all(&env, &ipod, &[asked], OnConflict::Replace);
        let names: Vec<_> = report.playlists.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, [found], "asking for {asked:?}");
    }
}

#[test]
fn an_ambiguous_or_unknown_name_is_an_error_and_nothing_at_all_is_written() {
    let (env, t) = library();
    let spec = Spec::new("P")
        .track(IpodTrack::of(A, &t[0]))
        .playlist("Jazz", &[A])
        .playlist("jazz", &[A])
        .playlist("Rock", &[A])
        .playlist("Twin", &[A])
        .playlist("Twin", &[A]);
    let fake = spec.file_only();
    let ipod = read(&env, &fake, false, None);
    let before = snapshot(&env.lists, false);
    let call = |names: &[&str]| {
        let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        import::import_ipod(env.store.conn(), &env.cfg, &ipod, &names, OnConflict::Skip)
    };
    // Equal under the case-insensitive pass, not under the exact one.
    assert!(matches!(call(&["JAZZ"]), Err(Error::AmbiguousName(n)) if n == "JAZZ"));
    // Two playlists of the very same name.
    assert!(matches!(call(&["Twin"]), Err(Error::AmbiguousName(_))));
    assert!(matches!(call(&["Nope"]), Err(Error::NoSuchPlaylist(n)) if n == "Nope"));
    // A good name before a bad one: the good one is not written either.
    assert!(matches!(
        call(&["Rock", "Nope"]),
        Err(Error::NoSuchPlaylist(_))
    ));
    assert!(matches!(
        call(&["Rock", "JAZZ"]),
        Err(Error::AmbiguousName(_))
    ));
    assert_eq!(snapshot(&env.lists, false), before);
    assert!(env.playlist_id("Rock").is_none());
    // The exact name settles it.
    assert_eq!(call(&["jazz"]).unwrap().playlists[0].name, "jazz");
}

#[test]
fn a_name_given_twice_is_imported_once() {
    let (env, t, fake) = two_playlists();
    let _ = t;
    let ipod = read(&env, &fake, false, None);
    let report = import_all(
        &env,
        &ipod,
        &["Other", "other", "Other"],
        OnConflict::Rename,
    );
    assert_eq!(report.playlists.len(), 1);
    assert_eq!(report.playlists[0].name, "Other");
}

#[test]
fn two_ipod_playlists_of_one_name_both_come_in_under_rename() {
    let (env, t) = library();
    let spec = Spec::new("P")
        .track(IpodTrack::of(A, &t[0]))
        .track(IpodTrack::of(B, &t[1]))
        .playlist("Twin", &[A])
        .playlist("Twin", &[B]);
    let fake = spec.file_only();
    let ipod = read(&env, &fake, false, None);
    let report = import_all(&env, &ipod, &[], OnConflict::Rename);
    let names: Vec<_> = report.playlists.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["Twin", "Twin (iPod)"]);
    assert_eq!(files_in(&env, "Twin"), [t[0].path.clone()]);
    assert_eq!(files_in(&env, "Twin (iPod)"), [t[1].path.clone()]);
}

/// An iPod of the first two tracks and the playlists given, in order.
fn ipod_of(env: &Env, t: &[TrackRow; 4], lists: &[(&str, &[u64])]) -> Ipod {
    let mut spec = Spec::new("P")
        .track(IpodTrack::of(A, &t[0]))
        .track(IpodTrack::of(B, &t[1]));
    for (name, members) in lists {
        spec = spec.playlist(name, members);
    }
    let fake = spec.file_only();
    read(env, &fake, false, None)
}

fn names_and_outcomes(report: &import::IpodReport) -> Vec<(String, Outcome)> {
    report
        .playlists
        .iter()
        .map(|i| (i.name.clone(), i.outcome.clone()))
        .collect()
}

// Prevents an import from skipping, replacing or hiding a playlist it wrote
// itself: the second of two iPod playlists of one name is always renamed.
#[test]
fn two_ipod_playlists_of_one_name_are_both_written_under_every_conflict_mode() {
    for mode in [OnConflict::Skip, OnConflict::Replace, OnConflict::Rename] {
        let (env, t) = library();
        let ipod = ipod_of(&env, &t, &[("Twin", &[A]), ("Twin", &[B])]);
        let report = import_all(&env, &ipod, &[], mode);
        assert_eq!(
            names_and_outcomes(&report),
            [
                ("Twin".to_owned(), Outcome::Created),
                ("Twin (iPod)".to_owned(), Outcome::Renamed)
            ],
            "{mode:?}"
        );
        // The first file is the first playlist's, not overwritten by the second.
        assert_eq!(files_in(&env, "Twin"), [t[0].path.clone()], "{mode:?}");
        assert_eq!(
            files_in(&env, "Twin (iPod)"),
            [t[1].path.clone()],
            "{mode:?}"
        );
        assert!(report.failed.is_empty(), "{mode:?}");
        // Both are in the index with their own items.
        let ids = |n| -> Vec<_> { env.indexed_items(n).into_iter().map(|r| r.1).collect() };
        assert_eq!(ids("Twin"), [Some(t[0].id)], "{mode:?}");
        assert_eq!(ids("Twin (iPod)"), [Some(t[1].id)], "{mode:?}");
    }
}

#[test]
fn names_that_come_to_one_file_name_are_both_written_under_every_conflict_mode() {
    // "A/B" is written as "A_B", the name of the second.
    for mode in [OnConflict::Skip, OnConflict::Replace, OnConflict::Rename] {
        let (env, t) = library();
        let ipod = ipod_of(&env, &t, &[("A/B", &[A]), ("A_B", &[B])]);
        let report = import_all(&env, &ipod, &[], mode);
        assert_eq!(
            names_and_outcomes(&report),
            [
                ("A_B".to_owned(), Outcome::Created),
                ("A_B (iPod)".to_owned(), Outcome::Renamed)
            ],
            "{mode:?}"
        );
        assert_eq!(files_in(&env, "A_B"), [t[0].path.clone()], "{mode:?}");
        assert_eq!(
            files_in(&env, "A_B (iPod)"),
            [t[1].path.clone()],
            "{mode:?}"
        );
    }
}

#[test]
fn a_name_the_import_renamed_to_is_not_taken_over_by_a_later_playlist() {
    // The second "Twin" becomes "Twin (iPod)"; a third playlist really
    // called that must not replace or skip it.
    for mode in [OnConflict::Skip, OnConflict::Replace] {
        let (env, t) = library();
        let ipod = ipod_of(
            &env,
            &t,
            &[("Twin", &[A]), ("Twin", &[B]), ("Twin (iPod)", &[A, B])],
        );
        let report = import_all(&env, &ipod, &[], mode);
        let names: Vec<_> = report.playlists.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            names,
            ["Twin", "Twin (iPod)", "Twin (iPod) (iPod)"],
            "{mode:?}"
        );
        assert_eq!(
            files_in(&env, "Twin (iPod)"),
            [t[1].path.clone()],
            "{mode:?}"
        );
        assert_eq!(
            files_in(&env, "Twin (iPod) (iPod)"),
            [t[0].path.clone(), t[1].path.clone()],
            "{mode:?}"
        );
    }
}

#[test]
fn a_name_taken_before_the_run_is_still_skipped_replaced_or_renamed_as_asked() {
    let mine = "#EXTM3U\n/somewhere/else.mp3\n";
    let case = |mode| {
        let (env, t) = library();
        env.write_playlist("Twin", mine);
        let ipod = ipod_of(&env, &t, &[("Twin", &[A]), ("Twin", &[B])]);
        let report = import_all(&env, &ipod, &[], mode);
        (env, t, report)
    };

    // Nothing is written, so no second playlist has a written name to meet.
    let (env, _t, report) = case(OnConflict::Skip);
    assert_eq!(
        names_and_outcomes(&report),
        [
            ("Twin".to_owned(), Outcome::Skipped),
            ("Twin".to_owned(), Outcome::Skipped)
        ]
    );
    assert_eq!(env.read_playlist("Twin"), mine);
    assert!(!env.playlist_file("Twin (iPod)").exists());

    // The first replaces what was there; the second may not replace the first.
    let (env, t, report) = case(OnConflict::Replace);
    assert_eq!(
        names_and_outcomes(&report),
        [
            ("Twin".to_owned(), Outcome::Replaced),
            ("Twin (iPod)".to_owned(), Outcome::Renamed)
        ]
    );
    assert_eq!(files_in(&env, "Twin"), [t[0].path.clone()]);
    assert_eq!(files_in(&env, "Twin (iPod)"), [t[1].path.clone()]);

    let (env, t, report) = case(OnConflict::Rename);
    assert_eq!(
        names_and_outcomes(&report),
        [
            ("Twin (iPod)".to_owned(), Outcome::Renamed),
            ("Twin (iPod 2)".to_owned(), Outcome::Renamed)
        ]
    );
    assert_eq!(env.read_playlist("Twin"), mine);
    assert_eq!(files_in(&env, "Twin (iPod)"), [t[0].path.clone()]);
    assert_eq!(files_in(&env, "Twin (iPod 2)"), [t[1].path.clone()]);
}

// ---- a playlist that cannot be written

/// A name whose file name, with `.m3u8`, is too long for the file system.
fn too_long() -> String {
    "L".repeat(300)
}

#[test]
fn a_playlist_that_cannot_be_written_is_reported_and_the_others_still_are() {
    for mode in [OnConflict::Skip, OnConflict::Replace, OnConflict::Rename] {
        let (env, t) = library();
        let long = too_long();
        let ipod = ipod_of(
            &env,
            &t,
            &[("First", &[A]), (&long, &[A, B]), ("Last", &[B])],
        );
        let report = import_all(&env, &ipod, &[], mode);
        let names: Vec<_> = report.playlists.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["First", "Last"], "{mode:?}");
        assert_eq!(report.failed.len(), 1, "{mode:?}: {:?}", report.failed);
        assert_eq!(report.failed[0].0, long, "{mode:?}");
        assert!(!report.failed[0].1.is_empty(), "{mode:?}");
        assert_eq!(files_in(&env, "First"), [t[0].path.clone()], "{mode:?}");
        assert_eq!(files_in(&env, "Last"), [t[1].path.clone()], "{mode:?}");
        // Nothing of the failed one is left: no file, no temporary file, no
        // index row.
        let mut files: Vec<_> = fs::read_dir(&env.lists)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        files.sort();
        assert_eq!(files, ["First.m3u8", "Last.m3u8"], "{mode:?}");
        assert!(env.playlist_id(&long).is_none(), "{mode:?}");
        // A failed playlist is not reported as left out members either.
        assert!(report.left_out.is_empty(), "{mode:?}");
    }
}

#[test]
fn a_run_where_nothing_fails_has_an_empty_failed_list() {
    let (env, _t, fake) = two_playlists();
    let ipod = read(&env, &fake, false, None);
    let report = import_all(&env, &ipod, &[], OnConflict::Skip);
    assert_eq!(report.playlists.len(), 2);
    assert!(report.failed.is_empty(), "{:?}", report.failed);
}

// ---- real data, read only

/// Reads the backup of the owner's iPod. Only `read_ipod` runs (the `--list`
/// of the CLI): nothing is imported, and the file is compared before and
/// after. Run with `cargo test -- --ignored`.
#[test]
#[ignore = "reads /mnt/seagate-1tb/iPod-backup-2026-10, which only the owner's machine has"]
fn the_real_ipod_backup_is_read_consistently_and_left_alone() {
    let db_path = Path::new("/mnt/seagate-1tb/iPod-backup-2026-10/iTunes/iTunesDB");
    if !db_path.is_file() {
        eprintln!("skipped: {} is absent", db_path.display());
        return;
    }
    let (env, _t) = library();
    let before = snapshot(db_path, false);
    let db = gpod::Database::open_file(db_path).unwrap();
    let ipod = import::read_ipod(env.store.conn(), &db, None, &|_, _| {}).unwrap();
    let c = &ipod.contents;
    assert!(!c.tracks.is_empty(), "the backup has tracks");
    assert!(!c.playlists.is_empty(), "the backup has playlists");
    let dbids: std::collections::HashSet<u64> = c.tracks.iter().map(|t| t.adopt.key).collect();
    assert_eq!(dbids.len(), c.tracks.len(), "dbids are unique");
    for pl in &c.playlists {
        assert!(
            pl.members.iter().all(|m| dbids.contains(m)),
            "{:?} names a track the iPod does not have",
            pl.name
        );
        let t = ipod.traced_playlist(pl);
        assert_eq!(
            t.tracks.len() + t.left_out.len(),
            pl.members.len(),
            "{:?}",
            pl.name
        );
    }
    assert_eq!(snapshot(db_path, false), before, "the backup changed");
}
