-- Track individual tracks that failed to import (for retry functionality)

CREATE TABLE IF NOT EXISTS failed_import_tracks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_import_id INTEGER NOT NULL,

    -- Track info from external service
    external_track_id TEXT,
    artist_name TEXT NOT NULL,
    track_name TEXT NOT NULL,
    album_name TEXT,
    duration_ms INTEGER,

    -- Failure info
    failure_reason TEXT,
    retry_count INTEGER NOT NULL DEFAULT 0,
    last_retry_at DATETIME,

    -- Status: 'failed', 'pending_retry', 'retry_success'
    status TEXT NOT NULL DEFAULT 'failed',

    -- If retry succeeded, link to the created item
    item_id INTEGER,

    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,

    FOREIGN KEY (playlist_import_id) REFERENCES playlist_imports(id) ON DELETE CASCADE,
    FOREIGN KEY (item_id) REFERENCES items(id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_failed_import_tracks_import_id ON failed_import_tracks(playlist_import_id);
CREATE INDEX IF NOT EXISTS idx_failed_import_tracks_status ON failed_import_tracks(status);
