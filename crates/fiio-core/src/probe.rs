//! The probe (technical document, section 9.4): playlists in every form the
//! X3 might read, written once to the card so the owner can see on the
//! player which ones play. The files they point at are copies of one small
//! MP3 from the library, under names that try accents, CJK, a pair of
//! names FAT might take for one, and a long path. Everything the probe
//! writes is named `zz-probe`, and `--clean` removes exactly that.

use std::io::Write;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::error::IoContext;
use crate::playlist::{WriteMode, write_atomic};
use crate::store::{self, TrackRow};
use crate::tags::FileKind;
use crate::{Error, Result};

const DIR: &str = "Music/zz-probe";
const PLAIN: &str = "Music/zz-probe/01 Probe.mp3";
const ACCENTS: &str = "Music/zz-probe/02 Élan café.mp3";
const CJK: &str = "Music/zz-probe/03 日本語の歌.mp3";
const CASE_LOWER: &str = "Music/zz-probe/04 é.mp3";
const CASE_UPPER: &str = "Music/zz-probe/04 É.mp3";

/// The folder and file of the long path, over 200 characters in all.
fn long_dir() -> String {
    format!(
        "{DIR}/05 {}",
        "A folder with a long name ".repeat(4).trim_end()
    )
}

fn long_file() -> String {
    format!(
        "{}/{}.mp3",
        long_dir(),
        "A file with a long name ".repeat(4).trim_end()
    )
}

/// One probe playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbePlaylist {
    /// Relative to the card root.
    pub path: String,
    /// What it tries.
    pub what: &'static str,
    pub text: String,
}

/// Everything the probe writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Folders it needs, parents first.
    pub dirs: Vec<String>,
    /// Copies of the source file, relative to the card root.
    pub audio: Vec<String>,
    pub playlists: Vec<ProbePlaylist>,
}

/// How a playlist is written.
struct Form {
    header: bool,
    bom: bool,
    crlf: bool,
}

const PLAIN_FORM: Form = Form {
    header: true,
    bom: false,
    crlf: false,
};

fn render(form: &Form, secs: i64, entries: &[(String, String)]) -> String {
    let eol = if form.crlf { "\r\n" } else { "\n" };
    let mut s = String::new();
    if form.bom {
        s.push('\u{feff}');
    }
    if form.header {
        s.push_str("#EXTM3U");
        s.push_str(eol);
    }
    for (title, location) in entries {
        if form.header {
            s.push_str(&format!("#EXTINF:{secs},fiio-tunes - {title}{eol}"));
        }
        s.push_str(location);
        s.push_str(eol);
    }
    s
}

/// The probe, its `#EXTINF` lines giving `secs`, the source's duration.
/// Pure.
pub fn plan(secs: i64) -> Plan {
    let one = |n: u32, loc: String| vec![(format!("Probe {n:02}"), loc)];
    let plain = || PLAIN.to_owned();
    let mut playlists = Vec::new();
    let mut add =
        |n: u32, dir: &str, what: &'static str, form: &Form, entries: Vec<(String, String)>| {
            playlists.push(ProbePlaylist {
                path: format!("{dir}zz-probe-{n:02}.m3u8"),
                what,
                text: render(form, secs, &entries),
            });
        };
    add(
        1,
        "",
        "card root, Music/... with /, UTF-8, LF, #EXTM3U and #EXTINF",
        &PLAIN_FORM,
        one(1, plain()),
    );
    add(
        2,
        "",
        "as 01, with \\",
        &PLAIN_FORM,
        one(2, PLAIN.replace('/', "\\")),
    );
    add(
        3,
        "",
        "as 01, with a UTF-8 BOM",
        &Form {
            bom: true,
            ..PLAIN_FORM
        },
        one(3, plain()),
    );
    add(
        4,
        "",
        "as 01, CRLF line endings",
        &Form {
            crlf: true,
            ..PLAIN_FORM
        },
        one(4, plain()),
    );
    add(
        5,
        "",
        "as 01, no #EXTM3U and no #EXTINF",
        &Form {
            header: false,
            ..PLAIN_FORM
        },
        one(5, plain()),
    );
    add(
        6,
        "",
        "as 01, a leading /",
        &PLAIN_FORM,
        one(6, format!("/{PLAIN}")),
    );
    add(
        7,
        "",
        "as 01, TF1:/Music/...",
        &PLAIN_FORM,
        one(7, format!("TF1:/{PLAIN}")),
    );
    add(
        8,
        "Playlists/",
        "in Playlists/, ../Music/...",
        &PLAIN_FORM,
        one(8, format!("../{PLAIN}")),
    );
    add(
        9,
        "Playlists/",
        "in Playlists/, Music/... (relative to the card root)",
        &PLAIN_FORM,
        one(9, plain()),
    );
    add(
        10,
        "",
        "as 01, accents, CJK, and é and É (two files, or one if the card merged them)",
        &PLAIN_FORM,
        [
            ("Probe 10 accents", ACCENTS),
            ("Probe 10 CJK", CJK),
            ("Probe 10 é", CASE_LOWER),
            ("Probe 10 É", CASE_UPPER),
        ]
        .map(|(t, p)| (t.to_owned(), p.to_owned()))
        .to_vec(),
    );
    add(
        11,
        "",
        "as 01, a path over 200 characters",
        &PLAIN_FORM,
        one(11, long_file()),
    );
    add(
        12,
        "",
        "as 01, 2,000 entries",
        &PLAIN_FORM,
        (1..=2000)
            .map(|i| (format!("Probe 12 #{i}"), plain()))
            .collect(),
    );
    Plan {
        dirs: vec!["Music".into(), DIR.into(), long_dir(), "Playlists".into()],
        audio: vec![
            PLAIN.into(),
            ACCENTS.into(),
            CJK.into(),
            CASE_LOWER.into(),
            CASE_UPPER.into(),
            long_file(),
        ],
        playlists,
    }
}

/// The library file the probe copies: a present MP3 whose tags were read,
/// preferably of one to ten minutes, the smallest.
pub fn source(conn: &Connection) -> Result<TrackRow> {
    let mp3: Vec<TrackRow> = store::all_tracks(conn)?
        .into_iter()
        .filter(|t| t.missing_since.is_none() && t.info.kind == FileKind::Mp3 && !t.drm)
        .collect();
    let song = |t: &&TrackRow| {
        t.tag_error.is_none()
            && t.info
                .duration_ms
                .is_some_and(|d| (60_000..=600_000).contains(&d))
    };
    mp3.iter()
        .filter(song)
        .min_by_key(|t| (t.size, t.id))
        .or_else(|| mp3.iter().min_by_key(|t| (t.size, t.id)))
        .cloned()
        .ok_or(Error::NoProbeSource)
}

/// What the probe left on the card, in the data folder, so `--clean` can
/// remove the folders it created and no other.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Record {
    card: PathBuf,
    /// Folders the probe created, parents first.
    created: Vec<String>,
}

fn load_record(path: &Path) -> Option<Record> {
    toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub files: usize,
    pub bytes: u64,
    /// Whether the card holds `04 é.mp3` and `04 É.mp3` as two files.
    pub case_pair_kept: bool,
}

fn exists(p: &Path) -> bool {
    p.symlink_metadata().is_ok()
}

/// Whether a file the probe writes is already on the card at `card`.
pub fn already_there(card: &Path) -> bool {
    let plan = plan(0);
    let mut targets = plan
        .audio
        .iter()
        .chain(plan.playlists.iter().map(|p| &p.path));
    targets.any(|t| exists(&card.join(t)))
}

/// Refuses a probe of the card at `card` when one of its files is already
/// there, or when `record` remembers a probe of another card not cleaned.
pub fn check(card: &Path, record: &Path) -> Result<()> {
    if already_there(card) {
        return Err(Error::ProbeThere(card.to_owned()));
    }
    // A record of another card that no longer holds probe files (cleaned
    // by hand, formatted, or this card mounted elsewhere now) is stale.
    match load_record(record) {
        Some(r) if r.card != card && already_there(&r.card) => Err(Error::ProbeElsewhere(r.card)),
        _ => Ok(()),
    }
}

/// Writes the probe to the card at `card`: the copies of `source`, each
/// read back and compared, then the playlists. `record` is the file in
/// the data folder that remembers the folders created. Refused when a
/// probe file is already there.
pub fn write(card: &Path, source: &Path, secs: i64, record: &Path) -> Result<Written> {
    check(card, record)?;
    let plan = plan(secs);
    let bytes = std::fs::read(source).at(source)?;
    let hash = blake3::hash(&bytes);
    // Recorded before anything is created, so a probe cut short can still
    // be cleaned.
    let mut rec = load_record(record)
        .filter(|r| r.card == card)
        .unwrap_or(Record {
            card: card.to_owned(),
            created: Vec::new(),
        });
    let new_dirs: Vec<&String> = plan
        .dirs
        .iter()
        .filter(|d| !exists(&card.join(d)))
        .collect();
    rec.created.extend(new_dirs.iter().map(|d| (*d).clone()));
    if let Some(dir) = record.parent() {
        std::fs::create_dir_all(dir).at(dir)?;
    }
    let text = toml::to_string(&rec).expect("the record always serializes");
    std::fs::write(record, text).at(record)?;
    for d in new_dirs {
        let p = card.join(d);
        std::fs::create_dir(&p).at(&p)?;
    }

    let mut out = Written {
        files: 0,
        bytes: 0,
        case_pair_kept: false,
    };
    for rel in &plan.audio {
        let p = card.join(rel);
        let mut f = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
        {
            // FAT took `04 É` for `04 é`, written just before.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && rel == CASE_UPPER => {
                continue;
            }
            r => r.at(&p)?,
        };
        f.write_all(&bytes).at(&p)?;
        f.sync_all().at(&p)?;
        drop(f);
        if crate::card::read_back(&p).at(&p)? != *hash.as_bytes() {
            return Err(Error::VerifyFailed(format!(
                "{} reads back different from {}",
                p.display(),
                source.display()
            )));
        }
        out.files += 1;
        out.bytes += bytes.len() as u64;
    }
    let dir = card.join(DIR);
    let names: Vec<String> = std::fs::read_dir(&dir)
        .at(&dir)?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    let file_name = |p: &str| p.rsplit('/').next().unwrap_or(p).to_owned();
    out.case_pair_kept = [CASE_LOWER, CASE_UPPER]
        .iter()
        .all(|p| names.contains(&file_name(p)));

    for pl in &plan.playlists {
        let p = card.join(&pl.path);
        write_atomic(&p, &pl.text, WriteMode::CreateNew)?;
        out.files += 1;
        out.bytes += pl.text.len() as u64;
    }
    Ok(out)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cleaned {
    pub files: usize,
    /// Folders removed.
    pub dirs: Vec<String>,
    /// Folders the probe created that were kept because something else is
    /// in them now.
    pub kept: Vec<String>,
}

/// Removes the probe from the card at `card`: its files, its own folders,
/// and the folders it created if they are empty. Anything else is left.
pub fn clean(card: &Path, record: &Path) -> Result<Cleaned> {
    let plan = plan(0);
    let mut out = Cleaned::default();
    let files = plan
        .audio
        .iter()
        .chain(plan.playlists.iter().map(|p| &p.path));
    for rel in files {
        let p = card.join(rel);
        match std::fs::remove_file(&p) {
            Ok(()) => out.files += 1,
            // Not there, or a file where its folder should be.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(e) => return Err(e).at(&p),
        }
    }
    // Temp files of a playlist write cut short, in the folders the probe
    // writes playlists to. No other fiio-tunes writes while the lock is
    // held.
    for d in ["", "Playlists"] {
        let Ok(entries) = std::fs::read_dir(card.join(d)) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".fiio-tunes-") && name.ends_with(".tmp") {
                std::fs::remove_file(e.path()).at(e.path())?;
                out.files += 1;
            }
        }
    }
    // A record of another card is left for that card.
    let rec = load_record(record);
    let ours = rec.as_ref().is_none_or(|r| r.card == card);
    let created = rec
        .filter(|r| r.card == card)
        .map(|r| r.created)
        .unwrap_or_default();
    // Its own folders always; the shared ones only if it made them.
    let mut dirs: Vec<String> = plan
        .dirs
        .iter()
        .filter(|d| d.starts_with(DIR) || created.contains(d))
        .cloned()
        .collect();
    dirs.reverse();
    for d in dirs {
        let p = card.join(&d);
        match std::fs::remove_dir(&p) {
            Ok(()) => out.dirs.push(d),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => out.kept.push(d),
            Err(e) => return Err(e).at(&p),
        }
    }
    if ours {
        match std::fs::remove_file(record) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).at(record),
            _ => {}
        }
    }
    for d in ["", "Playlists"] {
        if let Ok(f) = std::fs::File::open(card.join(d)) {
            let _ = f.sync_all();
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plan_has_the_twelve_variants_of_the_spec() {
        let p = plan(213);
        assert_eq!(p.playlists.len(), 12);
        let v = |n: usize| &p.playlists[n - 1];
        assert_eq!(v(1).path, "zz-probe-01.m3u8");
        assert_eq!(
            v(1).text,
            "#EXTM3U\n#EXTINF:213,fiio-tunes - Probe 01\nMusic/zz-probe/01 Probe.mp3\n"
        );
        assert!(v(2).text.contains("Music\\zz-probe\\01 Probe.mp3\n"));
        assert!(v(3).text.starts_with("\u{feff}#EXTM3U\n"));
        assert!(v(4).text.ends_with("01 Probe.mp3\r\n") && !v(4).text.contains("3U\n"));
        assert_eq!(v(5).text, "Music/zz-probe/01 Probe.mp3\n");
        assert!(v(6).text.contains("\n/Music/zz-probe/01 Probe.mp3\n"));
        assert!(v(7).text.contains("\nTF1:/Music/zz-probe/01 Probe.mp3\n"));
        assert_eq!(v(8).path, "Playlists/zz-probe-08.m3u8");
        assert!(v(8).text.contains("\n../Music/zz-probe/01 Probe.mp3\n"));
        assert_eq!(v(9).path, "Playlists/zz-probe-09.m3u8");
        assert_eq!(
            v(10).text.lines().filter(|l| !l.starts_with('#')).count(),
            4
        );
        let long = v(11).text.lines().last().unwrap();
        assert!(long.chars().count() > 200, "{}", long.chars().count());
        assert!(long.split('/').all(|c| c.encode_utf16().count() <= 255));
        assert_eq!(
            v(12).text.lines().filter(|l| !l.starts_with('#')).count(),
            2000
        );
        // Every location points at a file the probe writes.
        for pl in &p.playlists[..] {
            let lines = pl.text.lines().map(|l| l.trim_start_matches('\u{feff}'));
            for l in lines.filter(|l| !l.starts_with('#')) {
                let l = l
                    .trim_start_matches("TF1:")
                    .trim_start_matches("../")
                    .trim_start_matches('/')
                    .replace('\\', "/");
                assert!(p.audio.contains(&l), "{} points at {l}", pl.path);
            }
        }
    }

    #[test]
    fn write_then_clean_leaves_the_card_as_it_was() {
        let card = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let record = data.path().join("probe.toml");
        std::fs::create_dir(card.path().join("Music")).unwrap();
        std::fs::write(card.path().join("Music/mine.mp3"), b"keep").unwrap();
        let src = data.path().join("src.mp3");
        std::fs::write(&src, b"ID3 audio").unwrap();

        let w = write(card.path(), &src, 200, &record).unwrap();
        assert_eq!(w.files, 6 + 12);
        assert!(w.case_pair_kept, "ext4 and tmpfs keep both");
        assert_eq!(std::fs::read(card.path().join(CJK)).unwrap(), b"ID3 audio");
        assert!(matches!(
            write(card.path(), &src, 200, &record),
            Err(Error::ProbeThere(_))
        ));

        let c = clean(card.path(), &record).unwrap();
        assert_eq!(c.files, 18);
        // "Music" was there before: kept; "Playlists" was made: removed.
        assert!(c.dirs.contains(&"Playlists".to_owned()));
        assert!(!c.dirs.contains(&"Music".to_owned()));
        let mut left: Vec<_> = walkdir::WalkDir::new(card.path())
            .into_iter()
            .map(|e| {
                e.unwrap()
                    .path()
                    .strip_prefix(card.path())
                    .unwrap()
                    .to_owned()
            })
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                Path::new(""),
                Path::new("Music"),
                Path::new("Music/mine.mp3")
            ]
        );
        assert!(!record.exists());
        // Cleaning again finds nothing to do.
        assert_eq!(clean(card.path(), &record).unwrap(), Cleaned::default());
    }
}
