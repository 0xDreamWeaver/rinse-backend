-- Add metadata fields to items table for track enrichment
--
-- These fields store metadata fetched from external APIs:
-- - MusicBrainz: artist, album, title, year, genre, track_number, label, musicbrainz_id
-- - Cover Art Archive: album_art_url (linked via MusicBrainz ID)
-- - GetSongBPM: bpm, key
-- - Local file analysis: duration_ms
--
-- The metadata_sources column tracks which APIs contributed data (JSON array).
-- The metadata_fetched_at column records when metadata was last fetched.

-- Core metadata fields (must-haves)
ALTER TABLE items ADD COLUMN meta_artist TEXT;
ALTER TABLE items ADD COLUMN meta_album TEXT;
ALTER TABLE items ADD COLUMN meta_title TEXT;
ALTER TABLE items ADD COLUMN meta_bpm INTEGER;
ALTER TABLE items ADD COLUMN meta_key TEXT;
ALTER TABLE items ADD COLUMN meta_duration_ms INTEGER;
ALTER TABLE items ADD COLUMN meta_album_art_url TEXT;

-- Extended metadata fields (nice-to-haves)
ALTER TABLE items ADD COLUMN meta_genre TEXT;
ALTER TABLE items ADD COLUMN meta_year INTEGER;
ALTER TABLE items ADD COLUMN meta_track_number INTEGER;
ALTER TABLE items ADD COLUMN meta_label TEXT;

-- Source tracking
ALTER TABLE items ADD COLUMN meta_musicbrainz_id TEXT;
ALTER TABLE items ADD COLUMN metadata_fetched_at DATETIME;
ALTER TABLE items ADD COLUMN metadata_sources TEXT;  -- JSON array of sources

-- Rate limiting table for manual metadata refresh (24hr limit per track)
CREATE TABLE IF NOT EXISTS metadata_rate_limits (
    item_id INTEGER PRIMARY KEY REFERENCES items(id) ON DELETE CASCADE,
    last_lookup_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Indexes for search/filter functionality
CREATE INDEX idx_items_meta_artist ON items(meta_artist);
CREATE INDEX idx_items_meta_album ON items(meta_album);
CREATE INDEX idx_items_meta_bpm ON items(meta_bpm);
CREATE INDEX idx_items_meta_key ON items(meta_key);
CREATE INDEX idx_items_meta_genre ON items(meta_genre);
CREATE INDEX idx_items_meta_year ON items(meta_year);
