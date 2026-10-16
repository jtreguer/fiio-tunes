//! A library, a card that is a temp folder (`allow_folder`) and a data
//! folder for backups, with the steps the F5 tests repeat: adopt, load a
//! playlist, prepare, run, snapshot the card. Nothing here can find or
//! touch the real X3: the card is handed to `prepare` and `run` as a
//! `Found` folder.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use crate::common::{Env, Kind};
use fiio_core::backup::{self, Backup};
use fiio_core::config::Paths;
use fiio_core::device::Found;
use fiio_core::exec::{self, Done, Failed, Options};
use fiio_core::mounts::Mount;
use fiio_core::store::devices::{self, DeviceRow, TrackMapping};
use fiio_core::sync::{self, Prepared};
use fiio_core::{Result, playlists};

pub struct Rig {
    pub env: Env,
    pub card: PathBuf,
    pub data: PathBuf,
    _dir: tempfile::TempDir,
}

impl Rig {
    /// A library of MP3 fixtures `1..=n` at `t<i>.mp3`, scanned, and an
    /// empty card folder.
    pub fn new(n: u32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let card = base.join("card");
        let data = base.join("data");
        fs::create_dir(&card).unwrap();
        let mut env = Env::new();
        for i in 1..=n {
            env.add(&format!("t{i}.mp3"), Kind::Mp3, i);
        }
        env.scan();
        env.cfg.device.mountpoint = card.clone();
        env.cfg.device.allow_folder = true;
        Rig {
            env,
            card,
            data,
            _dir: dir,
        }
    }

    pub fn conn(&self) -> &rusqlite::Connection {
        self.env.store.conn()
    }

    pub fn found(&self) -> Found {
        Found {
            mount: Mount {
                mountpoint: self.card.clone(),
                fstype: String::new(),
                read_only: false,
                root: PathBuf::new(),
            },
            source: None,
            usb: None,
        }
    }

    pub fn paths(&self) -> Paths {
        Paths {
            config_file: self.data.join("config.toml"),
            data_dir: self.data.clone(),
        }
    }

    pub fn key(&self) -> String {
        format!("folder:{}", self.card.display())
    }

    pub fn device(&self) -> DeviceRow {
        devices::device(self.conn(), &self.key())
            .unwrap()
            .expect("device adopted")
    }

    /// The (empty) card adopted with no manifest: the device row only.
    pub fn adopt_empty(&self) -> i64 {
        devices::upsert_device(self.conn(), &self.key(), None, None).unwrap()
    }

    /// Library file `rel` (under the music folder) put on the card at
    /// `card_path` and mapped to its track, as an adoption leaves it.
    pub fn adopt_file(&self, dev: i64, rel: &str, card_path: &str) {
        let t = self.env.track(&self.env.music.join(rel));
        self.put(card_path, &fs::read(&t.path).unwrap());
        let mut tracks = devices::track_mappings(self.conn(), dev).unwrap();
        tracks.push(TrackMapping {
            track_id: t.id,
            card_path: card_path.to_owned(),
            synced_hash: t.hash,
            synced_size: t.size,
        });
        let lists = devices::playlist_mappings(self.conn(), dev).unwrap();
        devices::replace_mappings(self.conn(), dev, &tracks, &lists).unwrap();
    }

    /// A library playlist of `files` (names under the music folder).
    pub fn playlist(&self, name: &str, files: &[&str]) {
        let text: String = files
            .iter()
            .map(|f| format!("{}\n", self.env.music.join(f).display()))
            .collect();
        self.env.write_playlist(name, &text);
        playlists::sync_from_disk(self.conn(), &self.env.cfg, true).unwrap();
    }

    pub fn prepare(&self, playlists_only: bool) -> Result<Prepared> {
        sync::prepare_with(self.conn(), &self.env.cfg, &self.found(), playlists_only)
    }

    pub fn run(&self, p: &Prepared, opts: Options) -> std::result::Result<Done, Failed> {
        exec::run(
            self.conn(),
            &self.env.cfg,
            &self.paths(),
            &self.found(),
            p,
            opts,
            &|_| {},
            &std::sync::atomic::AtomicBool::new(false),
        )
    }

    /// [`Rig::run`] with a progress callback and a cancel flag of the
    /// test's own.
    pub fn run_with(
        &self,
        p: &Prepared,
        opts: Options,
        progress: &dyn Fn(exec::Progress),
        cancel: &std::sync::atomic::AtomicBool,
    ) -> std::result::Result<Done, Failed> {
        exec::run(
            self.conn(),
            &self.env.cfg,
            &self.paths(),
            &self.found(),
            p,
            opts,
            progress,
            cancel,
        )
    }

    /// `prepare(false)` then `run`: tracks and playlists, both expected to
    /// succeed.
    pub fn full_sync(&self) -> Done {
        let p = self.prepare(false).expect("prepare");
        self.run(&p, Options::default())
            .unwrap_or_else(|f| panic!("sync failed: {}", f.error))
    }

    /// The device's track mappings as library file name to card path.
    pub fn mapped(&self) -> BTreeMap<String, String> {
        let tracks = self.env.tracks();
        devices::track_mappings(self.conn(), self.device().id)
            .unwrap()
            .into_iter()
            .map(|m| {
                let t = tracks.iter().find(|t| t.id == m.track_id).unwrap();
                (
                    t.path
                        .strip_prefix(&self.env.music)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                    m.card_path,
                )
            })
            .collect()
    }

    /// `prepare(true)` then `run`, both expected to succeed.
    pub fn sync(&self) -> Done {
        let p = self.prepare(true).expect("prepare");
        self.run(&p, Options::default())
            .unwrap_or_else(|f| panic!("sync failed: {}", f.error))
    }

    pub fn put(&self, rel: &str, bytes: &[u8]) {
        let p = self.card.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }

    pub fn read(&self, rel: &str) -> Vec<u8> {
        fs::read(self.card.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.card.join(rel).symlink_metadata().is_ok()
    }

    /// Every file on the card, hidden ones included: path to bytes.
    pub fn snapshot(&self) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![self.card.clone()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let rel = p.strip_prefix(&self.card).unwrap().to_str().unwrap();
                    out.insert(rel.to_owned(), fs::read(&p).unwrap());
                }
            }
        }
        out
    }

    /// This device's backups, oldest first.
    pub fn backups(&self) -> Vec<Backup> {
        backup::list(&self.data.join("backups"), &self.key()).unwrap()
    }

    /// The backups folder of the device, as a list of entry names (hidden
    /// ones included).
    pub fn backup_entries(&self) -> Vec<String> {
        let base = backup::folder(&self.data.join("backups"), &self.key());
        match fs::read_dir(base) {
            Ok(r) => {
                let mut v: Vec<String> = r
                    .map(|e| e.unwrap().file_name().into_string().unwrap())
                    .collect();
                v.sort();
                v
            }
            Err(_) => Vec::new(),
        }
    }

    pub fn card_manifest(&self) -> serde_json::Value {
        serde_json::from_slice(&self.read(".fiio-tunes/manifest.json")).unwrap()
    }
}

/// Whether directory permission bits stop us writing (false as root).
pub fn readonly_dir_blocks_writes(dir: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
    let blocked = fs::File::create(dir.join(".probe")).is_err();
    let _ = fs::remove_file(dir.join(".probe"));
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
    blocked
}
