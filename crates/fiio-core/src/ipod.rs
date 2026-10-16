//! Reading an iPod, or a copy of its `iTunesDB`, into plain data for
//! import. Only reads: nothing here opens a file on the iPod for writing.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};

use crate::adopt::DeviceTrack;
use crate::error::IoContext;
use crate::mounts::{Mount, block_device, parse_mountinfo};
use crate::{Error, Result};

/// One iPod track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    /// The matcher's view; `key` is the dbid.
    pub adopt: DeviceTrack,
    /// Where the file is on the iPod, `:iPod_Control:Music:F00:ABCD.mp3`.
    pub ipod_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playlist {
    /// libgpod's persistent playlist id.
    pub id: u64,
    pub name: String,
    /// A smart playlist: `members` are the ones the iPod stored, its rules
    /// are not kept.
    pub smart: bool,
    /// dbids in playlist order, repeats included.
    pub members: Vec<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct Contents {
    /// The master playlist's name, which is the iPod's name.
    pub name: Option<String>,
    /// Upper-case hex without `0x`, when known.
    pub firewire_guid: Option<String>,
    pub tracks: Vec<Track>,
    /// Regular and smart playlists, in iPod order; not the master playlist
    /// nor Podcasts.
    pub playlists: Vec<Playlist>,
}

fn non_empty(s: Option<std::borrow::Cow<'_, str>>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty()).map(|s| s.into_owned())
}

/// The file behind an iPod path such as `:iPod_Control:Music:F12:ABCD.mp3`,
/// under `mountpoint`. `None` for a path that is empty or would leave the
/// mountpoint. FAT ignores case, so the iPod's spelling is used as is.
pub fn file_path(mountpoint: &Path, ipod_path: &str) -> Option<PathBuf> {
    let rel: PathBuf = ipod_path.strip_prefix(':')?.split(':').collect();
    let plain = rel.components().all(|c| matches!(c, Component::Normal(_)));
    (plain && rel.components().next().is_some()).then(|| mountpoint.join(rel))
}

/// Reads the tracks and playlists of `db`. When it was opened from a
/// mounted iPod, the GUID falls back on the iPod's device files and USB
/// serial.
pub fn read(db: &gpod::Database) -> Contents {
    let mut in_playlists: HashMap<u64, usize> = HashMap::new();
    let mut playlists = Vec::new();
    for pl in db.playlists() {
        // The master and Podcasts playlists belong to the iPod itself.
        if pl.is_master() || pl.is_podcasts() {
            continue;
        }
        let members: Vec<u64> = pl.members().map(|t| t.dbid()).collect();
        for dbid in members.iter().collect::<BTreeSet<_>>() {
            *in_playlists.entry(*dbid).or_default() += 1;
        }
        playlists.push(Playlist {
            id: pl.id(),
            name: pl.name().map(|n| n.into_owned()).unwrap_or_default(),
            smart: pl.is_smart(),
            members,
        });
    }
    let tracks = db
        .tracks()
        .map(|t| Track {
            adopt: DeviceTrack {
                key: t.dbid(),
                size: u64::from(t.size()),
                duration_ms: u32::try_from(t.tracklen()).unwrap_or(0),
                title: non_empty(t.title()),
                artist: non_empty(t.artist()),
                album: non_empty(t.album()),
                playlists: in_playlists.get(&t.dbid()).copied().unwrap_or(0),
                playcount: t.playcount(),
                hash: None,
            },
            ipod_path: t.ipod_path().map(|p| p.into_owned()),
        })
        .collect();
    Contents {
        name: db
            .master_playlist()
            .and_then(|p| p.name().map(|n| n.into_owned())),
        firewire_guid: db
            .firewire_guid()
            .map(|g| normalize_guid(&g))
            .or_else(|| firewire_guid(&db.mountpoint()?)),
        tracks,
        playlists,
    }
}

/// A FireWire GUID as upper-case hex without a `0x` prefix, so GUIDs from
/// libgpod, SysInfo and iprune's index compare equal.
pub fn normalize_guid(guid: &str) -> String {
    let g = guid.trim();
    g.strip_prefix("0x")
        .or_else(|| g.strip_prefix("0X"))
        .unwrap_or(g)
        .to_ascii_uppercase()
}

/// The FireWire GUID of the iPod at `mountpoint`, from `SysInfoExtended` or
/// `SysInfo`, else from the USB serial of the device mounted there.
pub fn firewire_guid(mountpoint: &Path) -> Option<String> {
    let dir = mountpoint.join("iPod_Control/Device");
    let clean = |s: &str| {
        let s = normalize_guid(s);
        (!s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit())).then_some(s)
    };
    if let Ok(text) = std::fs::read_to_string(dir.join("SysInfoExtended")) {
        let value = text
            .split("<key>FireWireGUID</key>")
            .nth(1)
            .and_then(|r| r.split("<string>").nth(1))
            .and_then(|r| r.split("</string>").next());
        if let Some(g) = value.and_then(clean) {
            return Some(g);
        }
    }
    let from_sysinfo = std::fs::read_to_string(dir.join("SysInfo"))
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|l| l.strip_prefix("FirewireGuid:"))
                .and_then(clean)
        });
    from_sysinfo.or_else(|| usb_guid(mountpoint))
}

/// The iPod's USB serial, which is its FireWire GUID. `None` for a folder.
fn usb_guid(mountpoint: &Path) -> Option<String> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let dev = block_device(&mountinfo, mountpoint)?;
    usb_guid_in(Path::new("/sys"), &dev)
}

/// [`usb_guid`] for the block device `dev` (`/dev/sdc2`) under the sysfs
/// root `sys`: the serial of the USB device above it, if it is Apple's
/// (vendor 05ac) and the serial is hex.
pub fn usb_guid_in(sys: &Path, dev: &str) -> Option<String> {
    let dev = std::fs::canonicalize(dev).unwrap_or_else(|_| PathBuf::from(dev));
    let sys = std::fs::canonicalize(sys).ok()?;
    let block = std::fs::canonicalize(sys.join("class/block").join(dev.file_name()?)).ok()?;
    let usb = block
        .ancestors()
        .take_while(|p| p.starts_with(&sys))
        .find(|p| p.join("idVendor").is_file())?;
    let read = |name: &str| std::fs::read_to_string(usb.join(name)).ok();
    if read("idVendor")?.trim() != "05ac" {
        return None;
    }
    let serial = read("serial")?;
    let serial = serial.trim();
    (!serial.is_empty() && serial.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| serial.to_ascii_uppercase())
}

/// The database file of an iPod mounted at `mountpoint`.
pub fn itunesdb(mountpoint: &Path) -> PathBuf {
    mountpoint.join("iPod_Control/iTunes/iTunesDB")
}

/// Whether detection is turned off (`FIIO_TUNES_NO_DETECT`, set for
/// everything cargo runs).
pub fn detection_off() -> bool {
    std::env::var_os("FIIO_TUNES_NO_DETECT").is_some_and(|v| !v.is_empty())
}

/// Mounted filesystems holding an iPod database.
pub fn find_ipods() -> Result<Vec<Mount>> {
    if detection_off() {
        return Err(Error::DetectionOff);
    }
    let path = Path::new("/proc/self/mountinfo");
    let text = std::fs::read_to_string(path).at(path)?;
    Ok(ipods_in(&text))
}

/// [`find_ipods`] on the text of a mountinfo file.
pub fn ipods_in(mountinfo: &str) -> Vec<Mount> {
    parse_mountinfo(mountinfo)
        .into_iter()
        .filter(|m| itunesdb(&m.mountpoint).is_file())
        .collect()
}

/// The one mounted iPod.
pub fn find_ipod() -> Result<PathBuf> {
    match &find_ipods()?[..] {
        [] => Err(Error::NoIpod),
        [one] => Ok(one.mountpoint.clone()),
        many => Err(Error::SeveralIpods(
            many.iter()
                .map(|m| m.mountpoint.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_path_stays_under_the_mountpoint() {
        let m = Path::new("/mnt/ipod");
        assert_eq!(
            file_path(m, ":iPod_Control:Music:F01:AB.mp3"),
            Some(PathBuf::from("/mnt/ipod/iPod_Control/Music/F01/AB.mp3"))
        );
        assert_eq!(file_path(m, ":iPod_Control:..:..:etc:passwd"), None);
        assert_eq!(file_path(m, "iPod_Control:Music"), None);
        assert_eq!(file_path(m, ":"), None);
    }

    #[test]
    fn guid_comes_from_sysinfo_extended_then_sysinfo() {
        let dir = tempfile::tempdir().unwrap();
        let dev = dir.path().join("iPod_Control/Device");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(
            dev.join("SysInfo"),
            "ModelNumStr: xB147\nFirewireGuid: 0x000a27001b2c3d4e\n",
        )
        .unwrap();
        assert_eq!(
            firewire_guid(dir.path()).as_deref(),
            Some("000A27001B2C3D4E")
        );
        std::fs::write(
            dev.join("SysInfoExtended"),
            "<dict><key>FireWireGUID</key>\n<string>000A27FFFFFFFFFF</string></dict>",
        )
        .unwrap();
        assert_eq!(
            firewire_guid(dir.path()).as_deref(),
            Some("000A27FFFFFFFFFF")
        );
    }

    #[test]
    fn normalize_guid_drops_prefix_and_case() {
        assert_eq!(normalize_guid(" 0xabc "), "ABC");
        assert_eq!(normalize_guid("ABC"), "ABC");
    }
}
