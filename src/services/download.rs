use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use anyhow::{Result, bail, Context};

use crate::db::Database;
use crate::models::{Item, List, DownloadStatus};
use crate::protocol::SoulseekClient;
use crate::services::FuzzyMatcher;

/// Download service that manages file downloads from Soulseek
pub struct DownloadService {
    db: Database,
    slsk_client: Arc<RwLock<Option<SoulseekClient>>>,
    storage_path: PathBuf,
    fuzzy_matcher: FuzzyMatcher,
}

impl DownloadService {
    /// Create a new download service
    pub fn new(db: Database, storage_path: PathBuf) -> Self {
        Self {
            db,
            slsk_client: Arc::new(RwLock::new(None)),
            storage_path,
            fuzzy_matcher: FuzzyMatcher::new(70), // 70% similarity threshold
        }
    }

    /// Connect to Soulseek network
    pub async fn connect(&self, username: &str, password: &str) -> Result<()> {
        let client = SoulseekClient::connect(username, password).await?;
        *self.slsk_client.write().await = Some(client);
        tracing::info!("Connected to Soulseek network as {}", username);
        Ok(())
    }

    /// Check if connected to Soulseek
    pub async fn is_connected(&self) -> bool {
        self.slsk_client.read().await.is_some()
    }

    /// Search for a single item and download it
    pub async fn search_and_download_item(&self, query: &str, user_id: i64) -> Result<Item> {
        tracing::info!("[DownloadService] Starting search for: '{}'", query);

        // Check if we already have this item (fuzzy match)
        let existing_items = self.db.get_all_items().await?;
        tracing::debug!("[DownloadService] Checking {} existing items for duplicates", existing_items.len());

        if let Some(existing) = self.fuzzy_matcher.find_duplicate(query, &existing_items) {
            tracing::info!("[DownloadService] Found existing item matching query '{}': {}", query, existing.filename);
            return Ok(existing.clone());
        }

        // Get Soulseek client
        let client = self.slsk_client.read().await;
        let client = client.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Not connected to Soulseek"))?;

        tracing::info!("[DownloadService] Sending search query to Soulseek network...");

        // Search for the item
        let results = client.search(query, 30).await?;

        tracing::info!("[DownloadService] Search completed. Results: {} users returned files", results.len());

        if results.is_empty() {
            tracing::warn!("[DownloadService] No results found for query: '{}'", query);
            bail!("No results found for query: {}", query);
        }

        // Log result details
        for (i, result) in results.iter().enumerate() {
            tracing::info!(
                "[DownloadService] Result #{}: User '{}' - {} files, slots: {}, speed: {} KB/s",
                i + 1,
                result.username,
                result.files.len(),
                result.has_slots,
                result.avg_speed / 1024
            );
            for file in &result.files {
                tracing::debug!(
                    "[DownloadService]   - {} ({} bytes, bitrate: {:?})",
                    file.filename,
                    file.size,
                    file.bitrate()
                );
            }
        }

        // Get best file (highest bitrate)
        let (username, file) = SoulseekClient::get_best_file(&results)
            .ok_or_else(|| anyhow::anyhow!("No suitable files found"))?;

        // Create item record in database
        let filename = file.filename.clone();
        let extension = Path::new(&filename)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        let file_path = self.storage_path.join(&filename);

        let mut item = self.db.create_item(
            &filename,
            query,
            file_path.to_str().unwrap(),
            file.size as i64,
            file.bitrate().map(|b| b as i32),
            file.duration().map(|d| d as i32),
            &extension,
            &username,
        ).await?;

        // Update status to downloading
        self.db.update_item_status(item.id, "downloading", 0.0, None).await?;

        // Download the file (with retry on next best file)
        let download_result = client.download_file(&username, &filename, &file_path).await;

        match download_result {
            Ok(_bytes) => {
                // Update status to completed
                self.db.update_item_status(item.id, "completed", 1.0, None).await?;
                item.download_status = "completed".to_string();
                item.download_progress = 1.0;
                Ok(item)
            }
            Err(e) => {
                tracing::warn!("First download attempt failed: {}. Trying next best file...", e);

                // Try next best file
                let remaining_results: Vec<_> = results.iter()
                    .filter(|r| r.username != username)
                    .cloned()
                    .collect();

                if let Some((username2, file2)) = SoulseekClient::get_best_file(&remaining_results) {
                    let file_path2 = self.storage_path.join(&file2.filename);

                    match client.download_file(&username2, &file2.filename, &file_path2).await {
                        Ok(_bytes) => {
                            // Update item with new info and mark completed
                            self.db.update_item_status(item.id, "completed", 1.0, None).await?;
                            item.download_status = "completed".to_string();
                            item.download_progress = 1.0;
                            Ok(item)
                        }
                        Err(e2) => {
                            // Both attempts failed
                            let error_msg = format!("Both download attempts failed: {} / {}", e, e2);
                            self.db.update_item_status(item.id, "failed", 0.0, Some(&error_msg)).await?;
                            item.download_status = "failed".to_string();
                            item.error_message = Some(error_msg.clone());
                            bail!(error_msg);
                        }
                    }
                } else {
                    // No alternative files
                    let error_msg = format!("Download failed and no alternative files available: {}", e);
                    self.db.update_item_status(item.id, "failed", 0.0, Some(&error_msg)).await?;
                    item.download_status = "failed".to_string();
                    item.error_message = Some(error_msg.clone());
                    bail!(error_msg);
                }
            }
        }
    }

    /// Search for and download a list of items
    pub async fn search_and_download_list(
        &self,
        queries: Vec<String>,
        list_name: Option<String>,
        user_id: i64,
    ) -> Result<List> {
        let name = list_name.unwrap_or_else(|| {
            chrono::Utc::now().format("List %Y-%m-%d %H:%M").to_string()
        });

        // Create the list record
        let mut list = self.db.create_list(&name, user_id, queries.len() as i32).await?;

        // Download each item in parallel
        let handles: Vec<_> = queries.into_iter()
            .enumerate()
            .map(|(index, query)| {
                let service = DownloadService {
                    db: self.db.clone(),
                    slsk_client: Arc::clone(&self.slsk_client),
                    storage_path: self.storage_path.clone(),
                    fuzzy_matcher: FuzzyMatcher::new(70),
                };
                let list_id = list.id;

                tokio::spawn(async move {
                    match service.search_and_download_item(&query, user_id).await {
                        Ok(item) => {
                            // Add item to list
                            if let Err(e) = service.db.add_item_to_list(list_id, item.id, index as i32).await {
                                tracing::error!("Failed to add item {} to list {}: {}", item.id, list_id, e);
                            }
                            Ok(item)
                        }
                        Err(e) => {
                            tracing::error!("Failed to download item for query '{}': {}", query, e);
                            Err(e)
                        }
                    }
                })
            })
            .collect();

        // Wait for all downloads to complete
        let mut completed = 0;
        let mut failed = 0;

        for handle in handles {
            match handle.await {
                Ok(Ok(_)) => completed += 1,
                Ok(Err(_)) => failed += 1,
                Err(e) => {
                    tracing::error!("Task panicked: {}", e);
                    failed += 1;
                }
            }
        }

        // Update list status
        let status = if completed == list.total_items {
            "completed"
        } else if completed > 0 {
            "partial"
        } else {
            "failed"
        };

        self.db.update_list_status(list.id, status, completed, failed).await?;

        list.status = status.to_string();
        list.completed_items = completed;
        list.failed_items = failed;

        Ok(list)
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
        self.db.delete_list(id).await
    }

    /// Delete item
    pub async fn delete_item(&self, id: i64) -> Result<()> {
        // Also delete the file from disk
        if let Some(item) = self.db.get_item(id).await? {
            if let Err(e) = tokio::fs::remove_file(&item.file_path).await {
                tracing::warn!("Failed to delete file {}: {}", item.file_path, e);
            }
        }

        self.db.delete_item(id).await
    }

    /// Batch delete items
    pub async fn delete_items(&self, ids: Vec<i64>) -> Result<()> {
        // Delete files from disk
        for id in &ids {
            if let Some(item) = self.db.get_item(*id).await? {
                if let Err(e) = tokio::fs::remove_file(&item.file_path).await {
                    tracing::warn!("Failed to delete file {}: {}", item.file_path, e);
                }
            }
        }

        self.db.delete_items(&ids).await
    }

    /// Batch delete lists
    pub async fn delete_lists(&self, ids: Vec<i64>) -> Result<()> {
        self.db.delete_lists(&ids).await
    }
}
