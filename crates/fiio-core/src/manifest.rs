//! `.fiio-tunes/manifest.json` on the card: which player and which card
//! fiio-tunes synced it as (technical document, section 9.1). The executor
//! writes it; adoption reads the identity of one already there.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::card::{STATE_DIR, open_file};
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
