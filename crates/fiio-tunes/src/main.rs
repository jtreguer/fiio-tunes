use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use fiio_core::config::{Config, PathStyle, Paths};
use fiio_core::import::{self, OnConflict};
use fiio_core::iprune::{IpruneConfig, IprunePaths};
use fiio_core::store::Store;
use fiio_core::{playlists, scan};

/// Exit code when what was written to the card reads back wrong.
const EXIT_VERIFY: u8 = 3;
/// Exit code when the user answers no to a confirmation.
const EXIT_DECLINED: u8 = 4;

#[derive(Parser)]
#[command(
    version,
    about = "Playlist manager and sync tool for the FiiO X3 2nd gen"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write the config file.
    Init {
        /// Music folder to index. Repeat for several.
        #[arg(long = "library", required = true, value_name = "DIR")]
        library: Vec<PathBuf>,
        /// Folder holding the .m3u8 playlists.
        #[arg(long, value_name = "DIR")]
        playlists: PathBuf,
        /// How tracks are written in playlist files (default absolute; with
        /// --force, the current setting when not given).
        #[arg(long, value_enum)]
        path_style: Option<Style>,
        /// Change an existing config file: the library folders and playlists
        /// folder given replace the old ones, every other setting is kept.
        #[arg(long)]
        force: bool,
    },
    /// Index the library and load the playlists.
    Scan,
    /// List and edit playlists.
    #[command(subcommand)]
    Playlist(PlaylistCommand),
    /// Copy playlists in from iprune's folder, an iPod or an iTunesDB file.
    #[command(subcommand)]
    Import(ImportCommand),
    /// The X3 and its card.
    #[command(subcommand)]
    Device(DeviceCommand),
    /// Match the files and playlists already on the card to the library.
    /// Writes nothing to the card.
    Adopt {
        /// Take the default of every proposal instead of saving nothing
        /// while some are pending.
        #[arg(long)]
        accept_proposals: bool,
        /// Replace the mappings of an earlier adoption.
        #[arg(long)]
        force: bool,
    },
    /// Sync the card with the library: show the plan, ask, apply it and
    /// check the card.
    Sync {
        /// Show the plan and save it to plan.json; change nothing.
        #[arg(long)]
        plan_only: bool,
        /// Leave the tracks as they are and write the playlists for the
        /// files on the card now.
        #[arg(long)]
        playlists_only: bool,
        /// List every change.
        #[arg(long)]
        verbose: bool,
        /// Print the plan as JSON.
        #[arg(long)]
        json: bool,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
        /// Apply a plan that removes more than a tenth of the managed tracks.
        #[arg(long)]
        allow_mass_removal: bool,
    },
    /// Show or set what the X3 syncs: the whole library, only some
    /// playlists, or all but some. One never set follows [sync] in the
    /// config.
    Scope {
        #[command(subcommand)]
        set: Option<ScopeCommand>,
        /// The player, by its key (`usb:<serial>`), instead of the connected
        /// one.
        #[arg(long, value_name = "KEY", global = true)]
        device: Option<String>,
    },
    /// List the backups of the card's playlists and fiio-tunes' record of it.
    Backups {
        /// The player, by its key, instead of the connected one.
        #[arg(long, value_name = "KEY")]
        device: Option<String>,
    },
    /// Put a backup back: the card's playlists and manifest, and the record
    /// of the card. Audio files are left as they are.
    Restore {
        stamp: String,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Find what a sync cut short or another program left on the card or in
    /// the index; with --fix, repair it.
    Doctor {
        /// Repair what is found, after asking.
        #[arg(long)]
        fix: bool,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Unmount the X3's card and power the player off.
    Eject,
    /// Write test playlists to the card, to see which forms the X3 plays.
    Probe {
        /// Remove the probe's files from the card instead.
        #[arg(long)]
        clean: bool,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum ScopeCommand {
    /// Sync the whole library.
    All,
    /// Sync only these playlists and their tracks.
    Only {
        #[arg(required = true)]
        playlists: Vec<String>,
    },
    /// Sync the whole library but these playlists and their tracks.
    Except {
        #[arg(required = true)]
        playlists: Vec<String>,
    },
    /// Follow [sync] in the config again.
    Default,
}

#[derive(Subcommand)]
enum DeviceCommand {
    /// Show the X3 found, its card and what is on it. Writes nothing.
    Info,
}

#[derive(Subcommand)]
enum ImportCommand {
    /// From iprune's playlists folder.
    Iprune {
        /// Playlists to import; all when none is given.
        names: Vec<String>,
        /// When fiio-tunes already has a playlist of the name.
        #[arg(long, value_enum, default_value = "skip")]
        on_conflict: Conflict,
        /// List the playlists that can be imported; write nothing.
        #[arg(long)]
        list: bool,
    },
    /// From a mounted iPod, or a copy of its database.
    Ipod {
        /// Playlists to import; all when none is given.
        names: Vec<String>,
        /// Read this iTunesDB file instead of a mounted iPod.
        #[arg(long, value_name = "ITUNESDB", conflicts_with = "mountpoint")]
        file: Option<PathBuf>,
        /// The iPod mounted here, instead of finding it.
        #[arg(long, value_name = "DIR")]
        mountpoint: Option<PathBuf>,
        /// When fiio-tunes already has a playlist of the name.
        #[arg(long, value_enum, default_value = "skip")]
        on_conflict: Conflict,
        /// Make the iPod's iprune sync scope the default one ([sync] in the
        /// config), when every playlist it names was imported.
        #[arg(long, conflicts_with = "list")]
        scope: bool,
        /// List the iPod's playlists and how many of their tracks have a
        /// library file; write nothing.
        #[arg(long)]
        list: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Conflict {
    /// Keep the existing playlist, import nothing.
    Skip,
    /// Write the imported playlist over the existing one.
    Replace,
    /// Import as "<name> (iPod)".
    Rename,
}

impl From<Conflict> for OnConflict {
    fn from(c: Conflict) -> Self {
        match c {
            Conflict::Skip => OnConflict::Skip,
            Conflict::Replace => OnConflict::Replace,
            Conflict::Rename => OnConflict::Rename,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Style {
    Absolute,
    Relative,
}

#[derive(Subcommand)]
enum PlaylistCommand {
    /// List playlists with their number of entries.
    List,
    /// Print a playlist's entries with their positions.
    Show { name: String },
    /// Create an empty playlist.
    New { name: String },
    /// Delete a playlist file.
    Rm {
        name: String,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Rename a playlist.
    Rename { old: String, new: String },
    /// Add library files to a playlist.
    Add {
        name: String,
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Insert at this 1-based position instead of at the end.
        #[arg(long)]
        at: Option<usize>,
    },
    /// Remove entries by 1-based position.
    Remove {
        name: String,
        #[arg(required = true)]
        positions: Vec<usize>,
    },
    /// Move the entry at FROM to position TO (both 1-based).
    Move {
        name: String,
        from: usize,
        to: usize,
    },
}

/// Whether a command ran to the end or the user declined.
enum Outcome {
    Done,
    Declined,
}

fn paths() -> Result<Paths> {
    Paths::from_env().ok_or_else(|| anyhow!("cannot determine the home directory"))
}

fn load() -> Result<(Config, Store)> {
    let paths = paths()?;
    if !paths.config_file.exists() {
        bail!(
            "no config at {}; run `fiio-tunes init` first",
            paths.config_file.display()
        );
    }
    let config = Config::load(&paths.config_file)?;
    let store = Store::open(&paths.database())?;
    Ok((config, store))
}

fn absolute_dir(p: &Path) -> Result<PathBuf> {
    let abs = std::path::absolute(p).with_context(|| format!("resolving {}", p.display()))?;
    if !abs.is_dir() {
        bail!("{} is not a folder", abs.display());
    }
    Ok(abs)
}

fn init(
    library: &[PathBuf],
    playlists_dir: &Path,
    style: Option<Style>,
    force: bool,
) -> Result<()> {
    let paths = paths()?;
    let exists = paths.config_file.exists();
    if exists && !force {
        bail!(
            "{} already exists; use --force to change its folders",
            paths.config_file.display()
        );
    }
    // With --force, only what is given changes.
    // One that no longer validates is mended here, so it is read unchecked;
    // one that cannot be read at all is left for the user to fix or remove.
    let mut config = match exists {
        true => Config::load_unchecked(&paths.config_file).with_context(|| {
            format!(
                "the current config cannot be read; fix it, or remove {} to start over",
                paths.config_file.display()
            )
        })?,
        false => Config::default(),
    };
    config.library.roots = library
        .iter()
        .map(|p| absolute_dir(p))
        .collect::<Result<_>>()?;
    config.playlists.dir = absolute_dir(playlists_dir)?;
    match style {
        Some(Style::Absolute) => config.playlists.path_style = PathStyle::Absolute,
        Some(Style::Relative) => config.playlists.path_style = PathStyle::Relative,
        None => {}
    }
    config.validate(&paths.config_file)?;
    config.save(&paths.config_file)?;
    println!("wrote {}", paths.config_file.display());
    // Tracks a new set of folders leaves out are marked missing by the next
    // scan, and the sync after it removes them from the card.
    let db = paths.database();
    if db.is_file() {
        let store = fiio_core::store::Store::open(&db)?;
        let outside = fiio_core::store::all_tracks(store.conn())?
            .iter()
            .filter(|t| t.missing_since.is_none())
            .filter(|t| !config.library.roots.iter().any(|r| under(&t.path, r)))
            .count();
        if outside > 0 {
            eprintln!(
                "warning: {outside} indexed tracks are outside the library folders now; the next scan \
                 marks them missing, and the sync after it removes them from the card"
            );
        }
    }
    Ok(())
}

/// Whether `path` is inside `root`, compared in NFC as the index looks paths up.
fn under(path: &Path, root: &Path) -> bool {
    use fiio_core::paths::key;
    match (key(path), key(root)) {
        (Some(p), Some(r)) => Path::new(&p).starts_with(&r),
        _ => path.starts_with(root),
    }
}

fn print_list<T>(
    out: &mut impl Write,
    title: &str,
    items: &[T],
    line: impl Fn(&T) -> String,
) -> io::Result<()> {
    if !items.is_empty() {
        writeln!(out, "{title}: {}", items.len())?;
        for i in items {
            writeln!(out, "  {}", line(i))?;
        }
    }
    Ok(())
}

fn scan_with_progress(config: &Config, store: &mut Store) -> Result<scan::ScanReport> {
    let tty = io::stderr().is_terminal();
    let progress = |p: scan::Progress| {
        if tty {
            match p {
                scan::Progress::Walking { files } => eprint!("\rfound {files} files"),
                scan::Progress::Reading { done, total } => {
                    eprint!("\rreading {done}/{total}      ")
                }
            }
        }
    };
    let r = scan::scan(store, config, &progress)?;
    if tty {
        eprintln!();
    }
    Ok(r)
}

fn run_scan() -> Result<()> {
    let (config, mut store) = load()?;
    let r = scan_with_progress(&config, &mut store)?;
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "library: {} unchanged, {} added, {} updated, {} back from missing, {} moved, {} missing",
        r.unchanged,
        r.added,
        r.updated,
        r.returned,
        r.moved.len(),
        r.missing.len()
    )?;
    print_list(&mut out, "moved", &r.moved, |(a, b)| {
        format!("{} -> {}", a.display(), b.display())
    })?;
    print_list(&mut out, "missing", &r.missing, |p| p.display().to_string())?;
    print_list(&mut out, "unreadable", &r.unreadable, |(p, why)| {
        format!("{}: {why}", p.display())
    })?;
    print_list(
        &mut out,
        "players inside the library folders, not indexed",
        &r.devices,
        |p| p.display().to_string(),
    )?;
    print_list(&mut out, "unsupported", &r.unsupported, |(p, why)| {
        format!("{}: {why}", p.display())
    })?;
    print_list(
        &mut out,
        "indexed without tags (tags could not be parsed)",
        &r.untagged,
        |(p, why)| format!("{}: {why}", p.display()),
    )?;
    print_list(
        &mut out,
        "FairPlay protected, indexed as AAC",
        &r.drm,
        |p| p.display().to_string(),
    )?;
    print_list(&mut out, "names not in UTF-8", &r.non_utf8, |p| {
        p.display().to_string()
    })?;
    print_list(
        &mut out,
        "playlists rewritten for moved files",
        &r.rewritten_playlists,
        |n| n.clone(),
    )?;
    let pl = &r.playlists;
    writeln!(
        out,
        "playlists: {} loaded, {} unresolved entries",
        pl.loaded,
        pl.unresolved.len()
    )?;
    print_list(&mut out, "unresolved", &pl.unresolved, |(n, e)| {
        format!("{n}: {e}")
    })?;
    print_list(
        &mut out,
        "playlists removed from the index",
        &pl.removed,
        |n| n.clone(),
    )?;
    print_list(
        &mut out,
        "playlist files skipped",
        &pl.skipped,
        |(p, why)| format!("{}: {why}", p.display()),
    )?;
    Ok(())
}

fn confirm(question: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("{question} needs confirmation; pass --yes");
    }
    eprint!("{question} [y/N] ");
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

fn playlist(cmd: &PlaylistCommand) -> Result<Outcome> {
    let (config, store) = load()?;
    let conn = store.conn();
    let mut out = io::stdout().lock();
    match cmd {
        PlaylistCommand::List => {
            playlists::sync_from_disk(conn, &config, false)?;
            for p in fiio_core::store::playlist_summaries(conn)? {
                let unresolved = if p.unresolved > 0 {
                    format!("  ({} unresolved)", p.unresolved)
                } else {
                    String::new()
                };
                writeln!(out, "{:>5}  {}{unresolved}", p.entries, p.name)?;
            }
        }
        PlaylistCommand::Show { name } => {
            for (i, item) in playlists::show(conn, &config, name)?.iter().enumerate() {
                let what = match &item.track {
                    Some(t) => format!(
                        "{} - {}",
                        t.tags.artist.as_deref().unwrap_or("?"),
                        t.display_title()
                    ),
                    None => format!("[unresolved] {}", item.entry.location),
                };
                writeln!(out, "{:>4}  {what}", i + 1)?;
            }
        }
        PlaylistCommand::New { name } => playlists::create(conn, &config, name)?,
        PlaylistCommand::Rm { name, yes } => {
            // Check it exists before asking.
            playlists::show(conn, &config, name)?;
            if !yes && !confirm(&format!("delete playlist {name:?}?"))? {
                return Ok(Outcome::Declined);
            }
            playlists::delete(conn, &config, name)?;
        }
        PlaylistCommand::Rename { old, new } => playlists::rename(conn, &config, old, new)?,
        PlaylistCommand::Add { name, files, at } => {
            playlists::add(conn, &config, name, files, *at)?
        }
        PlaylistCommand::Remove { name, positions } => {
            playlists::remove(conn, &config, name, positions)?
        }
        PlaylistCommand::Move { name, from, to } => {
            playlists::move_entry(conn, &config, name, *from, *to)?
        }
    }
    Ok(Outcome::Done)
}

fn iprune_paths() -> Result<IprunePaths> {
    IprunePaths::from_env().ok_or_else(|| anyhow!("cannot determine the home directory"))
}

/// Whether fiio-tunes already has a playlist of this name.
fn taken(config: &Config, name: &str) -> bool {
    playlists::file_for(config, name).is_ok_and(|f| f.exists())
}

/// Exits 1, after the report, when a playlist could not be written.
fn check_failed(failed: &[(String, String)]) -> Result<()> {
    if !failed.is_empty() {
        let names: Vec<&str> = failed.iter().map(|(n, _)| n.as_str()).collect();
        bail!(
            "{} playlists not imported: {}",
            failed.len(),
            names.join(", ")
        );
    }
    Ok(())
}

fn print_imported(out: &mut impl Write, done: &[import::Imported]) -> io::Result<()> {
    let count = |o: import::Outcome| done.iter().filter(|i| i.outcome == o).count();
    writeln!(
        out,
        "imported: {} new, {} replaced, {} renamed, {} skipped",
        count(import::Outcome::Created),
        count(import::Outcome::Replaced),
        count(import::Outcome::Renamed),
        count(import::Outcome::Skipped)
    )?;
    for i in done {
        let n = i.entries;
        match i.outcome {
            import::Outcome::Created => writeln!(out, "  + {} ({n} entries)", i.name)?,
            import::Outcome::Replaced => writeln!(out, "  ~ {} ({n} entries, replaced)", i.name)?,
            import::Outcome::Renamed => {
                writeln!(out, "  + {} ({n} entries, from {:?})", i.name, i.source)?
            }
            import::Outcome::Skipped => writeln!(
                out,
                "  = {:?} skipped: {:?} exists (--on-conflict replace or rename)",
                i.source, i.name
            )?,
        }
    }
    Ok(())
}

fn import_iprune(names: &[String], on_conflict: Conflict, list: bool) -> Result<()> {
    let (config, store) = load()?;
    let conn = store.conn();
    let ip = iprune_paths()?;
    let src = import::iprune_folder(&config, &ip.config_file)?;
    let mut out = io::stdout().lock();
    print_list(
        &mut out,
        "unreadable in iprune's folder",
        &src.problems,
        |(p, why)| format!("{}: {why}", p.display()),
    )?;
    if list {
        writeln!(out, "iprune's playlists in {}:", src.dir.display())?;
        for (name, file) in &src.playlists {
            let entries = fiio_core::playlist::read(file)
                .map_or_else(|e| format!("({e})"), |e| e.len().to_string());
            let mark = if taken(&config, name) {
                "  (name taken)"
            } else {
                ""
            };
            writeln!(out, "{entries:>5}  {name}{mark}")?;
        }
        return Ok(());
    }
    let r = import::import_iprune(conn, &config, &src, names, on_conflict.into())?;
    print_imported(&mut out, &r.playlists)?;
    print_list(
        &mut out,
        "entries with no library file, kept as they were",
        &r.unresolved,
        |(n, e)| format!("{n}: {e}"),
    )?;
    print_list(&mut out, "playlists not read", &r.unreadable, |(p, why)| {
        format!("{}: {why}", p.display())
    })?;
    print_list(&mut out, "playlists not written", &r.failed, |(n, why)| {
        format!("{n}: {why}")
    })?;
    check_failed(&r.failed)
}

fn how(h: Option<import::How>) -> String {
    use fiio_core::adopt::Reason;
    use import::How;
    match h {
        None => "no library file".into(),
        Some(How::Iprune) => "through iprune's index".into(),
        Some(How::Identical) => "identical file".into(),
        Some(How::Tags) => "same size, duration, title and artist".into(),
        Some(How::Proposal(r)) => match r {
            Reason::WeakTags => "same size and duration, title or artist differ",
            Reason::SeveralCandidates => "one of several identical library files",
            Reason::SharedWithAnotherDeviceTrack => "duplicate on the iPod",
            Reason::TagsOnly => "same artist, title and album, another size",
        }
        .into(),
    }
}

struct IpodArgs<'a> {
    names: &'a [String],
    file: Option<&'a Path>,
    mountpoint: Option<&'a Path>,
    on_conflict: Conflict,
    scope: bool,
    list: bool,
}

fn import_ipod(a: IpodArgs) -> Result<()> {
    let (mut config, store) = load()?;
    let conn = store.conn();
    let ip = iprune_paths()?;
    let source = match (a.file, a.mountpoint) {
        (Some(f), _) => import::IpodSource::File(f.to_owned()),
        (None, Some(m)) => import::IpodSource::Mounted(m.to_owned()),
        (None, None) => import::IpodSource::Mounted(fiio_core::ipod::find_ipod()?),
    };
    let db = import::open(&source)?;
    let tty = io::stderr().is_terminal();
    let progress = |done: usize, total: usize| {
        if tty {
            eprint!("\rcomparing iPod files with the library {done}/{total}  ");
        }
    };
    let ipod = import::read_ipod(conn, &db, Some(&ip.database), &progress)?;
    if tty && ipod.compared + ipod.unreadable.len() > 0 {
        eprintln!();
    }
    let mut out = io::stdout().lock();
    let c = &ipod.contents;
    writeln!(
        out,
        "iPod: {} (GUID {}), {} tracks, {} playlists",
        c.name.as_deref().unwrap_or("?"),
        c.firewire_guid.as_deref().unwrap_or("unknown"),
        c.tracks.len(),
        c.playlists.len()
    )?;
    match (&ipod.iprune_device, &ipod.iprune_error) {
        (_, Some(e)) => writeln!(
            out,
            "iprune's index could not be read ({e}); traced by tags only"
        )?,
        (Some(d), None) => writeln!(
            out,
            "iprune knows this iPod as {} ({})",
            d.name.as_deref().unwrap_or("?"),
            d.firewire_guid
        )?,
        (None, None) => writeln!(
            out,
            "iprune has no record of this iPod; traced by tags only"
        )?,
    }
    writeln!(out, "tracks in playlists:")?;
    for (h, n) in ipod.counts() {
        writeln!(out, "  {n}: {}", how(h))?;
    }
    if ipod.compared + ipod.unreadable.len() > 0 {
        writeln!(
            out,
            "iPod files compared with the library: {} ({} unreadable)",
            ipod.compared,
            ipod.unreadable.len()
        )?;
    }
    let describe = |dbid: u64| match ipod.track(dbid) {
        Some(t) => format!(
            "{} - {} [{}] {}",
            t.adopt.artist.as_deref().unwrap_or("?"),
            t.adopt.title.as_deref().unwrap_or("?"),
            t.adopt.album.as_deref().unwrap_or("?"),
            t.ipod_path.as_deref().unwrap_or("?")
        ),
        None => format!("{dbid:016x}"),
    };
    print_list(
        &mut out,
        "iPod files not read",
        &ipod.unreadable,
        |(d, why)| format!("{}: {why}", describe(*d)),
    )?;
    if a.list {
        writeln!(out, "playlists (tracks with a library file / entries):")?;
        for pl in &c.playlists {
            let t = ipod.traced_playlist(pl);
            let name = import::playlist_name(&pl.name);
            let smart = if pl.smart { "  (smart)" } else { "" };
            let mark = if taken(&config, &name) {
                "  (name taken)"
            } else {
                ""
            };
            writeln!(
                out,
                "{:>5}/{:<5}  {}{smart}{mark}",
                t.tracks.len(),
                pl.members.len(),
                pl.name
            )?;
        }
        return Ok(());
    }
    let r = import::import_ipod(conn, &config, &ipod, a.names, a.on_conflict.into())?;
    print_imported(&mut out, &r.playlists)?;
    for (name, left) in &r.left_out {
        print_list(
            &mut out,
            &format!("{name}: left out, no library file"),
            left,
            |d| describe(*d),
        )?;
    }
    print_list(&mut out, "playlists not written", &r.failed, |(n, why)| {
        format!("{n}: {why}")
    })?;
    if a.scope {
        let ic = match ip.config_file.exists() {
            true => IpruneConfig::load(&ip.config_file)?,
            false => IpruneConfig::default(),
        };
        match import::iprune_scope(&ic, &ipod, &r)? {
            import::ScopeCopy::Ready { scope, playlists } => {
                let paths = paths()?;
                let what = match scope {
                    fiio_core::config::Scope::All => "the whole library".to_owned(),
                    fiio_core::config::Scope::Playlists => format!("only {}", playlists.join(", ")),
                    fiio_core::config::Scope::Except => {
                        format!("all except {}", playlists.join(", "))
                    }
                };
                match import::set_scope(conn, &mut config, &paths.config_file, scope, playlists)? {
                    import::ScopeTarget::Device(key) => {
                        writeln!(out, "sync scope of {key} set to iprune's: {what}")?
                    }
                    import::ScopeTarget::Config => {
                        writeln!(out, "default sync scope set to iprune's: {what}")?
                    }
                }
            }
            import::ScopeCopy::Missing(missing) => {
                // A playlist that failed to write is the first thing to fix.
                check_failed(&r.failed)?;
                bail!(
                    "scope not copied: iprune's scope names playlists that were not imported: {}",
                    missing.join(", ")
                )
            }
        }
    }
    check_failed(&r.failed)
}

fn size_text(bytes: u64) -> String {
    match bytes {
        b if b < 1_000_000_000 => format!("{:.1} MB", b as f64 / 1e6),
        b => format!("{:.1} GB", b as f64 / 1e9),
    }
}

fn device_info() -> Result<()> {
    let (config, _store) = load()?;
    let found = fiio_core::device::locate(&config)?;
    let mp = &found.mount.mountpoint;
    let mut out = io::stdout().lock();
    match &found.usb {
        Some(u) => writeln!(
            out,
            "{} ({}, USB {}:{}, serial {})",
            u.name.as_deref().unwrap_or("USB device"),
            u.manufacturer.as_deref().unwrap_or("?"),
            u.vendor,
            u.product,
            u.serial.as_deref().unwrap_or("none")
        )?,
        None => writeln!(out, "not a USB device")?,
    }
    writeln!(
        out,
        "key: {}",
        found
            .key()
            .unwrap_or_else(|| "none (not an X3; a folder is named by its manifest)".into())
    )?;
    match &found.source {
        Some(src) => writeln!(
            out,
            "mounted at {} from {src}, {}, {}",
            mp.display(),
            found.mount.fstype,
            if found.mount.read_only {
                "read-only"
            } else {
                "read-write"
            }
        )?,
        None => writeln!(out, "folder {}, not a mount", mp.display())?,
    }
    let (size, free) = fiio_core::device::space(mp)?;
    writeln!(
        out,
        "space: {} free of {}",
        size_text(free),
        size_text(size)
    )?;
    match fiio_core::device::check_writable(&found, config.device.allow_folder) {
        Ok(()) => writeln!(out, "writes: allowed")?,
        Err(e) => writeln!(out, "writes: refused ({e})")?,
    }
    let card = fiio_core::card::read(mp)?;
    writeln!(
        out,
        "manifest: {}",
        if card.manifest {
            "present"
        } else {
            "none (the card has not been synced)"
        }
    )?;
    // FAT compares names without case.
    let music = fiio_core::paths::card_key(&format!("{}/", config.card.music_dir));
    let under = card
        .audio
        .iter()
        .filter(|f| fiio_core::paths::card_key(&f.path).starts_with(&music))
        .count();
    writeln!(
        out,
        "on the card: {} audio files ({}), {} in {}, {} playlists",
        card.audio.len(),
        size_text(card.audio_bytes()),
        under,
        config.card.music_dir,
        card.playlists.len()
    )?;
    print_list(&mut out, "playlists", &card.playlists, |f| f.path.clone())?;
    print_list(&mut out, "not read", &card.problems, |(p, why)| {
        format!("{p}: {why}")
    })?;
    Ok(())
}

fn probe(clean: bool, yes: bool) -> Result<Outcome> {
    use fiio_core::probe;
    let (config, store) = load()?;
    let paths = paths()?;
    let found = fiio_core::device::locate(&config)?;
    fiio_core::device::check_writable(&found, config.device.allow_folder)?;
    let card = found.mount.mountpoint.clone();
    let record = paths.data_dir.join("probe.toml");
    let _lock = fiio_core::lock::card(&paths)?;
    let mut out = io::stdout().lock();
    if clean {
        if !yes && !confirm(&format!("remove the probe files from {}?", card.display()))? {
            return Ok(Outcome::Declined);
        }
        let c = probe::clean(&card, &record)?;
        writeln!(
            out,
            "removed {} files and {} folders",
            c.files,
            c.dirs.len()
        )?;
        print_list(&mut out, "folders kept, not empty", &c.kept, |d| d.clone())?;
        return Ok(Outcome::Done);
    }
    probe::check(&card, &record)?;
    let src = probe::source(store.conn())?;
    let secs = src
        .info
        .duration_ms
        .map_or(-1, |d| i64::from(d.saturating_add(500) / 1000));
    let plan = probe::plan(secs);
    writeln!(
        out,
        "The probe writes to {}:\n  {} copies of {} ({}) in Music/zz-probe/\n  {} playlists:",
        card.display(),
        plan.audio.len(),
        src.path.display(),
        size_text(src.size * plan.audio.len() as u64),
        plan.playlists.len()
    )?;
    for p in &plan.playlists {
        writeln!(out, "    {}  {}", p.path, p.what)?;
    }
    out.flush()?;
    if !yes && !confirm("write the probe to the card?")? {
        return Ok(Outcome::Declined);
    }
    let w = probe::write(&card, &src.path, secs, &record)?;
    writeln!(
        out,
        "wrote {} files ({}), each read back and checked",
        w.files,
        size_text(w.bytes)
    )?;
    writeln!(
        out,
        "the card holds \"04 é.mp3\" and \"04 É.mp3\" as {}",
        if w.case_pair_kept {
            "two files"
        } else {
            "one file"
        }
    )?;
    writeln!(
        out,
        "On the X3: Browse Files, open each zz-probe playlist and note which play. \
         Then `fiio-tunes probe --clean`."
    )?;
    Ok(Outcome::Done)
}

fn adopt(accept_proposals: bool, force: bool) -> Result<()> {
    use fiio_core::adoption;
    let (config, store) = load()?;
    let paths = paths()?;
    let conn = store.conn();
    playlists::sync_from_disk(conn, &config, false)?;
    let found = fiio_core::device::locate(&config)?;
    let card = found.mount.mountpoint.clone();
    // Held while the card is read, so a sync cannot change it meanwhile.
    let _lock = fiio_core::lock::card(&paths)?;
    let manifest = fiio_core::manifest::identity(&card)?;
    let key = adoption::device_key(&found, manifest.as_ref(), config.device.allow_folder)?;
    eprintln!("reading the card at {}", card.display());
    let tty = io::stderr().is_terminal();
    let progress = |done: usize, total: usize| {
        if tty {
            eprint!("\rreading card files {done}/{total}      ");
            if done == total {
                eprintln!();
            }
        }
    };
    let run = adoption::run(conn, &config, &card, &key, accept_proposals, &progress)?;
    let report_path = paths.data_dir.join("adopt-report.txt");
    std::fs::create_dir_all(&paths.data_dir)?;
    std::fs::write(&report_path, adoption::report(&run))
        .with_context(|| format!("writing {}", report_path.display()))?;
    println!("{}", adoption::summary(&run));
    println!("full report: {}", report_path.display());
    if !accept_proposals && !run.adoption.proposals.is_empty() {
        println!(
            "{} proposals need a decision: review the report, then run again with \
             --accept-proposals. Nothing saved",
            run.adoption.proposals.len()
        );
        return Ok(());
    }
    let usb = found.usb.as_ref();
    let name = usb.and_then(|u| u.name.clone());
    let model = usb.map(|u| {
        format!(
            "{} {}",
            u.manufacturer.as_deref().unwrap_or("FiiO"),
            u.name.as_deref().unwrap_or("X3")
        )
    });
    adoption::save(conn, &run, name.as_deref(), model.as_deref(), force)?;
    println!(
        "saved: {} tracks and {} playlists mapped to {key}; nothing on the card changed",
        run.mapping.len(),
        run.playlists.matched.len()
    );
    Ok(())
}

struct SyncArgs {
    plan_only: bool,
    playlists_only: bool,
    verbose: bool,
    json: bool,
    yes: bool,
    allow_mass_removal: bool,
}

fn run_sync(args: SyncArgs) -> Result<Outcome> {
    let (config, mut store) = load()?;
    let paths = paths()?;
    let r = scan_with_progress(&config, &mut store)?;
    eprintln!(
        "library: {} unchanged, {} added, {} updated, {} moved, {} missing",
        r.unchanged,
        r.added,
        r.updated + r.returned,
        r.moved.len(),
        r.missing.len()
    );
    let found = fiio_core::device::locate(&config)?;
    // Held while the card is read, so another fiio-tunes cannot change it
    // meanwhile.
    let _lock = fiio_core::lock::card(&paths)?;
    eprintln!("reading the card at {}", found.mount.mountpoint.display());
    let prepared =
        fiio_core::sync::prepare_with(store.conn(), &config, &found, args.playlists_only)?;
    if prepared.taken_back > 0 {
        eprintln!(
            "{} files an interrupted sync left in place were taken back",
            prepared.taken_back
        );
    }
    if !prepared.contents.parts.is_empty() {
        eprintln!(
            "{} unfinished copies are on the card; `fiio-tunes doctor --fix` removes them",
            prepared.contents.parts.len()
        );
    }
    let plan_json = serde_json::to_string_pretty(&prepared.plan)?;
    std::fs::create_dir_all(&paths.data_dir)?;
    let plan_path = paths.data_dir.join("plan.json");
    std::fs::write(&plan_path, &plan_json)
        .with_context(|| format!("writing {}", plan_path.display()))?;
    let mut out = io::stdout().lock();
    if args.json {
        writeln!(out, "{plan_json}")?;
    } else {
        write!(
            out,
            "{}",
            fiio_core::sync::describe(&prepared, args.verbose)
        )?;
        writeln!(
            out,
            "syncs {}{}",
            prepared.scope.0.describe(),
            if prepared.scope.1 {
                ""
            } else {
                " (from [sync] in the config)"
            }
        )?;
        if args.plan_only {
            writeln!(
                out,
                "plan saved to {}; nothing changed",
                plan_path.display()
            )?;
        }
    }
    if args.plan_only {
        return Ok(Outcome::Done);
    }
    let opts = fiio_core::exec::Options {
        allow_mass_removal: args.allow_mass_removal,
    };
    // Refused before asking.
    fiio_core::exec::check(&prepared, opts)?;
    if prepared.plan.is_empty() {
        writeln!(out, "the card is up to date; nothing to do")?;
        return Ok(Outcome::Done);
    }
    out.flush()?;
    drop(out);
    if !args.yes && !confirm("Apply?")? {
        return Ok(Outcome::Declined);
    }
    let _inhibitor = applying("Writing to the X3's card");
    let tty = io::stderr().is_terminal();
    let progress = |p: fiio_core::exec::Progress| {
        use fiio_core::exec::Progress;
        if !tty {
            return;
        }
        // Errors ignored: once the terminal is gone (SIGHUP), writing to it
        // fails, and must not stop the sync.
        let mut err = io::stderr();
        let _ = match p {
            Progress::Removing { done, total } => {
                write!(err, "\r\x1b[Kremoving {}/{total}", done + 1)
            }
            Progress::Moving { done, total } => {
                write!(err, "\r\x1b[Kmoving {}/{total}", done + 1)
            }
            Progress::Copying {
                done,
                total,
                bytes,
                bytes_total,
                name,
            } => write!(
                err,
                "\r\x1b[Kcopying {}/{total} ({} of {}) {name}",
                done + 1,
                fiio_core::sync::bytes(bytes),
                fiio_core::sync::bytes(bytes_total)
            ),
            Progress::Playlists => write!(err, "\r\x1b[Kwriting the playlists…"),
            Progress::Verifying => write!(err, "\r\x1b[Kreading the card again to check it…"),
        };
    };
    let result = fiio_core::exec::run(
        store.conn(),
        &config,
        &paths,
        &found,
        &prepared,
        opts,
        &progress,
        &CANCEL,
    );
    if tty {
        let _ = writeln!(io::stderr());
    }
    let result = result.map_err(|failed| {
        let hint = match &failed.backup {
            Some(b) => format!(
                "the card was backed up as {} before writing; run `fiio-tunes restore {}` \
                 before syncing again: it puts its playlists and record back",
                b.stamp, b.stamp
            ),
            None => "nothing was written to the card".into(),
        };
        anyhow::Error::new(failed.error).context(hint)
    });
    fiio_core::notify::send(&match &result {
        Ok(done) => fiio_core::notify::synced(done),
        Err(e) => fiio_core::notify::failed(&format!("{e:#}")),
    });
    match result {
        Ok(done) => {
            // Errors ignored: the card is written; a terminal gone must not
            // turn that into a failure.
            let mut out = io::stdout().lock();
            if done.first {
                let _ = writeln!(out, "the card was checked as adopted and got its id");
            }
            let _ = writeln!(
                out,
                "tracks: {} copied ({}), {} copied again, {} moved, {} removed",
                done.copied,
                fiio_core::sync::bytes(done.bytes),
                done.recopied,
                done.moved,
                done.removed
            );
            match done.stopped {
                None => {
                    let _ = writeln!(
                        out,
                        "playlists: {} written, {} renamed, {} removed; the card checked as planned",
                        done.written, done.renamed, done.deleted
                    );
                }
                Some(_) => {
                    let _ = writeln!(out, "playlists: left for the next sync");
                }
            }
            for (path, why) in &done.skipped {
                let _ = writeln!(out, "not copied, {why}: {}", path.display());
            }
            if let Some(b) = &done.backup {
                let _ = writeln!(out, "backup: {}", b.stamp);
            }
            for w in &done.warnings {
                let _ = writeln!(out, "warning: {w}");
            }
            if let Some(why) = done.stopped {
                bail!(
                    "the sync stopped early ({why}); what was done is recorded and checked, and \
                     the next sync goes on from there"
                );
            }
            Ok(Outcome::Done)
        }
        Err(e) => Err(e),
    }
}

fn run_eject() -> Result<()> {
    let (config, _store) = load()?;
    let found = fiio_core::device::locate(&config)?;
    // Not while another fiio-tunes writes the card.
    let _lock = fiio_core::lock::card(&paths()?)?;
    match fiio_core::device::eject(&found)? {
        Some(warning) => println!("{warning}"),
        None => println!(
            "ejected {}: the X3 can be unplugged",
            found.mount.mountpoint.display()
        ),
    }
    Ok(())
}

fn run_doctor(fix: bool, yes: bool) -> Result<Outcome> {
    use fiio_core::doctor;
    let (config, mut store) = load()?;
    let paths = paths()?;
    // A repair forgets long-missing tracks: the library is scanned first,
    // so one whose file came back is not forgotten. If the scan fails (a
    // disk not mounted), nothing is forgotten this time.
    let scanned = !fix || {
        match scan_with_progress(&config, &mut store) {
            Ok(_) => true,
            Err(e) => {
                eprintln!(
                    "the library could not be scanned ({e:#}); no track is forgotten this time"
                );
                false
            }
        }
    };
    let found = fiio_core::device::locate(&config)?;
    let _lock = fiio_core::lock::card(&paths)?;
    let mut examined = doctor::examine(store.conn(), &config, &paths, &found)?;
    if !scanned {
        examined.report.forgotten.clear();
    }
    let r = &examined.report;
    let mut out = io::stdout().lock();
    let card = examined.card.display();
    let section = |out: &mut io::StdoutLock, title: &str, lines: Vec<String>| -> io::Result<()> {
        if !lines.is_empty() {
            writeln!(out, "{title} ({}):", lines.len())?;
            for l in lines {
                writeln!(out, "  {l}")?;
            }
        }
        Ok(())
    };
    section(&mut out, "unfinished copies, to delete", r.parts.clone())?;
    section(
        &mut out,
        "files left under a hidden name by a stopped sync",
        r.hops
            .iter()
            .map(|h| match &h.to {
                Some(to) => format!("{} -> {to}", h.path),
                None => format!("{} (no free place known; move it by hand)", h.path),
            })
            .collect(),
    )?;
    section(
        &mut out,
        "files a stopped sync left in place, to record",
        r.taken_back.iter().map(|m| m.card_path.clone()).collect(),
    )?;
    section(
        &mut out,
        "recorded files no longer on the card, to forget",
        r.stale.iter().map(|(_, p)| p.clone()).collect(),
    )?;
    section(
        &mut out,
        &format!(
            "files under {} fiio-tunes does not manage, to move to {}",
            config.card.music_dir,
            doctor::ORPHANS
        ),
        r.orphans.clone(),
    )?;
    section(
        &mut out,
        "library tracks missing for over 30 days, to drop from the index",
        r.forgotten
            .iter()
            .map(|(_, p)| p.display().to_string())
            .collect(),
    )?;
    if r.is_clean() {
        writeln!(out, "nothing to repair on {card}")?;
        return Ok(Outcome::Done);
    }
    if !fix {
        writeln!(out, "run `fiio-tunes doctor --fix` to repair")?;
        return Ok(Outcome::Done);
    }
    out.flush()?;
    drop(out);
    if !yes && !confirm(&format!("Repair {card}?"))? {
        return Ok(Outcome::Declined);
    }
    let _inhibitor = applying("Repairing the X3's card");
    let done = doctor::fix(store.conn(), &config, &paths, &found, &examined)?;
    let mut out = io::stdout().lock();
    let _ = writeln!(
        out,
        "removed {} unfinished copies, placed {} hidden files, recorded {}, forgot {} \
         recorded files, moved {} orphans, dropped {} library tracks",
        done.parts, done.hops, done.taken_back, done.stale, done.orphans, done.forgotten
    );
    if let Some(dir) = &done.orphans_dir {
        let _ = writeln!(
            out,
            "orphans are in {dir} on the card; delete it to free the space"
        );
    }
    if let Some(b) = &done.backup {
        let _ = writeln!(out, "backup: {}", b.stamp);
    }
    for w in &done.warnings {
        let _ = writeln!(out, "warning: {w}");
    }
    Ok(Outcome::Done)
}

fn run_backups(key: Option<&str>) -> Result<()> {
    let (config, _store) = load()?;
    let paths = paths()?;
    let found = match key {
        Some(_) => None,
        None => Some(fiio_core::device::locate(&config)?),
    };
    let key = fiio_core::restore::device_key(&config, found.as_ref(), key)?;
    let root = paths.data_dir.join("backups");
    let all = fiio_core::backup::list(&root, &key)?;
    let mut out = io::stdout().lock();
    if all.is_empty() {
        writeln!(out, "no backups of {key}")?;
    }
    for b in all {
        let what = match fiio_core::backup::saved(&b) {
            Ok(s) => format!(
                "{} tracks and {} playlists mapped",
                s.tracks.len(),
                s.playlists.len()
            ),
            Err(e) => format!("unreadable: {e}"),
        };
        writeln!(out, "{}  {what}", b.stamp)?;
    }
    Ok(())
}

fn run_restore(stamp: &str, yes: bool) -> Result<Outcome> {
    let (config, store) = load()?;
    let paths = paths()?;
    let found = fiio_core::device::locate(&config)?;
    let _lock = fiio_core::lock::card(&paths)?;
    let key = fiio_core::restore::device_key(&config, Some(&found), None)?;
    let b = fiio_core::backup::find(&paths.data_dir.join("backups"), &key, stamp)?;
    let saved = fiio_core::backup::saved(&b)?;
    let question = format!(
        "put back backup {stamp} on {}: its {} managed playlist files and manifest, and its \
         record of {} tracks and {} playlists?",
        found.mount.mountpoint.display(),
        saved.playlists.len(),
        saved.tracks.len(),
        saved.playlists.len()
    );
    if !yes && !confirm(&question)? {
        return Ok(Outcome::Declined);
    }
    let _inhibitor = applying("Restoring the X3's card");
    let r = fiio_core::restore::run(store.conn(), &config, &paths, &found, stamp).map_err(|e| {
        let restoring = fiio_core::store::devices::device(store.conn(), &key)
            .ok()
            .flatten()
            .and_then(|d| {
                fiio_core::store::devices::restoring(store.conn(), d.id)
                    .ok()
                    .flatten()
            });
        let hint = match restoring {
            Some(_) => {
                "the restore stopped part way, after the card was backed up (`fiio-tunes \
                 backups` lists it); syncs are refused until a restore succeeds"
            }
            None => "nothing was changed on the card",
        };
        anyhow::Error::new(e).context(hint)
    })?;
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "restored {stamp}: {} playlist files written, {} removed; the card was backed up first as {}",
        r.written, r.deleted, r.before.stamp
    )?;
    if r.tombstones > 0 {
        writeln!(
            out,
            "{} playlists deleted since are removed from the card by the next sync",
            r.tombstones
        )?;
    }
    if r.dropped > 0 {
        writeln!(
            out,
            "{} mapped tracks no longer in the library were dropped",
            r.dropped
        )?;
    }
    writeln!(
        out,
        "audio files removed since are copied again by the next sync"
    )?;
    for w in &r.warnings {
        writeln!(out, "warning: {w}")?;
    }
    Ok(Outcome::Done)
}

fn run_scope(set: Option<&ScopeCommand>, key: Option<&str>) -> Result<()> {
    use fiio_core::store::devices::{self, DeviceScope};
    use fiio_core::sync;
    let (config, store) = load()?;
    let conn = store.conn();
    let (key, card) = match key {
        Some(k) => (k.to_owned(), None),
        None => {
            let found = fiio_core::device::locate(&config)?;
            let manifest = fiio_core::manifest::identity(&found.mount.mountpoint)?;
            let key = fiio_core::adoption::device_key(
                &found,
                manifest.as_ref(),
                config.device.allow_folder,
            )?;
            (key, Some(found.mount.mountpoint))
        }
    };
    let device =
        devices::device(conn, &key)?.ok_or_else(|| fiio_core::Error::NotAdopted(key.clone()))?;
    playlists::sync_from_disk(conn, &config, false)?;
    let library = fiio_core::store::all_tracks(conn)?;
    let lists = devices::library_playlists(conn)?;
    let chosen = |kind, names: &[String]| {
        let mut playlists = names.to_vec();
        playlists.sort();
        playlists.dedup();
        DeviceScope { kind, playlists }
    };
    use fiio_core::config::Scope as Kind;
    let new = match set {
        None => None,
        Some(ScopeCommand::All) => Some(Some(chosen(Kind::All, &[]))),
        Some(ScopeCommand::Only { playlists }) => Some(Some(chosen(Kind::Playlists, playlists))),
        Some(ScopeCommand::Except { playlists }) => Some(Some(chosen(Kind::Except, playlists))),
        Some(ScopeCommand::Default) => Some(None),
    };
    if let Some(new) = new {
        if let Some(why) = new
            .as_ref()
            .and_then(|s| sync::scope_problem(s, true, &library, &lists))
        {
            bail!("{why}; nothing changed");
        }
        devices::set_scope(conn, device.id, new.as_ref())?;
    }
    let (scope, own) = sync::device_scope(conn, &config, device.id)?;
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "{} ({})",
        device.name.as_deref().unwrap_or("X3"),
        device.device_key
    )?;
    let from = if own {
        "set for this player"
    } else {
        "from [sync] in the config"
    };
    writeln!(out, "syncs {}, {from}", scope.describe())?;
    let (tracks, bytes) = scope.size(&library, &lists);
    let room = card
        .and_then(|c| fiio_core::device::space(&c).ok())
        .map(|(capacity, _)| format!("; the card holds {}", sync::bytes(capacity)))
        .unwrap_or_default();
    writeln!(out, "{tracks} tracks, {}{room}", sync::bytes(bytes))?;
    Ok(())
}

/// Set once a command starts changing the card or the index after its
/// confirmation: a signal then asks it to stop at the next safe point
/// instead of ending the process.
static APPLYING: AtomicBool = AtomicBool::new(false);
/// Asked to stop: copies end after the current file, and what was done is
/// recorded and checked.
static CANCEL: AtomicBool = AtomicBool::new(false);

/// Ctrl+C, SIGTERM (logout, shutdown) and SIGHUP (the terminal closed) end
/// fiio-tunes at once until it applies anything; from then on they only
/// ask it to stop, and it never exits in the middle of a write. A copy that
/// hangs is ended with `kill -9`, which leaves a `.part` file `fiio-tunes
/// doctor` removes.
fn install_signals() {
    let _ = ctrlc::set_handler(|| {
        if !APPLYING.load(Ordering::SeqCst) {
            std::process::exit(130);
        }
        let msg = if CANCEL.swap(true, Ordering::SeqCst) {
            "\nstill finishing: fiio-tunes stops once it is safe to"
        } else {
            "\nstopping as soon as it is safe to: what was done is recorded and checked first…"
        };
        let _ = writeln!(io::stderr(), "{msg}");
    });
}

/// Marks the command as applying, and keeps the machine from suspending or
/// shutting down until the returned inhibitor is dropped.
fn applying(why: &str) -> Option<fiio_core::inhibit::Inhibitor> {
    APPLYING.store(true, Ordering::SeqCst);
    let held = fiio_core::inhibit::hold(why);
    if held.is_none() {
        let _ = writeln!(
            io::stderr(),
            "could not keep the machine from sleeping (systemd-inhibit): do not suspend it until this is done"
        );
    }
    held
}

fn main() -> ExitCode {
    install_signals();
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Init {
            library,
            playlists,
            path_style,
            force,
        } => init(library, playlists, *path_style, *force).map(|()| Outcome::Done),
        Command::Scan => run_scan().map(|()| Outcome::Done),
        Command::Playlist(cmd) => playlist(cmd),
        Command::Import(ImportCommand::Iprune {
            names,
            on_conflict,
            list,
        }) => import_iprune(names, *on_conflict, *list).map(|()| Outcome::Done),
        Command::Import(ImportCommand::Ipod {
            names,
            file,
            mountpoint,
            on_conflict,
            scope,
            list,
        }) => import_ipod(IpodArgs {
            names,
            file: file.as_deref(),
            mountpoint: mountpoint.as_deref(),
            on_conflict: *on_conflict,
            scope: *scope,
            list: *list,
        })
        .map(|()| Outcome::Done),
        Command::Device(DeviceCommand::Info) => device_info().map(|()| Outcome::Done),
        Command::Probe { clean, yes } => probe(*clean, *yes),
        Command::Sync {
            plan_only,
            playlists_only,
            verbose,
            json,
            yes,
            allow_mass_removal,
        } => run_sync(SyncArgs {
            plan_only: *plan_only,
            playlists_only: *playlists_only,
            verbose: *verbose,
            json: *json,
            yes: *yes,
            allow_mass_removal: *allow_mass_removal,
        }),
        Command::Backups { device } => run_backups(device.as_deref()).map(|()| Outcome::Done),
        Command::Restore { stamp, yes } => run_restore(stamp, *yes),
        Command::Doctor { fix, yes } => run_doctor(*fix, *yes),
        Command::Eject => run_eject().map(|()| Outcome::Done),
        Command::Scope { set, device } => {
            run_scope(set.as_ref(), device.as_deref()).map(|()| Outcome::Done)
        }
        Command::Adopt {
            accept_proposals,
            force,
        } => adopt(*accept_proposals, *force).map(|()| Outcome::Done),
    };
    match result {
        Ok(Outcome::Done) => ExitCode::SUCCESS,
        Ok(Outcome::Declined) => ExitCode::from(EXIT_DECLINED),
        Err(e)
            if e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe) =>
        {
            ExitCode::SUCCESS
        }
        Err(e) => {
            let _ = writeln!(io::stderr(), "fiio-tunes: {e:#}");
            match e.downcast_ref::<fiio_core::Error>() {
                Some(fiio_core::Error::VerifyFailed(_)) => ExitCode::from(EXIT_VERIFY),
                _ => ExitCode::FAILURE,
            }
        }
    }
}
