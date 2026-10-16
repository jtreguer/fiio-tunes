//! Importing playlists from the window, run on worker threads: no GTK
//! here. A first worker reads the source and lists its playlists; a second
//! imports the ones picked. Only fiio-tunes' playlists folder, its index
//! and, for a scope copied, the X3's scope or the config are written; the
//! iPod, its backups and iprune's files are only read.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use fiio_core::config::{Config, Paths, Scope};
use fiio_core::import::{self, Imported, IpodSource, OnConflict, Outcome};
use fiio_core::iprune::{IpruneConfig, IprunePaths};
use fiio_core::store::Store;
use fiio_core::{ipod, playlists};

/// Where the playlists come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// iprune's playlists folder.
    Iprune,
    /// The one iPod mounted now.
    Ipod,
    /// An `iTunesDB` file, such as a backup's.
    File(PathBuf),
}

/// One playlist the source offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub name: String,
    /// How many entries, or for an iPod how many have a library file.
    pub detail: String,
    /// fiio-tunes already has a playlist of the name it would be imported as.
    pub taken: bool,
}

/// A source read, ready to import from.
pub enum Loaded {
    Iprune(import::IpruneFolder),
    Ipod(Box<import::Ipod>),
}

/// What the source holds, for the dialog.
pub struct Listing {
    pub loaded: Loaded,
    /// One or two lines on the source.
    pub about: String,
    pub offers: Vec<Offer>,
}

fn iprune_paths() -> Result<IprunePaths> {
    IprunePaths::from_env().ok_or_else(|| anyhow!("cannot determine the home directory"))
}

fn taken(config: &Config, name: &str) -> bool {
    playlists::file_for(config, name).is_ok_and(|f| f.exists())
}

/// Reads `source` and lists its playlists. `progress` reports `(done,
/// total)` iPod files compared with the library. Writes nothing.
pub fn load(
    config: &Config,
    paths: &Paths,
    source: &Source,
    progress: &dyn Fn(usize, usize),
) -> Result<Listing> {
    let ip = iprune_paths()?;
    if *source == Source::Iprune {
        let src = import::iprune_folder(config, &ip.config_file)?;
        let offers = src
            .playlists
            .iter()
            .map(|(name, file)| Offer {
                name: name.clone(),
                detail: fiio_core::playlist::read(file).map_or_else(
                    |e| format!("unreadable: {e}"),
                    |e| format!("{} entries", e.len()),
                ),
                taken: taken(config, name),
            })
            .collect();
        let mut about = format!("iprune's playlists folder, {}", src.dir.display());
        if !src.problems.is_empty() {
            about.push_str(&format!("; {} files could not be read", src.problems.len()));
        }
        return Ok(Listing {
            loaded: Loaded::Iprune(src),
            about,
            offers,
        });
    }
    let source = match source {
        Source::File(f) => IpodSource::File(f.clone()),
        _ => IpodSource::Mounted(ipod::find_ipod()?),
    };
    let store = Store::open(&paths.database())?;
    // libgpod's database stays on this thread; what it read is plain data.
    let db = import::open(&source)?;
    let read = import::read_ipod(store.conn(), &db, Some(&ip.database), progress)?;
    drop(db);
    let offers = read
        .contents
        .playlists
        .iter()
        .map(|pl| {
            let t = read.traced_playlist(pl);
            let smart = if pl.smart { ", smart" } else { "" };
            Offer {
                name: pl.name.clone(),
                detail: format!(
                    "{} of {} tracks in the library{smart}",
                    t.tracks.len(),
                    pl.members.len()
                ),
                taken: taken(config, &import::playlist_name(&pl.name)),
            }
        })
        .collect();
    let c = &read.contents;
    let mut about = format!(
        "iPod {}, {} tracks, {} playlists",
        c.name.as_deref().unwrap_or("?"),
        c.tracks.len(),
        c.playlists.len()
    );
    about.push_str(&match (&read.iprune_device, &read.iprune_error) {
        (_, Some(e)) => format!("\niprune's index could not be read ({e}): traced by tags only"),
        (Some(_), None) => "\ntraced through iprune's record of this iPod, then by tags".into(),
        (None, None) => "\niprune has no record of this iPod: traced by tags only".into(),
    });
    Ok(Listing {
        loaded: Loaded::Ipod(Box::new(read)),
        about,
        offers,
    })
}

/// What an import did, as the dialog shows it.
pub struct Done {
    pub text: String,
    /// Playlists that could not be written.
    pub failed: usize,
}

fn describe(done: &[Imported]) -> String {
    let count = |o: Outcome| done.iter().filter(|i| i.outcome == o).count();
    let mut s = format!(
        "Imported {} new, {} replaced, {} under a new name; {} skipped.",
        count(Outcome::Created),
        count(Outcome::Replaced),
        count(Outcome::Renamed),
        count(Outcome::Skipped)
    );
    for i in done {
        let n = i.entries;
        s.push('\n');
        s.push_str(&match i.outcome {
            Outcome::Created => format!("+ {} ({n} entries)", i.name),
            Outcome::Replaced => format!("~ {} ({n} entries, replaced)", i.name),
            Outcome::Renamed => format!("+ {} ({n} entries, from {:?})", i.name, i.source),
            Outcome::Skipped => format!("= {:?} skipped: {:?} exists", i.source, i.name),
        });
    }
    s
}

/// Imports the playlists `names` of `loaded` (all when empty), and with
/// `copy_scope` sets the iPod's iprune scope as what the X3 syncs.
pub fn run(
    config: &Config,
    paths: &Paths,
    loaded: Loaded,
    names: &[String],
    on_conflict: OnConflict,
    copy_scope: bool,
) -> Result<Done> {
    let store = Store::open(&paths.database())?;
    let conn = store.conn();
    let mut text;
    let failed;
    match loaded {
        Loaded::Iprune(src) => {
            let r = import::import_iprune(conn, config, &src, names, on_conflict)?;
            text = describe(&r.playlists);
            if !r.unresolved.is_empty() {
                text.push_str(&format!(
                    "\n{} entries name no library file and were kept as they were.",
                    r.unresolved.len()
                ));
            }
            for (p, why) in &r.unreadable {
                text.push_str(&format!("\nNot read: {}: {why}", p.display()));
            }
            for (n, why) in &r.failed {
                text.push_str(&format!("\nNot written: {n}: {why}"));
            }
            failed = r.failed.len();
        }
        Loaded::Ipod(read) => {
            let r = import::import_ipod(conn, config, &read, names, on_conflict)?;
            text = describe(&r.playlists);
            let left: usize = r.left_out.iter().map(|(_, l)| l.len()).sum();
            if left > 0 {
                text.push_str(&format!(
                    "\n{left} tracks have no library file and were left out \
                     (`fiio-tunes import ipod` lists them)."
                ));
            }
            for (n, why) in &r.failed {
                text.push_str(&format!("\nNot written: {n}: {why}"));
            }
            failed = r.failed.len();
            if copy_scope {
                text.push('\n');
                text.push_str(&copy(&store, config, paths, &read, &r));
            }
        }
    }
    Ok(Done { text, failed })
}

/// Copies the iPod's iprune scope; the line that says what happened.
fn copy(
    store: &Store,
    config: &Config,
    paths: &Paths,
    read: &import::Ipod,
    report: &import::IpodReport,
) -> String {
    let attempt = || -> Result<String> {
        let ip = iprune_paths()?;
        let ic = match ip.config_file.exists() {
            true => IpruneConfig::load(&ip.config_file)?,
            false => IpruneConfig::default(),
        };
        match import::iprune_scope(&ic, read, report)? {
            import::ScopeCopy::Ready { scope, playlists } => {
                let what = match scope {
                    Scope::All => "the whole library".to_owned(),
                    Scope::Playlists => format!("only {}", playlists.join(", ")),
                    Scope::Except => format!("all except {}", playlists.join(", ")),
                };
                // The file as it is now: the window's copy may be older than
                // an edit made since, which saving it would undo.
                let mut cfg = match paths.config_file.exists() {
                    true => Config::load(&paths.config_file)?,
                    false => config.clone(),
                };
                Ok(
                    match import::set_scope(
                        store.conn(),
                        &mut cfg,
                        &paths.config_file,
                        scope,
                        playlists,
                    )? {
                        import::ScopeTarget::Device(key) => {
                            format!("The sync scope of {key} is now iprune's: {what}.")
                        }
                        import::ScopeTarget::Config => {
                            format!("The default sync scope is now iprune's: {what}.")
                        }
                    },
                )
            }
            import::ScopeCopy::Missing(m) => Ok(format!(
                "The scope was not copied: it names playlists that were not imported: {}.",
                m.join(", ")
            )),
        }
    };
    attempt().unwrap_or_else(|e| format!("The scope was not copied: {e:#}."))
}

#[cfg(test)]
mod tests;
