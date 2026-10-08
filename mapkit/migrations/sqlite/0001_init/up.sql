-- mapkit-server zone registry + tile index (SQLite)

CREATE TABLE zones (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  slug         TEXT NOT NULL UNIQUE,
  name         TEXT NOT NULL,
  kind         TEXT NOT NULL DEFAULT '',
  district     TEXT NOT NULL DEFAULT '',
  region       TEXT NOT NULL DEFAULT '',
  center_lat   REAL NOT NULL DEFAULT 0,
  center_lon   REAL NOT NULL DEFAULT 0,
  span_m       REAL NOT NULL DEFAULT 0,
  mode         TEXT NOT NULL DEFAULT 'city',
  vscale       REAL NOT NULL DEFAULT 1.0,
  terrain_size INTEGER NOT NULL DEFAULT 0,
  n_inters     INTEGER NOT NULL DEFAULT 0,
  n_roads      INTEGER NOT NULL DEFAULT 0,
  n_buildings  INTEGER NOT NULL DEFAULT 0,
  n_rivers     INTEGER NOT NULL DEFAULT 0,
  bytes        INTEGER NOT NULL DEFAULT 0,
  status       TEXT NOT NULL DEFAULT 'ready',
  created_at   TEXT NOT NULL DEFAULT '',
  updated_at   TEXT NOT NULL DEFAULT ''
);

CREATE TABLE tiles (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  z          INTEGER NOT NULL,
  x          INTEGER NOT NULL,
  y          INTEGER NOT NULL,
  source     TEXT NOT NULL,
  bytes      INTEGER NOT NULL DEFAULT 0,
  fetched_at TEXT NOT NULL DEFAULT ''
);

CREATE UNIQUE INDEX idx_tiles_zxy_source ON tiles(z, x, y, source);
