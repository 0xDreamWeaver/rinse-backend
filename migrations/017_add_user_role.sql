-- Add role column to users table
ALTER TABLE users ADD COLUMN role TEXT NOT NULL DEFAULT 'user';

-- Set existing admin user to admin role
UPDATE users SET role = 'admin' WHERE username = 'admin';
