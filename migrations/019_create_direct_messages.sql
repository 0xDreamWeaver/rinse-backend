-- Direct messages between users
CREATE TABLE IF NOT EXISTS direct_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    sender_id INTEGER NOT NULL,
    recipient_id INTEGER NOT NULL,
    message TEXT NOT NULL,
    read_at DATETIME,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (sender_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (recipient_id) REFERENCES users(id) ON DELETE CASCADE
);

-- Index for fetching conversation threads (messages between two users)
CREATE INDEX idx_dm_participants ON direct_messages(sender_id, recipient_id, created_at DESC);

-- Index for finding unread messages for a recipient
CREATE INDEX idx_dm_recipient_unread ON direct_messages(recipient_id, read_at);
