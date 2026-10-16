//! Config::validate through Config::load: roots of `/`, and a device
//! mountpoint that overlaps a library root (the card's files would become
//! library tracks). Everything happens in temp dirs.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use fiio_core::Error;
use fiio_core::config::Config;

fn config_text(roots: &[&Path], mountpoint: Option<&Path>) -> String {
    let roots: Vec<String> = roots
        .iter()
        .map(|r| format!("{:?}", r.to_str().unwrap()))
        .collect();
    let mut s = format!(
        "[library]\nroots = [{}]\n[playlists]\ndir = \"/p\"\n",
        roots.join(", ")
    );
    if let Some(m) = mountpoint {
        s += &format!("[device]\nmountpoint = {:?}\n", m.to_str().unwrap());
    }
    s
}

fn load(dir: &Path, roots: &[&Path], mountpoint: Option<&Path>) -> fiio_core::Result<Config> {
    let file = dir.join("config.toml");
    fs::write(&file, config_text(roots, mountpoint)).unwrap();
    Config::load(&file)
}

fn refused(r: fiio_core::Result<Config>, field: &str) {
    match r {
        Err(Error::Config { message, .. }) => {
            assert!(message.contains(field), "{message}");
        }
        other => panic!("expected a config error about {field}, got {other:?}"),
    }
}

// Prevents: a library root of `/`, which would index (and scan for deletions
// across) the whole machine, the card included.
#[test]
fn a_root_of_slash_is_refused() {
    let t = tempfile::tempdir().unwrap();
    refused(load(t.path(), &[Path::new("/")], None), "library.roots");
    refused(
        load(t.path(), &[t.path(), Path::new("/")], None),
        "library.roots",
    );
    assert!(load(t.path(), &[t.path()], None).is_ok());
}

// Prevents: a card mounted inside the library, or the library inside the
// card, so the card's copies would be indexed as library tracks.
#[test]
fn a_mountpoint_inside_a_root_or_holding_one_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let base = t.path().canonicalize().unwrap();
    let music = base.join("music");
    fs::create_dir_all(music.join("card")).unwrap();
    fs::create_dir_all(base.join("music2")).unwrap();
    fs::create_dir_all(base.join("other")).unwrap();
    // The card inside the library, the library as the card, the library
    // inside the card.
    refused(
        load(&base, &[&music], Some(&music.join("card"))),
        "device.mountpoint",
    );
    refused(load(&base, &[&music], Some(&music)), "device.mountpoint");
    refused(
        load(&base, &[&music.join("card")], Some(&music)),
        "device.mountpoint",
    );
    refused(load(&base, &[&music], Some(&base)), "device.mountpoint");
    // Folders not there (an unplugged card) count by their name.
    refused(
        load(&base, &[&music], Some(&music.join("not/there/yet"))),
        "device.mountpoint",
    );
    // Through ..
    refused(
        load(&base, &[&music], Some(&base.join("other/../music/card"))),
        "device.mountpoint",
    );
    // A sibling whose name starts like the root's is fine, so is no mountpoint.
    assert!(load(&base, &[&music], Some(&base.join("music2"))).is_ok());
    assert!(load(&base, &[&music], Some(&base.join("other"))).is_ok());
    assert!(load(&base, &[&music], None).is_ok());
    assert!(load(&base, &[&music], Some(Path::new(""))).is_ok());
}

// Prevents: a link hiding the overlap between the library and the card.
#[test]
fn a_mountpoint_that_overlaps_a_root_through_a_link_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let base = t.path().canonicalize().unwrap();
    let real = base.join("real");
    fs::create_dir_all(real.join("card")).unwrap();
    let link = base.join("link");
    symlink(&real, &link).unwrap();
    // The root by its link, the card by the real path, and the other way round.
    refused(
        load(&base, &[&link], Some(&real.join("card"))),
        "device.mountpoint",
    );
    refused(
        load(&base, &[&real], Some(&link.join("card"))),
        "device.mountpoint",
    );
    // A link to the card folder from elsewhere, root above it.
    let clink = base.join("card-link");
    symlink(real.join("card"), &clink).unwrap();
    refused(load(&base, &[&real], Some(&clink)), "device.mountpoint");
    // A link to somewhere else entirely is no overlap.
    let other = base.join("other");
    fs::create_dir_all(&other).unwrap();
    let olink = base.join("other-link");
    symlink(&other, &olink).unwrap();
    assert!(load(&base, &[&real], Some(&olink)).is_ok());
}
