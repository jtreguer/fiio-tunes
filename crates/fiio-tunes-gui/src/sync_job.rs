//! The sync's work, run on a worker thread: no GTK here. Each step opens
//! its own connection to the index, reads the card and returns plain data.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use fiio_core::config::{Config, Paths};
use fiio_core::exec::{self, Progress};
use fiio_core::plan::Plan;
use fiio_core::store::Store;
use fiio_core::{device, scan, sync};

/// A plan ready to be shown.
#[derive(Debug, Clone)]
pub struct Planned {
    /// The whole plan, as Apply must find it again.
    pub plan: Plan,
    pub device_name: String,
    /// One line on what the scan found, and on files taken back.
    pub scan: String,
    /// The plan's counts, as `fiio-tunes sync` prints them.
    pub summary: String,
    /// Every change, one per line.
    pub details: String,
    /// One line on what Apply does.
    pub to_apply: String,
    /// Set when the plan removes many managed tracks or playlists at once.
    pub mass_removal: Option<String>,
    /// False when the plan does not fit on the card: Apply is refused.
    pub fits: bool,
}

/// What an applied sync did.
#[derive(Debug, Clone, Default)]
pub struct Applied {
    pub copied: usize,
    pub recopied: usize,
    pub moved: usize,
    pub removed: usize,
    pub bytes: u64,
    /// Playlist files written, renamed, removed.
    pub written: usize,
    pub renamed: usize,
    pub deleted: usize,
    /// The first sync after an adoption: the card got its id.
    pub first: bool,
    pub stamp: Option<String>,
    /// Set when the sync was stopped; what was done is recorded and checked.
    pub stopped: Option<String>,
    /// Files not copied (gone, unreadable or changed since the scan), with
    /// why.
    pub skipped: Vec<(PathBuf, String)>,
    pub warnings: Vec<String>,
}

fn open(paths: &Paths) -> Result<Store> {
    Ok(Store::open(&paths.database())?)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The line on what Apply does.
pub fn to_apply(plan: &Plan) -> String {
    if plan.is_empty() {
        return "The card is up to date: nothing to apply.".into();
    }
    let bytes: u64 = plan.copy.iter().map(|c| c.size).sum();
    let moved: std::collections::HashSet<i64> = plan.moves.iter().map(|m| m.track_id).collect();
    format!(
        "Apply copies {} ({}), copies {} again, moves {}, removes {} and makes {}.",
        plural(plan.copy.len(), "track", "tracks"),
        sync::bytes(bytes),
        plural(plan.replace.len(), "track", "tracks"),
        plural(moved.len(), "track", "tracks"),
        plural(plan.remove.len(), "file", "files"),
        plural(plan.playlists.len(), "playlist change", "playlist changes"),
    )
}

/// The plan as Apply compares it: without the free space, which another
/// program writing elsewhere on the card may change.
fn comparable(plan: &Plan) -> Plan {
    let mut p = plan.clone();
    p.space = Default::default();
    p
}

/// Scans the library, reads the card and plans. Writes only the index.
pub fn plan(config: &Config, paths: &Paths) -> Result<Planned> {
    let mut store = open(paths)?;
    let r = scan::scan(&mut store, config, &|_| {}).context("scanning the library")?;
    let mut scan = format!(
        "library: {} unchanged, {} added, {} updated, {} moved, {} missing",
        r.unchanged,
        r.added,
        r.updated + r.returned,
        r.moved.len(),
        r.missing.len()
    );
    let found = device::locate(config)?;
    // Not while another fiio-tunes writes the card.
    let _lock = fiio_core::lock::card(paths)?;
    let prepared = sync::prepare(store.conn(), config, &found)?;
    if prepared.taken_back > 0 {
        scan.push_str(&format!(
            "; {} files an interrupted sync left on the card were taken back",
            prepared.taken_back
        ));
    }
    if !prepared.contents.parts.is_empty() {
        scan.push_str(&format!(
            "; {} unfinished copies are on the card (`fiio-tunes doctor --fix` removes them)",
            prepared.contents.parts.len()
        ));
    }
    let mass_removal = match exec::check(&prepared, exec::Options::default()) {
        Err(e @ fiio_core::Error::MassRemoval(_)) => Some(e.to_string()),
        _ => None,
    };
    let scope = format!(
        "syncs {}{}",
        prepared.scope.0.describe(),
        if prepared.scope.1 {
            ""
        } else {
            " (from [sync] in the config)"
        }
    );
    Ok(Planned {
        fits: prepared.plan.space.fits,
        to_apply: to_apply(&prepared.plan),
        device_name: prepared.device.name.clone().unwrap_or_else(|| "X3".into()),
        scan,
        summary: format!("{}{scope}", sync::describe(&prepared, false)),
        details: sync::describe(&prepared, true),
        mass_removal,
        plan: prepared.plan,
    })
}

/// Applies `expected`, the plan the user saw. The index and the card are
/// read again first (the library is not scanned again); if the plan they give is not the same (free space
/// aside), nothing is written. `cancel` stops the copies after the current
/// file.
pub fn apply(
    config: &Config,
    paths: &Paths,
    expected: &Plan,
    allow_mass_removal: bool,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<Applied> {
    // Held from before the card is read until it is written and checked.
    let _lock = fiio_core::lock::card(paths)?;
    let store = open(paths)?;
    let conn = store.conn();
    let found = device::locate(config)?;
    let prepared = sync::prepare(conn, config, &found)?;
    if comparable(&prepared.plan) != comparable(expected) {
        bail!(
            "the library, the playlists or the card changed since the plan was made; nothing \
             was written. Sync again to see the new plan"
        );
    }
    if prepared.plan.is_empty() {
        return Ok(Applied::default());
    }
    let opts = exec::Options { allow_mass_removal };
    let done = exec::run(
        conn, config, paths, &found, &prepared, opts, progress, cancel,
    )
    .map_err(|f| {
        let hint = match &f.backup {
            Some(b) => format!(
                "the card was backed up as {} before writing; run `fiio-tunes restore {}` \
                     before syncing again: it puts its playlists and record back",
                b.stamp, b.stamp
            ),
            None => "nothing was written to the card".into(),
        };
        anyhow::Error::new(f.error).context(hint)
    })?;
    Ok(Applied {
        copied: done.copied,
        recopied: done.recopied,
        moved: done.moved,
        removed: done.removed,
        bytes: done.bytes,
        written: done.written,
        renamed: done.renamed,
        deleted: done.deleted,
        first: done.first,
        stamp: done.backup.map(|b| b.stamp),
        stopped: done.stopped,
        skipped: done.skipped,
        warnings: done.warnings,
    })
}

/// Whether the plan copies anything, which Stop can stop.
pub fn copies(plan: &Plan) -> bool {
    !plan.copy.is_empty() || !plan.replace.is_empty()
}

/// Whether the Stop button should be enabled after this progress report,
/// or `None` when the report does not change it. On during the copies
/// unless Stop was pressed; off once they are over.
pub fn stop_enabled(p: &Progress, cancelled: bool) -> Option<bool> {
    match p {
        Progress::Copying { .. } => Some(!cancelled),
        Progress::Playlists | Progress::Verifying => Some(false),
        Progress::Removing { .. } | Progress::Moving { .. } => None,
    }
}

#[cfg(test)]
mod tests;
