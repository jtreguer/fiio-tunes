//! Reading the card: its audio files and its `.m3u8` playlists. Folders are
//! opened with `O_NOATIME` and files are only stat'ed, so reading the card
//! leaves its access dates alone. Nothing here writes.

use std::ffi::CStr;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags};

use crate::Result;
use crate::error::IoContext;
use crate::tags::{AUDIO_EXTENSIONS, UNSUPPORTED_EXTENSIONS};

/// The folder fiio-tunes keeps its manifest in, on the card.
pub const STATE_DIR: &str = ".fiio-tunes";

/// A file on the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardFile {
    /// Relative to the card's root, with `/`, exactly as on the card.
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Contents {
    /// Audio files, the formats fiio-tunes copies and the ones it reports,
    /// sorted by path.
    pub audio: Vec<CardFile>,
    /// `.m3u8` files, sorted by path.
    pub playlists: Vec<CardFile>,
    /// `(path, why)` for what could not be read; the listing is then
    /// incomplete.
    pub problems: Vec<(String, String)>,
    /// Whether `.fiio-tunes/manifest.json` is there.
    pub manifest: bool,
}

impl Contents {
    pub fn audio_bytes(&self) -> u64 {
        self.audio.iter().map(|f| f.size).sum()
    }
}

/// Entries a walk of the card skips: dot entries (`.Trash-1000`,
/// `.fiio-tunes`, macOS' `._` files) and Windows' system folder.
fn skipped(name: &str) -> bool {
    name.starts_with('.') || name.eq_ignore_ascii_case("System Volume Information")
}

fn extension(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty()).then(|| ext.to_ascii_lowercase())
}

/// Whether `name` is an audio file by its extension.
pub fn is_audio(name: &str) -> bool {
    extension(name).is_some_and(|e| {
        AUDIO_EXTENSIONS.contains(&e.as_str()) || UNSUPPORTED_EXTENSIONS.contains(&e.as_str())
    })
}

pub fn is_playlist(name: &str) -> bool {
    extension(name).is_some_and(|e| e == "m3u8")
}

/// Opens a folder for listing without touching its access time.
/// `O_NOATIME` needs the caller to own it (on vfat, the `uid=` mount
/// option); otherwise a plain open. `follow` is for the root, which may be
/// given through a link; a folder below it that is a link is not walked.
fn open_dir<Fd: AsFd>(at: Fd, name: &CStr, follow: bool) -> std::io::Result<OwnedFd> {
    let mut flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    if !follow {
        flags |= OFlags::NOFOLLOW;
    }
    match rustix::fs::openat(&at, name, flags | OFlags::NOATIME, Mode::empty()) {
        Err(rustix::io::Errno::PERM) => {
            rustix::fs::openat(&at, name, flags, Mode::empty()).map_err(Into::into)
        }
        r => r.map_err(Into::into),
    }
}

/// Lists the card at `root`. A root that cannot be opened is an error; a
/// folder or file below it that cannot be read is a problem in the result.
pub fn read(root: &Path) -> Result<Contents> {
    let c_root = std::ffi::CString::new(root.as_os_str().as_encoded_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
        .at(root)?;
    let fd = open_dir(CWD, &c_root, true).at(root)?;
    let mut out = Contents::default();
    let manifest = format!("{STATE_DIR}/manifest.json");
    match std::fs::metadata(root.join(&manifest)) {
        Ok(m) => out.manifest = m.is_file(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => out.problems.push((manifest, e.to_string())),
    }
    walk(fd, "", &mut out);
    out.audio.sort_by(|a, b| a.path.cmp(&b.path));
    out.playlists.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn walk(dir: OwnedFd, prefix: &str, out: &mut Contents) {
    let shown = |name: &str| format!("{prefix}{name}");
    let entries = match Dir::read_from(&dir) {
        Ok(d) => d,
        Err(e) => {
            out.problems
                .push((prefix.trim_end_matches('/').to_owned(), e.to_string()));
            return;
        }
    };
    let mut subdirs = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                out.problems
                    .push((prefix.trim_end_matches('/').to_owned(), e.to_string()));
                break;
            }
        };
        let raw = entry.file_name();
        if matches!(raw.to_bytes(), b"." | b"..") {
            continue;
        }
        let lossy = raw.to_string_lossy();
        if skipped(&lossy) {
            continue;
        }
        let st = match rustix::fs::statat(&dir, raw, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            Err(e) => {
                out.problems.push((shown(&lossy), e.to_string()));
                continue;
            }
        };
        let Ok(name) = raw.to_str() else {
            // Only a name that could hold audio leaves the listing short.
            let kind = FileType::from_raw_mode(st.st_mode);
            if kind == FileType::Directory || is_audio(&lossy) || is_playlist(&lossy) {
                out.problems
                    .push((shown(&lossy), "name is not valid UTF-8".into()));
            }
            continue;
        };
        match FileType::from_raw_mode(st.st_mode) {
            FileType::Directory => subdirs.push((raw.to_owned(), name.to_owned())),
            FileType::RegularFile if is_audio(name) || is_playlist(name) => {
                let file = CardFile {
                    path: shown(name),
                    size: st.st_size as u64,
                    mtime_ns: st.st_mtime * 1_000_000_000 + st.st_mtime_nsec as i64,
                };
                match is_playlist(name) {
                    true => out.playlists.push(file),
                    false => out.audio.push(file),
                }
            }
            _ => {}
        }
    }
    for (raw, name) in subdirs {
        let path = shown(&name);
        match open_dir(&dir, &raw, false) {
            Ok(fd) => walk(fd, &format!("{path}/"), out),
            Err(e) => out.problems.push((path, e.to_string())),
        }
    }
}

/// Opens a file of the card for reading without updating its access time.
/// `O_NOATIME` needs the caller to own it (on vfat, the `uid=` mount
/// option); otherwise a plain open.
pub fn open_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let noatime = OFlags::NOATIME.bits() as i32;
    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(noatime)
        .open(path)
    {
        Err(e) if e.raw_os_error() == Some(rustix::io::Errno::PERM.raw_os_error()) => {
            std::fs::File::open(path)
        }
        r => r,
    }
}

/// BLAKE3 of the file at `path` as stored, not as cached: its pages are
/// dropped from the page cache first, so the bytes come from the card.
/// The file must have been synced (`sync_all`), or its dirty pages stay.
pub fn read_back(path: &Path) -> std::io::Result<[u8; 32]> {
    let f = open_file(path)?;
    rustix::fs::fadvise(&f, 0, None, rustix::fs::Advice::DontNeed)?;
    crate::scan::hash_reader(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_audio_and_playlists_and_skips_hidden_and_system_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (p, bytes) in [
            ("Music/A/01 é.mp3", &b"abc"[..]),
            ("Old Album/02.FLAC", b"x"),
            ("Old Album/cover.jpg", b"jpg"),
            ("Old Album/x.wma", b"w"),
            ("best.m3u8", b"#EXTM3U\n"),
            ("notes.txt", b"t"),
            (".fiio-tunes/manifest.json", b"{}"),
            (".Trash-1000/files/gone.mp3", b"g"),
            ("System Volume Information/IndexerVolumeGuid", b"g"),
            ("Music/._01.mp3", b"mac"),
        ] {
            let p = root.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }
        let c = read(root).unwrap();
        let paths: Vec<&str> = c.audio.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            ["Music/A/01 é.mp3", "Old Album/02.FLAC", "Old Album/x.wma"]
        );
        assert_eq!(c.audio[0].size, 3);
        assert_eq!(c.playlists.len(), 1);
        assert!(c.manifest);
        assert!(c.problems.is_empty(), "{:?}", c.problems);
        assert_eq!(c.audio_bytes(), 5);
    }

    #[test]
    fn a_missing_root_is_an_error() {
        assert!(read(Path::new("/nonexistent/card")).is_err());
    }
}
