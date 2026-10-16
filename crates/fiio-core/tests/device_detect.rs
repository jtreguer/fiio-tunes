//! Card detection and the write checks (`device.rs`) against fake mountinfo
//! text and a fake sysfs tree in a temp dir. The block device names used
//! (`/dev/fiiotest0`) do not exist on any machine, and nothing here reads
//! `/proc/self/mountinfo` expecting a card: the real X3 may be plugged in.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use fiio_core::Error;
use fiio_core::config::Config;
use fiio_core::device::{
    FIIO_VENDOR, Found, cards_in, check_writable_in, describe_in, detection_off, find_cards,
    locate, usb_device_in,
};

/// A fake `/sys`: `class/block/<name>` symlinks into `devices/...`.
struct Sys {
    dir: tempfile::TempDir,
}

struct UsbSpec<'a> {
    /// Folder name under `devices/usb1`, nested with `/` for a hub.
    bus: &'a str,
    vendor: &'a str,
    product: &'a str,
    serial: Option<&'a str>,
    name: Option<&'a str>,
    manufacturer: Option<&'a str>,
}

impl<'a> UsbSpec<'a> {
    fn fiio(bus: &'a str, serial: Option<&'a str>) -> Self {
        UsbSpec {
            bus,
            vendor: FIIO_VENDOR,
            product: "0003",
            serial,
            name: Some("X3 2nd gen\n"),
            manufacturer: Some("FiiO\n"),
        }
    }
}

impl Sys {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("class/block")).unwrap();
        Sys { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// A USB device folder, with the id files `spec` has.
    fn usb(&self, spec: &UsbSpec) -> PathBuf {
        let usb = self.path().join("devices/usb1").join(spec.bus);
        fs::create_dir_all(&usb).unwrap();
        fs::write(usb.join("idVendor"), format!("{}\n", spec.vendor)).unwrap();
        fs::write(usb.join("idProduct"), format!("{}\n", spec.product)).unwrap();
        for (f, v) in [
            ("serial", spec.serial),
            ("product", spec.name),
            ("manufacturer", spec.manufacturer),
        ] {
            if let Some(v) = v {
                fs::write(usb.join(f), v).unwrap();
            }
        }
        usb
    }

    /// `disk` with partitions `parts` below the USB device folder `usb`, both
    /// visible in `class/block` the way the kernel does it.
    fn block(&self, usb: &Path, disk: &str, parts: &[&str]) {
        let d = usb
            .join("1-3:1.0/host8/target8:0:0/8:0:0:0/block")
            .join(disk);
        for p in parts {
            fs::create_dir_all(d.join(p)).unwrap();
        }
        fs::create_dir_all(&d).unwrap();
        symlink(&d, self.path().join("class/block").join(disk)).unwrap();
        for p in parts {
            symlink(d.join(p), self.path().join("class/block").join(p)).unwrap();
        }
    }

    /// A block device with no USB device above it (an NVMe drive).
    fn non_usb_block(&self, name: &str) {
        let d = self.path().join("devices/pci0000:00/nvme/block").join(name);
        fs::create_dir_all(&d).unwrap();
        symlink(&d, self.path().join("class/block").join(name)).unwrap();
    }

    /// A FiiO card at `/dev/fiiotest0` (partition `fiiotest0p1`).
    fn fiio_card(serial: Option<&str>) -> Self {
        let sys = Sys::new();
        let usb = sys.usb(&UsbSpec::fiio("1-3", serial));
        sys.block(&usb, "fiiotest0", &["fiiotest0p1"]);
        sys
    }
}

fn line(mountpoint: &str, root: &str, opts: &str, fstype: &str, source: &str) -> String {
    line_sb(mountpoint, root, opts, fstype, source, "rw")
}

/// A mountinfo line with the superblock options (after the source) given.
fn line_sb(
    mountpoint: &str,
    root: &str,
    opts: &str,
    fstype: &str,
    source: &str,
    super_opts: &str,
) -> String {
    format!("801 29 8:32 {root} {mountpoint} {opts} shared:7 - {fstype} {source} {super_opts}\n")
}

const X3: &str = "/run/media/julien/X3";

fn mountpoints(found: &[Found]) -> Vec<&Path> {
    found.iter().map(|f| f.mount.mountpoint.as_path()).collect()
}

fn not_writable(r: fiio_core::Result<()>) -> String {
    match r {
        Err(Error::NotWritable(why)) => why,
        other => panic!("expected NotWritable, got {other:?}"),
    }
}

// Prevents: a card plugged in as a whole disk, or as a partition, being
// missed (or its USB ids mixed up) when sysfs lists the partition below the
// disk.
#[test]
fn a_partition_and_a_whole_disk_both_lead_to_the_usb_device() {
    let sys = Sys::fiio_card(Some("323820474077"));
    for dev in ["/dev/fiiotest0", "/dev/fiiotest0p1"] {
        let info = line(X3, "/", "rw,nosuid", "vfat", dev);
        let found = cards_in(&info, sys.path());
        assert_eq!(found.len(), 1, "{dev}: {found:?}");
        let f = &found[0];
        assert_eq!(f.source.as_deref(), Some(dev));
        assert_eq!(f.key().as_deref(), Some("usb:323820474077"), "{dev}");
        let usb = f.usb.as_ref().unwrap();
        assert_eq!(
            (usb.vendor.as_str(), usb.product.as_str()),
            ("2972", "0003")
        );
        // Trailing newlines of the sysfs files are gone.
        assert_eq!(usb.name.as_deref(), Some("X3 2nd gen"));
        assert_eq!(usb.manufacturer.as_deref(), Some("FiiO"));
    }
}

// Prevents: a card whose label has a space (`\040` in mountinfo) being
// found under a mangled mountpoint, so every later path would be wrong.
#[test]
fn octal_escapes_in_the_mountpoint_are_decoded() {
    let sys = Sys::fiio_card(Some("1"));
    let info = line(
        "/run/media/julien/X3\\040de\\040Julien",
        "/",
        "rw",
        "vfat",
        "/dev/fiiotest0p1",
    );
    let found = cards_in(&info, sys.path());
    assert_eq!(
        mountpoints(&found),
        [Path::new("/run/media/julien/X3 de Julien")]
    );
    // `describe_in` finds the same mount by the decoded path.
    let f = describe_in(
        &info,
        sys.path(),
        Path::new("/run/media/julien/X3 de Julien"),
    );
    assert_eq!(f.source.as_deref(), Some("/dev/fiiotest0p1"));
    assert_eq!(f.key().as_deref(), Some("usb:1"));
}

// Prevents: a stale entry under a mountpoint that something else was later
// mounted over being taken for the card (or the card hidden by it).
#[test]
fn a_mountpoint_mounted_over_belongs_to_the_last_entry() {
    let sys = Sys::fiio_card(Some("1"));
    sys.non_usb_block("nvmetest0");
    let card = line(X3, "/", "rw", "vfat", "/dev/fiiotest0p1");
    let over = line(X3, "/", "rw", "ext4", "/dev/nvmetest0");
    // The card first, then an ext4 mounted over it: no card.
    let info = format!("{card}{over}");
    assert!(cards_in(&info, sys.path()).is_empty());
    let f = describe_in(&info, sys.path(), Path::new(X3));
    assert_eq!(f.source.as_deref(), Some("/dev/nvmetest0"));
    assert_eq!(f.mount.fstype, "ext4");
    assert_eq!(f.key(), None);
    // The other way round, the card is the one in effect.
    let info = format!("{over}{card}");
    let found = cards_in(&info, sys.path());
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].source.as_deref(), Some("/dev/fiiotest0p1"));
    let f = describe_in(&info, sys.path(), Path::new(X3));
    assert_eq!(
        (f.mount.fstype.as_str(), f.key().as_deref()),
        ("vfat", Some("usb:1"))
    );
}

// Prevents: a bind mount of a folder of the card being taken for the card
// itself (writing "the card" would then write into a subfolder).
#[test]
fn a_bind_mount_of_a_subfolder_is_not_a_card_and_is_not_writable() {
    let sys = Sys::fiio_card(Some("1"));
    let info = line("/mnt/bind", "/Music", "rw", "vfat", "/dev/fiiotest0p1");
    assert!(cards_in(&info, sys.path()).is_empty());
    // The same device, mounted whole, is a card.
    let info = format!("{info}{}", line(X3, "/", "rw", "vfat", "/dev/fiiotest0p1"));
    assert_eq!(mountpoints(&cards_in(&info, sys.path())), [Path::new(X3)]);
    let bind = describe_in(&info, sys.path(), Path::new("/mnt/bind"));
    assert_eq!(bind.mount.root, Path::new("/Music"));
    not_writable(check_writable_in(&info, &bind, false));
    check_writable_in(&info, &describe_in(&info, sys.path(), Path::new(X3)), false).unwrap();
}

// Prevents: a non-vfat filesystem on the X3's USB device (an ext4 card)
// being offered as a card.
#[test]
fn only_vfat_counts_even_on_a_fiio_device() {
    let sys = Sys::fiio_card(Some("1"));
    for fstype in ["ext4", "exfat", "ntfs3", "fuseblk"] {
        let info = line(X3, "/", "rw", fstype, "/dev/fiiotest0p1");
        assert!(cards_in(&info, sys.path()).is_empty(), "{fstype}");
        let f = describe_in(&info, sys.path(), Path::new(X3));
        not_writable(check_writable_in(&info, &f, false));
    }
    // A source that is not a /dev node (a fuse mount) is not a card either.
    let info = line(X3, "/", "rw", "vfat", "fiiotest0p1");
    assert!(cards_in(&info, sys.path()).is_empty());
}

// Prevents: any USB stick (or the machine's own /boot) being taken for the
// X3, then written to.
#[test]
fn another_vendor_or_a_non_usb_device_is_not_a_card_and_is_refused() {
    let sys = Sys::new();
    let other = sys.usb(&UsbSpec {
        vendor: "05ac",
        ..UsbSpec::fiio("1-4", Some("ABC"))
    });
    sys.block(&other, "stick0", &["stick0p1"]);
    sys.non_usb_block("nvmetest0");
    let info = [
        line(
            "/run/media/julien/STICK",
            "/",
            "rw",
            "vfat",
            "/dev/stick0p1",
        ),
        line("/boot", "/", "rw", "vfat", "/dev/nvmetest0"),
    ]
    .concat();
    assert!(cards_in(&info, sys.path()).is_empty());
    let stick = describe_in(&info, sys.path(), Path::new("/run/media/julien/STICK"));
    assert_eq!(stick.usb.as_ref().unwrap().vendor, "05ac");
    assert_eq!(stick.key(), None, "a stick has no X3 key, serial or not");
    let why = not_writable(check_writable_in(&info, &stick, false));
    assert!(why.contains("05ac"), "{why}");
    let boot = describe_in(&info, sys.path(), Path::new("/boot"));
    assert!(boot.usb.is_none());
    not_writable(check_writable_in(&info, &boot, false));
    // allow_folder lifts the device checks, which is what tests rely on.
    check_writable_in(&info, &boot, true).unwrap();
}

// Prevents: the hub above the player (FiiO's vendor id) making any stick on
// it look like the X3: only the nearest ancestor with an idVendor counts.
#[test]
fn only_the_nearest_usb_ancestor_decides_the_vendor() {
    let sys = Sys::new();
    let hub = sys.usb(&UsbSpec::fiio("1-1", Some("HUB")));
    let stick = sys.usb(&UsbSpec {
        bus: "1-1/1-1.2",
        vendor: "0781",
        ..UsbSpec::fiio("", Some("STICK"))
    });
    assert!(stick.starts_with(&hub));
    sys.block(&stick, "stick0", &["stick0p1"]);
    let info = line(X3, "/", "rw", "vfat", "/dev/stick0p1");
    assert!(cards_in(&info, sys.path()).is_empty());
    let usb = usb_device_in(sys.path(), "/dev/stick0p1").unwrap();
    assert_eq!(
        (usb.vendor.as_str(), usb.serial.as_deref()),
        ("0781", Some("STICK"))
    );
}

// Prevents: a symlink in class/block that leaves the sysfs root leading to
// a vendor file elsewhere.
#[test]
fn a_block_device_outside_the_sysfs_root_is_not_looked_up() {
    let sys = Sys::new();
    let elsewhere = tempfile::tempdir().unwrap();
    fs::write(elsewhere.path().join("idVendor"), "2972\n").unwrap();
    let blk = elsewhere.path().join("block/fiiotest0");
    fs::create_dir_all(&blk).unwrap();
    symlink(&blk, sys.path().join("class/block/fiiotest0")).unwrap();
    assert_eq!(usb_device_in(sys.path(), "/dev/fiiotest0"), None);
    // And a device sysfs does not know, or no sysfs at all.
    assert_eq!(usb_device_in(sys.path(), "/dev/fiiotest9"), None);
    assert_eq!(
        usb_device_in(&sys.path().join("nonexistent"), "/dev/fiiotest0"),
        None
    );
}

// Prevents: a player without a serial getting a key (`usb:`) shared by
// every such player; the card is still found, but has no key.
#[test]
fn a_missing_or_empty_serial_gives_no_key() {
    for serial in [None, Some(""), Some("  \n")] {
        let sys = Sys::fiio_card(serial);
        let info = line(X3, "/", "rw", "vfat", "/dev/fiiotest0p1");
        let found = cards_in(&info, sys.path());
        assert_eq!(found.len(), 1, "{serial:?}");
        assert_eq!(found[0].usb.as_ref().unwrap().serial, None, "{serial:?}");
        assert_eq!(found[0].key(), None, "{serial:?}");
    }
}

// Prevents: optional sysfs files missing (no product string, no product id)
// making the lookup give up on a player that is plainly FiiO's.
#[test]
fn optional_sysfs_files_may_be_missing() {
    let sys = Sys::new();
    let usb = sys.usb(&UsbSpec {
        name: None,
        manufacturer: None,
        ..UsbSpec::fiio("1-3", Some("S"))
    });
    fs::remove_file(usb.join("idProduct")).unwrap();
    sys.block(&usb, "fiiotest0", &["fiiotest0p1"]);
    let u = usb_device_in(sys.path(), "/dev/fiiotest0p1").unwrap();
    assert!(u.is_fiio());
    assert_eq!(u.product, "");
    assert_eq!((&u.name, &u.manufacturer), (&None, &None));
    assert_eq!(u.key().as_deref(), Some("usb:S"));
    // Without an idVendor there is no USB device at all.
    fs::remove_file(usb.join("idVendor")).unwrap();
    assert_eq!(usb_device_in(sys.path(), "/dev/fiiotest0p1"), None);
}

// Prevents: the vendor id compared case-sensitively (`2972` has no letters,
// but a hex id could): both spellings of FiiO's id pass, a neighbour's not.
#[test]
fn vendor_ids_match_exactly_ignoring_case() {
    let sys = Sys::fiio_card(Some("1"));
    let mut usb = usb_device_in(sys.path(), "/dev/fiiotest0").unwrap();
    assert!(usb.is_fiio());
    for (v, want) in [
        ("2972", true),
        ("29720", false),
        ("297", false),
        ("", false),
    ] {
        usb.vendor = v.into();
        assert_eq!(usb.is_fiio(), want, "{v:?}");
    }
}

// Prevents: two cards being collapsed into one, or reported in a different
// order from mountinfo.
#[test]
fn several_cards_are_all_found_in_mountinfo_order() {
    let sys = Sys::new();
    for (bus, serial, disk) in [("1-3", "AAA", "fiiotest0"), ("1-5", "BBB", "fiiotest1")] {
        let usb = sys.usb(&UsbSpec::fiio(bus, Some(serial)));
        sys.block(&usb, disk, &[&format!("{disk}p1")]);
    }
    let info = [
        line("/run/media/julien/B", "/", "rw", "vfat", "/dev/fiiotest1p1"),
        line("/run/media/julien/A", "/", "rw", "vfat", "/dev/fiiotest0p1"),
    ]
    .concat();
    let found = cards_in(&info, sys.path());
    assert_eq!(
        mountpoints(&found),
        [
            Path::new("/run/media/julien/B"),
            Path::new("/run/media/julien/A")
        ]
    );
    let keys: Vec<_> = found.iter().map(|f| f.key().unwrap()).collect();
    assert_eq!(keys, ["usb:BBB", "usb:AAA"]);
}

// Prevents: a read-only card being written to (the checks run before the
// first byte), including with allow_folder, which lifts only the device
// checks.
#[test]
fn a_read_only_mount_is_refused_even_with_allow_folder() {
    let sys = Sys::fiio_card(Some("1"));
    let ro = line(X3, "/", "ro,nosuid", "vfat", "/dev/fiiotest0p1");
    // Found as a card (it is one), but not writable.
    let found = cards_in(&ro, sys.path());
    assert_eq!(found.len(), 1);
    assert!(found[0].mount.read_only);
    for allow in [false, true] {
        not_writable(check_writable_in(&ro, &found[0], allow));
    }
    let rw = line(X3, "/", "rw,nosuid", "vfat", "/dev/fiiotest0p1");
    check_writable_in(&rw, &found[0], false).unwrap();

    // A plain folder inside a read-only filesystem, as the tests use.
    let t = tempfile::tempdir().unwrap();
    let folder = t.path().canonicalize().unwrap().join("card");
    fs::create_dir(&folder).unwrap();
    let ro_parent = line(
        folder.parent().unwrap().to_str().unwrap(),
        "/",
        "ro",
        "ext4",
        "/dev/nvmetest0",
    );
    let f = describe_in(&ro_parent, sys.path(), &folder);
    not_writable(check_writable_in(&ro_parent, &f, true));
    // The same folder on a read-write filesystem passes with allow_folder
    // and is refused without it (it is a folder, not a card).
    let rw_parent = ro_parent.replace(" ro ", " rw ");
    check_writable_in(&rw_parent, &f, true).unwrap();
    not_writable(check_writable_in(&rw_parent, &f, false));
}

// Prevents: the wrong mount deciding read-only: the deepest mountpoint
// holding the folder decides, and among entries for one mountpoint the
// last.
#[test]
fn the_deepest_and_last_mount_holding_a_folder_decides_read_only() {
    let t = tempfile::tempdir().unwrap();
    let base = t.path().canonicalize().unwrap();
    let inner = base.join("outer/inner");
    fs::create_dir_all(&inner).unwrap();
    let sys = Sys::new();
    let b = |p: &Path| p.to_str().unwrap().to_owned();
    let mount = |p: &Path, opts: &str| line(&b(p), "/", opts, "ext4", "/dev/nvmetest0");
    let folder = describe_in("", sys.path(), &inner);
    // ro outside, rw inside: writable. rw outside, ro inside: not.
    // Both orders in mountinfo: the deepest decides, not the last.
    for flip in [false, true] {
        let order = |mut v: Vec<String>| {
            if flip {
                v.reverse();
            }
            v.concat()
        };
        let info = order(vec![mount(&base.join("outer"), "ro"), mount(&inner, "rw")]);
        check_writable_in(&info, &folder, true).unwrap();
        let info = order(vec![mount(&base.join("outer"), "rw"), mount(&inner, "ro")]);
        not_writable(check_writable_in(&info, &folder, true));
    }
    // Mounted over: ro then rw is writable, rw then ro is not.
    let info = [mount(&inner, "ro"), mount(&inner, "rw")].concat();
    check_writable_in(&info, &folder, true).unwrap();
    let info = [mount(&inner, "rw"), mount(&inner, "ro")].concat();
    not_writable(check_writable_in(&info, &folder, true));
    // A mount whose path only shares a prefix (`inner2`) does not hold it.
    let info = mount(&base.join("outer/inner2"), "ro");
    check_writable_in(&info, &folder, true).unwrap();
}

// Prevents: a symlinked path to the card (or `..`) missing its mount, so
// that a read-only card passes the check by name.
#[test]
fn describe_and_the_read_only_check_resolve_symlinks() {
    let t = tempfile::tempdir().unwrap();
    let base = t.path().canonicalize().unwrap();
    let real = base.join("real");
    fs::create_dir(&real).unwrap();
    let link = base.join("link");
    symlink(&real, &link).unwrap();
    let sys = Sys::fiio_card(Some("1"));
    let info = line(
        real.to_str().unwrap(),
        "/",
        "ro",
        "vfat",
        "/dev/fiiotest0p1",
    );
    let f = describe_in(&info, sys.path(), &link);
    assert_eq!(f.mount.mountpoint, real);
    assert_eq!(f.source.as_deref(), Some("/dev/fiiotest0p1"));
    assert!(f.mount.read_only);
    not_writable(check_writable_in(&info, &f, true));
}

// Prevents: a folder that is not a mount being described with another
// mount's device, fstype or USB ids.
#[test]
fn a_plain_folder_has_no_source_no_usb_and_no_key() {
    let t = tempfile::tempdir().unwrap();
    let sys = Sys::fiio_card(Some("1"));
    let info = line(X3, "/", "rw", "vfat", "/dev/fiiotest0p1");
    let f = describe_in(&info, sys.path(), t.path());
    assert_eq!(f.source, None);
    assert_eq!(f.usb, None);
    assert_eq!(f.key(), None);
    assert_eq!(f.mount.fstype, "");
    assert!(!f.mount.read_only);
    assert_eq!(f.mount.mountpoint, t.path().canonicalize().unwrap());
    // A folder of the card is not the card.
    let sub = describe_in(&info, sys.path(), &Path::new(X3).join("Music"));
    assert_eq!(sub.source, None);
    assert_eq!(sub.key(), None);
}

// Prevents: tests (or a user with the variable set) finding the real card.
#[test]
fn detection_is_off_under_cargo_and_find_cards_then_refuses() {
    assert!(
        detection_off(),
        "cargo's .cargo/config.toml must set FIIO_TUNES_NO_DETECT; refusing to look at the real mounts"
    );
    match find_cards() {
        Err(Error::DetectionOff(hint)) => assert!(hint.contains("device.mountpoint"), "{hint}"),
        other => panic!("expected DetectionOff, got {other:?}"),
    }
    let err = locate(&Config::default()).unwrap_err();
    assert!(matches!(err, Error::DetectionOff(_)), "{err:?}");
    assert!(err.to_string().contains("device.mountpoint"), "{err}");
}

// Prevents: `device.mountpoint` being ignored, or a plain folder given there
// being treated as a card with a device.
#[test]
fn locate_uses_device_mountpoint_when_it_is_set() {
    let t = tempfile::tempdir().unwrap();
    let folder = t.path().canonicalize().unwrap();
    let mut cfg = Config::default();
    cfg.device.mountpoint = folder.clone();
    // Works with detection off: naming the mountpoint is the way around it.
    let f = locate(&cfg).unwrap();
    assert_eq!(f.mount.mountpoint, folder);
    assert_eq!((&f.source, &f.usb, f.key()), (&None, &None, None));
}

// Prevents: a filesystem the kernel remounted read-only after an error
// (`ro` only in the superblock options, after the source) being taken for
// writable: the per-mount options say `rw` there.
#[test]
fn read_only_in_the_superblock_options_makes_the_mount_read_only() {
    use fiio_core::mounts::parse_mountinfo;
    let sys = Sys::fiio_card(Some("1"));
    for (opts, sb, ro) in [
        ("rw,nosuid", "ro,uid=1000", true),
        ("rw,nosuid", "rw,uid=1000,errors=remount-ro", false),
        ("ro,nosuid", "rw,uid=1000", true),
        ("ro", "ro", true),
        ("rw", "rw", false),
        // `ro` as a part of another option is not read-only.
        ("rw,errors=remount-ro", "rw,nro", false),
    ] {
        let info = line_sb(X3, "/", opts, "vfat", "/dev/fiiotest0p1", sb);
        let mounts = parse_mountinfo(&info);
        assert_eq!(mounts[0].read_only, ro, "{opts} / {sb}");
        let found = cards_in(&info, sys.path());
        assert_eq!(found.len(), 1, "a read-only card is still found");
        assert_eq!(found[0].mount.read_only, ro, "{opts} / {sb}");
        let described = describe_in(&info, sys.path(), Path::new(X3));
        assert_eq!(described.mount.read_only, ro, "{opts} / {sb}");
        for allow in [false, true] {
            let r = check_writable_in(&info, &found[0], allow);
            match ro {
                true => {
                    not_writable(r);
                }
                false => r.unwrap_or_else(|e| panic!("{opts} / {sb}: {e}")),
            }
        }
    }
}

// Prevents: a card whose root is mounted twice (a bind mount of the card
// itself) being reported as two cards, so `locate` would stop with
// "several cards"; a bind of a subfolder is still not a card.
#[test]
fn the_same_card_mounted_twice_is_one_card_at_the_first_mountpoint() {
    let sys = Sys::fiio_card(Some("1"));
    let first = line(X3, "/", "rw", "vfat", "/dev/fiiotest0p1");
    let bind = line("/mnt/again", "/", "rw", "vfat", "/dev/fiiotest0p1");
    let sub = line("/mnt/sub", "/Music", "rw", "vfat", "/dev/fiiotest0p1");
    let found = cards_in(&[first.as_str(), &bind, &sub].concat(), sys.path());
    assert_eq!(mountpoints(&found), [Path::new(X3)]);
    let found = cards_in(&[bind.as_str(), &first, &sub].concat(), sys.path());
    assert_eq!(mountpoints(&found), [Path::new("/mnt/again")]);
    // A subfolder bind listed first does not take the place of the card.
    let found = cards_in(&[sub.as_str(), &first, &bind].concat(), sys.path());
    assert_eq!(mountpoints(&found), [Path::new(X3)]);
    // Two different cards are still two.
    let usb = sys.usb(&UsbSpec::fiio("1-5", Some("2")));
    sys.block(&usb, "fiiotest1", &["fiiotest1p1"]);
    let other = line("/mnt/other", "/", "rw", "vfat", "/dev/fiiotest1p1");
    let found = cards_in(&[first.as_str(), &bind, &other].concat(), sys.path());
    assert_eq!(
        mountpoints(&found),
        [Path::new(X3), Path::new("/mnt/other")]
    );
}
