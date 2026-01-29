-- Add email and verification fields to users table
-- Note: SQLite doesn't support UNIQUE constraint in ALTER TABLE ADD COLUMN
-- We use a unique index instead
ALTER TABLE users ADD COLUMN email TEXT;
ALTER TABLE users ADD COLUMN email_verified INTEGER NOT NULL DEFAULT 0;
ALTER TABLE users ADD COLUMN verification_token TEXT;
ALTER TABLE users ADD COLUMN verification_token_expires_at DATETIME;

-- Create unique index for email lookups (enforces uniqueness)
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_email ON users(email);

-- Create index for verification token lookups
CREATE INDEX IF NOT EXISTS idx_users_verification_token ON users(verification_token);
