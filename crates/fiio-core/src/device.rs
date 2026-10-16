//! Finding the X3 and checking that its card may be written: FAT32
//! filesystems mounted from a USB device of FiiO's (vendor `2972`), read
//! from `/proc/self/mountinfo` and sysfs. Nothing here writes.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::IoContext;
use crate::mounts::{Mount, block_device, parse_line, parse_mountinfo};
use crate::{Error, Result};

/// FiiO's USB vendor id.
pub const FIIO_VENDOR: &str = "2972";

/// The USB device a block device belongs to, as sysfs describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDevice {
    /// `idVendor`, four hex digits.
    pub vendor: String,
    /// `idProduct`, four hex digits.
    pub product: String,
    pub serial: Option<String>,
    pub manufacturer: Option<String>,
    /// The `product` string, `X3 2nd gen`.
    pub name: Option<String>,
}

impl UsbDevice {
    pub fn is_fiio(&self) -> bool {
        self.vendor.eq_ignore_ascii_case(FIIO_VENDOR)
    }

    /// `usb:<serial>`, the device key of an X3, when it has a serial.
    pub fn key(&self) -> Option<String> {
        self.serial.as_ref().map(|s| format!("usb:{s}"))
    }
}

/// A mounted filesystem with the device it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub mount: Mount,
    /// `/dev/sdc1`, when mounted from a block device.
    pub source: Option<String>,
    /// The USB device above the block device, if any.
    pub usb: Option<UsbDevice>,
}

impl Found {
    /// `usb:<serial>` for an X3; `None` for a folder or another device.
    pub fn key(&self) -> Option<String> {
        self.usb.as_ref().filter(|u| u.is_fiio())?.key()
    }
}

/// Whether detection is turned off (`FIIO_TUNES_NO_DETECT`, set for
/// everything cargo runs).
pub fn detection_off() -> bool {
    std::env::var_os("FIIO_TUNES_NO_DETECT").is_some_and(|v| !v.is_empty())
}

/// The USB device above the block device `dev` (`/dev/sdc1`) under the
/// sysfs root `sys`: the nearest ancestor with an `idVendor`.
pub fn usb_device_in(sys: &Path, dev: &str) -> Option<UsbDevice> {
    let dev = std::fs::canonicalize(dev).unwrap_or_else(|_| PathBuf::from(dev));
    let sys = std::fs::canonicalize(sys).ok()?;
    let block = std::fs::canonicalize(sys.join("class/block").join(dev.file_name()?)).ok()?;
    let usb = block
        .ancestors()
        .take_while(|p| p.starts_with(&sys))
        .find(|p| p.join("idVendor").is_file())?;
    let read = |name: &str| {
        std::fs::read_to_string(usb.join(name))
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    Some(UsbDevice {
        vendor: read("idVendor")?,
        product: read("idProduct").unwrap_or_default(),
        serial: read("serial"),
        manufacturer: read("manufacturer"),
        name: read("product"),
    })
}

/// Every mountinfo line with its source, the later entry winning for a
/// mountpoint mounted over.
fn mounts_with_source(mountinfo: &str) -> Vec<(Mount, String)> {
    let mut out: Vec<(Mount, String)> = Vec::new();
    for (m, source) in mountinfo.lines().filter_map(parse_line) {
        out.retain(|(o, _)| o.mountpoint != m.mountpoint);
        out.push((m, source));
    }
    out
}

/// The X3 cards among the mounts in `mountinfo`, with sysfs at `sys`: vfat
/// filesystems mounted whole from a `/dev` block device of a FiiO USB
/// device.
pub fn cards_in(mountinfo: &str, sys: &Path) -> Vec<Found> {
    let mut sources = std::collections::HashSet::new();
    mounts_with_source(mountinfo)
        .into_iter()
        .filter(|(m, source)| {
            m.fstype == "vfat" && m.root == Path::new("/") && source.starts_with("/dev/")
        })
        // A card mounted at two places (a bind mount of its root) is one
        // card, at the first.
        .filter(|(_, source)| sources.insert(source.clone()))
        .filter_map(|(mount, source)| {
            let usb = usb_device_in(sys, &source).filter(UsbDevice::is_fiio)?;
            Some(Found {
                mount,
                source: Some(source),
                usb: Some(usb),
            })
        })
        .collect()
}

/// The mounted X3 cards.
pub fn find_cards() -> Result<Vec<Found>> {
    if detection_off() {
        return Err(Error::DetectionOff("set device.mountpoint in the config"));
    }
    let path = Path::new("/proc/self/mountinfo");
    let text = std::fs::read_to_string(path).at(path)?;
    Ok(cards_in(&text, Path::new("/sys")))
}

/// What is mounted at `mountpoint`, from `mountinfo` and sysfs at `sys`.
/// A folder that is not itself a mount has no source and no USB device.
pub fn describe_in(mountinfo: &str, sys: &Path, mountpoint: &Path) -> Found {
    let path = std::fs::canonicalize(mountpoint).unwrap_or_else(|_| mountpoint.to_owned());
    let source = block_device(mountinfo, mountpoint);
    let mount = mounts_with_source(mountinfo)
        .into_iter()
        .map(|(m, _)| m)
        .find(|m| m.mountpoint == path)
        .unwrap_or(Mount {
            mountpoint: path,
            fstype: String::new(),
            read_only: false,
            root: PathBuf::new(),
        });
    let usb = source.as_deref().and_then(|s| usb_device_in(sys, s));
    Found { mount, source, usb }
}

/// The card to use: `device.mountpoint` when set, else the one mounted X3.
pub fn locate(cfg: &Config) -> Result<Found> {
    let mp = &cfg.device.mountpoint;
    if !mp.as_os_str().is_empty() {
        let path = Path::new("/proc/self/mountinfo");
        let text = std::fs::read_to_string(path).at(path)?;
        return Ok(describe_in(&text, Path::new("/sys"), mp));
    }
    match &find_cards()?[..] {
        [] => Err(Error::NoCard),
        [one] => Ok(one.clone()),
        many => Err(Error::SeveralCards(
            many.iter()
                .map(|f| f.mount.mountpoint.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        )),
    }
}

/// Refuses to write to the card `found` unless it is the root of a vfat
/// filesystem mounted read-write from a FiiO USB device. With
/// `allow_folder` (tests), a plain folder passes, provided the filesystem
/// holding it is not read-only.
pub fn check_writable(found: &Found, allow_folder: bool) -> Result<()> {
    let path = Path::new("/proc/self/mountinfo");
    let text = std::fs::read_to_string(path).at(path)?;
    check_writable_in(&text, found, allow_folder)
}

/// [`check_writable`] against the text of a mountinfo file.
pub fn check_writable_in(mountinfo: &str, found: &Found, allow_folder: bool) -> Result<()> {
    let mp = &found.mount.mountpoint;
    if !allow_folder {
        check_device(found)?;
    }
    check_mount(&parse_mountinfo(mountinfo), mp)
}

/// Refuses a card that is not the root of a mounted FAT32 device of
/// FiiO's: a folder (a copy of a card, the library) must never be written
/// as if it were one.
fn check_device(found: &Found) -> Result<()> {
    let mp = found.mount.mountpoint.display();
    let Some(source) = &found.source else {
        return Err(Error::NotWritable(format!(
            "{mp} is a folder, not a mounted card; fiio-tunes writes only to the X3's card itself"
        )));
    };
    if found.mount.root != Path::new("/") {
        return Err(Error::NotWritable(format!(
            "{mp} is a folder of a device mounted elsewhere, not the card itself"
        )));
    }
    if found.mount.fstype != "vfat" {
        return Err(Error::NotWritable(format!(
            "{mp} is {} from {source}, and fiio-tunes writes only to FAT32 (vfat) cards",
            found.mount.fstype
        )));
    }
    match &found.usb {
        Some(u) if u.is_fiio() => Ok(()),
        Some(u) => Err(Error::NotWritable(format!(
            "{mp} is on USB device {}:{}, not a FiiO player",
            u.vendor, u.product
        ))),
        None => Err(Error::NotWritable(format!(
            "{mp} is not on a USB device; fiio-tunes writes only to the X3's card"
        ))),
    }
}

fn check_mount(mounts: &[Mount], mountpoint: &Path) -> Result<()> {
    // Symlinks and `..` resolved first. The filesystem holding the path is
    // the longest mountpoint it is under, the last such entry winning.
    let path = std::fs::canonicalize(mountpoint).unwrap_or_else(|_| mountpoint.to_owned());
    let holder = mounts
        .iter()
        .enumerate()
        .filter(|(_, m)| path.starts_with(&m.mountpoint))
        .max_by_key(|(i, m)| (m.mountpoint.components().count(), *i))
        .map(|(_, m)| m);
    match holder {
        Some(m) if m.read_only => Err(Error::NotWritable(format!(
            "{} is on a read-only mount",
            mountpoint.display()
        ))),
        _ => Ok(()),
    }
}

/// Size and free space of the filesystem holding `path`, in bytes.
pub fn space(path: &Path) -> Result<(u64, u64)> {
    let s = rustix::fs::statvfs(path)
        .map_err(std::io::Error::from)
        .at(path)?;
    Ok((s.f_blocks * s.f_frsize, s.f_bavail * s.f_frsize))
}

/// Unmounts the X3's card `found` and powers the player off, through
/// `udisksctl`, as the file manager would. Refuses anything but the block
/// device of a FiiO player (a folder used as a card, a mistyped
/// mountpoint). Returns a warning when it was unmounted but could not be
/// powered off: it can be unplugged then too, since unmounting writes
/// everything out.
pub fn eject(found: &Found) -> Result<Option<String>> {
    let fiio = found.usb.as_ref().is_some_and(|u| u.is_fiio());
    match &found.source {
        Some(dev) if fiio && dev.starts_with("/dev/") => eject_device(dev, &mut udisksctl),
        _ => Err(Error::Eject(format!(
            "{} is not the card of a connected X3",
            found.mount.mountpoint.display()
        ))),
    }
}

/// The two steps of [`eject`] on `dev`, with `run` standing for `udisksctl`
/// (a seam for tests).
fn eject_device(
    dev: &str,
    run: &mut dyn FnMut(&[&str]) -> std::result::Result<(), String>,
) -> Result<Option<String>> {
    run(&["unmount", "--block-device", dev])
        .map_err(|e| Error::Eject(format!("could not unmount {dev}: {e}")))?;
    Ok(run(&["power-off", "--block-device", dev])
        .err()
        .map(|e| format!("unmounted, but not powered off ({e}); it can be unplugged")))
}

/// Runs `udisksctl` with `args`, without asking anything; its error output
/// on failure.
fn udisksctl(args: &[&str]) -> std::result::Result<(), String> {
    run_tool("udisksctl", args)
}

/// [`udisksctl`] with the program named, so tests can use a fake one.
fn run_tool(program: &str, args: &[&str]) -> std::result::Result<(), String> {
    let out = std::process::Command::new(program)
        .args(args)
        .arg("--no-user-interaction")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("udisksctl: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    Err(if err.is_empty() {
        format!("udisksctl {}: {}", args[0], out.status)
    } else {
        err
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eject_refuses_a_folder_and_a_device_that_is_not_fiio() {
        let folder = Found {
            mount: crate::mounts::Mount {
                mountpoint: PathBuf::from("/tmp/card"),
                fstype: String::new(),
                read_only: false,
                root: PathBuf::from("/"),
            },
            source: None,
            usb: None,
        };
        match eject(&folder).unwrap_err() {
            Error::Eject(m) => assert!(m.contains("not the card of a connected X3"), "{m}"),
            other => panic!("{other:?}"),
        }
        let boot = Found {
            source: Some("/dev/nvme0n1p1".into()),
            ..folder
        };
        assert!(matches!(eject(&boot), Err(Error::Eject(_))));
    }

    fn run_recording(fail: &'static [&'static str]) -> (Vec<Vec<String>>, Result<Option<String>>) {
        let mut calls: Vec<Vec<String>> = Vec::new();
        let r = eject_device("/dev/sdc1", &mut |a| {
            calls.push(a.iter().map(|s| s.to_string()).collect());
            match fail.contains(&a[0]) {
                true => Err(format!("{} failed", a[0])),
                false => Ok(()),
            }
        });
        (calls, r)
    }

    #[test]
    fn eject_unmounts_then_powers_off_the_same_device() {
        let (calls, r) = run_recording(&[]);
        assert_eq!(r.unwrap(), None);
        assert_eq!(
            calls,
            vec![
                vec!["unmount", "--block-device", "/dev/sdc1"],
                vec!["power-off", "--block-device", "/dev/sdc1"],
            ]
        );
    }

    #[test]
    fn eject_does_not_power_off_when_unmounting_fails() {
        let (calls, r) = run_recording(&["unmount"]);
        match r.unwrap_err() {
            Error::Eject(m) => assert!(m.contains("could not unmount /dev/sdc1"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(calls.len(), 1, "{calls:?}");
    }

    #[test]
    fn eject_warns_when_only_power_off_fails() {
        let (calls, r) = run_recording(&["power-off"]);
        let warning = r.unwrap().expect("a warning");
        assert!(
            warning.contains("unmounted") && warning.contains("power-off failed"),
            "{warning}"
        );
        assert_eq!(calls.len(), 2);
    }

    /// A script that appends its arguments to `log` and exits with `code`,
    /// writing `err` to stderr.
    fn fake_tool(dir: &Path, code: i32, err: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let (tool, log) = (dir.join("fake-udisksctl"), dir.join("log"));
        std::fs::write(
            &tool,
            format!(
                "#!/bin/sh\necho \"$@\" >> '{}'\necho '{err}' >&2\nexit {code}\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        (tool, log)
    }

    #[test]
    fn run_tool_adds_no_user_interaction_and_reports_failures() {
        let _turn = crate::SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let (tool, log) = fake_tool(dir.path(), 0, "");
        run_tool(
            tool.to_str().unwrap(),
            &["unmount", "--block-device", "/dev/sdc1"],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(log).unwrap().trim(),
            "unmount --block-device /dev/sdc1 --no-user-interaction"
        );
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = fake_tool(dir.path(), 1, "Device is busy");
        assert_eq!(
            run_tool(tool.to_str().unwrap(), &["unmount"]).unwrap_err(),
            "Device is busy"
        );
        assert!(
            run_tool("/nonexistent/udisksctl", &["x"])
                .unwrap_err()
                .starts_with("udisksctl:")
        );
    }

    /// A fake sysfs: `block/<dev>` under a USB device with the given ids.
    fn sysfs(vendor: &str, serial: &str) -> tempfile::TempDir {
        let sys = tempfile::tempdir().unwrap();
        let usb = sys.path().join("devices/usb1/1-3");
        let block = usb.join("1-3:1.0/host8/block/sdc");
        std::fs::create_dir_all(block.join("sdc1")).unwrap();
        for (f, v) in [
            ("idVendor", vendor),
            ("idProduct", "0003"),
            ("serial", serial),
            ("product", "X3 2nd gen\n"),
        ] {
            std::fs::write(usb.join(f), v).unwrap();
        }
        std::fs::create_dir_all(sys.path().join("class/block")).unwrap();
        std::os::unix::fs::symlink(&block, sys.path().join("class/block/sdc")).unwrap();
        std::os::unix::fs::symlink(block.join("sdc1"), sys.path().join("class/block/sdc1"))
            .unwrap();
        sys
    }

    const INFO: &str = "\
22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw
249 32 259:1 / /boot rw,relatime - vfat /dev/nvme0n1p1 rw
801 29 8:32 / /run/media/julien/X3 rw,nosuid shared:743 - vfat /dev/sdc rw,uid=1000
802 29 8:33 /sub /mnt/bind rw - vfat /dev/sdc rw
";

    #[test]
    fn the_card_is_the_vfat_mount_of_a_fiio_usb_device() {
        let sys = sysfs("2972", "323820474077\n");
        let found = cards_in(INFO, sys.path());
        assert_eq!(found.len(), 1, "{found:?}");
        let f = &found[0];
        assert_eq!(f.mount.mountpoint, Path::new("/run/media/julien/X3"));
        assert_eq!(f.key().as_deref(), Some("usb:323820474077"));
        assert_eq!(f.usb.as_ref().unwrap().name.as_deref(), Some("X3 2nd gen"));
        let partition = INFO.replace("/dev/sdc rw,uid", "/dev/sdc1 rw,uid");
        assert_eq!(cards_in(&partition, sys.path()).len(), 1);
    }

    #[test]
    fn another_vendor_is_not_a_card_and_cannot_be_written() {
        let sys = sysfs("05ac", "ABC");
        assert!(cards_in(INFO, sys.path()).is_empty());
        let f = describe_in(INFO, sys.path(), Path::new("/run/media/julien/X3"));
        assert_eq!(f.key(), None);
        let err = check_writable_in(INFO, &f, false).unwrap_err();
        assert!(err.to_string().contains("not a FiiO"), "{err}");
    }

    #[test]
    fn writes_need_the_root_of_a_vfat_fiio_mount_unless_folders_are_allowed() {
        let sys = sysfs("2972", "1");
        let card = describe_in(INFO, sys.path(), Path::new("/run/media/julien/X3"));
        check_writable_in(INFO, &card, false).unwrap();
        let ro = INFO.replace("X3 rw,nosuid", "X3 ro,nosuid");
        assert!(check_writable_in(&ro, &card, false).is_err());
        let folder = describe_in(INFO, sys.path(), Path::new("/run/media/julien/X3/Music"));
        assert!(folder.source.is_none());
        assert!(check_writable_in(INFO, &folder, false).is_err());
        let boot = describe_in(INFO, sys.path(), Path::new("/boot"));
        assert!(check_writable_in(INFO, &boot, false).is_err());
        let bind = describe_in(INFO, sys.path(), Path::new("/mnt/bind"));
        assert!(check_writable_in(INFO, &bind, false).is_err());
        let tmp = tempfile::tempdir().unwrap();
        let plain = describe_in(INFO, sys.path(), tmp.path());
        check_writable_in(INFO, &plain, true).unwrap();
    }
}
