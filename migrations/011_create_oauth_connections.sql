-- OAuth connections for external services (Spotify, Tidal, etc.)
-- Tokens are encrypted at rest using AES-256-GCM

CREATE TABLE IF NOT EXISTS oauth_connections (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL,

    -- Service identifier: 'spotify', 'tidal', 'soundcloud', 'beatport'
    service TEXT NOT NULL,

    -- External service user info
    external_user_id TEXT,
    external_username TEXT,

    -- OAuth tokens (encrypted with AES-256-GCM)
    access_token_encrypted TEXT NOT NULL,
    refresh_token_encrypted TEXT,

    -- Token expiry (UTC)
    token_expires_at DATETIME,

    -- OAuth scopes granted (JSON array)
    scopes TEXT,

    -- Connection status: 'active', 'expired', 'revoked'
    status TEXT NOT NULL DEFAULT 'active',

    -- Timestamps
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_used_at DATETIME,

    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    UNIQUE(user_id, service)
);

CREATE INDEX IF NOT EXISTS idx_oauth_connections_user_id ON oauth_connections(user_id);
CREATE INDEX IF NOT EXISTS idx_oauth_connections_service ON oauth_connections(service);
CREATE INDEX IF NOT EXISTS idx_oauth_connections_status ON oauth_connections(status);

-- Temporary storage for OAuth state during authorization flow
-- Entries expire after 5 minutes
CREATE TABLE IF NOT EXISTS oauth_pending_states (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL,
    service TEXT NOT NULL,
    state TEXT NOT NULL UNIQUE,
    code_verifier TEXT NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at DATETIME NOT NULL,

    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_oauth_pending_states_state ON oauth_pending_states(state);
CREATE INDEX IF NOT EXISTS idx_oauth_pending_states_expires ON oauth_pending_states(expires_at);
