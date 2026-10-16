-- The library index: one row per audio file under the library roots.
-- Ids are never handed out again once deleted (AUTOINCREMENT): a backup's
-- mappings name rows by id, and a restore must not attach an old card file
-- to a new song or playlist that took a freed id.
CREATE TABLE tracks (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  path          TEXT NOT NULL UNIQUE,        -- exact on-disk path, absolute
  path_key      TEXT NOT NULL UNIQUE,        -- NFC form of path, for lookups
  size          INTEGER NOT NULL,
  mtime_ns      INTEGER NOT NULL,
  hash          BLOB NOT NULL,               -- blake3 of the whole file
  duration_ms   INTEGER,
  filetype      TEXT NOT NULL,               -- mp3, aac, alac, flac, wav, ...
  bitrate       INTEGER, samplerate INTEGER,
  title TEXT, artist TEXT, album TEXT, album_artist TEXT, composer TEXT,
  genre TEXT, year INTEGER,
  track_no INTEGER, track_total INTEGER, disc_no INTEGER, disc_total INTEGER,
  compilation   INTEGER NOT NULL DEFAULT 0,
  tags_hash     BLOB NOT NULL,               -- hash of the tag fields read
  added_at      INTEGER NOT NULL,
  missing_since INTEGER,                     -- set when the file disappears
  tag_error     TEXT,                        -- why the tags could not be read
  drm           INTEGER NOT NULL DEFAULT 0,  -- FairPlay protected
  tags_version  INTEGER NOT NULL DEFAULT 0   -- tag reader version that read it
);
CREATE INDEX tracks_hash ON tracks(hash);

-- A playlist whose file is deleted while a card playlist is mapped to it
-- stays as a tombstone (removed_at set, items dropped) until a sync deletes
-- the card's file.
CREATE TABLE playlists (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  name          TEXT NOT NULL UNIQUE,
  file          TEXT NOT NULL UNIQUE,
  file_mtime_ns INTEGER NOT NULL,
  removed_at    INTEGER
);

CREATE TABLE playlist_items (
  playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
  position    INTEGER NOT NULL,
  track_id    INTEGER REFERENCES tracks(id),   -- NULL when the entry is unresolved
  raw_entry   TEXT NOT NULL,                   -- the line from the file
  PRIMARY KEY (playlist_id, position)
);
CREATE INDEX playlist_items_track ON playlist_items(track_id);

-- A player, identified by `usb:<serial>` (a folder used as a card in tests:
-- the key in its manifest). `card_id` is the random id fiio-tunes wrote in
-- the manifest of the card it adopted: another card in the same player, or
-- the same card changed by another copy of the index, has another one.
CREATE TABLE devices (
  id            INTEGER PRIMARY KEY,
  device_key    TEXT NOT NULL UNIQUE,
  card_id       TEXT,
  name TEXT, model TEXT,
  last_sync_at  INTEGER,
  restoring     TEXT,                          -- stamp of an unfinished restore
  scope         TEXT                           -- 'all', 'playlists', 'except';
                                               -- NULL follows [sync]
);

CREATE TABLE device_scope_playlists (
  device_id INTEGER NOT NULL REFERENCES devices(id),
  name      TEXT NOT NULL,
  PRIMARY KEY (device_id, name)
);

-- Which card file holds which library track. `card_path` is relative to the
-- card's root, with `/` separators, exactly as on the card. FAT32 compares
-- names without case, Unicode included, so uniqueness is on `card_key`
-- (`paths::card_key`: NFC, each character upper-cased one to one).
CREATE TABLE device_tracks (
  device_id   INTEGER NOT NULL REFERENCES devices(id),
  track_id    INTEGER NOT NULL REFERENCES tracks(id),
  card_path   TEXT NOT NULL,
  card_key    TEXT NOT NULL,
  synced_hash BLOB NOT NULL,                   -- blake3 of the file on the card
  synced_size INTEGER NOT NULL,
  PRIMARY KEY (device_id, track_id),
  UNIQUE (device_id, card_key)
);

-- Which card playlist file stands for which library playlist.
CREATE TABLE device_playlists (
  device_id   INTEGER NOT NULL REFERENCES devices(id),
  playlist_id INTEGER NOT NULL REFERENCES playlists(id),
  card_name   TEXT NOT NULL,                   -- file name on the card, with extension
  card_key    TEXT NOT NULL,                   -- `paths::card_key` of card_name
  PRIMARY KEY (device_id, playlist_id),
  UNIQUE (device_id, card_key)
);
