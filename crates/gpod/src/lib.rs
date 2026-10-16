//! Read-only safe wrapper around libgpod, used to import playlists from an
//! iPod or a copy of its `iTunesDB`. This is the only crate in the workspace
//! that contains `unsafe` code. Nothing here writes a database.

mod db;
mod error;
mod list;
mod playlist;
mod track;

pub use db::{Database, DeviceInfo};
pub use error::{Error, Result};
pub use playlist::PlaylistRef;
pub use track::TrackRef;
