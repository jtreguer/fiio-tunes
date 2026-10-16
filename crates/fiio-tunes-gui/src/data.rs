//! Loading what the window shows from the config, the index and the
//! playlist files, and the playlist edits, which go through `fiio-core`.
//! Nothing here writes to the card.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use fiio_core::config::{Config, Paths, Scope};
use fiio_core::plan::LibPlaylist;
use fiio_core::store::{self, Store, TrackRow, devices};
use fiio_core::{adoption, device, manifest, playlists, sync};

use crate::view_model::{self, Row};

pub struct Data {
    /// Read again after a change made from the window (a scope copied).
    config: std::cell::RefCell<Config>,
    paths: Paths,
    store: Store,
    /// Set when the playlists folder could not be read at start; the index
    /// still has the playlists as last read.
    pub warning: Option<String>,
}

/// The X3 as the sidebar shows it.
#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub name: String,
    pub connected: bool,
    /// False for a connected X3 fiio-tunes has not adopted.
    pub adopted: bool,
    pub mapped_tracks: usize,
    pub free: Option<u64>,
    pub capacity: Option<u64>,
    pub last_sync: Option<i64>,
    /// The device row of an adopted X3.
    pub id: Option<i64>,
    /// What it syncs, in a few words, for an adopted X3.
    pub selection: Option<String>,
}

/// A selection in a few words, for the sidebar.
pub fn selection_summary(s: &devices::DeviceScope) -> String {
    let n = s.playlists.len();
    let playlists = if n == 1 { "playlist" } else { "playlists" };
    match s.kind {
        Scope::Playlists => format!("syncs {n} {playlists}"),
        Scope::Except if n > 0 => format!("syncs all but {n} {playlists}"),
        Scope::All | Scope::Except => "syncs the whole library".into(),
    }
}

impl Data {
    pub fn open() -> Result<Data> {
        let paths =
            Paths::from_env().ok_or_else(|| anyhow!("cannot determine the home directory"))?;
        if !paths.config_file.exists() {
            bail!(
                "no config at {}; run `fiio-tunes init` first",
                paths.config_file.display()
            );
        }
        let config = Config::load(&paths.config_file)?;
        let store = Store::open(&paths.database())?;
        // Picks up playlist files changed by another program; only the index
        // is written. An unreadable folder (disk unmounted) leaves the index
        // as it was rather than closing the library.
        let warning = playlists::sync_from_disk(store.conn(), &config, false)
            .context("reading the playlists folder")
            .err()
            .map(|e| format!("{e:#}"));
        Ok(Data {
            config: std::cell::RefCell::new(config),
            paths,
            store,
            warning,
        })
    }

    pub fn library(&self) -> Result<Vec<Row>> {
        let tracks = store::all_tracks(self.store.conn())?;
        Ok(view_model::library_rows(&tracks))
    }

    /// Playlist names, as the files are named.
    pub fn playlist_names(&self) -> Result<Vec<String>> {
        Ok(store::playlists(self.store.conn())?
            .into_iter()
            .map(|p| p.name)
            .collect())
    }

    /// The playlists holding the library track `track_id`, by name.
    pub fn playlists_holding(&self, track_id: i64) -> Result<Vec<String>> {
        Ok(store::playlists_holding(self.store.conn(), track_id)?)
    }

    pub fn playlist(&self, name: &str) -> Result<Vec<Row>> {
        let items = playlists::show(self.store.conn(), &self.config.borrow(), name)?;
        Ok(view_model::playlist_rows(&items))
    }

    /// The playlist file's modification time, if it can be read.
    pub fn playlist_mtime(&self, name: &str) -> Option<i64> {
        let file = playlists::file_for(&self.config.borrow(), name).ok()?;
        fiio_core::playlist::mtime_ns(&file).ok()
    }

    /// For the workers, which open their own connection to the index.
    pub fn config(&self) -> Config {
        self.config.borrow().clone()
    }

    /// Reads the config file again.
    pub fn reload_config(&self) -> Result<()> {
        *self.config.borrow_mut() = Config::load(&self.paths.config_file)?;
        Ok(())
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn playlists_dir(&self) -> PathBuf {
        self.config.borrow().playlists.dir.clone()
    }

    /// Re-reads playlist files changed since they were last read. Only the
    /// index is written.
    pub fn reload_playlists(&self) -> Result<playlists::LoadReport> {
        Ok(playlists::sync_from_disk(
            self.store.conn(),
            &self.config.borrow(),
            false,
        )?)
    }

    /// Adds library files to a playlist, at 1-based `at` or at the end.
    pub fn add(&self, name: &str, files: &[PathBuf], at: Option<usize>) -> Result<()> {
        Ok(playlists::add(
            self.store.conn(),
            &self.config.borrow(),
            name,
            files,
            at,
        )?)
    }

    /// Removes the entries at 1-based `positions`.
    pub fn remove(&self, name: &str, positions: &[usize]) -> Result<()> {
        Ok(playlists::remove(
            self.store.conn(),
            &self.config.borrow(),
            name,
            positions,
        )?)
    }

    /// Moves the entries at 1-based `positions` before `before`.
    pub fn move_entries(&self, name: &str, positions: &[usize], before: usize) -> Result<()> {
        Ok(playlists::move_entries(
            self.store.conn(),
            &self.config.borrow(),
            name,
            positions,
            before,
        )?)
    }

    pub fn create(&self, name: &str) -> Result<()> {
        Ok(playlists::create(
            self.store.conn(),
            &self.config.borrow(),
            name,
        )?)
    }

    pub fn rename(&self, old: &str, new: &str) -> Result<()> {
        Ok(playlists::rename(
            self.store.conn(),
            &self.config.borrow(),
            old,
            new,
        )?)
    }

    /// Deletes the playlist file. One mapped to the card stays as a
    /// tombstone until the next sync deletes it there.
    pub fn delete(&self, name: &str) -> Result<()> {
        Ok(playlists::delete(
            self.store.conn(),
            &self.config.borrow(),
            name,
        )?)
    }

    /// What the X3 `id` syncs, and whether it was set for it (false: it
    /// follows the config).
    pub fn selection(&self, id: i64) -> Result<(devices::DeviceScope, bool)> {
        Ok(sync::device_scope(
            self.store.conn(),
            &self.config.borrow(),
            id,
        )?)
    }

    /// The library and playlists a selection is sized against.
    pub fn selection_inputs(&self) -> Result<(Vec<TrackRow>, Vec<LibPlaylist>)> {
        let conn = self.store.conn();
        Ok((store::all_tracks(conn)?, devices::library_playlists(conn)?))
    }

    /// Sets what the X3 `id` syncs. Refuses a selection that names a
    /// playlist that is gone or keeps playlists holding no track.
    pub fn set_selection(&self, id: i64, s: &devices::DeviceScope) -> Result<()> {
        let conn = self.store.conn();
        let (library, lists) = self.selection_inputs()?;
        if let Some(why) = sync::scope_problem(s, true, &library, &lists) {
            bail!("{why}");
        }
        devices::set_scope(conn, id, Some(s))?;
        Ok(())
    }

    /// The X3 to show: the connected one, adopted or not, else the first
    /// adopted one, not connected.
    pub fn device(&self) -> Result<Option<DeviceInfo>> {
        let conn = self.store.conn();
        let known = devices::devices(conn)?;
        let info = |row: &devices::DeviceRow, at: Option<&std::path::Path>| -> Result<DeviceInfo> {
            let space = at.and_then(|m| device::space(m).ok());
            Ok(DeviceInfo {
                name: row.name.clone().unwrap_or_else(|| "X3".into()),
                connected: at.is_some(),
                adopted: true,
                mapped_tracks: devices::mapped_count(conn, row.id)?,
                free: space.map(|(_, free)| free),
                capacity: space.map(|(capacity, _)| capacity),
                last_sync: row.last_sync_at,
                id: Some(row.id),
                selection: Some(selection_summary(
                    &sync::device_scope(conn, &self.config.borrow(), row.id)?.0,
                )),
            })
        };
        // No card found, or several, or detection off: none connected. A
        // configured mountpoint counts only when it is a FiiO card there
        // now (a folder used as a card in tests, with `allow_folder`).
        let allow_folder = self.config.borrow().device.allow_folder;
        let located = device::locate(&self.config.borrow())
            .ok()
            .filter(|f| f.mount.mountpoint.is_dir() && (allow_folder || f.key().is_some()));
        if let Some(found) = located {
            let card = found.mount.mountpoint.clone();
            let key = manifest::identity(&card)
                .ok()
                .and_then(|m| adoption::device_key(&found, m.as_ref(), allow_folder).ok());
            if let Some(row) = key.and_then(|k| known.iter().find(|d| d.device_key == k)) {
                return info(row, Some(&card)).map(Some);
            }
            let space = device::space(&card).ok();
            return Ok(Some(DeviceInfo {
                name: "X3 not adopted".into(),
                connected: true,
                adopted: false,
                mapped_tracks: 0,
                free: space.map(|(_, free)| free),
                capacity: space.map(|(capacity, _)| capacity),
                last_sync: None,
                id: None,
                selection: None,
            }));
        }
        known.first().map(|row| info(row, None)).transpose()
    }
}

#[cfg(test)]
mod tests;
