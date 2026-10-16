//! Mounted filesystems, read from `/proc/self/mountinfo`.

use std::path::{Path, PathBuf};

/// A mounted filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub mountpoint: PathBuf,
    pub fstype: String,
    pub read_only: bool,
    /// The folder of the filesystem mounted here: `/` for a whole
    /// filesystem, another path for a bind mount of a subfolder.
    pub root: PathBuf,
}

/// A mountinfo or fstab path with the kernel's octal escapes (`\040` for a
/// space) decoded.
pub fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // Computed wide: a corrupt `\777` must not overflow a byte.
        let value = (i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)))
            .then(|| {
                b[i + 1..i + 4]
                    .iter()
                    .fold(0u32, |v, c| v * 8 + u32::from(c - b'0'))
            })
            .filter(|&v| v <= 255);
        if let (b'\\', Some(v)) = (b[i], value) {
            out.push(v as u8);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One mountinfo line: the mount and its source (`/dev/sdc2`).
pub(crate) fn parse_line(line: &str) -> Option<(Mount, String)> {
    let (left, right) = line.split_once(" - ")?;
    let left: Vec<&str> = left.split(' ').collect();
    let root = unescape_mount(left.get(3)?);
    let mountpoint = unescape_mount(left.get(4)?);
    let options = left.get(5)?;
    let mut right = right.split(' ');
    let fstype = right.next()?.to_owned();
    let source = unescape_mount(right.next().unwrap_or(""));
    // The superblock's options: a filesystem the kernel remounted read-only
    // after an error (`errors=remount-ro`) says so only there.
    let super_options = right.next().unwrap_or("");
    let ro = |opts: &str| opts.split(',').any(|o| o == "ro");
    Some((
        Mount {
            mountpoint: PathBuf::from(mountpoint),
            fstype,
            read_only: ro(options) || ro(super_options),
            root: PathBuf::from(root),
        },
        source,
    ))
}

/// Parses `/proc/self/mountinfo`. Malformed lines are skipped.
pub fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|l| Some(parse_line(l)?.0))
        .collect()
}

/// The block device mounted exactly at `mountpoint`, from mountinfo
/// `text`: the last such entry, the one in effect. `None` when
/// `mountpoint` is not itself a mount of a `/dev/` device.
pub fn block_device(text: &str, mountpoint: &Path) -> Option<String> {
    let path = std::fs::canonicalize(mountpoint).unwrap_or_else(|_| mountpoint.to_owned());
    text.lines()
        .filter_map(parse_line)
        .rfind(|(m, _)| m.mountpoint == path)
        .map(|(_, source)| source)
        .filter(|s| s.starts_with("/dev/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mountinfo_lines() {
        let text = "\
22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw
95 22 8:33 / /run/media/julien/X3\\040de\\040Julien rw,nosuid,nodev,relatime shared:50 - vfat /dev/sdc1 rw,uid=1000
96 22 8:34 / /mnt/ro ro,relatime - ext4 /dev/sdd1 ro
garbage line
";
        let m = parse_mountinfo(text);
        assert_eq!(m.len(), 3);
        assert_eq!(m[1].mountpoint, Path::new("/run/media/julien/X3 de Julien"));
        assert_eq!((m[1].fstype.as_str(), m[1].read_only), ("vfat", false));
        assert!(m[2].read_only);
    }

    const INFO: &str = "\
22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw
95 22 8:33 / /run/media/julien/X3\\040de\\040Julien rw,nosuid shared:50 - vfat /dev/sdc1 rw,uid=1000
96 22 0:50 / /run/media/julien/tmpfs rw - tmpfs tmpfs rw
97 22 0:51 / /run/media/julien/fuse rw - fuse.x /dev rw
";

    #[test]
    fn parse_line_returns_the_source_with_escapes_decoded() {
        let (m, src) =
            parse_line("95 22 8:33 / /mnt/a\\040b ro,nosuid - vfat /dev/disk/by-label/A\\040B rw")
                .unwrap();
        assert_eq!(m.mountpoint, Path::new("/mnt/a b"));
        assert!(m.read_only);
        assert_eq!(src, "/dev/disk/by-label/A B");
        assert!(parse_line("garbage").is_none());
    }

    #[test]
    fn block_device_decodes_octal_escapes_in_the_mountpoint() {
        assert_eq!(
            block_device(INFO, Path::new("/run/media/julien/X3 de Julien")).as_deref(),
            Some("/dev/sdc1")
        );
    }

    #[test]
    fn block_device_is_none_for_a_folder_that_is_not_a_mount() {
        // A folder inside a mount, and the parent of one.
        assert_eq!(
            block_device(INFO, Path::new("/run/media/julien/X3 de Julien/Music")),
            None
        );
        assert_eq!(block_device(INFO, Path::new("/run/media/julien")), None);
    }

    #[test]
    fn block_device_ignores_a_sibling_that_only_shares_a_prefix() {
        let text = "95 22 8:33 / /mnt/card2 rw - vfat /dev/sdc1 rw\n";
        assert_eq!(block_device(text, Path::new("/mnt/card")), None);
        assert_eq!(
            block_device(text, Path::new("/mnt/card2")).as_deref(),
            Some("/dev/sdc1")
        );
    }

    #[test]
    fn block_device_is_none_when_the_source_is_not_under_dev() {
        assert_eq!(
            block_device(INFO, Path::new("/run/media/julien/tmpfs")),
            None
        );
        // "/dev" itself is not a device node either.
        assert_eq!(
            block_device(INFO, Path::new("/run/media/julien/fuse")),
            None
        );
    }

    #[test]
    fn block_device_takes_the_last_entry_for_a_mountpoint() {
        let text = "\
1 0 8:1 / /mnt/x rw - vfat /dev/sdb1 rw
2 0 8:2 / /mnt/x rw - vfat /dev/sdc1 rw
";
        assert_eq!(
            block_device(text, Path::new("/mnt/x")).as_deref(),
            Some("/dev/sdc1")
        );
        // A later non-device entry hides the earlier device.
        let text = format!("{text}3 0 0:5 / /mnt/x rw - tmpfs tmpfs rw\n");
        assert_eq!(block_device(&text, Path::new("/mnt/x")), None);
    }

    #[test]
    fn block_device_follows_symlinks_to_the_mountpoint() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let text = format!("1 0 8:1 / {} rw - vfat /dev/sdb1 rw\n", real.display());
        assert_eq!(block_device(&text, &link).as_deref(), Some("/dev/sdb1"));
    }

    #[test]
    fn unescape_handles_edge_cases() {
        assert_eq!(unescape_mount("a\\134b"), "a\\b");
        assert_eq!(unescape_mount("tail\\04"), "tail\\04");
        assert_eq!(unescape_mount("x\\0"), "x\\0");
    }
}
