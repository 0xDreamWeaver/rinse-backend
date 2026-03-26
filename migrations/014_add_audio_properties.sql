-- Add sample_rate and bit_depth columns for audio quality display
ALTER TABLE items ADD COLUMN sample_rate INTEGER;
ALTER TABLE items ADD COLUMN bit_depth INTEGER;
