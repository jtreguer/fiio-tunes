//! Copying the iPod's iprune sync scope (`import::iprune_scope`) once its
//! playlists are imported.

mod common;
#[path = "common/ipod.rs"]
mod fake;

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use common::{Env, Kind};
use fake::{FakeIpod, IpodTrack, Iprune, Spec, snapshot};
use fiio_core::Error;
use fiio_core::config::Scope;
use fiio_core::import::{self, Ipod, IpodReport, IpodSource, OnConflict, ScopeCopy};
use fiio_core::iprune::IpruneConfig;

const A: u64 = 0xA1;
const B: u64 = 0xB2;
const C: u64 = 0xC3;
const GUID: &str = "000A27001234ABCD";

struct Rig {
    env: Env,
    fake: FakeIpod,
    ip: Iprune,
    _work: tempfile::TempDir,
    /// iPod playlist id by name.
    ids: HashMap<String, u64>,
}

fn read(env: &Env, fake: &FakeIpod, db: Option<&Path>) -> Ipod {
    let handle = import::open(&IpodSource::Mounted(fake.root())).unwrap();
    import::read_ipod(env.store.conn(), &handle, db, &|_, _| {}).unwrap()
}

/// A library of three files; an iPod with the playlists Rock, Jazz and
/// "Dance/Trance" of one track each; a fake iprune with no device yet.
fn rig() -> Rig {
    let mut env = Env::new();
    let paths = [
        env.add("a.mp3", Kind::Mp3, 1),
        env.add("b.mp3", Kind::Mp3, 2),
        env.add("c.mp3", Kind::Mp3, 3),
    ];
    env.scan();
    let t = paths.map(|p| env.track(&p));
    let spec = Spec::new("P")
        .track(IpodTrack::of(A, &t[0]))
        .track(IpodTrack::of(B, &t[1]))
        .track(IpodTrack::of(C, &t[2]))
        .playlist("Rock", &[A])
        .playlist("Jazz", &[B])
        .playlist("Dance/Trance", &[C]);
    let fake = spec.mounted();
    let work = tempfile::tempdir().unwrap();
    let ip = Iprune::new(
        &work.path().join("config"),
        &work.path().join("data"),
        &work.path().join("iprune-playlists"),
        "",
    );
    let ids = read(&env, &fake, None)
        .contents
        .playlists
        .iter()
        .map(|p| (p.name.clone(), p.id))
        .collect();
    Rig {
        env,
        fake,
        ip,
        _work: work,
        ids,
    }
}

impl Rig {
    /// Reads the iPod through the index as it is now and imports `names`.
    fn import(&self, names: &[&str], mode: OnConflict) -> (Ipod, IpodReport) {
        let ipod = read(&self.env, &self.fake, Some(&self.ip.database));
        let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        let report =
            import::import_ipod(self.env.store.conn(), &self.env.cfg, &ipod, &names, mode).unwrap();
        (ipod, report)
    }

    fn scope(&self, ipod: &Ipod, report: &IpodReport) -> fiio_core::Result<ScopeCopy> {
        let cfg = IpruneConfig::load(&self.ip.config_file).unwrap();
        import::iprune_scope(&cfg, ipod, report)
    }
}

fn ready(scope: Scope, names: &[&str]) -> ScopeCopy {
    ScopeCopy::Ready {
        scope,
        playlists: names.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn every_scoped_playlist_imported_gives_the_scope_in_fiio_tunes_names() {
    let rig = rig();
    let idx = rig.ip.index();
    let dev = idx.device(GUID, "P", Some("playlists"));
    // iprune's names for the three iPod playlists: one named differently
    // from the iPod's and linked to it by id; one that differs by case; one
    // that has the characters a file name cannot hold replaced.
    idx.scope_playlist(dev, "Rock Mix");
    idx.scope_playlist(dev, "jazz");
    idx.scope_playlist(dev, "Dance_Trance");
    idx.device_playlist(dev, "Rock Mix", Some(rig.ids["Rock"]));

    let (ipod, report) = rig.import(&[], OnConflict::Skip);
    assert_eq!(
        rig.scope(&ipod, &report).unwrap(),
        ready(Scope::Playlists, &["Dance_Trance", "Jazz", "Rock"])
    );
}

#[test]
fn the_name_is_the_one_the_playlist_was_imported_under() {
    let rig = rig();
    let idx = rig.ip.index();
    let dev = idx.device(GUID, "P", Some("except"));
    idx.scope_playlist(dev, "Rock");
    rig.env.write_playlist("Rock", "#EXTM3U\n/mine.mp3\n");

    let (ipod, report) = rig.import(&["Rock"], OnConflict::Rename);
    assert_eq!(
        rig.scope(&ipod, &report).unwrap(),
        ready(Scope::Except, &["Rock (iPod)"])
    );
}

#[test]
fn a_scoped_playlist_that_was_skipped_or_not_asked_for_or_unknown_is_missing() {
    let rig = rig();
    let idx = rig.ip.index();
    let dev = idx.device(GUID, "P", Some("playlists"));
    for n in ["Gone", "Jazz", "Rock", "Dance_Trance"] {
        idx.scope_playlist(dev, n);
    }
    // Jazz exists already and is skipped; Dance_Trance is not asked for;
    // Gone is not on the iPod at all.
    rig.env.write_playlist("Jazz", "#EXTM3U\n");
    let (ipod, report) = rig.import(&["Rock", "Jazz"], OnConflict::Skip);
    assert_eq!(
        rig.scope(&ipod, &report).unwrap(),
        ScopeCopy::Missing(vec!["Dance_Trance".into(), "Gone".into(), "Jazz".into()])
    );
}

#[test]
fn scope_all_has_no_playlists_to_name() {
    let rig = rig();
    let idx = rig.ip.index();
    let dev = idx.device(GUID, "P", Some("all"));
    idx.scope_playlist(dev, "Stale");
    // iprune's own default is not the device's.
    rig.ip
        .write_config("[sync]\nscope = \"playlists\"\nplaylists = [\"Rock\"]");
    let (ipod, report) = rig.import(&["Rock"], OnConflict::Skip);
    assert_eq!(rig.scope(&ipod, &report).unwrap(), ready(Scope::All, &[]));
}

#[test]
fn a_device_with_no_scope_of_its_own_follows_iprunes_sync_section() {
    let rig = rig();
    let idx = rig.ip.index();
    let dev = idx.device(GUID, "P", None);
    // Rows left from an earlier scope are not the device's scope.
    idx.scope_playlist(dev, "Jazz");
    rig.ip
        .write_config("[sync]\nscope = \"except\"\nplaylists = [\"Rock\", \"Dance_Trance\"]");
    let (ipod, report) = rig.import(&[], OnConflict::Skip);
    assert_eq!(
        rig.scope(&ipod, &report).unwrap(),
        ready(Scope::Except, &["Dance_Trance", "Rock"])
    );
}

#[test]
fn an_ipod_iprune_does_not_know_has_no_scope_to_copy() {
    let rig = rig();
    // No index at all.
    let ipod = read(&rig.env, &rig.fake, None);
    let report = import::import_ipod(
        rig.env.store.conn(),
        &rig.env.cfg,
        &ipod,
        &[],
        OnConflict::Skip,
    )
    .unwrap();
    let err = rig.scope(&ipod, &report).unwrap_err();
    assert!(matches!(err, Error::Iprune(_)), "{err:?}");
}

#[test]
fn a_scope_iprune_does_not_define_is_an_error_not_a_guess() {
    let rig = rig();
    let idx = rig.ip.index();
    idx.device(GUID, "P", Some("sideways"));
    drop(idx);
    let (ipod, report) = rig.import(&[], OnConflict::Skip);
    let err = rig.scope(&ipod, &report).unwrap_err();
    assert!(matches!(err, Error::Iprune(_)), "{err:?}");
}

#[test]
fn reading_the_scope_leaves_iprunes_files_and_the_ipod_alone() {
    let rig = rig();
    let idx = rig.ip.index();
    let dev = idx.device(GUID, "P", Some("playlists"));
    idx.scope_playlist(dev, "Rock");
    drop(idx);
    let tree = |p: &Path| snapshot(p, false);
    let config_dir = rig.ip.config_file.parent().unwrap().to_owned();
    let data_dir = rig.ip.database.parent().unwrap().to_owned();
    let before = (
        tree(&config_dir),
        tree(&data_dir),
        tree(&rig.ip.playlists),
        tree(&rig.fake.root()),
    );
    let (ipod, report) = rig.import(&[], OnConflict::Skip);
    rig.scope(&ipod, &report).unwrap();
    let after = (
        tree(&config_dir),
        tree(&data_dir),
        tree(&rig.ip.playlists),
        tree(&rig.fake.root()),
    );
    assert_eq!(before, after);
}

// Prevents a recorded iPod playlist id that names nothing on the iPod (an
// older database) from deciding: the scoped name is then looked for by name.
#[test]
fn an_id_iprune_recorded_for_a_playlist_the_ipod_no_longer_has_falls_back_to_the_name() {
    let rig = rig();
    let idx = rig.ip.index();
    let dev = idx.device(GUID, "P", Some("playlists"));
    idx.scope_playlist(dev, "Rock");
    let stale = 0xDEAD_BEEF;
    assert!(!rig.ids.values().any(|id| *id == stale));
    idx.device_playlist(dev, "Rock", Some(stale));

    let (ipod, report) = rig.import(&[], OnConflict::Skip);
    assert_eq!(
        rig.scope(&ipod, &report).unwrap(),
        ready(Scope::Playlists, &["Rock"])
    );
}

#[test]
fn a_stale_id_does_not_make_up_for_a_name_the_ipod_does_not_have() {
    let rig = rig();
    let idx = rig.ip.index();
    let dev = idx.device(GUID, "P", Some("playlists"));
    idx.scope_playlist(dev, "Nowhere");
    idx.device_playlist(dev, "Nowhere", Some(0xDEAD_BEEF));

    let (ipod, report) = rig.import(&[], OnConflict::Skip);
    assert_eq!(
        rig.scope(&ipod, &report).unwrap(),
        ScopeCopy::Missing(vec!["Nowhere".into()])
    );
}

#[test]
fn a_recorded_id_that_is_on_the_ipod_still_wins_over_the_name() {
    let rig = rig();
    let idx = rig.ip.index();
    let dev = idx.device(GUID, "P", Some("playlists"));
    // iprune's "Jazz" is the iPod's Rock, by id.
    idx.scope_playlist(dev, "Jazz");
    idx.device_playlist(dev, "Jazz", Some(rig.ids["Rock"]));

    let (ipod, report) = rig.import(&["Rock"], OnConflict::Skip);
    assert_eq!(
        rig.scope(&ipod, &report).unwrap(),
        ready(Scope::Playlists, &["Rock"])
    );
}

// Prevents the scope from being read from iprune's index file after the
// iPod was read: it comes from the copy `read_ipod` made, whatever iprune
// does to its own file meanwhile.
#[test]
fn the_scope_is_the_one_in_the_copy_read_with_the_ipod_not_in_the_file_as_it_is_now() {
    let rig = rig();
    let idx = rig.ip.index();
    let dev = idx.device(GUID, "P", Some("playlists"));
    idx.scope_playlist(dev, "Rock");

    let ipod = read(&rig.env, &rig.fake, Some(&rig.ip.database));
    let report = import::import_ipod(
        rig.env.store.conn(),
        &rig.env.cfg,
        &ipod,
        &[],
        OnConflict::Skip,
    )
    .unwrap();
    // iprune changes the scope afterwards.
    idx.scope_playlist(dev, "Jazz");
    assert_eq!(
        rig.scope(&ipod, &report).unwrap(),
        ready(Scope::Playlists, &["Rock"])
    );
    // And the file going away does not matter either.
    drop(idx);
    fs::remove_file(&rig.ip.database).unwrap();
    assert_eq!(
        rig.scope(&ipod, &report).unwrap(),
        ready(Scope::Playlists, &["Rock"])
    );
}
