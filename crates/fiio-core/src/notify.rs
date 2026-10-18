//! Desktop notifications at the end of a sync, through `notify-send`. The
//! wording is the same from the CLI and the window. A notification that
//! cannot be shown is ignored: the sync's own output says the same.

use std::process::{Command, Stdio};

use crate::exec::Done;
use crate::sync::bytes;

/// One notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub summary: String,
    pub body: String,
    /// Shown until dismissed: the sync failed.
    pub urgent: bool,
}

/// Longest error text put in a notification; the CLI or the window shows
/// all of it.
const MAX_ERROR: usize = 300;

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// What a sync that wrote the card did: an early stop included, since what
/// was done before it is recorded and checked.
pub fn synced(o: &Done) -> Note {
    let mut done = Vec::new();
    if o.copied > 0 {
        done.push(format!(
            "copied {} ({})",
            count(o.copied, "track", "tracks"),
            bytes(o.bytes)
        ));
    }
    if o.recopied > 0 {
        done.push(format!(
            "copied {} again",
            count(o.recopied, "track", "tracks")
        ));
    }
    if o.moved > 0 {
        done.push(format!("moved {}", count(o.moved, "track", "tracks")));
    }
    if o.removed > 0 {
        done.push(format!("removed {}", count(o.removed, "file", "files")));
    }
    let playlists = o.written + o.renamed + o.deleted;
    if playlists > 0 {
        done.push(format!(
            "made {}",
            count(playlists, "playlist change", "playlist changes")
        ));
    }
    let mut body = match done.split_first() {
        None => "No track or playlist changed.".to_owned(),
        Some((first, rest)) => {
            let mut s = first.clone();
            s[..1].make_ascii_uppercase();
            for (i, part) in rest.iter().enumerate() {
                s.push_str(if i + 1 == rest.len() { " and " } else { ", " });
                s.push_str(part);
            }
            s + "."
        }
    };
    if !o.skipped.is_empty() {
        body.push_str(&format!(
            " {} not copied.",
            count(o.skipped.len(), "file", "files")
        ));
    }
    if !o.warnings.is_empty() {
        body.push_str(&format!(
            " {}.",
            count(o.warnings.len(), "warning", "warnings")
        ));
    }
    let summary = match &o.stopped {
        None => "X3 synced".to_owned(),
        Some(why) => {
            body = format!(
                "It stopped ({why}). What was done is recorded and checked; the next sync goes \
                 on from there. {body}"
            );
            "X3 sync stopped early".to_owned()
        }
    };
    Note {
        summary,
        body,
        urgent: false,
    }
}

/// A sync that failed once its plan was confirmed.
pub fn failed(error: &str) -> Note {
    let mut body = error.trim().to_owned();
    if body.chars().count() > MAX_ERROR {
        body = body.chars().take(MAX_ERROR - 1).collect::<String>() + "…";
    }
    Note {
        summary: "X3 sync failed".into(),
        body,
        urgent: true,
    }
}

/// The arguments `notify-send` gets for `note`.
pub fn args(note: &Note) -> Vec<String> {
    vec![
        "--app-name=fiio-tunes".into(),
        "--icon=multimedia-player".into(),
        format!(
            "--urgency={}",
            if note.urgent { "critical" } else { "normal" }
        ),
        "--".into(),
        note.summary.clone(),
        note.body.clone(),
    ]
}

/// Shows `note`, without waiting for it.
pub fn send(note: &Note) {
    let _ = send_with("notify-send", note);
}

fn send_with(program: &str, note: &Note) -> std::io::Result<()> {
    let mut child = Command::new(program)
        .args(args(note))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Reaped on a thread: notify-send can wait up to 25 s for a
    // notification daemon that is not running. A CLI that exits first
    // leaves it to finish on its own.
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use super::*;

    #[test]
    fn synced_describes_the_counts_that_are_not_zero() {
        // (what is set, the body it must give)
        type Set = fn(&mut Done);
        let cases: Vec<(&str, Set, &str)> = vec![
            ("nothing", |_| {}, "No track or playlist changed."),
            (
                "one track",
                |o| {
                    o.copied = 1;
                    o.bytes = 5_000_000;
                },
                "Copied 1 track (5.0 MB).",
            ),
            (
                "many tracks",
                |o| {
                    o.copied = 3;
                    o.bytes = 999;
                },
                "Copied 3 tracks (999 B).",
            ),
            (
                "recopied only",
                |o| o.recopied = 2,
                "Copied 2 tracks again.",
            ),
            ("one moved", |o| o.moved = 1, "Moved 1 track."),
            ("many moved", |o| o.moved = 6, "Moved 6 tracks."),
            ("one removed file", |o| o.removed = 1, "Removed 1 file."),
            ("removed only", |o| o.removed = 4, "Removed 4 files."),
            (
                "one playlist written",
                |o| o.written = 1,
                "Made 1 playlist change.",
            ),
            (
                "renamed counts",
                |o| o.renamed = 2,
                "Made 2 playlist changes.",
            ),
            (
                "deleted counts",
                |o| o.deleted = 3,
                "Made 3 playlist changes.",
            ),
            (
                "playlist counters are summed",
                |o| {
                    o.written = 4;
                    o.renamed = 2;
                    o.deleted = 1;
                },
                "Made 7 playlist changes.",
            ),
            (
                "two parts join with and",
                |o| {
                    o.removed = 2;
                    o.written = 1;
                },
                "Removed 2 files and made 1 playlist change.",
            ),
            (
                "three parts: comma then and",
                |o| {
                    o.copied = 2;
                    o.moved = 1;
                    o.deleted = 3;
                    o.bytes = 2_000;
                },
                "Copied 2 tracks (2.0 kB), moved 1 track and made 3 playlist changes.",
            ),
            (
                "all five",
                |o| {
                    o.copied = 1;
                    o.recopied = 2;
                    o.moved = 3;
                    o.removed = 4;
                    o.written = 5;
                },
                "Copied 1 track (0 B), copied 2 tracks again, moved 3 tracks, removed 4 \
                 files and made 5 playlist changes.",
            ),
            (
                "one skipped file",
                |o| o.skipped = vec![("/a".into(), "gone".into())],
                "No track or playlist changed. 1 file not copied.",
            ),
            (
                "skipped files and warnings",
                |o| {
                    o.removed = 1;
                    o.skipped = vec![("/a".into(), "x".into()), ("/b".into(), "y".into())];
                    o.warnings = vec!["w".into()];
                },
                "Removed 1 file. 2 files not copied. 1 warning.",
            ),
            (
                "many warnings",
                |o| o.warnings = vec!["a".into(), "b".into(), "c".into()],
                "No track or playlist changed. 3 warnings.",
            ),
        ];
        for (name, set, body) in cases {
            let mut o = Done::default();
            set(&mut o);
            let note = synced(&o);
            assert_eq!(note.body, body, "{name}");
            assert_eq!(note.summary, "X3 synced", "{name}");
            assert!(!note.urgent, "{name}");
        }
    }

    #[test]
    fn synced_after_an_early_stop_says_why_and_keeps_the_counts() {
        let o = Done {
            copied: 2,
            bytes: 1_000_000,
            stopped: Some("interrupted".into()),
            ..Done::default()
        };
        let note = synced(&o);
        assert_eq!(note.summary, "X3 sync stopped early");
        assert!(!note.urgent, "what was written stands");
        assert!(
            note.body.starts_with("It stopped (interrupted)."),
            "{}",
            note.body
        );
        assert!(
            note.body.ends_with("Copied 2 tracks (1.0 MB)."),
            "{}",
            note.body
        );
    }

    #[test]
    fn failed_is_urgent_and_trims() {
        let note = failed("  \n disk full \t\n");
        assert_eq!(note.summary, "X3 sync failed");
        assert_eq!(note.body, "disk full");
        assert!(note.urgent);
    }

    #[test]
    fn failed_truncates_after_exactly_300_characters() {
        let at = "a".repeat(300);
        assert_eq!(failed(&at).body, at, "300 is kept whole");
        let over = "a".repeat(301);
        let body = failed(&over).body;
        assert_eq!(body, format!("{}…", "a".repeat(299)));
        assert_eq!(body.chars().count(), 300);
    }

    #[test]
    fn failed_never_splits_a_multibyte_character() {
        // 2-byte, 3-byte and 4-byte characters around the cut.
        for ch in ["é", "日", "🎵"] {
            let body = failed(&ch.repeat(400)).body;
            assert_eq!(body, format!("{}…", ch.repeat(299)), "{ch}");
        }
        // Trimmed length is what counts, not the padding.
        let padded = format!("   {}   ", "é".repeat(300));
        assert_eq!(failed(&padded).body, "é".repeat(300));
    }

    #[test]
    fn args_set_urgency_and_put_double_dash_before_the_text() {
        for (urgent, level) in [(false, "--urgency=normal"), (true, "--urgency=critical")] {
            let a = args(&Note {
                summary: "--summary".into(),
                body: "-b".into(),
                urgent,
            });
            assert!(a.contains(&level.to_owned()), "{a:?}");
            assert!(a.contains(&"--app-name=fiio-tunes".to_owned()), "{a:?}");
            let dash = a.iter().position(|s| s == "--").unwrap();
            assert_eq!(&a[dash + 1..], ["--summary", "-b"], "{a:?}");
            assert!(
                a[..dash].iter().all(|s| s.starts_with("--")),
                "only options before --: {a:?}"
            );
            assert_eq!(a.iter().filter(|s| s.starts_with("--urgency")).count(), 1);
        }
    }

    use crate::SPAWN;

    fn fake_program(dir: &Path) -> (PathBuf, PathBuf) {
        let (tool, log) = (dir.join("fake-notify-send"), dir.join("log"));
        std::fs::write(
            &tool,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done >> '{}'\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        (tool, log)
    }

    #[test]
    fn send_with_runs_the_program_with_the_note_arguments() {
        let _turn = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let (tool, log) = fake_program(dir.path());
        let note = Note {
            summary: "S".into(),
            body: "two\nlines".into(),
            urgent: true,
        };
        send_with(tool.to_str().unwrap(), &note).unwrap();
        // It does not wait: poll for the log.
        let want = args(&note).join("\n") + "\n";
        let mut got = String::new();
        for _ in 0..200 {
            got = std::fs::read_to_string(&log).unwrap_or_default();
            if got == want {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(got, want);
    }

    #[test]
    fn send_with_a_missing_program_is_an_error_not_a_panic() {
        // It still forks.
        let _turn = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        let note = failed("x");
        let e = send_with("/nonexistent/notify-send", &note).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
    }
}
