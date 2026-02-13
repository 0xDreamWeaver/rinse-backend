//! Cover Art Archive API client for album artwork
//!
//! The Cover Art Archive (CAA) is a joint project between MusicBrainz and
//! Internet Archive that provides album artwork linked to MusicBrainz releases.
//!
//! Requires a MusicBrainz Release MBID to look up artwork.
//! No rate limiting required for CAA.

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use tracing;

/// Cover Art Archive client
pub struct CoverArtClient {
    http_client: Client,
}

/// CAA response for release artwork
#[derive(Debug, Deserialize)]
struct CoverArtResponse {
    images: Vec<CoverArtImage>,
}

#[derive(Debug, Deserialize)]
struct CoverArtImage {
    /// URL to the full-size image
    image: String,
    /// Available thumbnail sizes
    thumbnails: Thumbnails,
    /// Whether this is the front cover
    front: bool,
    /// Image types (e.g., "Front", "Back")
    types: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Thumbnails {
    /// 250px thumbnail
    #[serde(rename = "250")]
    small: Option<String>,
    /// 500px thumbnail
    #[serde(rename = "500")]
    large: Option<String>,
    /// 1200px thumbnail
    #[serde(rename = "1200")]
    xlarge: Option<String>,
}

impl CoverArtClient {
    /// Create a new Cover Art Archive client
    pub fn new(http_client: Client) -> Self {
        Self { http_client }
    }

    /// Get the front cover URL for a MusicBrainz release
    ///
    /// # Arguments
    /// * `release_mbid` - MusicBrainz Release ID
    ///
    /// # Returns
    /// * URL to the cover art image (500px version preferred)
    pub async fn get_front_cover_url(&self, release_mbid: &str) -> Result<String> {
        tracing::debug!("[CoverArt] Looking up cover for release: {}", release_mbid);

        // First try to get the cover art metadata
        let url = format!("https://coverartarchive.org/release/{}", release_mbid);

        let response = self
            .http_client
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await
            .context("Failed to send Cover Art Archive request")?;

        match response.status().as_u16() {
            200 => {
                let cover_art: CoverArtResponse = response
                    .json()
                    .await
                    .context("Failed to parse Cover Art Archive response")?;

                // Find the front cover
                let front_cover = cover_art
                    .images
                    .into_iter()
                    .find(|img| img.front || img.types.contains(&"Front".to_string()))
                    .ok_or_else(|| anyhow::anyhow!("No front cover found"))?;

                // Prefer 500px thumbnail, fall back to others
                let url = front_cover
                    .thumbnails
                    .large
                    .or(front_cover.thumbnails.xlarge)
                    .or(front_cover.thumbnails.small)
                    .unwrap_or(front_cover.image);

                tracing::info!("[CoverArt] Found cover art: {}", url);
                Ok(url)
            }
            404 => {
                tracing::debug!("[CoverArt] No cover art found for release: {}", release_mbid);
                anyhow::bail!("No cover art available for release: {}", release_mbid)
            }
            status => {
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!("Cover Art Archive error: {} - {}", status, body)
            }
        }
    }

    /// Get the front cover URL directly (redirects to image)
    ///
    /// This is a simpler approach that uses CAA's redirect endpoint.
    /// Returns the final URL after following redirects.
    pub async fn get_front_cover_url_direct(&self, release_mbid: &str) -> Result<String> {
        tracing::debug!(
            "[CoverArt] Getting direct cover URL for release: {}",
            release_mbid
        );

        // Use the front-500 endpoint which redirects to the actual image
        let url = format!(
            "https://coverartarchive.org/release/{}/front-500",
            release_mbid
        );

        // Send HEAD request to get the redirect URL without downloading the image
        let response = self
            .http_client
            .head(&url)
            .send()
            .await
            .context("Failed to send Cover Art Archive request")?;

        match response.status().as_u16() {
            200 | 307 | 302 => {
                // If we got redirected, the final URL is in the response
                let final_url = response.url().to_string();
                tracing::info!("[CoverArt] Found cover art URL: {}", final_url);
                Ok(final_url)
            }
            404 => {
                tracing::debug!("[CoverArt] No cover art found for release: {}", release_mbid);
                anyhow::bail!("No cover art available for release: {}", release_mbid)
            }
            status => {
                anyhow::bail!("Cover Art Archive error: {}", status)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_creation() {
        let _client = CoverArtClient::new(Client::new());
    }
}
