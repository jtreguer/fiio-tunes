//! A library, a card that is a temp folder (`device.mountpoint` with
//! `allow_folder`) and the config and data folders the workers get, all in
//! temp dirs. Shared by the worker tests (`sync_job`, `adopt_job`,
//! `import_job`), which include it with `#[path]` next to their own
//! `mod common;` (the core crate's fixtures). Nothing here can reach the real
//! card, library, config or index: every path is under the temp dir.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use fiio_core::adoption::{self, Run};
use fiio_core::backup::{self, Backup};
use fiio_core::config::{Config, Paths};
use fiio_core::store::{Store, devices};
use fiio_core::{playlists, scan};

use super::common::{Env, Kind};

pub struct World {
    pub env: Env,
    pub paths: Paths,
    pub card: PathBuf,
    _dir: tempfile::TempDir,
}

/// Every file and folder of a tree: relative path to the bytes, `None` for a
/// folder. Hidden files included.
pub type Tree = BTreeMap<String, Option<Vec<u8>>>;

pub fn tree(root: &std::path::Path) -> Tree {
    let mut out = Tree::new();
    let mut stack = vec![root.to_owned()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().to_str().unwrap().to_owned();
            if p.is_dir() {
                out.insert(rel, None);
                stack.push(p);
            } else {
                out.insert(rel, Some(fs::read(&p).unwrap()));
            }
        }
    }
    out
}

impl World {
    /// A library of MP3 fixtures `1..=n` at `t<i>.mp3` (not scanned yet) and
    /// an empty card folder.
    pub fn new(n: u32) -> World {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let card = base.join("card");
        fs::create_dir(&card).unwrap();
        let mut env = Env::new();
        for i in 1..=n {
            env.add(&format!("t{i}.mp3"), Kind::Mp3, i);
        }
        env.cfg.device.mountpoint = card.clone();
        env.cfg.device.allow_folder = true;
        World {
            env,
            paths: Paths {
                config_file: base.join("work/config/config.toml"),
                data_dir: base.join("work/data"),
            },
            card,
            _dir: dir,
        }
    }

    pub fn cfg(&self) -> Config {
        self.env.cfg.clone()
    }

    /// The index the workers open.
    pub fn store(&self) -> Store {
        Store::open(&self.paths.database()).unwrap()
    }

    pub fn scan(&self) {
        let mut store = self.store();
        scan::scan(&mut store, &self.env.cfg, &|_| {}).expect("scan");
    }

    /// A library playlist `name` holding `files` (names under the music
    /// folder), loaded into the index.
    pub fn playlist(&self, name: &str, files: &[&str]) {
        let text: String = files
            .iter()
            .map(|f| format!("{}\n", self.env.music.join(f).display()))
            .collect();
        self.env.write_playlist(name, &text);
        playlists::sync_from_disk(self.store().conn(), &self.env.cfg, true).unwrap();
    }

    pub fn key(&self) -> String {
        format!("folder:{}", self.card.display())
    }

    /// Scans, then adopts the card as it is, taking every proposal's
    /// default: what `fiio-tunes adopt --accept-proposals` does.
    pub fn adopt(&self) -> Run {
        self.scan();
        let store = self.store();
        let conn = store.conn();
        playlists::sync_from_disk(conn, &self.env.cfg, false).unwrap();
        let run = adoption::run(
            conn,
            &self.env.cfg,
            &self.card,
            &self.key(),
            true,
            &|_, _| {},
        )
        .expect("adoption runs");
        adoption::save(conn, &run, None, None, false).expect("adoption saves");
        run
    }

    /// The card's files and folders now.
    pub fn card_tree(&self) -> Tree {
        tree(&self.card)
    }

    /// This card's backups, oldest first.
    pub fn backups(&self) -> Vec<Backup> {
        backup::list(&self.paths.data_dir.join("backups"), &self.key()).unwrap()
    }

    /// `(card path, library file name)` of the card's track mappings.
    pub fn mapped(&self) -> Vec<(String, String)> {
        let store = self.store();
        let conn = store.conn();
        let Some(d) = devices::device(conn, &self.key()).unwrap() else {
            return Vec::new();
        };
        let tracks = fiio_core::store::all_tracks(conn).unwrap();
        let mut out: Vec<(String, String)> = devices::track_mappings(conn, d.id)
            .unwrap()
            .into_iter()
            .map(|m| {
                let t = tracks.iter().find(|t| t.id == m.track_id).unwrap();
                (
                    m.card_path,
                    t.path.file_name().unwrap().to_str().unwrap().to_owned(),
                )
            })
            .collect();
        out.sort();
        out
    }
}
