-- Schema for the collab signaling worker.

CREATE TABLE IF NOT EXISTS bindings (
  endpoint_id TEXT PRIMARY KEY,
  cert_hash   TEXT NOT NULL,
  bound_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS trusted (
  endpoint_id TEXT NOT NULL,
  trusted_id  TEXT NOT NULL,
  PRIMARY KEY (endpoint_id, trusted_id)
);

CREATE INDEX IF NOT EXISTS trusted_by_endpoint ON trusted(endpoint_id);

-- Permanent. Worker rejects any bind whose cert hash is present.
CREATE TABLE IF NOT EXISTS blacklist (
  cert_hash TEXT PRIMARY KEY,
  reason    TEXT NOT NULL,
  added_at  INTEGER NOT NULL
);
