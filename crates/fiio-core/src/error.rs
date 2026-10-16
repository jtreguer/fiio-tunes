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
    #[error(
        "device detection is off (FIIO_TUNES_NO_DETECT is set); name the iPod with --mountpoint"
    )]
    DetectionOff,
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
