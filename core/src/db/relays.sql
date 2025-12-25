/* SQLite */
CREATE TABLE IF NOT EXISTS relays (
    id TEXT PRIMARY KEY,

    host TEXT NOT NULL,
    port INTEGER NOT NULL,

    last_avg_latency INTEGER,

    -- When was relay seen in any resolved list
    last_seen INTEGER NOT NULL,

    -- When was relay last connected
    last_connect INTEGER,

    last_version INTEGER NOT NULL,

    reputation INTEGER NOT NULL DEFAULT 0
)