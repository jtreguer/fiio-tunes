//! The players fiio-tunes knows and what it put on each card: `devices`,
//! `device_tracks` and `device_playlists`.

use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::paths::card_key;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    pub id: i64,
    pub device_key: String,
    /// The id in the manifest of the card adopted; `None` until a sync, or
    /// an adoption of a card that has a manifest, sets it.
    pub card_id: Option<String>,
    pub name: Option<String>,
    pub model: Option<String>,
    pub last_sync_at: Option<i64>,
}

/// A library track and the card file that holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackMapping {
    pub track_id: i64,
    /// Relative to the card root, with `/`, exactly as on the card.
    pub card_path: String,
    /// BLAKE3 of the card file.
    pub synced_hash: [u8; 32],
    pub synced_size: u64,
}

/// A library playlist and the card playlist file that stands for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistMapping {
    pub playlist_id: i64,
    /// File name on the card, with its extension.
    pub card_name: String,
}

pub fn device(conn: &Connection, key: &str) -> Result<Option<DeviceRow>> {
    Ok(conn
        .query_row(
            "SELECT id, device_key, card_id, name, model, last_sync_at FROM devices \
             WHERE device_key = ?1",
            [key],
            |r| {
                Ok(DeviceRow {
                    id: r.get(0)?,
                    device_key: r.get(1)?,
                    card_id: r.get(2)?,
                    name: r.get(3)?,
                    model: r.get(4)?,
                    last_sync_at: r.get(5)?,
                })
            },
        )
        .optional()?)
}

/// The device's id, inserting it if new; name and model are updated.
pub fn upsert_device(
    conn: &Connection,
    key: &str,
    name: Option<&str>,
    model: Option<&str>,
) -> Result<i64> {
    Ok(conn.query_row(
        "INSERT INTO devices (device_key, name, model) VALUES (?1, ?2, ?3) \
         ON CONFLICT(device_key) DO UPDATE SET name = COALESCE(excluded.name, name), \
         model = COALESCE(excluded.model, model) \
         RETURNING id",
        params![key, name, model],
        |r| r.get(0),
    )?)
}

pub fn set_card_id(conn: &Connection, device_id: i64, card_id: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE devices SET card_id = ?2 WHERE id = ?1",
        params![device_id, card_id],
    )?;
    Ok(())
}

/// How many tracks and playlists are mapped to the device.
pub fn mapped_count(conn: &Connection, device_id: i64) -> Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM device_tracks WHERE device_id = ?1) \
         + (SELECT COUNT(*) FROM device_playlists WHERE device_id = ?1)",
        [device_id],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}

/// Replaces every mapping of the device. Tombstones no device maps any more
/// have nothing left to delete on a card and go.
pub fn replace_mappings(
    conn: &Connection,
    device_id: i64,
    tracks: &[TrackMapping],
    playlists: &[PlaylistMapping],
) -> Result<()> {
    crate::store::savepoint(conn, || {
        conn.execute(
            "DELETE FROM device_tracks WHERE device_id = ?1",
            [device_id],
        )?;
        conn.execute(
            "DELETE FROM device_playlists WHERE device_id = ?1",
            [device_id],
        )?;
        let mut stmt = conn.prepare(
            "INSERT INTO device_tracks (device_id, track_id, card_path, card_key, synced_hash, synced_size) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for m in tracks {
            stmt.execute(params![
                device_id,
                m.track_id,
                m.card_path,
                card_key(&m.card_path),
                &m.synced_hash[..],
                m.synced_size as i64
            ])?;
        }
        let mut stmt = conn.prepare(
            "INSERT INTO device_playlists (device_id, playlist_id, card_name, card_key) \
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for m in playlists {
            stmt.execute(params![
                device_id,
                m.playlist_id,
                m.card_name,
                card_key(&m.card_name)
            ])?;
        }
        conn.execute(
            "DELETE FROM playlists WHERE removed_at IS NOT NULL \
             AND id NOT IN (SELECT playlist_id FROM device_playlists)",
            [],
        )?;
        Ok(())
    })
}

pub fn track_mappings(conn: &Connection, device_id: i64) -> Result<Vec<TrackMapping>> {
    let mut stmt = conn.prepare(
        "SELECT track_id, card_path, synced_hash, synced_size FROM device_tracks \
         WHERE device_id = ?1 ORDER BY card_path",
    )?;
    let rows = stmt
        .query_map([device_id], |r| {
            let hash: Vec<u8> = r.get(2)?;
            Ok(TrackMapping {
                track_id: r.get(0)?,
                card_path: r.get(1)?,
                synced_hash: hash.try_into().map_err(|_| {
                    rusqlite::Error::InvalidColumnType(
                        2,
                        "synced_hash".into(),
                        rusqlite::types::Type::Blob,
                    )
                })?,
                synced_size: r.get::<_, i64>(3)? as u64,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

pub fn playlist_mappings(conn: &Connection, device_id: i64) -> Result<Vec<PlaylistMapping>> {
    let mut stmt = conn.prepare(
        "SELECT playlist_id, card_name FROM device_playlists WHERE device_id = ?1 \
         ORDER BY card_name",
    )?;
    let rows = stmt
        .query_map([device_id], |r| {
            Ok(PlaylistMapping {
                playlist_id: r.get(0)?,
                card_name: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}
