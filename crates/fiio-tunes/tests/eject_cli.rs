//! End to end test of `fiio-tunes eject` refusing a card that is a folder.
//! HOME and the XDG folders are a temp dir, FIIO_TUNES_NO_DETECT is set, and
//! PATH holds only a stand-in `udisksctl` that records its arguments and
//! fails: the real one is never reachable, and a refusal that failed to
//! refuse would show up as a recorded call.

#[path = "../../fiio-core/tests/common/mod.rs"]
mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use common::{FakeCard, Kind, fixture};
use fiio_core::config::Config;

struct Sandbox {
    dir: tempfile::TempDir,
    card: FakeCard,
    bin: PathBuf,
    log: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let (music, lists, card, bin) = (
            base.join("music"),
            base.join("lists"),
            base.join("card"),
            base.join("bin"),
        );
        for d in [&music, &lists, &card, &bin] {
            fs::create_dir(d).unwrap();
        }
        let log = base.join("udisksctl.log");
        let tool = bin.join("udisksctl");
        fs::write(
            &tool,
            format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 1\n", log.display()),
        )
        .unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let mut card = FakeCard::new(card);
        card.copy("Music/a.mp3", Kind::Mp3, 1);
        card.age();
        let sb = Sandbox {
            dir,
            card,
            bin,
            log,
        };
        assert_eq!(
            sb.run(&[
                "init",
                "--library",
                music.to_str().unwrap(),
                "--playlists",
                lists.to_str().unwrap()
            ])
            .status
            .code(),
            Some(0)
        );
        fs::copy(fixture(Kind::Mp3, 1), music.join("a.mp3")).unwrap();
        sb
    }

    fn base(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn edit_config(&self, f: impl FnOnce(&mut Config)) {
        let file = self.base().join("config/fiio-tunes/config.toml");
        let mut c = Config::load(&file).unwrap();
        f(&mut c);
        c.save(&file).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fiio-tunes"))
            .env("HOME", self.base().join("home"))
            .env("XDG_CONFIG_HOME", self.base().join("config"))
            .env("XDG_DATA_HOME", self.base().join("data"))
            .env("FIIO_TUNES_NO_DETECT", "1")
            .env("PATH", &self.bin)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// The arguments `udisksctl` was called with, one call per line.
    fn udisksctl_calls(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

// Prevents: `eject` unmounting or powering off something that is not the
// X3's card: a folder used as a card (`allow_folder`) has no block device,
// so the command must exit 1 naming it, call no `udisksctl`, and leave the
// card as it was.
#[test]
fn eject_refuses_a_folder_card_calls_no_udisksctl_and_leaves_the_card_alone() {
    let mut sb = Sandbox::new();
    let root = sb.card.root.clone();
    sb.edit_config(|c| {
        c.device.mountpoint = root.clone();
        c.device.allow_folder = true;
    });
    let o = sb.run(&["eject"]);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", err(&o));
    assert!(err(&o).contains(root.to_str().unwrap()), "{}", err(&o));
    assert!(String::from_utf8_lossy(&o.stdout).trim().is_empty());
    assert_eq!(sb.udisksctl_calls(), "", "udisksctl must not be called");
    sb.card.assert_untouched();
}

// Prevents: `eject` guessing a card when none is configured and detection is
// off: it must fail and call nothing.
#[test]
fn eject_without_a_card_fails_and_calls_no_udisksctl() {
    let sb = Sandbox::new();
    let o = sb.run(&["eject"]);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", err(&o));
    assert_eq!(sb.udisksctl_calls(), "");
}
