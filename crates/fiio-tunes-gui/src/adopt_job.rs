//! Adoption from the window, run on a worker thread: no GTK here. The
//! worker scans the library, reads the card and matches them; the window
//! shows the proposals, one row each, and the user picks a library file for
//! each or leaves the card file unmanaged. Only the index is written.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fiio_core::adoption::{self, Run};
use fiio_core::config::{Config, Paths};
use fiio_core::store::{Store, TrackRow, devices};
use fiio_core::{adopt, device, manifest, playlists, scan, sync};

use crate::view_model;

/// One choice in a proposal's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// The library track, or `None` to leave the card file unmanaged.
    pub track_id: Option<i64>,
    pub label: String,
}

/// One proposal, as the review grid shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRow {
    /// The card file's key in the run.
    pub key: u64,
    /// "Artist – Title" from its tags, or its path when they were not read.
    pub card: String,
    pub album: String,
    /// Path, size and time of the card's file.
    pub detail: String,
    pub why: &'static str,
    /// Leaving it unmanaged first, then the candidates, best first.
    pub choices: Vec<Choice>,
    /// The index in `choices` that `fiio-tunes adopt --accept-proposals`
    /// takes.
    pub suggested: usize,
}

/// An adoption ready to be reviewed.
pub struct Review {
    pub run: Run,
    pub device_name: String,
    /// From the player's USB descriptors, recorded with the device.
    pub name: Option<String>,
    pub model: Option<String>,
    /// What was matched, as `fiio-tunes adopt` prints it.
    pub summary: String,
    /// Where the full report was written.
    pub report: PathBuf,
    pub rows: Vec<ReviewRow>,
}

/// What a saved adoption holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub tracks: usize,
    pub playlists: usize,
    /// Card files left unmanaged: unresolved proposals and files with no
    /// library file.
    pub unmanaged: usize,
    /// Library tracks no card file holds, copied by the next sync.
    pub to_copy: usize,
}

fn text(v: &Option<String>) -> &str {
    v.as_deref().filter(|s| !s.trim().is_empty()).unwrap_or("?")
}

/// The last two parts of a path: enough to tell duplicates apart.
fn short_path(p: &Path) -> String {
    let parts: Vec<_> = p.iter().collect();
    let tail = &parts[parts.len().saturating_sub(2)..];
    tail.iter()
        .map(|s| s.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn library_label(t: &TrackRow) -> String {
    let mut s = format!("{} – {}", text(&t.tags.artist), t.display_title());
    if let Some(album) = t.tags.album.as_deref().filter(|a| !a.trim().is_empty()) {
        s.push_str(&format!(" ({album})"));
    }
    if let Some(ms) = t.info.duration_ms {
        s.push_str(&format!(" · {}", view_model::duration(ms)));
    }
    s.push_str(&format!(
        " · {} · {}",
        sync::bytes(t.size),
        short_path(&t.path)
    ));
    s
}

/// The review grid's rows, one per proposal, in the run's order. Pure.
pub fn review_rows(run: &Run) -> Vec<ReviewRow> {
    let suggested: HashMap<u64, i64> = adopt::resolve(&run.adoption, true).0.into_iter().collect();
    run.adoption
        .proposals
        .iter()
        .map(|p| {
            let t = run.tracks.get(p.key as usize);
            let mut choices = vec![Choice {
                track_id: None,
                label: "Leave it on the card, unmanaged".into(),
            }];
            choices.extend(p.candidates.iter().filter_map(|id| {
                Some(Choice {
                    track_id: Some(*id),
                    label: library_label(run.library.get(id)?),
                })
            }));
            let pick = suggested.get(&p.key).copied();
            ReviewRow {
                key: p.key,
                card: t.map_or_else(
                    || format!("card file {}", p.key),
                    |t| match (&t.artist, &t.title) {
                        (None, None) => t.file.path.clone(),
                        _ => format!("{} – {}", text(&t.artist), text(&t.title)),
                    },
                ),
                album: t.and_then(|t| t.album.clone()).unwrap_or_default(),
                detail: t.map_or_else(String::new, |t| {
                    let mut d = format!("{} · {}", t.file.path, sync::bytes(t.file.size));
                    if let Some(ms) = t.duration_ms {
                        d.push_str(&format!(" · {}", view_model::duration(ms)));
                    }
                    d
                }),
                why: adoption::reason(p.reason),
                suggested: choices
                    .iter()
                    .position(|c| c.track_id.is_some() && c.track_id == pick)
                    .unwrap_or(0),
                choices,
            }
        })
        .collect()
}

/// The decision per proposal from the index picked in each row. A pick out
/// of range leaves the file unmanaged.
pub fn choices(rows: &[ReviewRow], picked: &[usize]) -> HashMap<u64, Option<i64>> {
    rows.iter()
        .zip(picked)
        .map(|(r, &i)| (r.key, r.choices.get(i).and_then(|c| c.track_id)))
        .collect()
}

/// One line per library file picked for more than one card file, or for
/// one when another already matched it: Save is refused while there are any.
pub fn conflicts(review: &Review, picked: &[usize]) -> Vec<String> {
    let chosen = choices(&review.rows, picked);
    let Err(conflicts) = adopt::resolve_choices(&review.run.adoption, &chosen) else {
        return Vec::new();
    };
    conflicts
        .iter()
        .map(|c| {
            let name = review
                .run
                .library
                .get(&c.track_id)
                .map_or_else(|| c.track_id.to_string(), library_label);
            let rows = c
                .keys
                .iter()
                .filter(|k| review.rows.iter().any(|r| r.key == **k))
                .count();
            match (c.keys.len() - rows, rows) {
                (0, _) => format!("{name} is picked for {rows} card files"),
                (_, 1) => format!("{name} is picked, but another card file already matched it"),
                _ => format!(
                    "{name} is picked for {rows} card files, and another already matched it"
                ),
            }
        })
        .collect()
}

/// The device key of the card connected now.
fn key_now(config: &Config) -> Result<(device::Found, String)> {
    let found = device::locate(config)?;
    let identity = manifest::identity(&found.mount.mountpoint)?;
    let key = adoption::device_key(&found, identity.as_ref(), config.device.allow_folder)?;
    Ok((found, key))
}

fn refuse_adopted(store: &Store, key: &str) -> Result<()> {
    let conn = store.conn();
    if let Some(d) = devices::device(conn, key)?
        && devices::mapped_count(conn, d.id)? > 0
    {
        bail!("this X3 is already adopted; `fiio-tunes adopt --force` adopts it again");
    }
    Ok(())
}

/// Scans the library, reads the card and matches them. Refuses an X3
/// already adopted: adopting again replaces its mappings, which only
/// `fiio-tunes adopt --force` does. `progress` reports `(done, total)` card
/// files read. Writes the index and `adopt-report.txt`.
pub fn prepare(config: &Config, paths: &Paths, progress: &dyn Fn(usize, usize)) -> Result<Review> {
    let mut store = Store::open(&paths.database())?;
    scan::scan(&mut store, config, &|_| {}).context("scanning the library")?;
    let conn = store.conn();
    playlists::sync_from_disk(conn, config, false).context("reading the playlists folder")?;
    let (found, key) = key_now(config)?;
    refuse_adopted(&store, &key)?;
    // Held while the card is read, so a sync cannot change it meanwhile.
    let _lock = fiio_core::lock::card(paths)?;
    let run = adoption::run(conn, config, &found.mount.mountpoint, &key, false, progress)?;
    std::fs::create_dir_all(&paths.data_dir)
        .with_context(|| format!("creating {}", paths.data_dir.display()))?;
    let report = paths.data_dir.join("adopt-report.txt");
    std::fs::write(&report, adoption::report(&run))
        .with_context(|| format!("writing {}", report.display()))?;
    let usb = found.usb.as_ref();
    let name = usb.and_then(|u| u.name.clone());
    let model = usb.map(|u| {
        format!(
            "{} {}",
            u.manufacturer.as_deref().unwrap_or("FiiO"),
            u.name.as_deref().unwrap_or("X3")
        )
    });
    Ok(Review {
        device_name: name.clone().unwrap_or_else(|| "X3".into()),
        name,
        model,
        summary: adoption::summary(&run),
        rows: review_rows(&run),
        report,
        run,
    })
}

/// Saves the adoption with the picks made in the grid, if the card
/// connected is still the one reviewed.
pub fn save(config: &Config, paths: &Paths, mut review: Review, picked: &[usize]) -> Result<Saved> {
    match key_now(config) {
        Ok((_, key)) if key == review.run.key => {}
        Ok(_) => bail!("the X3 connected is not the one reviewed; nothing saved"),
        Err(_) => bail!("the X3 reviewed is no longer connected; nothing saved"),
    }
    let store = Store::open(&paths.database())?;
    let conn = store.conn();
    let chosen = choices(&review.rows, picked);
    adoption::choose(&mut review.run, &chosen)?;
    adoption::save(
        conn,
        &review.run,
        review.name.as_deref(),
        review.model.as_deref(),
        false,
    )?;
    let run = &review.run;
    Ok(Saved {
        tracks: run.mapping.len(),
        playlists: run.playlists.matched.len(),
        unmanaged: run.left.len() + run.adoption.unmatched_device.len(),
        to_copy: run.library.len() - run.mapping.len(),
    })
}

#[cfg(test)]
mod tests;
