//! `device::eject` refuses anything but the block device of a FiiO player.
//! Only the refusals are tested here: the success path runs `udisksctl`,
//! which no test may run. (The two steps and the tool's arguments are tested
//! in `device.rs` against a recording stand-in.) Device names below do not
//! exist, so even a refusal that failed would do nothing to a real disk.

mod common;

use std::fs;
use std::path::PathBuf;

use common::FakeCard;
use fiio_core::Error;
use fiio_core::device::{self, Found, UsbDevice};
use fiio_core::mounts::Mount;

fn usb(vendor: &str) -> UsbDevice {
    UsbDevice {
        vendor: vendor.into(),
        product: "0003".into(),
        serial: Some("SER1".into()),
        manufacturer: Some("FiiO".into()),
        name: Some("X3 2nd gen".into()),
    }
}

fn found(mountpoint: PathBuf, source: Option<&str>, usb: Option<UsbDevice>) -> Found {
    Found {
        mount: Mount {
            mountpoint,
            fstype: "vfat".into(),
            read_only: false,
            root: PathBuf::from("/"),
        },
        source: source.map(String::from),
        usb,
    }
}

// Prevents: eject being run on a folder used as a card, a stick that is not
// a FiiO player, or a mount that is not a block device: unmounting the
// wrong thing, or powering off a disk that is not the X3. Each refusal is an
// `Eject` error naming the mountpoint, and the card is untouched.
#[test]
fn only_the_block_device_of_a_fiio_player_is_ejected() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("card");
    fs::create_dir(&root).unwrap();
    let mut card = FakeCard::new(root.clone());
    card.put("Music/a.mp3", b"audio");
    card.age();

    let cases: Vec<(&str, Found)> = vec![
        ("a folder", found(root.clone(), None, None)),
        (
            "a FiiO USB device but no block device",
            found(root.clone(), None, Some(usb("2972"))),
        ),
        (
            "a block device with no USB device above it",
            found(root.clone(), Some("/dev/fiio-test-sdz9"), None),
        ),
        (
            "a USB stick of another maker",
            found(root.clone(), Some("/dev/fiio-test-sdz9"), Some(usb("0781"))),
        ),
        (
            "a FiiO device whose source is not under /dev",
            found(root.clone(), Some("//server/share"), Some(usb("2972"))),
        ),
    ];
    for (what, f) in cases {
        match device::eject(&f) {
            Err(Error::Eject(m)) => {
                assert!(m.contains(&root.display().to_string()), "{what}: {m}");
            }
            other => panic!("{what}: expected an Eject error, got {other:?}"),
        }
    }
    card.assert_untouched();
}
