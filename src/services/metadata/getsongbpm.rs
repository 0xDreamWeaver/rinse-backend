//! GetSongBPM API client for BPM and musical key lookup
//!
//! GetSongBPM provides:
//! - BPM (tempo)
//! - Musical key (e.g., "Am", "C#m", "G")
//!
//! IMPORTANT: Attribution is REQUIRED when using this API.
//! A link to getsongbpm.com must be displayed in your application.
//!
//! API Key: Required (set via GETSONGBPM_API_KEY environment variable)

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use tracing;

/// GetSongBPM API client
pub struct GetSongBpmClient {
    http_client: Client,
    api_key: Option<String>,
}

/// Search response from GetSongBPM
#[derive(Debug, Deserialize)]
struct SearchResponse {
    search: Option<Vec<SongResult>>,
}

/// Individual song result
#[derive(Debug, Deserialize)]
struct SongResult {
    id: String,
    title: String,
    uri: Option<String>,
    artist: Option<SongArtist>,
}

#[derive(Debug, Deserialize)]
struct SongArtist {
    id: String,
    name: String,
}

/// Detailed song response
#[derive(Debug, Deserialize)]
struct SongResponse {
    song: Option<SongDetail>,
}

#[derive(Debug, Deserialize)]
struct SongDetail {
    id: String,
    title: String,
    uri: Option<String>,
    tempo: Option<f32>,
    time_sig: Option<String>,
    key_of: Option<String>,
    open_key: Option<String>,
    artist: Option<SongArtist>,
    album: Option<SongAlbum>,
}

#[derive(Debug, Deserialize)]
struct SongAlbum {
    title: Option<String>,
    uri: Option<String>,
}

/// Result from GetSongBPM lookup
#[derive(Debug, Clone)]
pub struct BpmKeyResult {
    pub bpm: Option<i32>,
    pub key: Option<String>,
    /// Attribution URL - MUST be displayed per API terms
    pub attribution_url: String,
}

impl GetSongBpmClient {
    /// Create a new GetSongBPM client
    ///
    /// # Arguments
    /// * `http_client` - Shared reqwest client
    /// * `api_key` - GetSongBPM API key (optional, but required for actual API calls)
    pub fn new(http_client: Client, api_key: Option<String>) -> Self {
        if api_key.is_none() {
            tracing::warn!(
                "[GetSongBPM] No API key provided. BPM/Key lookups will be disabled. \
                Set GETSONGBPM_API_KEY environment variable to enable."
            );
        }
        Self {
            http_client,
            api_key,
        }
    }

    /// Check if the client has a valid API key
    pub fn is_configured(&self) -> bool {
        self.api_key.is_some()
    }

    /// Search for a song and get BPM/Key
    ///
    /// # Arguments
    /// * `query` - Search query (artist and/or title)
    ///
    /// # Returns
    /// * BpmKeyResult with bpm, key, and attribution URL
    pub async fn search(&self, query: &str) -> Result<BpmKeyResult> {
        let api_key = self
            .api_key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("GetSongBPM API key not configured"))?;

        tracing::info!("[GetSongBPM] Searching for: {}", query);

        // First, search for the song
        let search_url = format!(
            "https://api.getsongbpm.com/search/?api_key={}&type=song&lookup={}",
            api_key,
            urlencoding::encode(query)
        );

        let response = self
            .http_client
            .get(&search_url)
            .send()
            .await
            .context("Failed to send GetSongBPM search request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("GetSongBPM search error: {} - {}", status, body);
        }

        let search_result: SearchResponse = response
            .json()
            .await
            .context("Failed to parse GetSongBPM search response")?;

        // Get the first matching song
        let song = search_result
            .search
            .and_then(|s| s.into_iter().next())
            .ok_or_else(|| anyhow::anyhow!("No songs found for query: {}", query))?;

        // Now get detailed song info including BPM and key
        let song_url = format!(
            "https://api.getsongbpm.com/song/?api_key={}&id={}",
            api_key, song.id
        );

        let response = self
            .http_client
            .get(&song_url)
            .send()
            .await
            .context("Failed to send GetSongBPM song request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("GetSongBPM song error: {} - {}", status, body);
        }

        let song_response: SongResponse = response
            .json()
            .await
            .context("Failed to parse GetSongBPM song response")?;

        let detail = song_response
            .song
            .ok_or_else(|| anyhow::anyhow!("Song details not found"))?;

        // Build attribution URL
        let attribution_url = detail
            .uri
            .unwrap_or_else(|| format!("https://getsongbpm.com/song/{}", detail.id));

        let result = BpmKeyResult {
            bpm: detail.tempo.map(|t| t.round() as i32),
            key: detail.key_of,
            attribution_url,
        };

        tracing::info!(
            "[GetSongBPM] Found: bpm={:?}, key={:?}",
            result.bpm,
            result.key
        );

        Ok(result)
    }

    /// Search with separate artist and title for more precise matching
    pub async fn search_precise(&self, artist: &str, title: &str) -> Result<BpmKeyResult> {
        // GetSongBPM works best with combined query
        let query = format!("{} {}", artist, title);
        self.search(&query).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_without_key() {
        let client = GetSongBpmClient::new(Client::new(), None);
        assert!(!client.is_configured());
    }

    #[test]
    fn test_client_with_key() {
        let client = GetSongBpmClient::new(Client::new(), Some("test_key".to_string()));
        assert!(client.is_configured());
    }
}
