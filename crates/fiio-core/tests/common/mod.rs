//! Shared helpers for the integration tests: ffmpeg-generated audio fixtures
//! (built once per test binary, then copied), and a throwaway library with its
//! own playlists folder and SQLite index. Nothing here touches the real home
//! or the real music library.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use fiio_core::config::{Config, PathStyle};
use fiio_core::scan::{self, ScanReport};
use fiio_core::store::{self, Store, TrackRow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Mp3,
    Aac,
    Alac,
    Wav,
    Aiff,
    Flac,
    /// Ogg Vorbis, named .ogg.
    Ogg,
    /// Opus in an Ogg file, named .opus: a format the X3 does not play.
    Opus,
    /// FLAC audio inside an MP4 container, named .m4a.
    FlacInM4a,
    /// Opus audio inside an MP4 container, named .m4a.
    OpusInM4a,
}

impl Kind {
    pub fn ext(self) -> &'static str {
        match self {
            Kind::Mp3 => "mp3",
            Kind::Aac | Kind::Alac | Kind::FlacInM4a | Kind::OpusInM4a => "m4a",
            Kind::Wav => "wav",
            Kind::Aiff => "aiff",
            Kind::Flac => "flac",
            Kind::Ogg => "ogg",
            Kind::Opus => "opus",
        }
    }

    fn ffmpeg_args(self) -> &'static [&'static str] {
        match self {
            Kind::Mp3 => &["-c:a", "libmp3lame"],
            Kind::Aac => &["-c:a", "aac"],
            Kind::Alac => &["-c:a", "alac"],
            Kind::Wav | Kind::Aiff => &[],
            Kind::Flac => &["-c:a", "flac"],
            Kind::Ogg => &["-c:a", "libvorbis"],
            Kind::Opus => &["-c:a", "libopus"],
            Kind::FlacInM4a => &["-c:a", "flac", "-strict", "-2"],
            Kind::OpusInM4a => &["-c:a", "libopus"],
        }
    }
}

static GENERATE: Mutex<()> = Mutex::new(());

/// A cached audio file of `kind`. Each `variant` has a different tone, title
/// ("Title<variant>") and artist ("Artist<variant>"), so its bytes differ.
pub fn fixture(kind: Kind, variant: u32) -> PathBuf {
    fixture_secs(kind, variant, 1)
}

/// [`fixture`] lasting `secs` seconds, for tests that need a different
/// duration (the planner copies audio again when it changes by over a second).
pub fn fixture_secs(kind: Kind, variant: u32, secs: u32) -> PathBuf {
    build(kind, variant, secs, "", &[])
}

/// The Vorbis comment names ([`rich_fixture`]) and values every tag the index
/// keeps is set to, besides title ("Title<variant>") and artist.
pub const RICH_TAGS: &[(&str, &str)] = &[
    ("album", "Album"),
    ("albumartist", "AlbumArtist"),
    ("genre", "Genre"),
    ("composer", "Composer"),
    ("date", "2001"),
    ("tracknumber", "3"),
    ("tracktotal", "12"),
    ("discnumber", "2"),
    ("disctotal", "4"),
    ("compilation", "1"),
];

/// A Flac or Ogg fixture carrying all of [`RICH_TAGS`] (a Vorbis comment
/// format: the names above are written as they are).
pub fn rich_fixture(kind: Kind, variant: u32) -> PathBuf {
    assert!(matches!(kind, Kind::Flac | Kind::Ogg));
    build(kind, variant, 1, "Rich", RICH_TAGS)
}

fn build(kind: Kind, variant: u32, secs: u32, prefix: &str, extra: &[(&str, &str)]) -> PathBuf {
    let dir = target_tmp().join("audio-fixtures");
    let name = match secs {
        1 => format!("{prefix}{kind:?}-{variant}.{}", kind.ext()),
        n => format!("{prefix}{kind:?}-{variant}-{n}s.{}", kind.ext()),
    };
    let path = dir.join(&name);
    let _guard = GENERATE.lock().unwrap_or_else(|e| e.into_inner());
    if path.exists() {
        return path;
    }
    fs::create_dir_all(&dir).unwrap();
    // Unique per thread as well as per process: a test binary that includes
    // this file more than once (the GUI crate's) has one lock per copy.
    let tmp = dir.join(format!(
        ".{}-{:?}-{name}",
        std::process::id(),
        std::thread::current().id()
    ));
    let out = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!(
            "sine=frequency={}:duration={secs}",
            300 + variant * 37
        ))
        .arg("-metadata")
        .arg(format!("title=Title{variant}"))
        .arg("-metadata")
        .arg(format!("artist=Artist{variant}"))
        .args(
            extra
                .iter()
                .flat_map(|(k, v)| ["-metadata".to_owned(), format!("{k}={v}")]),
        )
        .args(kind.ffmpeg_args())
        // The output name must keep its extension for ffmpeg to pick a muxer.
        .arg("-f")
        .arg(match kind {
            Kind::Mp3 => "mp3",
            Kind::Aac | Kind::Alac => "ipod",
            Kind::Wav => "wav",
            Kind::Aiff => "aiff",
            Kind::Flac => "flac",
            Kind::Ogg => "ogg",
            Kind::Opus => "opus",
            Kind::FlacInM4a | Kind::OpusInM4a => "mp4",
        })
        .arg(&tmp)
        .output()
        .expect("ffmpeg must be installed");
    assert!(
        out.status.success(),
        "ffmpeg failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::rename(&tmp, &path).unwrap();
    path
}

/// One ID3v2.4 frame: id, syncsafe size, no flags, then `data`.
pub fn id3v24_frame(id: &str, data: &[u8]) -> Vec<u8> {
    assert_eq!(id.len(), 4);
    let n = data.len() as u32;
    assert!(n < 1 << 28);
    let mut f = id.as_bytes().to_vec();
    f.extend([
        (n >> 21) as u8 & 0x7f,
        (n >> 14) as u8 & 0x7f,
        (n >> 7) as u8 & 0x7f,
        n as u8 & 0x7f,
    ]);
    f.extend([0, 0]);
    f.extend(data);
    f
}

/// A text frame: encoding byte, then the raw text bytes.
pub fn id3v24_text(id: &str, encoding: u8, text: &[u8]) -> Vec<u8> {
    let mut d = vec![encoding];
    d.extend(text);
    id3v24_frame(id, d.as_slice())
}

/// The bytes of the cached Mp3 fixture `variant` with its ID3v2 tag replaced
/// by a hand-built ID3v2.4 tag made of `frames`. The audio is untouched.
pub fn mp3_with_id3v2(variant: u32, frames: &[Vec<u8>]) -> Vec<u8> {
    mp3_secs_with_id3v2(variant, 1, frames)
}

/// [`mp3_with_id3v2`] on the fixture lasting `secs` seconds.
pub fn mp3_secs_with_id3v2(variant: u32, secs: u32, frames: &[Vec<u8>]) -> Vec<u8> {
    let raw = fs::read(fixture_secs(Kind::Mp3, variant, secs)).unwrap();
    let audio = if raw.starts_with(b"ID3") {
        let size = raw[6..10]
            .iter()
            .fold(0usize, |a, b| (a << 7) | *b as usize);
        &raw[10 + size..]
    } else {
        &raw[..]
    };
    let body: Vec<u8> = frames.concat();
    let n = body.len() as u32;
    let mut out = b"ID3\x04\x00\x00".to_vec();
    out.extend([
        (n >> 21) as u8 & 0x7f,
        (n >> 14) as u8 & 0x7f,
        (n >> 7) as u8 & 0x7f,
        n as u8 & 0x7f,
    ]);
    out.extend(body);
    out.extend(audio);
    out
}

/// The bytes of the cached Aac fixture `variant` with the `mp4a` sample entry
/// renamed `drms`, which is how FairPlay protected iTunes Store files look.
pub fn fairplay_m4a(variant: u32) -> Vec<u8> {
    let mut b = fs::read(fixture(Kind::Aac, variant)).unwrap();
    let stsd = b.windows(4).position(|w| w == b"stsd").expect("stsd atom");
    // stsd: fourcc, version/flags (4), entry count (4), then entry size (4) and format.
    let at = stsd + 4 + 4 + 4 + 4;
    assert_eq!(&b[at..at + 4], b"mp4a");
    b[at..at + 4].copy_from_slice(b"drms");
    b
}

pub struct Env {
    _dir: tempfile::TempDir,
    pub music: PathBuf,
    pub lists: PathBuf,
    pub cfg: Config,
    pub store: Store,
}

impl Env {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let music = base.join("music");
        let lists = base.join("lists");
        fs::create_dir(&music).unwrap();
        fs::create_dir(&lists).unwrap();
        let mut cfg = Config::default();
        cfg.library.roots = vec![music.clone()];
        cfg.playlists.dir = lists.clone();
        // The fake cards are folders in a temp dir.
        cfg.device.allow_folder = true;
        Env {
            _dir: dir,
            music,
            lists,
            cfg,
            store: Store::open(&base.join("db/fiio-tunes.sqlite")).unwrap(),
        }
    }

    pub fn relative_playlists(mut self) -> Self {
        self.cfg.playlists.path_style = PathStyle::Relative;
        self
    }

    /// Writes `bytes` to `rel` under the music folder.
    pub fn add_bytes(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let dest = self.music.join(rel);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, bytes).unwrap();
        dest
    }

    /// Copies a fixture to `rel` under the music folder.
    pub fn add(&self, rel: &str, kind: Kind, variant: u32) -> PathBuf {
        self.add_at(&self.music, rel, kind, variant)
    }

    pub fn add_at(&self, root: &Path, rel: &str, kind: Kind, variant: u32) -> PathBuf {
        let dest = root.join(rel);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::copy(fixture(kind, variant), &dest).unwrap();
        dest
    }

    pub fn scan(&mut self) -> ScanReport {
        self.try_scan().expect("scan succeeds")
    }

    pub fn try_scan(&mut self) -> fiio_core::Result<ScanReport> {
        scan::scan(&mut self.store, &self.cfg, &|_| {})
    }

    pub fn tracks(&self) -> Vec<TrackRow> {
        store::all_tracks(self.store.conn()).unwrap()
    }

    pub fn track(&self, path: &Path) -> TrackRow {
        self.tracks()
            .into_iter()
            .find(|t| t.path == path)
            .unwrap_or_else(|| panic!("no track at {}", path.display()))
    }

    pub fn playlist_file(&self, name: &str) -> PathBuf {
        self.lists.join(format!("{name}.m3u8"))
    }

    pub fn write_playlist(&self, name: &str, text: &str) -> PathBuf {
        let p = self.playlist_file(name);
        fs::write(&p, text).unwrap();
        p
    }

    pub fn read_playlist(&self, name: &str) -> String {
        fs::read_to_string(self.playlist_file(name)).unwrap()
    }

    /// The location lines of a playlist file, in order.
    pub fn locations(&self, name: &str) -> Vec<String> {
        self.read_playlist(name)
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(String::from)
            .collect()
    }

    /// `(position, track_id, raw_entry)` rows of a playlist in the index.
    pub fn indexed_items(&self, name: &str) -> Vec<(i64, Option<i64>, String)> {
        let mut stmt = self
            .store
            .conn()
            .prepare(
                "SELECT i.position, i.track_id, i.raw_entry FROM playlist_items i \
                 JOIN playlists p ON p.id = i.playlist_id WHERE p.name = ?1 ORDER BY i.position",
            )
            .unwrap();
        stmt.query_map([name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    pub fn playlist_id(&self, name: &str) -> Option<i64> {
        store::playlists(self.store.conn())
            .unwrap()
            .into_iter()
            .find(|p| p.name == name)
            .map(|p| p.id)
    }
}

/// Whether directory permission bits stop us reading (false when running as root).
pub fn chmod_000_blocks_reads(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).unwrap();
    let blocked = fs::read_dir(dir).is_err();
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
    blocked
}

/// Where fixtures and tools are built. Cargo sets `CARGO_TARGET_TMPDIR` for
/// integration tests; a unit-test binary that includes this file (the GUI
/// crate's) uses the workspace's `target/tmp` instead: removed by `cargo
/// clean`, and not shared with other users as `/tmp` would be.
fn target_tmp() -> &'static Path {
    Path::new(
        option_env!("CARGO_TARGET_TMPDIR")
            .unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/tmp")),
    )
}

/// A card that is a folder, and a way to prove nothing touched it. It
/// remembers every file put on it. [`FakeCard::age`] gives every entry the
/// same old access time and a newer modification time (a plain read would
/// then update the access time even under relatime), and
/// [`FakeCard::assert_untouched`] checks bytes, modification times, access
/// times and the set of entries against what was put there.
pub struct FakeCard {
    pub root: PathBuf,
    files: std::collections::BTreeMap<String, Vec<u8>>,
    control: PathBuf,
    atime_tracked: bool,
}

const OLD_ATIME: u64 = 1_000_000;
const OLD_MTIME: u64 = 2_000_000;

impl FakeCard {
    /// `root` is an existing, empty folder. A control file is made next to
    /// it (`root/../atime-control`), which is not part of the card.
    pub fn new(root: PathBuf) -> Self {
        let control = root.parent().unwrap().join("atime-control");
        fs::write(&control, b"c").unwrap();
        FakeCard {
            root,
            files: Default::default(),
            control,
            atime_tracked: true,
        }
    }

    pub fn put(&mut self, rel: &str, bytes: &[u8]) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
        self.files.insert(rel.to_owned(), bytes.to_vec());
    }

    /// Copies a fixture to `rel`.
    pub fn copy(&mut self, rel: &str, kind: Kind, variant: u32) {
        self.put(rel, &fs::read(fixture(kind, variant)).unwrap());
    }

    pub fn remove(&mut self, rel: &str) {
        fs::remove_file(self.root.join(rel)).unwrap();
        self.files.remove(rel);
    }

    /// Every entry's relative path, folders included, and the root as "".
    fn entries(&self) -> std::collections::BTreeSet<String> {
        let mut all = std::collections::BTreeSet::new();
        all.insert(String::new());
        for f in self.files.keys() {
            all.insert(f.clone());
            let mut p = Path::new(f).parent();
            while let Some(d) = p.filter(|d| !d.as_os_str().is_empty()) {
                all.insert(d.to_str().unwrap().to_owned());
                p = d.parent();
            }
        }
        all
    }

    fn set_times(path: &Path) {
        use std::time::{Duration, UNIX_EPOCH};
        let f = fs::File::open(path).unwrap();
        f.set_times(
            fs::FileTimes::new()
                .set_accessed(UNIX_EPOCH + Duration::from_secs(OLD_ATIME))
                .set_modified(UNIX_EPOCH + Duration::from_secs(OLD_MTIME)),
        )
        .unwrap();
    }

    /// Gives every entry the old times, and checks that this file system
    /// moves an access time on a plain read, which the later check needs.
    pub fn age(&mut self) {
        use std::os::unix::fs::MetadataExt;
        for rel in self.entries() {
            Self::set_times(&self.root.join(rel));
        }
        Self::set_times(&self.control);
        let _ = fs::read(&self.control).unwrap();
        self.atime_tracked = fs::metadata(&self.control).unwrap().atime() != OLD_ATIME as i64;
        Self::set_times(&self.control);
    }

    /// Nothing on the card changed since [`FakeCard::age`]: same bytes, same
    /// times, no entry added or removed. Ages the card again afterwards (the
    /// check lists folders the plain way).
    pub fn assert_untouched(&mut self) {
        use std::io::Read;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        for rel in self.entries() {
            let p = self.root.join(&rel);
            let md = fs::metadata(&p).unwrap();
            let shown = if rel.is_empty() {
                "the card root"
            } else {
                &rel
            };
            assert_eq!(md.mtime(), OLD_MTIME as i64, "{shown}: modified");
            if self.atime_tracked {
                assert_eq!(md.atime(), OLD_ATIME as i64, "{shown}: read the plain way");
            } else {
                eprintln!("atime check skipped: file system does not update atime");
            }
            if let Some(want) = self.files.get(&rel) {
                // O_NOATIME, so checking does not move the access time.
                let mut f = fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(0o1_000_000)
                    .open(&p)
                    .unwrap();
                let mut got = Vec::new();
                f.read_to_end(&mut got).unwrap();
                assert!(&got == want, "{shown}: bytes changed");
            }
        }
        let mut found = std::collections::BTreeSet::new();
        found.insert(String::new());
        let mut stack = vec![self.root.clone()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap() {
                let e = e.unwrap();
                let rel = e
                    .path()
                    .strip_prefix(&self.root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned();
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                }
                found.insert(rel);
            }
        }
        assert_eq!(
            found,
            self.entries(),
            "entries added to or removed from the card"
        );
        self.age();
    }
}
