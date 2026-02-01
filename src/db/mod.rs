use sqlx::{sqlite::SqlitePool, SqliteConnection, Row};
use anyhow::{Result, Context};
use crate::models::*;

#[cfg(test)]
mod tests;

/// Database connection pool
#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Create a new database connection
    pub async fn new(database_url: &str) -> Result<Self> {
        let pool = SqlitePool::connect(database_url)
            .await
            .context("Failed to connect to database")?;

        // Run migrations
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("Failed to run migrations")?;

        Ok(Self { pool })
    }

    /// Get the pool
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // User methods

    /// Create a new user with email and verification token
    pub async fn create_user(
        &self,
        username: &str,
        email: &str,
        password_hash: &str,
        verification_token: &str,
        token_expires_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<User> {
        let user = sqlx::query_as::<_, User>(
            r#"
            INSERT INTO users (username, email, password_hash, verification_token, verification_token_expires_at)
            VALUES (?, ?, ?, ?, ?)
            RETURNING *
            "#
        )
        .bind(username)
        .bind(email)
        .bind(password_hash)
        .bind(verification_token)
        .bind(token_expires_at)
        .fetch_one(&self.pool)
        .await
        .context("Failed to create user")?;

        Ok(user)
    }

    /// Create a user without email verification (for admin/legacy users)
    pub async fn create_user_verified(&self, username: &str, password_hash: &str) -> Result<User> {
        let user = sqlx::query_as::<_, User>(
            r#"
            INSERT INTO users (username, password_hash, email_verified)
            VALUES (?, ?, 1)
            RETURNING *
            "#
        )
        .bind(username)
        .bind(password_hash)
        .fetch_one(&self.pool)
        .await
        .context("Failed to create user")?;

        Ok(user)
    }

    /// Get user by username
    pub async fn get_user_by_username(&self, username: &str) -> Result<Option<User>> {
        let user = sqlx::query_as::<_, User>(
            "SELECT * FROM users WHERE username = ?"
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to get user")?;

        Ok(user)
    }

    /// Get user by ID
    pub async fn get_user_by_id(&self, id: i64) -> Result<Option<User>> {
        let user = sqlx::query_as::<_, User>(
            "SELECT * FROM users WHERE id = ?"
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to get user")?;

        Ok(user)
    }

    /// Get user by email
    pub async fn get_user_by_email(&self, email: &str) -> Result<Option<User>> {
        let user = sqlx::query_as::<_, User>(
            "SELECT * FROM users WHERE email = ?"
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to get user by email")?;

        Ok(user)
    }

    /// Get user by email or username (for login)
    pub async fn get_user_by_identifier(&self, identifier: &str) -> Result<Option<User>> {
        let user = sqlx::query_as::<_, User>(
            "SELECT * FROM users WHERE email = ? OR username = ?"
        )
        .bind(identifier)
        .bind(identifier)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to get user by identifier")?;

        Ok(user)
    }

    /// Get user by verification token
    pub async fn get_user_by_verification_token(&self, token: &str) -> Result<Option<User>> {
        let user = sqlx::query_as::<_, User>(
            "SELECT * FROM users WHERE verification_token = ?"
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to get user by verification token")?;

        Ok(user)
    }

    /// Mark user email as verified
    pub async fn verify_user_email(&self, user_id: i64) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE users
            SET email_verified = 1, verification_token = NULL, verification_token_expires_at = NULL, updated_at = CURRENT_TIMESTAMP
            WHERE id = ?
            "#
        )
        .bind(user_id)
        .execute(&self.pool)
        .await
        .context("Failed to verify user email")?;

        Ok(())
    }

    /// Update verification token (for resend)
    pub async fn update_verification_token(
        &self,
        user_id: i64,
        token: &str,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE users
            SET verification_token = ?, verification_token_expires_at = ?, updated_at = CURRENT_TIMESTAMP
            WHERE id = ?
            "#
        )
        .bind(token)
        .bind(expires_at)
        .bind(user_id)
        .execute(&self.pool)
        .await
        .context("Failed to update verification token")?;

        Ok(())
    }

    /// Delete a user by ID
    pub async fn delete_user(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .context("Failed to delete user")?;

        Ok(())
    }

    // Item methods

    /// Create a new item
    pub async fn create_item(
        &self,
        filename: &str,
        original_query: &str,
        file_path: &str,
        file_size: i64,
        bitrate: Option<i32>,
        duration: Option<i32>,
        extension: &str,
        source_username: &str,
    ) -> Result<Item> {
        let item = sqlx::query_as::<_, Item>(
            r#"
            INSERT INTO items (filename, original_query, file_path, file_size, bitrate, duration, extension, source_username)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            RETURNING *
            "#
        )
        .bind(filename)
        .bind(original_query)
        .bind(file_path)
        .bind(file_size)
        .bind(bitrate)
        .bind(duration)
        .bind(extension)
        .bind(source_username)
        .fetch_one(&self.pool)
        .await
        .context("Failed to create item")?;

        Ok(item)
    }

    /// Get item by ID
    pub async fn get_item(&self, id: i64) -> Result<Option<Item>> {
        let item = sqlx::query_as::<_, Item>(
            "SELECT * FROM items WHERE id = ?"
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to get item")?;

        Ok(item)
    }

    /// Get all items
    /// Get all items (excludes soft-deleted items)
    pub async fn get_all_items(&self) -> Result<Vec<Item>> {
        let items = sqlx::query_as::<_, Item>(
            "SELECT * FROM items WHERE download_status != 'deleted' ORDER BY created_at DESC"
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to get items")?;

        Ok(items)
    }

    /// Find item by filename (fuzzy match)
    pub async fn find_item_by_filename(&self, filename: &str) -> Result<Option<Item>> {
        let item = sqlx::query_as::<_, Item>(
            "SELECT * FROM items WHERE filename LIKE ? LIMIT 1"
        )
        .bind(format!("%{}%", filename))
        .fetch_optional(&self.pool)
        .await
        .context("Failed to find item")?;

        Ok(item)
    }

    /// Find completed item by original query (exact match)
    pub async fn find_completed_item_by_query(&self, query: &str) -> Result<Option<Item>> {
        let item = sqlx::query_as::<_, Item>(
            "SELECT * FROM items WHERE original_query = ? AND download_status = 'completed' LIMIT 1"
        )
        .bind(query)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to find item by query")?;

        Ok(item)
    }

    /// Find completed items with similar queries (for fuzzy duplicate detection)
    /// Returns all completed items to allow fuzzy matching in application code
    pub async fn get_completed_items(&self) -> Result<Vec<Item>> {
        let items = sqlx::query_as::<_, Item>(
            "SELECT * FROM items WHERE download_status = 'completed' ORDER BY created_at DESC"
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to get completed items")?;

        Ok(items)
    }

    /// Find item by file path
    pub async fn find_item_by_path(&self, file_path: &str) -> Result<Option<Item>> {
        let item = sqlx::query_as::<_, Item>(
            "SELECT * FROM items WHERE file_path = ?"
        )
        .bind(file_path)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to find item by path")?;

        Ok(item)
    }

    /// Update item status
    pub async fn update_item_status(
        &self,
        id: i64,
        status: &str,
        progress: f64,
        error_message: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE items
            SET download_status = ?, download_progress = ?, error_message = ?,
                completed_at = CASE WHEN ? = 'completed' THEN CURRENT_TIMESTAMP ELSE completed_at END
            WHERE id = ?
            "#
        )
        .bind(status)
        .bind(progress)
        .bind(error_message)
        .bind(status)
        .bind(id)
        .execute(&self.pool)
        .await
        .context("Failed to update item status")?;

        Ok(())
    }

    /// Update item file info (used when retry downloads from a different peer)
    pub async fn update_item_file_info(
        &self,
        id: i64,
        filename: &str,
        file_path: &str,
        source_username: &str,
        file_size: i64,
        bitrate: Option<i32>,
    ) -> Result<()> {
        let extension = std::path::Path::new(filename)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        sqlx::query(
            r#"
            UPDATE items
            SET filename = ?, file_path = ?, source_username = ?, file_size = ?, bitrate = ?, extension = ?
            WHERE id = ?
            "#
        )
        .bind(filename)
        .bind(file_path)
        .bind(source_username)
        .bind(file_size)
        .bind(bitrate)
        .bind(&extension)
        .bind(id)
        .execute(&self.pool)
        .await
        .context("Failed to update item file info")?;

        Ok(())
    }

    // List methods

    /// Create a new list
    pub async fn create_list(&self, name: &str, user_id: i64, total_items: i32) -> Result<List> {
        let list = sqlx::query_as::<_, List>(
            r#"
            INSERT INTO lists (name, user_id, total_items)
            VALUES (?, ?, ?)
            RETURNING *
            "#
        )
        .bind(name)
        .bind(user_id)
        .bind(total_items)
        .fetch_one(&self.pool)
        .await
        .context("Failed to create list")?;

        Ok(list)
    }

    /// Get list by ID
    pub async fn get_list(&self, id: i64) -> Result<Option<List>> {
        let list = sqlx::query_as::<_, List>(
            "SELECT * FROM lists WHERE id = ?"
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to get list")?;

        Ok(list)
    }

    /// Get all lists for a user
    pub async fn get_user_lists(&self, user_id: i64) -> Result<Vec<List>> {
        let lists = sqlx::query_as::<_, List>(
            "SELECT * FROM lists WHERE user_id = ? ORDER BY created_at DESC"
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .context("Failed to get user lists")?;

        Ok(lists)
    }

    /// Add item to list
    pub async fn add_item_to_list(&self, list_id: i64, item_id: i64, position: i32) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO list_items (list_id, item_id, position)
            VALUES (?, ?, ?)
            "#
        )
        .bind(list_id)
        .bind(item_id)
        .bind(position)
        .execute(&self.pool)
        .await
        .context("Failed to add item to list")?;

        Ok(())
    }

    /// Remove item from list (does not delete the item itself)
    pub async fn remove_item_from_list(&self, list_id: i64, item_id: i64) -> Result<()> {
        let result = sqlx::query("DELETE FROM list_items WHERE list_id = ? AND item_id = ?")
            .bind(list_id)
            .bind(item_id)
            .execute(&self.pool)
            .await
            .context("Failed to remove item from list")?;

        // Update total_items count if we actually removed something
        if result.rows_affected() > 0 {
            sqlx::query("UPDATE lists SET total_items = total_items - 1 WHERE id = ? AND total_items > 0")
                .bind(list_id)
                .execute(&self.pool)
                .await
                .context("Failed to update list total_items")?;
        }

        Ok(())
    }

    /// Batch remove items from list
    pub async fn remove_items_from_list(&self, list_id: i64, item_ids: &[i64]) -> Result<()> {
        if item_ids.is_empty() {
            return Ok(());
        }

        let placeholders = item_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query_str = format!(
            "DELETE FROM list_items WHERE list_id = ? AND item_id IN ({})",
            placeholders
        );

        let mut query = sqlx::query(&query_str).bind(list_id);
        for id in item_ids {
            query = query.bind(id);
        }

        let result = query.execute(&self.pool)
            .await
            .context("Failed to batch remove items from list")?;

        // Update total_items count by the number of items actually removed
        let removed_count = result.rows_affected() as i64;
        if removed_count > 0 {
            sqlx::query("UPDATE lists SET total_items = MAX(0, total_items - ?) WHERE id = ?")
                .bind(removed_count)
                .bind(list_id)
                .execute(&self.pool)
                .await
                .context("Failed to update list total_items")?;
        }

        Ok(())
    }

    /// Get items in a list
    pub async fn get_list_items(&self, list_id: i64) -> Result<Vec<Item>> {
        let items = sqlx::query_as::<_, Item>(
            r#"
            SELECT items.*
            FROM items
            INNER JOIN list_items ON items.id = list_items.item_id
            WHERE list_items.list_id = ?
            ORDER BY list_items.position
            "#
        )
        .bind(list_id)
        .fetch_all(&self.pool)
        .await
        .context("Failed to get list items")?;

        Ok(items)
    }

    /// Count list items by download status
    /// Returns (completed_count, failed_count, total_in_list)
    pub async fn count_list_items_by_status(&self, list_id: i64) -> Result<(i32, i32, i32)> {
        let row = sqlx::query(
            r#"
            SELECT
                COUNT(*) as total,
                SUM(CASE WHEN items.download_status = 'completed' THEN 1 ELSE 0 END) as completed,
                SUM(CASE WHEN items.download_status = 'failed' THEN 1 ELSE 0 END) as failed
            FROM list_items
            INNER JOIN items ON items.id = list_items.item_id
            WHERE list_items.list_id = ?
            "#
        )
        .bind(list_id)
        .fetch_one(&self.pool)
        .await
        .context("Failed to count list items by status")?;

        let total: i32 = row.get::<i64, _>("total") as i32;
        let completed: i32 = row.get::<i64, _>("completed") as i32;
        let failed: i32 = row.get::<i64, _>("failed") as i32;

        Ok((completed, failed, total))
    }

    /// Update list status
    pub async fn update_list_status(
        &self,
        id: i64,
        status: &str,
        completed_items: i32,
        failed_items: i32,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE lists
            SET status = ?, completed_items = ?, failed_items = ?,
                completed_at = CASE WHEN ? = 'completed' THEN CURRENT_TIMESTAMP ELSE completed_at END
            WHERE id = ?
            "#
        )
        .bind(status)
        .bind(completed_items)
        .bind(failed_items)
        .bind(status)
        .bind(id)
        .execute(&self.pool)
        .await
        .context("Failed to update list status")?;

        Ok(())
    }

    /// Delete list
    pub async fn delete_list(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM lists WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .context("Failed to delete list")?;

        Ok(())
    }

    /// Delete item
    /// Soft delete item - marks status as 'deleted' instead of removing record
    /// This preserves the item in lists while indicating it's been removed
    pub async fn delete_item(&self, id: i64) -> Result<()> {
        sqlx::query(
            "UPDATE items SET download_status = 'deleted', download_progress = 0.0 WHERE id = ?"
        )
            .bind(id)
            .execute(&self.pool)
            .await
            .context("Failed to soft delete item")?;

        Ok(())
    }

    /// Batch soft delete items
    pub async fn delete_items(&self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }

        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query_str = format!(
            "UPDATE items SET download_status = 'deleted', download_progress = 0.0 WHERE id IN ({})",
            placeholders
        );

        let mut query = sqlx::query(&query_str);
        for id in ids {
            query = query.bind(id);
        }

        query.execute(&self.pool)
            .await
            .context("Failed to batch soft delete items")?;

        Ok(())
    }

    /// Batch delete lists
    pub async fn delete_lists(&self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }

        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query_str = format!("DELETE FROM lists WHERE id IN ({})", placeholders);

        let mut query = sqlx::query(&query_str);
        for id in ids {
            query = query.bind(id);
        }

        query.execute(&self.pool)
            .await
            .context("Failed to batch delete lists")?;

        Ok(())
    }

    // ========================================================================
    // Search Queue methods
    // ========================================================================

    /// Enqueue a single search
    pub async fn enqueue_search(
        &self,
        user_id: i64,
        query: &str,
        format: Option<&str>,
        list_id: Option<i64>,
        list_position: Option<i32>,
        client_id: Option<&str>,
    ) -> Result<QueuedSearch> {
        let queued = sqlx::query_as::<_, QueuedSearch>(
            r#"
            INSERT INTO search_queue (user_id, query, format, list_id, list_position, status, client_id)
            VALUES (?, ?, ?, ?, ?, 'pending', ?)
            RETURNING *
            "#
        )
        .bind(user_id)
        .bind(query)
        .bind(format)
        .bind(list_id)
        .bind(list_position)
        .bind(client_id)
        .fetch_one(&self.pool)
        .await
        .context("Failed to enqueue search")?;

        Ok(queued)
    }

    /// Enqueue multiple searches for a list
    /// Returns the list of created queue entries
    pub async fn enqueue_searches_for_list(
        &self,
        user_id: i64,
        queries: &[String],
        format: Option<&str>,
        list_id: i64,
    ) -> Result<Vec<QueuedSearch>> {
        let mut results = Vec::with_capacity(queries.len());

        for (position, query) in queries.iter().enumerate() {
            let queued = self.enqueue_search(
                user_id,
                query,
                format,
                Some(list_id),
                Some(position as i32),
                None, // List searches don't need individual client IDs
            ).await?;
            results.push(queued);
        }

        Ok(results)
    }

    /// Get the next pending or retry search (FIFO order)
    /// Both 'pending' and 'retry' statuses are picked up for processing
    pub async fn get_next_pending_search(&self) -> Result<Option<QueuedSearch>> {
        let queued = sqlx::query_as::<_, QueuedSearch>(
            r#"
            SELECT * FROM search_queue
            WHERE status IN ('pending', 'retry')
            ORDER BY created_at ASC
            LIMIT 1
            "#
        )
        .fetch_optional(&self.pool)
        .await
        .context("Failed to get next pending search")?;

        Ok(queued)
    }

    /// Mark a search as processing (sets started_at)
    /// Returns true if the update succeeded (entry was pending or retry)
    pub async fn mark_search_processing(&self, id: i64) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE search_queue
            SET status = 'processing', started_at = CURRENT_TIMESTAMP
            WHERE id = ? AND status IN ('pending', 'retry')
            "#
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .context("Failed to mark search as processing")?;

        Ok(result.rows_affected() > 0)
    }

    /// Mark a search as completed (sets item_id and completed_at)
    pub async fn mark_search_completed(&self, id: i64, item_id: i64) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE search_queue
            SET status = 'completed', item_id = ?, completed_at = CURRENT_TIMESTAMP
            WHERE id = ?
            "#
        )
        .bind(item_id)
        .bind(id)
        .execute(&self.pool)
        .await
        .context("Failed to mark search as completed")?;

        Ok(())
    }

    /// Delete a completed search entry from the queue
    /// Called after list progress is updated to keep the queue clean
    pub async fn delete_completed_search(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM search_queue WHERE id = ? AND status = 'completed'")
            .bind(id)
            .execute(&self.pool)
            .await
            .context("Failed to delete completed search")?;

        Ok(())
    }

    /// Mark a search as failed (sets error_message and completed_at)
    pub async fn mark_search_failed(&self, id: i64, error_message: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE search_queue
            SET status = 'failed', error_message = ?, completed_at = CURRENT_TIMESTAMP
            WHERE id = ?
            "#
        )
        .bind(error_message)
        .bind(id)
        .execute(&self.pool)
        .await
        .context("Failed to mark search as failed")?;

        Ok(())
    }

    /// Mark a search for retry (resets to back of queue with incremented retry_count)
    /// Sets status='retry', updates created_at to now (moves to back of queue),
    /// increments retry_count, clears started_at and completed_at
    pub async fn mark_search_for_retry(&self, id: i64, error_message: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE search_queue
            SET status = 'retry',
                error_message = ?,
                retry_count = retry_count + 1,
                created_at = CURRENT_TIMESTAMP,
                started_at = NULL,
                completed_at = NULL
            WHERE id = ?
            "#
        )
        .bind(error_message)
        .bind(id)
        .execute(&self.pool)
        .await
        .context("Failed to mark search for retry")?;

        Ok(())
    }

    /// Delete a search entry permanently (used when retry also fails)
    pub async fn delete_search(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM search_queue WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .context("Failed to delete search")?;

        Ok(())
    }

    /// Get queue status counts
    pub async fn get_queue_status(&self) -> Result<(i64, i64)> {
        let row = sqlx::query(
            r#"
            SELECT
                COALESCE(SUM(CASE WHEN status = 'pending' THEN 1 ELSE 0 END), 0) as pending,
                COALESCE(SUM(CASE WHEN status = 'processing' THEN 1 ELSE 0 END), 0) as processing
            FROM search_queue
            "#
        )
        .fetch_one(&self.pool)
        .await
        .context("Failed to get queue status")?;

        let pending: i64 = row.get("pending");
        let processing: i64 = row.get("processing");

        Ok((pending, processing))
    }

    /// Get queue status counts for a specific user
    pub async fn get_user_queue_status(&self, user_id: i64) -> Result<(i64, i64)> {
        let row = sqlx::query(
            r#"
            SELECT
                COALESCE(SUM(CASE WHEN status = 'pending' THEN 1 ELSE 0 END), 0) as pending,
                COALESCE(SUM(CASE WHEN status = 'processing' THEN 1 ELSE 0 END), 0) as processing
            FROM search_queue
            WHERE user_id = ?
            "#
        )
        .bind(user_id)
        .fetch_one(&self.pool)
        .await
        .context("Failed to get user queue status")?;

        let pending: i64 = row.get("pending");
        let processing: i64 = row.get("processing");

        Ok((pending, processing))
    }

    /// Get count of active downloads (items with status 'downloading')
    pub async fn get_active_downloads_count(&self) -> Result<i64> {
        let row = sqlx::query(
            "SELECT COUNT(*) as count FROM items WHERE download_status = 'downloading'"
        )
        .fetch_one(&self.pool)
        .await
        .context("Failed to get active downloads count")?;

        let count: i64 = row.get("count");
        Ok(count)
    }

    /// Get queue position for a search entry
    /// Returns the number of pending items created before this one
    pub async fn get_queue_position(&self, id: i64) -> Result<i64> {
        let row = sqlx::query(
            r#"
            SELECT COUNT(*) as position
            FROM search_queue
            WHERE status = 'pending'
            AND created_at < (SELECT created_at FROM search_queue WHERE id = ?)
            "#
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .context("Failed to get queue position")?;

        let position: i64 = row.get("position");
        Ok(position)
    }

    /// Get all queue items for a user
    pub async fn get_user_queue_items(&self, user_id: i64) -> Result<Vec<QueuedSearch>> {
        let items = sqlx::query_as::<_, QueuedSearch>(
            r#"
            SELECT * FROM search_queue
            WHERE user_id = ?
            ORDER BY created_at DESC
            "#
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .context("Failed to get user queue items")?;

        Ok(items)
    }

    /// Get pending queue items for a user (for display)
    pub async fn get_user_pending_queue(&self, user_id: i64) -> Result<Vec<QueuedSearch>> {
        let items = sqlx::query_as::<_, QueuedSearch>(
            r#"
            SELECT * FROM search_queue
            WHERE user_id = ? AND status IN ('pending', 'processing')
            ORDER BY created_at ASC
            "#
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .context("Failed to get user pending queue")?;

        Ok(items)
    }

    /// Get queue items for a list
    pub async fn get_list_queue_items(&self, list_id: i64) -> Result<Vec<QueuedSearch>> {
        let items = sqlx::query_as::<_, QueuedSearch>(
            r#"
            SELECT * FROM search_queue
            WHERE list_id = ?
            ORDER BY list_position ASC
            "#
        )
        .bind(list_id)
        .fetch_all(&self.pool)
        .await
        .context("Failed to get list queue items")?;

        Ok(items)
    }

    /// Cancel a pending search (only works if still pending)
    /// Returns true if cancelled, false if not pending
    pub async fn cancel_pending_search(&self, id: i64) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM search_queue WHERE id = ? AND status = 'pending'"
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .context("Failed to cancel pending search")?;

        Ok(result.rows_affected() > 0)
    }

    /// Cancel all pending searches for a list
    pub async fn cancel_list_pending_searches(&self, list_id: i64) -> Result<u64> {
        let result = sqlx::query(
            "DELETE FROM search_queue WHERE list_id = ? AND status = 'pending'"
        )
        .bind(list_id)
        .execute(&self.pool)
        .await
        .context("Failed to cancel list pending searches")?;

        Ok(result.rows_affected())
    }

    // ========================================================================
    // Recovery methods (called on startup)
    // ========================================================================

    /// Reset any 'processing' searches back to 'pending' (for restart recovery)
    /// Returns the number of searches reset
    pub async fn recover_processing_searches(&self) -> Result<u64> {
        let result = sqlx::query(
            r#"
            UPDATE search_queue
            SET status = 'pending', started_at = NULL
            WHERE status = 'processing'
            "#
        )
        .execute(&self.pool)
        .await
        .context("Failed to recover processing searches")?;

        Ok(result.rows_affected())
    }

    /// Mark any 'downloading' items as failed (for restart recovery)
    /// Returns the number of items marked as failed
    pub async fn mark_interrupted_downloads(&self) -> Result<u64> {
        let result = sqlx::query(
            r#"
            UPDATE items
            SET download_status = 'failed',
                error_message = 'Download interrupted by server restart'
            WHERE download_status = 'downloading'
            "#
        )
        .execute(&self.pool)
        .await
        .context("Failed to mark interrupted downloads")?;

        Ok(result.rows_affected())
    }

    /// Get a queued search by ID
    pub async fn get_queued_search(&self, id: i64) -> Result<Option<QueuedSearch>> {
        let queued = sqlx::query_as::<_, QueuedSearch>(
            "SELECT * FROM search_queue WHERE id = ?"
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to get queued search")?;

        Ok(queued)
    }
}
