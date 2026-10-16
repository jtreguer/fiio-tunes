//! What fiio-tunes reads of iprune's files: its config (the playlists folder
//! and the default sync scope) and its index (which library file each iPod
//! track is, and each iPod's scope). Nothing here writes to them: the index
//! is copied to a temp folder and the copy is opened.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;

use crate::config::Scope;
use crate::error::IoContext;
use crate::ipod::normalize_guid;
use crate::{Error, Result};

/// Where iprune keeps its files. Follows `XDG_CONFIG_HOME` and `XDG_DATA_HOME`.
#[derive(Debug, Clone)]
pub struct IprunePaths {
    pub config_file: PathBuf,
    pub database: PathBuf,
}

impl IprunePaths {
    pub fn from_env() -> Option<Self> {
        let dirs = directories::ProjectDirs::from("", "", "iprune")?;
        Some(IprunePaths {
            config_file: dirs.config_dir().join("config.toml"),
            database: dirs.data_dir().join("iprune.sqlite"),
        })
    }
}

/// The part of iprune's config fiio-tunes uses.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct IpruneConfig {
    pub playlists: IprunePlaylists,
    pub sync: IpruneSync,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct IprunePlaylists {
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct IpruneSync {
    pub scope: Scope,
    pub playlists: Vec<String>,
}

impl IpruneConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).at(path)?;
        toml::from_str(&text).map_err(|e| Error::Config {
            path: path.to_owned(),
            message: e.to_string(),
        })
    }

    /// iprune's playlists folder, which must be set and absolute.
    pub fn playlists_dir(&self, path: &Path) -> Result<&Path> {
        let dir = &self.playlists.dir;
        if !dir.is_absolute() {
            return Err(Error::Config {
                path: path.to_owned(),
                message: "iprune's playlists.dir is not set to an absolute path".into(),
            });
        }
        Ok(dir)
    }
}

/// An iPod iprune knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: i64,
    /// Normalized, see [`normalize_guid`].
    pub firewire_guid: String,
    pub name: Option<String>,
}

/// A device's sync scope in iprune: `None` follows iprune's `[sync]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceScope {
    pub scope: Option<Scope>,
    pub playlists: Vec<String>,
}

/// A read-only copy of iprune's index.
pub struct Index {
    conn: Connection,
    _dir: tempfile::TempDir,
}

impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index")
            .field("dir", &self._dir.path())
            .finish()
    }
}

impl Index {
    /// Copies `database` and its `-wal` to a temp folder and opens the copy
    /// for queries only. `None` when iprune has no index.
    pub fn open_copy(database: &Path) -> Result<Option<Self>> {
        if !database.is_file() {
            return Ok(None);
        }
        let dir = tempfile::Builder::new()
            .prefix("fiio-tunes-iprune-")
            .tempdir()
            .at(std::env::temp_dir())?;
        let copy = dir.path().join("iprune.sqlite");
        std::fs::copy(database, &copy).at(database)?;
        let mut wal = database.as_os_str().to_owned();
        wal.push("-wal");
        let wal = PathBuf::from(wal);
        if wal.is_file() {
            std::fs::copy(&wal, dir.path().join("iprune.sqlite-wal")).at(&wal)?;
        }
        let conn = Connection::open(&copy)?;
        conn.pragma_update(None, "query_only", true)?;
        Ok(Some(Index { conn, _dir: dir }))
    }

    pub fn devices(&self) -> Result<Vec<Device>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, firewire_guid, name FROM devices ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Device {
                    id: r.get(0)?,
                    firewire_guid: normalize_guid(&r.get::<_, String>(1)?),
                    name: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// The library file iprune mapped each iPod track (by dbid) to, for
    /// device `device`.
    pub fn mappings(&self, device: i64) -> Result<HashMap<u64, PathBuf>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.dbid, t.path FROM device_tracks d JOIN tracks t ON t.id = d.track_id \
             WHERE d.device_id = ?1",
        )?;
        let rows = stmt
            .query_map([device], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    PathBuf::from(r.get::<_, String>(1)?),
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// The dbids iprune mapped for `device`.
    fn dbids(&self, device: i64) -> Result<HashSet<u64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT dbid FROM device_tracks WHERE device_id = ?1")?;
        let rows = stmt
            .query_map([device], |r| Ok(r.get::<_, i64>(0)? as u64))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn scope(&self, device: i64) -> Result<DeviceScope> {
        let scope: Option<String> = self
            .conn
            .query_row("SELECT scope FROM devices WHERE id = ?1", [device], |r| {
                r.get(0)
            })
            .optional()?
            .flatten();
        let scope = match scope.as_deref() {
            None => None,
            Some("all") => Some(Scope::All),
            Some("playlists") => Some(Scope::Playlists),
            Some("except") => Some(Scope::Except),
            Some(other) => {
                return Err(Error::Iprune(format!(
                    "iprune's index has an unknown scope {other:?}"
                )));
            }
        };
        let mut stmt = self.conn.prepare(
            "SELECT name FROM device_scope_playlists WHERE device_id = ?1 ORDER BY name",
        )?;
        let playlists = stmt
            .query_map([device], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(DeviceScope { scope, playlists })
    }

    /// iprune's playlist names, each with the persistent id of the iPod
    /// playlist it is synced to on `device`.
    pub fn playlist_ipod_ids(&self, device: i64) -> Result<HashMap<String, u64>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.name, d.ipod_id FROM device_playlists d JOIN playlists p ON p.id = d.playlist_id \
             WHERE d.device_id = ?1 AND d.ipod_id IS NOT NULL",
        )?;
        let rows = stmt
            .query_map([device], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// The iPod iprune knows as the one `guid` names; failing that (a copy
    /// of a database, whose GUID is unknown), the one whose mappings name
    /// the most of `dbids`, if any does.
    pub fn device_for(&self, guid: Option<&str>, dbids: &HashSet<u64>) -> Result<Option<Device>> {
        let devices = self.devices()?;
        if let Some(g) = guid.map(normalize_guid)
            && let Some(d) = devices.iter().find(|d| d.firewire_guid == g)
        {
            return Ok(Some(d.clone()));
        }
        let mut best: Option<(usize, Device)> = None;
        for d in devices {
            let n = self.dbids(d.id)?.intersection(dbids).count();
            if n > 0 && best.as_ref().is_none_or(|(m, _)| n > *m) {
                best = Some((n, d));
            }
        }
        Ok(best.map(|(_, d)| d))
    }
}
