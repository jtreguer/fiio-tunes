use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use fiio_core::config::{Config, PathStyle, Paths};
use fiio_core::store::Store;
use fiio_core::{playlists, scan};

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

fn main() -> ExitCode {
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
            ExitCode::FAILURE
        }
    }
}
