-- Add retry_count column to search_queue for tracking retry attempts
--
-- Retry logic:
-- - retry_count = 0: First attempt (status was 'pending')
-- - retry_count = 1: Retry attempt (status was 'retry')
-- - If retry fails with retry_count >= 1, entry is deleted (permanent failure)
--
-- The 'retry' status functions like 'pending' - queue worker picks up both.
-- When a search fails:
--   - If retry_count == 0: set status='retry', update created_at (back of queue), increment retry_count
--   - If retry_count >= 1: delete the entry

ALTER TABLE search_queue ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0;

-- Update the index to include 'retry' status for queue processing
DROP INDEX IF EXISTS idx_search_queue_pending;
CREATE INDEX idx_search_queue_pending_retry ON search_queue(status, created_at)
    WHERE status IN ('pending', 'retry');
