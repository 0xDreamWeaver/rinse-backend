-- Create items table for downloaded files
CREATE TABLE IF NOT EXISTS items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    filename TEXT NOT NULL,
    original_query TEXT NOT NULL,
    file_path TEXT NOT NULL UNIQUE,
    file_size INTEGER NOT NULL,
    bitrate INTEGER,
    duration INTEGER,
    extension TEXT NOT NULL,
    source_username TEXT NOT NULL,
    download_status TEXT NOT NULL DEFAULT 'pending', -- pending, downloading, completed, failed
    download_progress REAL NOT NULL DEFAULT 0.0, -- 0.0 to 1.0
    error_message TEXT,
    metadata TEXT, -- JSON string for additional metadata
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    completed_at DATETIME
);

-- Indexes for faster queries
CREATE INDEX idx_items_filename ON items(filename);
CREATE INDEX idx_items_status ON items(download_status);
CREATE INDEX idx_items_query ON items(original_query);
CREATE INDEX idx_items_created_at ON items(created_at DESC);
