//! The probe (technical document, section 9.4): playlists in every form the
//! X3 might read, written once to the card so the owner can see on the
//! player which ones play. Each variant points at its own copy of one small
//! MP3 from the library, tagged with a title that names the variant, so
//! what plays on the X3 says which variant worked. Everything the probe
//! writes is named `zz-probe`, and `--clean` removes exactly that.

use std::io::{Cursor, Seek, Write};
use std::path::{Path, PathBuf};

use lofty::TextEncoding;
use lofty::config::WriteOptions;
use lofty::id3::v2::{Frame, FrameId, Id3v2Tag, TextInformationFrame};
use lofty::prelude::TagExt;
use lofty::tag::TagType;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::error::IoContext;
use crate::playlist::{WriteMode, write_atomic};
use crate::store::{self, TrackRow};
use crate::tags::FileKind;
use crate::{Error, Result};

const DIR: &str = "Music/zz-probe";

/// The folder of the long path, over 200 characters with its file.
fn long_dir() -> String {
    format!(
        "{DIR}/11 {}",
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

/// One copy of the source on the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeAudio {
    /// Relative to the card root.
    pub path: String,
    /// The title tag of the copy: what the X3 shows when it plays.
    pub title: String,
    /// Its artist tag: `fiio-tunes`, or for variant 10 the accented or CJK
    /// text the X3 must show.
    pub artist: String,
}

/// One probe playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbePlaylist {
    /// Relative to the card root.
    pub path: String,
    /// What it tries.
    pub what: &'static str,
    pub text: String,
    /// The titles of the files its entries point at, in order, once each.
    pub titles: Vec<String>,
    /// Entries it holds.
    pub entries: usize,
    /// What else to look at on the X3, for the checklist; may be empty.
    pub hint: &'static str,
}

/// Everything the probe writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Folders it needs, parents first.
    pub dirs: Vec<String>,
    /// Copies of the source file.
    pub audio: Vec<ProbeAudio>,
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

/// `entries` are `(#EXTINF title, location)`.
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
            s.push_str(&format!("#EXTINF:{secs},{title}{eol}"));
        }
        s.push_str(location);
        s.push_str(eol);
    }
    s
}

/// One variant: its playlist, how it is written, and its files as
/// `(label, path under DIR, artist)`, each written once and named by its
/// label.
struct Variant {
    n: u32,
    playlist: String,
    what: &'static str,
    form: Form,
    files: Vec<(&'static str, String, &'static str)>,
    /// The location written for a file's path relative to the card root.
    locate: fn(&str) -> String,
    /// Above 1, the playlist holds this many entries, each numbered: the
    /// first file for all but the last, the second file last.
    repeat: usize,
    hint: &'static str,
}

const ARTIST: &str = "fiio-tunes";

fn variants() -> Vec<Variant> {
    let root = |n: u32| format!("zz-probe-{n:02}.m3u8");
    let file = |n: u32| format!("{n:02} Probe.mp3");
    let plain = |s: &str| s.to_owned();
    let one = |n: u32,
               playlist: String,
               what: &'static str,
               label: &'static str,
               form: Form,
               locate: fn(&str) -> String| Variant {
        n,
        playlist,
        what,
        form,
        files: vec![(label, file(n), ARTIST)],
        locate,
        repeat: 1,
        hint: "",
    };
    vec![
        one(
            1,
            root(1),
            "card root, Music/... with /, UTF-8, LF, #EXTM3U and #EXTINF",
            "basic",
            PLAIN_FORM,
            plain,
        ),
        one(2, root(2), "as 01, with \\", "backslash", PLAIN_FORM, |s| {
            s.replace('/', "\\")
        }),
        one(
            3,
            root(3),
            "as 01, a UTF-8 BOM",
            "BOM",
            Form {
                bom: true,
                ..PLAIN_FORM
            },
            plain,
        ),
        one(
            4,
            root(4),
            "as 01, CRLF line endings",
            "CRLF",
            Form {
                crlf: true,
                ..PLAIN_FORM
            },
            plain,
        ),
        Variant {
            hint: "it has no #EXTINF line, so no \"List 05 ...\" text can show",
            ..one(
                5,
                root(5),
                "as 01, no #EXTM3U and no #EXTINF",
                "no header",
                Form {
                    header: false,
                    ..PLAIN_FORM
                },
                plain,
            )
        },
        one(
            6,
            root(6),
            "as 01, a leading /",
            "leading slash",
            PLAIN_FORM,
            |s| format!("/{s}"),
        ),
        one(
            7,
            root(7),
            "as 01, TF1:/Music/...",
            "TF1",
            PLAIN_FORM,
            |s| format!("TF1:/{s}"),
        ),
        one(
            8,
            format!("Playlists/{}", root(8)),
            "in Playlists/, ../Music/...",
            "dot-dot",
            PLAIN_FORM,
            |s| format!("../{s}"),
        ),
        one(
            9,
            format!("Playlists/{}", root(9)),
            "in Playlists/, Music/... (relative to the card root)",
            "root-relative",
            PLAIN_FORM,
            plain,
        ),
        Variant {
            n: 10,
            playlist: root(10),
            what: "as 01, two entries: accents, then CJK",
            form: PLAIN_FORM,
            files: vec![
                ("accents", "10 Élan café.mp3".into(), "Élan café"),
                ("CJK", "10 日本語の歌.mp3".into(), "日本語の歌"),
            ],
            locate: plain,
            repeat: 1,
            hint: "play both entries; note whether the artist reads \"Élan café\", then \
                   \"日本語の歌\" (tags in UTF-16), and whether the file names show right",
        },
        Variant {
            n: 11,
            playlist: root(11),
            what: "as 01, a path over 200 characters",
            form: PLAIN_FORM,
            files: vec![("long path", long_file()[DIR.len() + 1..].to_owned(), ARTIST)],
            locate: plain,
            repeat: 1,
            hint: "",
        },
        Variant {
            n: 12,
            playlist: root(12),
            what: "as 01, 2,000 entries: one file 1,999 times, then another",
            form: PLAIN_FORM,
            files: vec![
                ("2000 entries", file(12), ARTIST),
                ("last", "12 Last.mp3".into(), ARTIST),
            ],
            locate: plain,
            repeat: 2000,
            hint: "go to the end of the list: the last entry should be number 2000 \
                   (\"List 12 #2000\") and play \"Probe 12 last\"; a list that ends \
                   earlier shows the X3's limit",
        },
        one(
            13,
            "zz-probe-13.m3u".into(),
            "as 01, extension .m3u",
            "m3u",
            PLAIN_FORM,
            plain,
        ),
        one(
            14,
            format!("{DIR}/zz-probe-14.m3u8"),
            "in Music/zz-probe/, the bare file name",
            "same folder",
            PLAIN_FORM,
            |s| s.rsplit('/').next().unwrap_or(s).to_owned(),
        ),
    ]
}

/// The probe, its `#EXTINF` lines giving `secs`, the source's duration.
/// Pure.
pub fn plan(secs: i64) -> Plan {
    let mut audio = Vec::new();
    let mut playlists = Vec::new();
    for v in variants() {
        let mut entries = Vec::new();
        let mut titles = Vec::new();
        for (label, name, artist) in &v.files {
            let path = format!("{DIR}/{name}");
            let title = format!("Probe {:02} {label}", v.n);
            entries.push((format!("List {:02} {label}", v.n), (v.locate)(&path)));
            titles.push(title.clone());
            audio.push(ProbeAudio {
                path,
                title,
                artist: (*artist).to_owned(),
            });
        }
        if v.repeat > 1 {
            let (first, last) = (&entries[0].1, &entries[1].1);
            entries = (1..=v.repeat)
                .map(|i| {
                    let at = if i == v.repeat { last } else { first };
                    (format!("List {:02} #{i:04}", v.n), at.clone())
                })
                .collect();
        }
        playlists.push(ProbePlaylist {
            text: render(&v.form, secs, &entries),
            path: v.playlist,
            what: v.what,
            titles,
            entries: entries.len(),
            hint: v.hint,
        });
    }
    Plan {
        dirs: vec!["Music".into(), DIR.into(), long_dir(), "Playlists".into()],
        audio,
        playlists,
    }
}

/// What the owner checks on the X3, one block per playlist. Pure.
pub fn checklist(plan: &Plan) -> String {
    let mut s = String::from(
        "First run `fiio-tunes eject` and unplug the X3. On the X3, open Browse \
         Files (not the categories) and, for each playlist below, note:\n  \
         A. is it listed, in the folder its line names?\n  \
         B. opened, how many entries does it show, and with which text?\n  \
         C. does each entry play, and which title is on the screen: \"Probe NN ...\" \
         (the file's tag), \"List NN ...\" (the playlist's line) or the file name?\n\
         Then plug the X3 in and run `fiio-tunes probe --clean`.\n",
    );
    for p in &plan.playlists {
        s.push_str(&format!(
            "\n{}\n  tries: {}\n  should show {} {}, and play {}\n",
            p.path,
            p.what,
            p.entries,
            if p.entries == 1 { "entry" } else { "entries" },
            p.titles
                .iter()
                .map(|t| format!("\"{t}\""))
                .collect::<Vec<_>>()
                .join(" then ")
        ));
        if !p.hint.is_empty() {
            s.push_str(&format!("  also: {}\n", p.hint));
        }
    }
    s
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

/// An ID3v2.3 text frame: Latin-1 when `text` is ASCII, so a player that
/// reads no UTF-16 still shows it; UTF-16 otherwise.
fn text_frame(id: &'static str, text: &str) -> Frame<'static> {
    let encoding = match text.is_ascii() {
        true => TextEncoding::Latin1,
        false => TextEncoding::UTF16,
    };
    Frame::Text(TextInformationFrame::new(
        FrameId::Valid(id.into()),
        encoding,
        text.to_owned(),
    ))
}

/// `source`, an MP3, with every tag it had replaced by one ID3v2.3 tag:
/// `title`, `artist` and the album `zz-probe`. Works on the bytes in
/// memory.
pub fn tagged(source: &[u8], title: &str, artist: &str) -> std::result::Result<Vec<u8>, String> {
    let opts = WriteOptions::new().use_id3v23(true);
    let mut file = Cursor::new(source.to_vec());
    for t in [TagType::Id3v2, TagType::Ape] {
        file.rewind().map_err(|e| e.to_string())?;
        t.remove_from(&mut file, opts).map_err(|e| e.to_string())?;
    }
    // By hand: lofty's ID3v1 removal appends an empty tag to a file that
    // has none.
    let len = file.get_ref().len();
    if len >= 128 && file.get_ref()[len - 128..].starts_with(b"TAG") {
        file.get_mut().truncate(len - 128);
    }
    let mut tag = Id3v2Tag::new();
    tag.insert(text_frame("TIT2", title));
    tag.insert(text_frame("TPE1", artist));
    tag.insert(text_frame("TALB", "zz-probe"));
    file.rewind().map_err(|e| e.to_string())?;
    tag.save_to(&mut file, opts).map_err(|e| e.to_string())?;
    Ok(file.into_inner())
}

/// What the probe left on the card, in the data folder, so `--clean` can
/// remove the folders it created and no other.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Record {
    card: PathBuf,
    /// Folders the probe created, parents first.
    created: Vec<String>,
}

/// Where the checklist of the probe `record` remembers is saved, next to
/// it; `clean` deletes it with the record.
pub fn checklist_path(record: &Path) -> PathBuf {
    record.with_file_name("probe-checklist.txt")
}

fn load_record(path: &Path) -> Option<Record> {
    toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub files: usize,
    pub bytes: u64,
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
        .map(|a| &a.path)
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

/// Writes the probe to the card at `card`: the tagged copies of `source`,
/// each read back and compared, then the playlists. `record` is the file in
/// the data folder that remembers the folders created. Refused when a
/// probe file is already there.
pub fn write(card: &Path, source: &Path, secs: i64, record: &Path) -> Result<Written> {
    check(card, record)?;
    let plan = plan(secs);
    let bytes = std::fs::read(source).at(source)?;
    // Every copy is tagged before anything is written: a source lofty
    // cannot rewrite leaves the card as it was.
    let copies = plan
        .audio
        .iter()
        .map(|a| {
            tagged(&bytes, &a.title, &a.artist)
                .map_err(|why| Error::ProbeSourceTags(source.to_owned(), why))
        })
        .collect::<Result<Vec<_>>>()?;
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

    let mut out = Written { files: 0, bytes: 0 };
    for (a, copy) in plan.audio.iter().zip(&copies) {
        let p = card.join(&a.path);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
            .at(&p)?;
        f.write_all(copy).at(&p)?;
        f.sync_all().at(&p)?;
        drop(f);
        if crate::card::read_back(&p).at(&p)? != *blake3::hash(copy).as_bytes() {
            return Err(Error::VerifyFailed(format!(
                "{} reads back different from what was written",
                p.display()
            )));
        }
        out.files += 1;
        out.bytes += copy.len() as u64;
    }
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
        .map(|a| &a.path)
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
    for d in ["", "Playlists", DIR] {
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
    // The checklist only with a record of this card: without one, it may be
    // that of a probe elsewhere whose record was lost.
    let mine = rec.as_ref().is_some_and(|r| r.card == card);
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
    let mut gone = Vec::new();
    if ours {
        gone.push(record.to_owned());
    }
    if mine {
        gone.push(checklist_path(record));
    }
    for f in gone {
        match std::fs::remove_file(&f) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).at(f),
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
    fn the_plan_has_the_fourteen_variants_of_the_spec() {
        let p = plan(213);
        assert_eq!(p.playlists.len(), 14);
        let v = |n: usize| &p.playlists[n - 1];
        assert_eq!(v(1).path, "zz-probe-01.m3u8");
        assert_eq!(
            v(1).text,
            "#EXTM3U\n#EXTINF:213,List 01 basic\nMusic/zz-probe/01 Probe.mp3\n"
        );
        assert_eq!(v(1).titles, ["Probe 01 basic"]);
        assert!(v(2).text.contains("\nMusic\\zz-probe\\02 Probe.mp3\n"));
        assert!(v(3).text.starts_with("\u{feff}#EXTM3U\n"));
        assert!(v(4).text.ends_with("04 Probe.mp3\r\n") && !v(4).text.contains("3U\n"));
        assert_eq!(v(5).text, "Music/zz-probe/05 Probe.mp3\n");
        assert!(v(6).text.contains("\n/Music/zz-probe/06 Probe.mp3\n"));
        assert!(v(7).text.contains("\nTF1:/Music/zz-probe/07 Probe.mp3\n"));
        assert_eq!(v(8).path, "Playlists/zz-probe-08.m3u8");
        assert!(v(8).text.contains("\n../Music/zz-probe/08 Probe.mp3\n"));
        assert_eq!(v(9).path, "Playlists/zz-probe-09.m3u8");
        assert_eq!(v(10).titles, ["Probe 10 accents", "Probe 10 CJK"]);
        let long = v(11).text.lines().last().unwrap();
        assert!(long.chars().count() > 200, "{}", long.chars().count());
        assert!(long.split('/').all(|c| c.encode_utf16().count() <= 255));
        assert_eq!(v(12).entries, 2000);
        assert!(
            v(12)
                .text
                .ends_with("#EXTINF:213,List 12 #2000\nMusic/zz-probe/12 Last.mp3\n")
        );
        assert_eq!(v(12).titles, ["Probe 12 2000 entries", "Probe 12 last"]);
        assert_eq!(v(13).path, "zz-probe-13.m3u");
        assert_eq!(v(14).path, "Music/zz-probe/zz-probe-14.m3u8");
        assert!(v(14).text.ends_with("\n14 Probe.mp3\n"));
        // One file per entry label, each title used once.
        let mut titles: Vec<&str> = p.audio.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles.len(), 16);
        titles.sort();
        titles.dedup();
        assert_eq!(titles.len(), 16);
    }
}
