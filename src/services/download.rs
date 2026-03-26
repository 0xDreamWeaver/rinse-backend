use std::path::PathBuf;
use std::sync::Arc;
use anyhow::Result;

use crate::db::Database;
use crate::models::{Item, List};
use crate::protocol::SoulseekClient;

/// Download service that manages Soulseek connection and data operations
///
/// Note: Actual search and download operations are now handled by QueueService.
/// This service is responsible for:
/// - Soulseek connection management
/// - Item/List CRUD operations
/// - File deletion from disk
pub struct DownloadService {
    db: Database,
    slsk_client: Option<Arc<SoulseekClient>>,
    #[allow(dead_code)]
    storage_path: PathBuf,
}

impl DownloadService {
    /// Create a new download service
    pub fn new(db: Database, storage_path: PathBuf) -> Self {
        Self {
            db,
            slsk_client: None,
            storage_path,
        }
    }

    /// Connect to Soulseek network
    ///
    /// Returns `Arc<SoulseekClient>` for sharing with other services.
    pub async fn connect(&mut self, username: &str, password: &str) -> Result<Arc<SoulseekClient>> {
        let client = SoulseekClient::connect(username, password).await?;
        self.slsk_client = Some(Arc::clone(&client));
        tracing::info!("Connected to Soulseek network as {}", username);
        Ok(client)
    }

    /// Get the Soulseek client Arc (for sharing with QueueService)
    pub fn slsk_client(&self) -> Option<Arc<SoulseekClient>> {
        self.slsk_client.clone()
    }

    /// Get item by ID
    pub async fn get_item(&self, id: i64) -> Result<Option<Item>> {
        self.db.get_item(id).await
    }

    /// Get all items
    pub async fn get_all_items(&self) -> Result<Vec<Item>> {
        self.db.get_all_items().await
    }

    /// Get list by ID
    pub async fn get_list(&self, id: i64) -> Result<Option<List>> {
        self.db.get_list(id).await
    }

    /// Get list with items
    pub async fn get_list_with_items(&self, id: i64) -> Result<Option<(List, Vec<Item>)>> {
        if let Some(list) = self.db.get_list(id).await? {
            let items = self.db.get_list_items(id).await?;
            Ok(Some((list, items)))
        } else {
            Ok(None)
        }
    }

    /// Get all lists for a user
    pub async fn get_user_lists(&self, user_id: i64) -> Result<Vec<List>> {
        self.db.get_user_lists(user_id).await
    }

    /// Delete list
    pub async fn delete_list(&self, id: i64) -> Result<()> {
        let list_name = self.db.get_list(id).await
            .ok().flatten()
            .map(|l| l.name)
            .unwrap_or_default();

        let result = self.db.delete_list(id).await;
        match &result {
            Ok(_) => tracing::info!("[DownloadService] Deleted list id={} '{}' (db: ok)", id, list_name),
            Err(e) => tracing::error!("[DownloadService] Failed to delete list id={}: {}", id, e),
        }
        result
    }

    /// Delete item
    pub async fn delete_item(&self, id: i64) -> Result<()> {
        let mut filename = String::new();
        let mut file_result = "no_file";

        if let Some(item) = self.db.get_item(id).await? {
            filename = item.filename.clone();
            file_result = match tokio::fs::remove_file(&item.file_path).await {
                Ok(_) => "ok",
                Err(_) => "failed",
            };
        }

        // Clean up cached cover art
        let _ = tokio::fs::remove_file(format!("{}/covers/{}_full.webp", self.storage_path.display(), id)).await;
        let _ = tokio::fs::remove_file(format!("{}/covers/{}_thumb.webp", self.storage_path.display(), id)).await;

        let result = self.db.delete_item(id).await;
        match &result {
            Ok(_) => tracing::info!("[DownloadService] Deleted item id={} '{}' (file: {}, db: ok)", id, filename, file_result),
            Err(e) => tracing::error!("[DownloadService] Failed to delete item id={}: {}", id, e),
        }
        result
    }

    /// Batch delete items
    pub async fn delete_items(&self, ids: Vec<i64>) -> Result<()> {
        let mut files_ok = 0;
        let mut files_failed = 0;
        for id in &ids {
            if let Some(item) = self.db.get_item(*id).await? {
                match tokio::fs::remove_file(&item.file_path).await {
                    Ok(_) => files_ok += 1,
                    Err(_) => files_failed += 1,
                }
            }
            // Clean up cached cover art
            let _ = tokio::fs::remove_file(format!("{}/covers/{}_full.webp", self.storage_path.display(), id)).await;
            let _ = tokio::fs::remove_file(format!("{}/covers/{}_thumb.webp", self.storage_path.display(), id)).await;
        }

        let result = self.db.delete_items(&ids).await;
        match &result {
            Ok(_) => tracing::info!(
                "[DownloadService] Batch deleted {} items (files: {} ok, {} failed; db: ok)",
                ids.len(), files_ok, files_failed
            ),
            Err(e) => tracing::error!("[DownloadService] Failed to batch delete items: {}", e),
        }
        result
    }

    /// Batch delete lists
    pub async fn delete_lists(&self, ids: Vec<i64>) -> Result<()> {
        let result = self.db.delete_lists(&ids).await;
        match &result {
            Ok(_) => tracing::info!("[DownloadService] Batch deleted {} lists (db: ok)", ids.len()),
            Err(e) => tracing::error!("[DownloadService] Failed to batch delete lists: {}", e),
        }
        result
    }

    /// Remove item from list (does not delete the item)
    pub async fn remove_item_from_list(&self, list_id: i64, item_id: i64) -> Result<()> {
        tracing::info!("[DownloadService] Removing item {} from list {}", item_id, list_id);
        self.db.remove_item_from_list(list_id, item_id).await
    }

    /// Batch remove items from list (does not delete items)
    pub async fn remove_items_from_list(&self, list_id: i64, item_ids: Vec<i64>) -> Result<()> {
        tracing::info!("[DownloadService] Removing {} items from list {}", item_ids.len(), list_id);
        self.db.remove_items_from_list(list_id, &item_ids).await
    }
}
