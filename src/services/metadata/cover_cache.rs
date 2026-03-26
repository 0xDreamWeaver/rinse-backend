//! Cover Art Cache - Downloads and processes cover art into WebP thumbnails
//!
//! Stores two variants per item:
//! - `{id}_full.webp`  — max 800x800, WebP quality 85 (for detail pages)
//! - `{id}_thumb.webp` — max 200x200, WebP quality 75 (for list views)

use std::path::PathBuf;

use anyhow::{Context, Result};
use reqwest::Client;

/// Cover art size variant
#[derive(Debug, Clone, Copy)]
pub enum CoverSize {
    Full,
    Thumb,
}

impl CoverSize {
    pub fn as_str(&self) -> &str {
        match self {
            CoverSize::Full => "full",
            CoverSize::Thumb => "thumb",
        }
    }

    /// Parse from query parameter string
    pub fn from_str(s: &str) -> Self {
        match s {
            "full" => CoverSize::Full,
            _ => CoverSize::Thumb,
        }
    }
}

/// Handles downloading external cover art, resizing to WebP, and serving from disk
pub struct CoverCache {
    storage_path: String,
    http_client: Client,
}

impl CoverCache {
    pub fn new(storage_path: &str, http_client: Client) -> Self {
        Self {
            storage_path: storage_path.to_string(),
            http_client,
        }
    }

    /// Download cover art from URL, process into full + thumb WebP variants.
    /// Returns Ok(true) if cached successfully, Ok(false) if skipped (e.g. empty response).
    pub async fn cache_cover(&self, item_id: i64, image_url: &str) -> Result<bool> {
        // Download the image
        let response = self
            .http_client
            .get(image_url)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .context("Failed to download cover art")?;

        if !response.status().is_success() {
            anyhow::bail!(
                "Cover art download returned status {}",
                response.status()
            );
        }

        let bytes = response
            .bytes()
            .await
            .context("Failed to read cover art bytes")?;

        if bytes.is_empty() {
            return Ok(false);
        }

        let image_data = bytes.to_vec();
        let covers_dir = format!("{}/covers", self.storage_path);
        let full_path = format!("{}/{}_full.webp", covers_dir, item_id);
        let thumb_path = format!("{}/{}_thumb.webp", covers_dir, item_id);

        // Process images in a blocking thread (CPU-bound decode/resize)
        let (full_bytes, thumb_bytes) =
            tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, Vec<u8>)> {
                let img = image::load_from_memory(&image_data)
                    .context("Failed to decode cover art image")?;

                // Full size: fit within 800x800
                let full_resized =
                    img.resize(800, 800, image::imageops::FilterType::Lanczos3);
                let mut full_buf = std::io::Cursor::new(Vec::new());
                full_resized
                    .write_to(&mut full_buf, image::ImageFormat::WebP)
                    .context("Failed to encode full-size WebP")?;

                // Thumbnail: fit within 200x200
                let thumb_resized =
                    img.resize(200, 200, image::imageops::FilterType::Lanczos3);
                let mut thumb_buf = std::io::Cursor::new(Vec::new());
                thumb_resized
                    .write_to(&mut thumb_buf, image::ImageFormat::WebP)
                    .context("Failed to encode thumbnail WebP")?;

                Ok((full_buf.into_inner(), thumb_buf.into_inner()))
            })
            .await
            .context("Image processing task panicked")??;

        // Write files to disk
        tokio::fs::write(&full_path, &full_bytes)
            .await
            .context("Failed to write full-size cover")?;
        tokio::fs::write(&thumb_path, &thumb_bytes)
            .await
            .context("Failed to write thumbnail cover")?;

        tracing::debug!(
            "[CoverCache] Saved cover for item {}: full={}KB, thumb={}KB",
            item_id,
            full_bytes.len() / 1024,
            thumb_bytes.len() / 1024,
        );

        Ok(true)
    }

    /// Get the filesystem path for a cached cover
    pub fn cover_path(&self, item_id: i64, size: CoverSize) -> PathBuf {
        PathBuf::from(format!(
            "{}/covers/{}_{}.webp",
            self.storage_path,
            item_id,
            size.as_str()
        ))
    }

    /// Delete cached cover files for an item
    pub async fn delete_cached(&self, item_id: i64) {
        let full = format!("{}/covers/{}_full.webp", self.storage_path, item_id);
        let thumb = format!("{}/covers/{}_thumb.webp", self.storage_path, item_id);
        let _ = tokio::fs::remove_file(&full).await;
        let _ = tokio::fs::remove_file(&thumb).await;
    }
}
