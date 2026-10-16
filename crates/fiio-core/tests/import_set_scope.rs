//! `import::set_scope`: where a scope copied from iprune lands. The only
//! adopted X3 gets it as its own scope; with none or several adopted it goes
//! to `[sync]` in the config. The config file is in a temp dir.

mod common;

use common::Env;
use fiio_core::config::{Config, Scope};
use fiio_core::import::{self, ScopeTarget};
use fiio_core::store::devices::{self, DeviceScope};

fn names(n: &[&str]) -> Vec<String> {
    n.iter().map(|s| s.to_string()).collect()
}

struct World {
    env: Env,
    file: std::path::PathBuf,
}

impl World {
    fn new() -> Self {
        let env = Env::new();
        // Next to the playlists folder, in the same temp dir.
        let file = env.lists.parent().unwrap().join("config/config.toml");
        World { env, file }
    }

    fn device(&self, key: &str) -> i64 {
        devices::upsert_device(self.env.store.conn(), key, None, None).unwrap()
    }

    fn set(&self, cfg: &mut Config, scope: Scope, lists: &[&str]) -> ScopeTarget {
        import::set_scope(self.env.store.conn(), cfg, &self.file, scope, names(lists)).unwrap()
    }

    fn own(&self, id: i64) -> Option<DeviceScope> {
        devices::scope(self.env.store.conn(), id).unwrap()
    }
}

// Prevents: the copied scope going to the config while an X3 has its own
// (which wins over the config, so the copy would change nothing), or being
// written to the config file as well.
#[test]
fn the_only_adopted_x3_gets_it_as_its_own_scope_and_the_config_is_left_alone() {
    let w = World::new();
    let id = w.device("usb:ONE");
    // An earlier selection is replaced, not merged.
    devices::set_scope(
        w.env.store.conn(),
        id,
        Some(&DeviceScope {
            kind: Scope::Except,
            playlists: names(&["Old"]),
        }),
    )
    .unwrap();
    let mut cfg = w.env.cfg.clone();
    let target = w.set(&mut cfg, Scope::Playlists, &["Rock", "Jazz"]);
    assert_eq!(target, ScopeTarget::Device("usb:ONE".into()));
    assert_eq!(
        w.own(id),
        Some(DeviceScope {
            kind: Scope::Playlists,
            playlists: names(&["Jazz", "Rock"]),
        })
    );
    assert_eq!(cfg, w.env.cfg, "the config in memory is as it was");
    assert!(!w.file.exists(), "no config file is written");
}

// Prevents: "the whole library" being stored as "follows the config": an
// explicit All must stay the X3's own.
#[test]
fn the_whole_library_is_an_own_scope_too() {
    let w = World::new();
    let id = w.device("usb:ONE");
    let mut cfg = w.env.cfg.clone();
    w.set(&mut cfg, Scope::All, &[]);
    assert_eq!(
        w.own(id),
        Some(DeviceScope {
            kind: Scope::All,
            playlists: vec![]
        })
    );
}

// Prevents: a scope silently written into one X3 of several, or into none
// of them and the config too; and the rest of the config being lost in the
// save.
#[test]
fn with_no_x3_or_several_it_goes_to_the_config_file_and_keeps_the_rest_of_it() {
    let w = World::new();
    let mut cfg = w.env.cfg.clone();
    cfg.sync.keep_backups = 7;
    let target = w.set(&mut cfg, Scope::Except, &["Podcasts"]);
    assert_eq!(target, ScopeTarget::Config);
    assert_eq!(cfg.sync.scope, Scope::Except);
    assert_eq!(cfg.sync.playlists, ["Podcasts"]);
    let saved = Config::load(&w.file).unwrap();
    assert_eq!(saved, cfg);
    assert_eq!(saved.sync.keep_backups, 7);
    assert_eq!(saved.library, w.env.cfg.library);

    // Two X3s: still the config, and neither gets an own scope.
    let (one, two) = (w.device("usb:ONE"), w.device("usb:TWO"));
    let target = w.set(&mut cfg, Scope::Playlists, &["Rock"]);
    assert_eq!(target, ScopeTarget::Config);
    assert_eq!(w.own(one), None);
    assert_eq!(w.own(two), None);
    assert_eq!(Config::load(&w.file).unwrap().sync.playlists, ["Rock"]);
}
