-- Add separate artist and track fields for search queries
-- This enables more accurate metadata lookups by knowing which part is artist vs title

-- Add original_artist column (what user typed in artist field)
ALTER TABLE items ADD COLUMN original_artist TEXT;

-- Add original_track column (what user typed in track field)
ALTER TABLE items ADD COLUMN original_track TEXT;

-- Also add to search_queue for pending searches
ALTER TABLE search_queue ADD COLUMN original_artist TEXT;
ALTER TABLE search_queue ADD COLUMN original_track TEXT;

-- Index for potential filtering by artist
CREATE INDEX IF NOT EXISTS idx_items_original_artist ON items(original_artist);
