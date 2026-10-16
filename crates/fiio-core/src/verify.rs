//! The check after a sync (technical document, 11.3): the card is listed
//! again and compared with what the plan expects.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;

use crate::Result;
use crate::card;
use crate::paths::card_key;
use crate::plan::Expected;

/// Every difference between the card at `card` and `expected`: a managed
/// file missing or of another size, a managed playlist missing or with
/// other bytes, an unmanaged file gone or changed in size, and any audio or
/// playlist file the plan does not account for (one that should have been
/// removed). Empty when the card is as planned.
pub fn differences(card: &Path, expected: &Expected) -> Result<Vec<String>> {
    let now = card::read(card)?;
    let mut out: Vec<String> = now
        .problems
        .iter()
        .map(|(p, why)| format!("{p}: could not be read ({why})"))
        .collect();
    let audio: HashMap<String, &card::CardFile> =
        now.audio.iter().map(|f| (card_key(&f.path), f)).collect();
    let lists: HashMap<String, &card::CardFile> = now
        .playlists
        .iter()
        .map(|f| (card_key(&f.path), f))
        .collect();
    let mut known: HashSet<String> = HashSet::new();
    for f in &expected.files {
        known.insert(card_key(&f.path));
        match audio.get(&card_key(&f.path)) {
            None => out.push(format!("{}: missing", f.path)),
            Some(c) if c.size != f.size => {
                out.push(format!("{}: {} bytes, {} expected", f.path, c.size, f.size))
            }
            Some(_) => {}
        }
    }
    for p in &expected.playlists {
        known.insert(card_key(&p.path));
        match lists.get(&card_key(&p.path)) {
            None => out.push(format!("{}: missing", p.path)),
            Some(c) => {
                let path = card.join(&c.path);
                let mut bytes = Vec::new();
                match card::open_file(&path).and_then(|mut f| f.read_to_end(&mut bytes)) {
                    Err(e) => out.push(format!("{}: could not be read ({e})", p.path)),
                    Ok(_) if bytes != p.text.as_bytes() => {
                        out.push(format!("{}: not the playlist written", p.path))
                    }
                    Ok(_) => {}
                }
            }
        }
    }
    for (path, size) in &expected.unmanaged {
        known.insert(card_key(path));
        let found = audio
            .get(&card_key(path))
            .or_else(|| lists.get(&card_key(path)));
        match found {
            None => out.push(format!("{path}: an unmanaged file is gone")),
            Some(c) if c.size != *size => out.push(format!(
                "{path}: an unmanaged file changed size ({} bytes, {size} before)",
                c.size
            )),
            Some(_) => {}
        }
    }
    for f in now.audio.iter().chain(&now.playlists) {
        if !known.contains(&card_key(&f.path)) {
            out.push(format!("{}: on the card, not in the plan", f.path));
        }
    }
    Ok(out)
}
