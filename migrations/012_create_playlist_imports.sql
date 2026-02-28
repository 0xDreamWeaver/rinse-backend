-- Track imported playlists from external services

CREATE TABLE IF NOT EXISTS playlist_imports (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL,

    -- Source service and playlist info
    service TEXT NOT NULL,
    external_playlist_id TEXT NOT NULL,
    external_playlist_name TEXT,
    external_playlist_url TEXT,
    external_owner_name TEXT,

    -- Associated Rinse list (created on import)
    list_id INTEGER,

    -- Import statistics
    total_tracks INTEGER NOT NULL DEFAULT 0,
    imported_tracks INTEGER NOT NULL DEFAULT 0,
    failed_tracks INTEGER NOT NULL DEFAULT 0,
    skipped_tracks INTEGER NOT NULL DEFAULT 0,

    -- Import status: 'pending', 'importing', 'completed', 'partial', 'failed'
    status TEXT NOT NULL DEFAULT 'pending',
    error_message TEXT,

    -- Timestamps
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    started_at DATETIME,
    completed_at DATETIME,

    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (list_id) REFERENCES lists(id) ON DELETE SET NULL,
    UNIQUE(user_id, service, external_playlist_id)
);

CREATE INDEX IF NOT EXISTS idx_playlist_imports_user_id ON playlist_imports(user_id);
CREATE INDEX IF NOT EXISTS idx_playlist_imports_service ON playlist_imports(service);
CREATE INDEX IF NOT EXISTS idx_playlist_imports_status ON playlist_imports(status);
CREATE INDEX IF NOT EXISTS idx_playlist_imports_list_id ON playlist_imports(list_id);
