//! Fake iPods and a fake iprune, for the import tests. Shared by the core
//! and the CLI test binaries through `#[path]`; the including file also has
//! `mod common;` at its root.
//!
//! - [`Spec`] describes an iPod (tracks, playlists) and has libgpod's own C
//!   writer (`tests/c/make_ipod.c`) build the `iTunesDB`, so the Rust reader
//!   under test is not checked against the Rust writer.
//! - [`Index`] builds an `iprune.sqlite` with the tables fiio-tunes queries.
//! - [`snapshot`] records bytes and mtimes of a tree, to prove a read left
//!   it alone.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use fiio_core::store::TrackRow;
use rusqlite::Connection;

const MAKE_IPOD_C: &str = include_str!("../c/make_ipod.c");

static BUILD: Mutex<()> = Mutex::new(());

/// The compiled `make_ipod`, built once per source text under the cargo
/// target folder.
fn make_ipod() -> PathBuf {
    let mut h = DefaultHasher::new();
    MAKE_IPOD_C.hash(&mut h);
    // Cargo sets CARGO_TARGET_TMPDIR for integration tests only; a unit-test
    // binary that includes this file (the GUI crate's) uses the workspace's
    // `target/tmp`.
    let target = Path::new(
        option_env!("CARGO_TARGET_TMPDIR")
            .unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/tmp")),
    );
    let dir = target.join("ipod-tools");
    let exe = dir.join(format!("make_ipod-{:016x}", h.finish()));
    let _guard = BUILD.lock().unwrap_or_else(|e| e.into_inner());
    if exe.exists() {
        return exe;
    }
    fs::create_dir_all(&dir).unwrap();
    let tid = format!("{}-{:?}", std::process::id(), std::thread::current().id());
    let src = dir.join(format!(".make_ipod-{tid}.c"));
    fs::write(&src, MAKE_IPOD_C).unwrap();
    let flags = Command::new("pkg-config")
        .args(["--cflags", "--libs", "libgpod-1.0", "glib-2.0"])
        .output()
        .expect("pkg-config must be installed");
    assert!(flags.status.success(), "pkg-config cannot find libgpod");
    let flags = String::from_utf8(flags.stdout).unwrap();
    let tmp = dir.join(format!(".make_ipod-{tid}"));
    let status = Command::new("cc")
        .arg("-o")
        .arg(&tmp)
        .arg(&src)
        .args(flags.split_whitespace())
        .status()
        .expect("cc must be installed");
    assert!(status.success(), "compiling make_ipod.c");
    fs::rename(&tmp, &exe).unwrap();
    let _ = fs::remove_file(&src);
    exe
}

/// One iPod track.
#[derive(Debug, Clone)]
pub struct IpodTrack {
    pub dbid: u64,
    pub size: u64,
    pub len_ms: u32,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Bytes to place at the track's `ipod_path` of a mounted iPod.
    pub file: Option<PathBuf>,
}

impl IpodTrack {
    /// A track as iTunes would have copied the library track `row`: same
    /// size, duration, title and artist, and the file itself on a mounted
    /// iPod.
    pub fn of(dbid: u64, row: &TrackRow) -> Self {
        IpodTrack {
            dbid,
            size: row.size,
            len_ms: row.info.duration_ms.unwrap_or(0),
            title: Some(row.display_title().into_owned()),
            artist: row.tags.artist.clone(),
            album: row.tags.album.clone(),
            file: Some(row.path.clone()),
        }
    }

    /// A track no library file can match by size or tags.
    pub fn stranger(dbid: u64) -> Self {
        IpodTrack {
            dbid,
            size: 7,
            len_ms: 123_456,
            title: Some(format!("Stranger {dbid:x}")),
            artist: Some("Nobody".into()),
            album: None,
            file: None,
        }
    }

    pub fn title(mut self, t: &str) -> Self {
        self.title = Some(t.to_owned());
        self
    }

    pub fn artist(mut self, t: &str) -> Self {
        self.artist = Some(t.to_owned());
        self
    }

    pub fn len_ms(mut self, ms: u32) -> Self {
        self.len_ms = ms;
        self
    }

    pub fn no_file(mut self) -> Self {
        self.file = None;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlKind {
    Regular,
    Smart,
    Podcasts,
}

/// What a fake iPod holds.
#[derive(Debug, Clone)]
pub struct Spec {
    pub name: String,
    pub guid: Option<String>,
    pub tracks: Vec<IpodTrack>,
    playlists: Vec<(PlKind, String, Vec<u64>)>,
}

impl Spec {
    pub fn new(name: &str) -> Self {
        Spec {
            name: name.to_owned(),
            guid: Some("000A27001234ABCD".into()),
            tracks: Vec::new(),
            playlists: Vec::new(),
        }
    }

    pub fn guid(mut self, g: Option<&str>) -> Self {
        self.guid = g.map(str::to_owned);
        self
    }

    pub fn track(mut self, t: IpodTrack) -> Self {
        self.tracks.push(t);
        self
    }

    pub fn playlist(mut self, name: &str, members: &[u64]) -> Self {
        self.playlists
            .push((PlKind::Regular, name.to_owned(), members.to_vec()));
        self
    }

    pub fn smart(mut self, name: &str, members: &[u64]) -> Self {
        self.playlists
            .push((PlKind::Smart, name.to_owned(), members.to_vec()));
        self
    }

    pub fn podcasts(mut self, name: &str, members: &[u64]) -> Self {
        self.playlists
            .push((PlKind::Podcasts, name.to_owned(), members.to_vec()));
        self
    }

    fn manifest(&self) -> String {
        let field = |o: &Option<String>| o.clone().unwrap_or_default();
        let mut m = format!("M\t{}\n", self.name);
        for t in &self.tracks {
            m += &format!(
                "T\t{:x}\t{}\t{}\t{}\t{}\t{}\t0\n",
                t.dbid,
                t.size,
                t.len_ms,
                field(&t.title),
                field(&t.artist),
                field(&t.album)
            );
        }
        for (kind, name, members) in &self.playlists {
            let tag = match kind {
                PlKind::Regular => "P",
                PlKind::Smart => "S",
                PlKind::Podcasts => "C",
            };
            let ids: Vec<String> = members.iter().map(|d| format!("{d:x}")).collect();
            m += &format!("{tag}\t{name}\t{}\n", ids.join(","));
        }
        m
    }

    /// Has libgpod write the `iTunesDB` of this iPod to `db`.
    pub fn write_db(&self, db: &Path) {
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("manifest.tsv");
        fs::write(&file, self.manifest()).unwrap();
        let out = Command::new(make_ipod())
            .arg(db)
            .arg(&file)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "make_ipod failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A folder laid out like a mounted iPod: the `iTunesDB`, a `SysInfo`
    /// holding the GUID when there is one, and each track's file (when it
    /// has one) where libgpod's `ipod_path` for it says.
    pub fn mounted(&self) -> FakeIpod {
        let dir = tempfile::tempdir().unwrap();
        let ctl = dir.path().join("iPod_Control");
        fs::create_dir_all(ctl.join("iTunes")).unwrap();
        fs::create_dir_all(ctl.join("Device")).unwrap();
        for n in 0..50 {
            fs::create_dir_all(ctl.join(format!("Music/F{n:02}"))).unwrap();
        }
        self.write_db(&ctl.join("iTunes/iTunesDB"));
        if let Some(g) = &self.guid {
            fs::write(ctl.join("Device/SysInfo"), format!("FirewireGuid: 0x{g}\n")).unwrap();
        }
        let ipod = FakeIpod { dir };
        for (i, t) in self.tracks.iter().enumerate() {
            if let Some(src) = &t.file {
                let dest = ipod.file_of(i);
                fs::copy(src, &dest).unwrap();
            }
        }
        ipod
    }

    /// A folder with just an `iTunesDB`, as in a backup.
    pub fn file_only(&self) -> FakeIpod {
        let dir = tempfile::tempdir().unwrap();
        self.write_db(&dir.path().join("iTunesDB"));
        FakeIpod { dir }
    }
}

/// A fake iPod folder, or a folder holding a copy of its `iTunesDB`.
pub struct FakeIpod {
    pub dir: tempfile::TempDir,
}

impl FakeIpod {
    pub fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// The `iTunesDB`, in whichever layout this is.
    pub fn itunesdb(&self) -> PathBuf {
        let full = self.root().join("iPod_Control/iTunes/iTunesDB");
        if full.exists() {
            full
        } else {
            self.root().join("iTunesDB")
        }
    }

    /// Where the file of the `i`-th track of the spec is, as `make_ipod.c`
    /// names it.
    pub fn file_of(&self, i: usize) -> PathBuf {
        self.root()
            .join(format!("iPod_Control/Music/F{:02}/T{i:04}.mp3", i % 50))
    }
}

// ---- iprune

/// iprune's files in a sandbox: `<config_home>/iprune/config.toml` and
/// `<data_home>/iprune/iprune.sqlite`, where `directories` looks for them
/// when `XDG_CONFIG_HOME` and `XDG_DATA_HOME` say so.
pub struct Iprune {
    pub config_file: PathBuf,
    pub database: PathBuf,
    pub playlists: PathBuf,
}

impl Iprune {
    /// Makes the folders and a config whose `[playlists] dir` is
    /// `playlists` (created) followed by `extra` toml.
    pub fn new(config_home: &Path, data_home: &Path, playlists: &Path, extra: &str) -> Self {
        let cdir = config_home.join("iprune");
        let ddir = data_home.join("iprune");
        fs::create_dir_all(&cdir).unwrap();
        fs::create_dir_all(&ddir).unwrap();
        fs::create_dir_all(playlists).unwrap();
        let ip = Iprune {
            config_file: cdir.join("config.toml"),
            database: ddir.join("iprune.sqlite"),
            playlists: playlists.to_owned(),
        };
        ip.write_config(extra);
        ip
    }

    pub fn write_config(&self, extra: &str) {
        fs::write(
            &self.config_file,
            format!("[playlists]\ndir = {:?}\n\n{extra}\n", self.playlists),
        )
        .unwrap();
    }

    pub fn write_playlist(&self, name: &str, text: &str) -> PathBuf {
        let p = self.playlists.join(format!("{name}.m3u8"));
        fs::write(&p, text).unwrap();
        p
    }

    /// The index file, created with the tables fiio-tunes queries.
    pub fn index(&self) -> Index {
        Index::create(&self.database)
    }
}

/// An `iprune.sqlite` with the columns of iprune's schema that fiio-tunes
/// reads (see iprune's technical.md section 6).
pub struct Index {
    pub conn: Connection,
    pub path: PathBuf,
}

impl Index {
    pub fn create(path: &Path) -> Self {
        Self::open(path, false)
    }

    /// Like [`Index::create`] in WAL mode with checkpoints off, so the
    /// writes stay in the `-wal` file as they do while iprune is open.
    pub fn create_wal(path: &Path) -> Self {
        Self::open(path, true)
    }

    fn open(path: &Path, wal: bool) -> Self {
        let conn = Connection::open(path).unwrap();
        if wal {
            conn.pragma_update(None, "journal_mode", "wal").unwrap();
            conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        }
        conn.execute_batch(
            "CREATE TABLE devices (id INTEGER PRIMARY KEY, firewire_guid TEXT NOT NULL UNIQUE, \
                name TEXT, model TEXT, last_sync_at INTEGER, scope TEXT);
             CREATE TABLE tracks (id INTEGER PRIMARY KEY, path TEXT NOT NULL);
             CREATE TABLE playlists (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             CREATE TABLE device_tracks (device_id INTEGER NOT NULL, track_id INTEGER NOT NULL, \
                dbid INTEGER NOT NULL, PRIMARY KEY (device_id, dbid));
             CREATE TABLE device_playlists (device_id INTEGER NOT NULL, playlist_id INTEGER NOT NULL, \
                ipod_name TEXT, ipod_id INTEGER, PRIMARY KEY (device_id, playlist_id));
             CREATE TABLE device_scope_playlists (device_id INTEGER NOT NULL, name TEXT NOT NULL, \
                PRIMARY KEY (device_id, name));",
        )
        .unwrap();
        Index {
            conn,
            path: path.to_owned(),
        }
    }

    /// Copies the index file and its `-wal` to `dest` (a file path) while
    /// this connection still holds them open.
    pub fn copy_with_wal(&self, dest: &Path) {
        fs::copy(&self.path, dest).unwrap();
        let wal = |p: &Path| {
            let mut s = p.as_os_str().to_owned();
            s.push("-wal");
            PathBuf::from(s)
        };
        fs::copy(wal(&self.path), wal(dest)).unwrap();
    }

    /// Adds an iPod; `guid` is stored as given.
    pub fn device(&self, guid: &str, name: &str, scope: Option<&str>) -> i64 {
        self.conn
            .execute(
                "INSERT INTO devices (firewire_guid, name, scope) VALUES (?1, ?2, ?3)",
                rusqlite::params![guid, name, scope],
            )
            .unwrap();
        self.conn.last_insert_rowid()
    }

    fn track_id(&self, path: &Path) -> i64 {
        let p = path.to_str().unwrap();
        if let Ok(id) = self
            .conn
            .query_row("SELECT id FROM tracks WHERE path = ?1", [p], |r| r.get(0))
        {
            return id;
        }
        self.conn
            .execute("INSERT INTO tracks (path) VALUES (?1)", [p])
            .unwrap();
        self.conn.last_insert_rowid()
    }

    /// Records that `dbid` on `device` is the library file `path` (stored
    /// as given, as iprune stores it). The dbid is stored as an `i64`.
    pub fn map(&self, device: i64, dbid: u64, path: &Path) {
        let track = self.track_id(path);
        self.conn
            .execute(
                "INSERT INTO device_tracks (device_id, track_id, dbid) VALUES (?1, ?2, ?3)",
                rusqlite::params![device, track, dbid as i64],
            )
            .unwrap();
    }

    pub fn scope_playlist(&self, device: i64, name: &str) {
        self.conn
            .execute(
                "INSERT INTO device_scope_playlists (device_id, name) VALUES (?1, ?2)",
                rusqlite::params![device, name],
            )
            .unwrap();
    }

    /// iprune's playlist `name` is synced to `device` as the iPod playlist
    /// `ipod_id`.
    pub fn device_playlist(&self, device: i64, name: &str, ipod_id: Option<u64>) {
        self.conn
            .execute("INSERT INTO playlists (name) VALUES (?1)", [name])
            .unwrap();
        let pid = self.conn.last_insert_rowid();
        self.conn
            .execute(
                "INSERT INTO device_playlists (device_id, playlist_id, ipod_name, ipod_id) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![device, pid, name, ipod_id.map(|i| i as i64)],
            )
            .unwrap();
    }
}

// ---- proof that a read changed nothing

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub kind: char,
    pub mtime_ns: i128,
    /// Length and hash of the bytes of a file.
    pub content: Option<(u64, u64)>,
    pub atime_ns: Option<i128>,
}

/// Every entry under `root` (and `root` itself, or its absence): kind,
/// mtime, a hash of the bytes. With `atime`, access times too.
pub fn snapshot(root: &Path, atime: bool) -> BTreeMap<PathBuf, Option<Stat>> {
    fn walk(p: &Path, atime: bool, out: &mut BTreeMap<PathBuf, Option<Stat>>) {
        let Ok(m) = fs::symlink_metadata(p) else {
            out.insert(p.to_owned(), None);
            return;
        };
        let kind = if m.is_dir() {
            'd'
        } else if m.is_file() {
            'f'
        } else {
            'l'
        };
        let content = m.is_file().then(|| {
            let bytes = fs::read(p).unwrap();
            let mut h = DefaultHasher::new();
            bytes.hash(&mut h);
            (bytes.len() as u64, h.finish())
        });
        out.insert(
            p.to_owned(),
            Some(Stat {
                kind,
                mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
                content,
                atime_ns: atime
                    .then(|| i128::from(m.atime()) * 1_000_000_000 + i128::from(m.atime_nsec())),
            }),
        );
        if m.is_dir() {
            for e in fs::read_dir(p).unwrap() {
                walk(&e.unwrap().path(), atime, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, atime, &mut out);
    out
}
