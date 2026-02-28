-- Search history table for tracking completed and failed searches
--
-- This table is separate from search_queue to keep the queue clean and fast.
-- Entries are inserted here when a search completes (successfully or with failure).

CREATE TABLE IF NOT EXISTS search_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,

    -- Who submitted this search
    user_id INTEGER NOT NULL,

    -- The search query string
    query TEXT NOT NULL,

    -- Original artist/track from user input (for display)
    original_artist TEXT,
    original_track TEXT,

    -- Optional format filter that was requested
    format TEXT,

    -- Final status: completed or failed
    status TEXT NOT NULL,

    -- Reference to created item (if search succeeded)
    item_id INTEGER,

    -- Error message (if search failed)
    error_message TEXT,

    -- Timestamps
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    completed_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,

    -- Foreign keys
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (item_id) REFERENCES items(id) ON DELETE SET NULL
);

-- Index for user's search history
CREATE INDEX idx_search_history_user_id ON search_history(user_id);

-- Index for recent searches (most common query)
CREATE INDEX idx_search_history_completed_at ON search_history(completed_at DESC);

-- Index for status filtering
CREATE INDEX idx_search_history_status ON search_history(status);
