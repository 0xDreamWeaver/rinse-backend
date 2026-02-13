//! AcoustID audio fingerprinting - Phase 2 scaffold
//!
//! This module will provide audio fingerprint-based track identification
//! using AcoustID and Chromaprint.
//!
//! Use cases:
//! - Identify tracks when filename parsing fails
//! - Verify track identity with high confidence
//! - Find correct metadata for mislabeled files
//!
//! ## How it works
//!
//! 1. Generate audio fingerprint using Chromaprint library
//! 2. Submit fingerprint to AcoustID API
//! 3. Receive MusicBrainz Recording IDs for matching tracks
//! 4. Use MusicBrainz IDs to fetch full metadata
//!
//! ## Dependencies (to add in Phase 2)
//!
//! ```toml
//! # Chromaprint for fingerprint generation
//! chromaprint = "0.1"  # Or use FFI bindings
//! ```
//!
//! ## API Requirements
//!
//! - AcoustID API key required (free registration at acoustid.org)
//! - Set via `ACOUSTID_API_KEY` environment variable

use anyhow::Result;
use reqwest::Client;

/// AcoustID fingerprint lookup result
#[derive(Debug, Clone)]
pub struct AcoustIdResult {
    /// AcoustID track identifier
    pub acoustid: String,
    /// MusicBrainz Recording IDs associated with this fingerprint
    pub musicbrainz_ids: Vec<String>,
    /// Confidence score (0.0 - 1.0)
    pub score: f32,
}

/// AcoustID client for audio fingerprint-based track identification
///
/// Phase 2 implementation will use Chromaprint for fingerprinting
/// and the AcoustID web service for lookup.
pub struct AcoustIdClient {
    http_client: Client,
    api_key: Option<String>,
}

impl AcoustIdClient {
    /// Create a new AcoustID client
    ///
    /// # Arguments
    /// * `http_client` - Shared reqwest client
    /// * `api_key` - AcoustID API key (optional, but required for lookups)
    pub fn new(http_client: Client, api_key: Option<String>) -> Self {
        if api_key.is_none() {
            tracing::warn!(
                "[AcoustID] No API key provided. Fingerprint lookups will be disabled. \
                Set ACOUSTID_API_KEY environment variable to enable."
            );
        }
        Self {
            http_client,
            api_key,
        }
    }

    /// Check if the client is properly configured
    pub fn is_configured(&self) -> bool {
        self.api_key.is_some()
    }

    /// Generate fingerprint and identify track
    ///
    /// # Arguments
    /// * `file_path` - Path to the audio file
    ///
    /// # Returns
    /// * AcoustIdResult with matching IDs and confidence
    ///
    /// # Phase 2 Implementation Notes
    ///
    /// 1. Use Chromaprint to generate fingerprint:
    ///    - Decode audio to raw PCM (16-bit, mono, 44100Hz)
    ///    - Call `chromaprint_get_fingerprint()`
    ///
    /// 2. Submit to AcoustID API:
    ///    - POST to `https://api.acoustid.org/v2/lookup`
    ///    - Include: client (api_key), fingerprint, duration
    ///    - Request metadata: `meta=recordings+releasegroups`
    ///
    /// 3. Parse response and return MusicBrainz IDs
    pub async fn identify(&self, _file_path: &str) -> Result<AcoustIdResult> {
        let _api_key = self
            .api_key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("AcoustID API key not configured"))?;

        // TODO: Phase 2 implementation
        //
        // Pseudocode:
        // ```
        // // 1. Generate fingerprint
        // let (fingerprint, duration) = chromaprint::fingerprint(file_path)?;
        //
        // // 2. Submit to AcoustID
        // let response = self.http_client
        //     .post("https://api.acoustid.org/v2/lookup")
        //     .form(&[
        //         ("client", api_key),
        //         ("fingerprint", &fingerprint),
        //         ("duration", &duration.to_string()),
        //         ("meta", "recordings+releasegroups"),
        //     ])
        //     .send()
        //     .await?;
        //
        // // 3. Parse response
        // let result: AcoustIdResponse = response.json().await?;
        // Ok(AcoustIdResult {
        //     acoustid: result.results[0].id,
        //     musicbrainz_ids: result.results[0].recordings.iter().map(|r| r.id.clone()).collect(),
        //     score: result.results[0].score,
        // })
        // ```

        unimplemented!(
            "AcoustID fingerprinting is planned for Phase 2. \
            Requires Chromaprint library for fingerprint generation."
        )
    }

    /// Generate fingerprint without lookup (for debugging)
    ///
    /// Returns the raw Chromaprint fingerprint string.
    pub async fn generate_fingerprint(&self, _file_path: &str) -> Result<String> {
        // TODO: Phase 2 implementation
        //
        // Use chromaprint crate or FFI bindings to generate fingerprint

        unimplemented!(
            "Fingerprint generation is planned for Phase 2. \
            Requires Chromaprint library."
        )
    }
}

impl Default for AcoustIdClient {
    fn default() -> Self {
        Self::new(Client::new(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_without_key() {
        let client = AcoustIdClient::new(Client::new(), None);
        assert!(!client.is_configured());
    }

    #[test]
    fn test_client_with_key() {
        let client = AcoustIdClient::new(Client::new(), Some("test_key".to_string()));
        assert!(client.is_configured());
    }
}
