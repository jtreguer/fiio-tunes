//! `.fiio-tunes/manifest.json` on the card: which player and which card
//! fiio-tunes synced it as (technical document, section 9.1). The executor
//! writes it; adoption reads the identity of one already there.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::card::{STATE_DIR, open_file};
use crate::playlist::{WriteMode, mtime_ns, write_atomic};
use crate::{Error, Result};

/// Where the manifest is on the card at `card`.
pub fn path(card: &Path) -> PathBuf {
    card.join(STATE_DIR).join("manifest.json")
}

/// The fields that say whose card it is. Other fields are ignored here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Identity {
    /// `usb:<serial>`, or the key of a folder used as a card in tests.
    pub device_key: String,
    /// The random id of the card, as in `devices.card_id`.
    pub card_id: String,
}

/// The identity in the card's manifest; `None` when there is none. A
/// manifest that cannot be read or parsed is an error: it is not taken
/// for no manifest.
pub fn identity(card: &Path) -> Result<Option<Identity>> {
    let p = path(card);
    let file = match open_file(&p) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io { path: p, source: e }),
    };
    serde_json::from_reader(std::io::BufReader::new(file))
        .map(Some)
        .map_err(|e| Error::BadManifest(p, e.to_string()))
}

/// The manifest fiio-tunes writes: whose card it is, and what it put there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub device_key: String,
    pub card_id: String,
    /// Unix time of the sync that wrote it.
    pub written_at: i64,
    /// The managed audio files.
    pub tracks: Vec<ManifestTrack>,
    /// The managed playlist files, by card path.
    pub playlists: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestTrack {
    pub path: String,
    pub size: u64,
    /// BLAKE3, hex.
    pub hash: String,
}

/// A new random card id: 32 hex digits from the kernel's random source.
pub fn new_card_id() -> Result<String> {
    let p = Path::new("/proc/sys/kernel/random/uuid");
    let uuid = std::fs::read_to_string(p).map_err(|source| Error::Io {
        path: p.to_owned(),
        source,
    })?;
    Ok(uuid.trim().replace('-', ""))
}

/// The bytes of the card's manifest, `None` when there is none.
pub fn raw(card: &Path) -> Result<Option<Vec<u8>>> {
    use std::io::Read;
    let p = path(card);
    let mut bytes = Vec::new();
    match open_file(&p).and_then(|mut f| f.read_to_end(&mut bytes)) {
        Ok(_) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io { path: p, source }),
    }
}

/// Writes `bytes` as the card's manifest, whole, and reads it back.
pub fn write_raw(card: &Path, bytes: &[u8]) -> Result<()> {
    let p = path(card);
    let dir = card.join(STATE_DIR);
    std::fs::create_dir_all(&dir).map_err(|source| Error::Io {
        path: dir.clone(),
        source,
    })?;
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Error::BadManifest(p.clone(), "not UTF-8".into()))?;
    let mode = match mtime_ns(&p) {
        Ok(mtime_ns) => WriteMode::Replace { mtime_ns },
        Err(_) => WriteMode::CreateNew,
    };
    write_atomic(&p, text, mode)?;
    let back = crate::card::read_back(&p).map_err(|source| Error::Io {
        path: p.clone(),
        source,
    })?;
    if back != *blake3::hash(bytes).as_bytes() {
        return Err(Error::VerifyFailed(format!(
            "{} reads back wrong",
            p.display()
        )));
    }
    Ok(())
}

/// Writes the manifest to the card at `card`.
pub fn write(card: &Path, m: &Manifest) -> Result<()> {
    let json = serde_json::to_vec_pretty(m).expect("plain data serializes");
    write_raw(card, &json)
}

/// Removes the card's manifest, if there is one, and its folder when it
/// is left empty.
pub fn remove(card: &Path) -> Result<()> {
    let p = path(card);
    match std::fs::remove_file(&p) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(Error::Io { path: p, source: e });
        }
        _ => {}
    }
    // Only when empty: the probe or a later version may keep files there.
    let _ = std::fs::remove_dir(card.join(STATE_DIR));
    Ok(())
}

/// Hex of a hash.
pub fn hex(h: &[u8; 32]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

/// A hash from its hex.
pub fn unhex(s: &str) -> Option<[u8; 32]> {
    // Hex digits only: `from_str_radix` would take a sign.
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_identity_and_ignores_the_rest() {
        let card = tempfile::tempdir().unwrap();
        assert_eq!(identity(card.path()).unwrap(), None);
        std::fs::create_dir(card.path().join(STATE_DIR)).unwrap();
        std::fs::write(
            path(card.path()),
            r#"{"version": 1, "device_key": "usb:1", "card_id": "abc", "tracks": []}"#,
        )
        .unwrap();
        assert_eq!(
            identity(card.path()).unwrap(),
            Some(Identity {
                device_key: "usb:1".into(),
                card_id: "abc".into()
            })
        );
        std::fs::write(path(card.path()), "{").unwrap();
        assert!(matches!(identity(card.path()), Err(Error::BadManifest(..))));
    }
}
