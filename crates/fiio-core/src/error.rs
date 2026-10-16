use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("config {}: {message}", path.display())]
    Config { path: PathBuf, message: String },
    #[error("no playlist named {0:?}")]
    NoSuchPlaylist(String),
    #[error("a playlist named {0:?} already exists")]
    PlaylistExists(String),
    #[error("invalid playlist name {0:?}: {1}")]
    InvalidName(String, &'static str),
    #[error("{} is not in the library index; run `fiio-tunes scan` first", .0.display())]
    NotIndexed(PathBuf),
    #[error("library root {} is not a readable folder (is the disk mounted?)", .0.display())]
    RootMissing(PathBuf),
    #[error("found no audio files under {}; refusing to mark {tracks} tracks missing (is the disk mounted?)", root.display())]
    EmptyRoot { root: PathBuf, tracks: usize },
    #[error("the library index is empty; run `fiio-tunes scan` first")]
    EmptyIndex,
    #[error("{}: not valid UTF-8", .0.display())]
    NotUtf8(PathBuf),
    #[error("{} was changed by another program while fiio-tunes was editing it; nothing written, try again", .0.display())]
    ChangedOnDisk(PathBuf),
    #[error("{0}")]
    NotWritable(String),
    #[error(
        "another fiio-tunes is syncing, repairing, restoring or ejecting the card; try again once it is done"
    )]
    CardBusy,
    #[error(
        "{0:?} names several playlists; give the exact name, or import without names to take them all"
    )]
    AmbiguousName(String),
    #[error("{0}")]
    Iprune(String),
    #[error(transparent)]
    Gpod(#[from] gpod::Error),
    #[error("no iPod found among the mounted filesystems")]
    NoIpod,
    #[error("several iPods are mounted ({0}); name one with --mountpoint")]
    SeveralIpods(String),
    /// Detection is off; the text says how to name the device instead.
    #[error("device detection is off (FIIO_TUNES_NO_DETECT is set); {0}")]
    DetectionOff(&'static str),
    #[error(
        "no FiiO X3 found among the mounted FAT32 filesystems; connect it in USB storage mode, or set device.mountpoint"
    )]
    NoCard,
    #[error("several FiiO cards are mounted ({0}); set device.mountpoint to the one to use")]
    SeveralCards(String),
    #[error("{} already holds probe files; run `fiio-tunes probe --clean` first", .0.display())]
    ProbeThere(PathBuf),
    #[error(
        "the probe written to {} has not been cleaned; run `fiio-tunes probe --clean` on that card first",
        .0.display()
    )]
    ProbeElsewhere(PathBuf),
    #[error("{} is not an X3's card; adopt works on the card of a connected X3", .0.display())]
    NotAnX3(PathBuf),
    #[error("{} cannot be read: {}; move it aside to adopt this card", .0.display(), .1)]
    BadManifest(PathBuf, String),
    #[error(
        "the card holds the manifest of another player ({found}, not {key}); move .fiio-tunes/manifest.json aside to adopt it for this one"
    )]
    ForeignManifest { found: String, key: String },
    #[error("{0} was adopted already; run again with --force to replace its mappings")]
    AlreadyAdopted(String),
    #[error("{0} has not been adopted; run `fiio-tunes adopt` first")]
    NotAdopted(String),
    #[error(
        "{key} was adopted with nothing matched, and the card holds {files} audio files: a sync would copy their songs a second time. Adopt it again once the library holds them, or remove them"
    )]
    NothingMatched { key: String, files: usize },
    #[error("a restore of this card ({0}) is unfinished; run `fiio-tunes restore {0}` again")]
    RestoreUnfinished(String),
    #[error("this card is not the one adopted ({0}); adopt it again")]
    WrongCard(String),
    #[error("{0}")]
    BadScope(String),
    #[error("the card could not be read completely ({0}); nothing planned")]
    CardIncomplete(String),
    #[error(
        "the plan does not fit on the card: it needs {needed} bytes at the peak, {free} are free"
    )]
    NoSpace { needed: i64, free: u64 },
    #[error("the plan removes {0}; run again with --allow-mass-removal to apply it")]
    MassRemoval(String),
    #[error(
        "a sync stopped between the two renames of a swap and left {0} files under a hidden name; run `fiio-tunes doctor --fix` first"
    )]
    StoppedMidMove(usize),
    #[error("{0}: would overwrite a file; nothing more written")]
    WouldOverwrite(String),
    #[error("the card changed since it was adopted ({0}); adopt it again")]
    CardChanged(String),
    #[error("no backup {0} for this card; `fiio-tunes backups` lists them")]
    NoSuchBackup(String),
    #[error("backup: {0}")]
    BadBackup(String),
    /// A file written to the card reads back wrong: exit code 3.
    #[error("verification failed: {0}")]
    VerifyFailed(String),
    #[error("the library index has no MP3 file to copy for the probe")]
    NoProbeSource,
    #[error("position {position} is out of range (playlist has {len} entries)")]
    BadPosition { position: usize, len: usize },
}

pub(crate) trait IoContext<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T> {
        self.map_err(|source| Error::Io {
            path: path.into(),
            source,
        })
    }
}
