use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
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
    slsk_client: Arc<RwLock<Option<SoulseekClient>>>,
    #[allow(dead_code)]
    storage_path: PathBuf,
}

impl DownloadService {
    /// Create a new download service
    pub fn new(db: Database, storage_path: PathBuf) -> Self {
        Self {
            db,
            slsk_client: Arc::new(RwLock::new(None)),
            storage_path,
        }
    }

    /// Connect to Soulseek network
    pub async fn connect(&self, username: &str, password: &str) -> Result<()> {
        let client = SoulseekClient::connect(username, password).await?;
        *self.slsk_client.write().await = Some(client);
        tracing::info!("Connected to Soulseek network as {}", username);
        Ok(())
    }

    /// Get a clone of the Soulseek client Arc (for sharing with QueueService)
    pub fn slsk_client(&self) -> Arc<RwLock<Option<SoulseekClient>>> {
        Arc::clone(&self.slsk_client)
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
        tracing::info!("[DownloadService] === DELETE LIST ===");
        tracing::info!("[DownloadService] List ID: {}", id);

        // Get list info before deletion for logging
        if let Ok(Some(list)) = self.db.get_list(id).await {
            tracing::info!("[DownloadService] List name: {}", list.name);
            tracing::info!("[DownloadService] List status: {}", list.status);
            tracing::info!("[DownloadService] List items: {}", list.total_items);
        }

        let result = self.db.delete_list(id).await;
        match &result {
            Ok(_) => tracing::info!("[DownloadService] List {} deleted successfully from database", id),
            Err(e) => tracing::error!("[DownloadService] Failed to delete list {} from database: {}", id, e),
        }
        result
    }

    /// Delete item
    pub async fn delete_item(&self, id: i64) -> Result<()> {
        tracing::info!("[DownloadService] === DELETE ITEM ===");
        tracing::info!("[DownloadService] Item ID: {}", id);

        // Also delete the file from disk
        if let Some(item) = self.db.get_item(id).await? {
            tracing::info!("[DownloadService] Item filename: {}", item.filename);
            tracing::info!("[DownloadService] Item file_path: {}", item.file_path);
            tracing::info!("[DownloadService] Item status: {}", item.download_status);

            tracing::info!("[DownloadService] Attempting to delete file from disk...");
            match tokio::fs::remove_file(&item.file_path).await {
                Ok(_) => tracing::info!("[DownloadService] File deleted from disk: {}", item.file_path),
                Err(e) => tracing::warn!("[DownloadService] Failed to delete file {} from disk: {}", item.file_path, e),
            }
        } else {
            tracing::warn!("[DownloadService] Item {} not found in database", id);
        }

        tracing::info!("[DownloadService] Deleting item {} from database...", id);
        let result = self.db.delete_item(id).await;
        match &result {
            Ok(_) => tracing::info!("[DownloadService] Item {} deleted successfully from database", id),
            Err(e) => tracing::error!("[DownloadService] Failed to delete item {} from database: {}", id, e),
        }
        result
    }

    /// Batch delete items
    pub async fn delete_items(&self, ids: Vec<i64>) -> Result<()> {
        tracing::info!("[DownloadService] === BATCH DELETE ITEMS ===");
        tracing::info!("[DownloadService] Item IDs: {:?}", ids);
        tracing::info!("[DownloadService] Count: {}", ids.len());

        // Delete files from disk
        let mut files_deleted = 0;
        let mut files_failed = 0;
        for id in &ids {
            if let Some(item) = self.db.get_item(*id).await? {
                tracing::info!("[DownloadService] Processing item {}: {} ({})", id, item.filename, item.file_path);
                match tokio::fs::remove_file(&item.file_path).await {
                    Ok(_) => {
                        files_deleted += 1;
                        tracing::info!("[DownloadService] Deleted file: {}", item.file_path);
                    }
                    Err(e) => {
                        files_failed += 1;
                        tracing::warn!("[DownloadService] Failed to delete file {}: {}", item.file_path, e);
                    }
                }
            } else {
                tracing::warn!("[DownloadService] Item {} not found in database", id);
            }
        }

        tracing::info!("[DownloadService] Files deleted: {}, Files failed: {}", files_deleted, files_failed);
        tracing::info!("[DownloadService] Deleting {} items from database...", ids.len());

        let result = self.db.delete_items(&ids).await;
        match &result {
            Ok(_) => tracing::info!("[DownloadService] {} items deleted successfully from database", ids.len()),
            Err(e) => tracing::error!("[DownloadService] Failed to delete items from database: {}", e),
        }
        result
    }

    /// Batch delete lists
    pub async fn delete_lists(&self, ids: Vec<i64>) -> Result<()> {
        tracing::info!("[DownloadService] === BATCH DELETE LISTS ===");
        tracing::info!("[DownloadService] List IDs: {:?}", ids);
        tracing::info!("[DownloadService] Count: {}", ids.len());

        // Log each list before deletion
        for id in &ids {
            if let Ok(Some(list)) = self.db.get_list(*id).await {
                tracing::info!("[DownloadService] List {}: {} (status: {}, items: {})",
                    id, list.name, list.status, list.total_items);
            }
        }

        let result = self.db.delete_lists(&ids).await;
        match &result {
            Ok(_) => tracing::info!("[DownloadService] {} lists deleted successfully from database", ids.len()),
            Err(e) => tracing::error!("[DownloadService] Failed to delete lists from database: {}", e),
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
