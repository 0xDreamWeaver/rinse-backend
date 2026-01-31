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
    pub async fn get_all_items(&self) -> Result<Vec<Item>> {
        let items = sqlx::query_as::<_, Item>(
            "SELECT * FROM items ORDER BY created_at DESC"
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
    pub async fn delete_item(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM items WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .context("Failed to delete item")?;

        Ok(())
    }

    /// Batch delete items
    pub async fn delete_items(&self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }

        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query_str = format!("DELETE FROM items WHERE id IN ({})", placeholders);

        let mut query = sqlx::query(&query_str);
        for id in ids {
            query = query.bind(id);
        }

        query.execute(&self.pool)
            .await
            .context("Failed to batch delete items")?;

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
}
