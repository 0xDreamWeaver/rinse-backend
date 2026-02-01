-- Search queue for managing pending and in-progress searches
--
-- The queue separates the SEARCH phase from the DOWNLOAD phase:
-- - Queue status: pending -> processing -> completed/failed (search phase)
-- - Item status: pending -> downloading -> completed/failed (download phase)
--
-- When a search succeeds, an item is created and linked via item_id.
-- The download then proceeds independently (possibly spawned as background task).

CREATE TABLE IF NOT EXISTS search_queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,

    -- Who submitted this search
    user_id INTEGER NOT NULL,

    -- The search query string
    query TEXT NOT NULL,

    -- Optional format filter (mp3, flac, m4a, wav, aiff, ogg)
    format TEXT,

    -- If part of a list, reference to the list
    -- NULL for standalone single searches
    list_id INTEGER,

    -- Position within the list (0-indexed), NULL for standalone searches
    list_position INTEGER,

    -- Queue status: pending, processing, completed, failed
    -- Note: 'completed' means search succeeded, download may still be in progress
    status TEXT NOT NULL DEFAULT 'pending',

    -- Reference to created item (set when search succeeds and item is created)
    -- NULL while pending/processing, or if search failed
    item_id INTEGER,

    -- Error message if status is 'failed'
    error_message TEXT,

    -- Timestamps
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    started_at DATETIME,      -- When processing began
    completed_at DATETIME,    -- When processing finished (success or failure)

    -- Foreign keys
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (list_id) REFERENCES lists(id) ON DELETE CASCADE,
    FOREIGN KEY (item_id) REFERENCES items(id) ON DELETE SET NULL
);

-- Index for finding next pending item (FIFO order)
CREATE INDEX idx_search_queue_pending ON search_queue(status, created_at)
    WHERE status = 'pending';

-- Index for user's queue items
CREATE INDEX idx_search_queue_user_id ON search_queue(user_id);

-- Index for list's queue items
CREATE INDEX idx_search_queue_list_id ON search_queue(list_id);

-- Index for status queries
CREATE INDEX idx_search_queue_status ON search_queue(status);
