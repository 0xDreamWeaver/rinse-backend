//! Discogs API client for track metadata lookup
//!
//! Discogs provides comprehensive music metadata including:
//! - Artist, Album, Title
//! - Year, Label, Genre, Style
//! - Catalog number
//!
//! Requires DISCOGS_TOKEN environment variable for API access.
//! Rate limit: 60 requests per minute for authenticated requests.

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use tracing;

use crate::models::TrackMetadata;

/// Discogs API client
pub struct DiscogsClient {
    http_client: Client,
    user_agent: String,
    token: Option<String>,
}

/// Discogs search response
#[derive(Debug, Deserialize)]
struct SearchResponse {
    results: Option<Vec<SearchResult>>,
}

#[derive(Debug, Deserialize)]
struct SearchResult {
    id: i64,
    #[serde(rename = "type")]
    result_type: String,
    title: Option<String>,
    year: Option<String>,
    label: Option<Vec<String>>,
    genre: Option<Vec<String>>,
    style: Option<Vec<String>>,
    country: Option<String>,
    catno: Option<String>,
    cover_image: Option<String>,
    thumb: Option<String>,
    format: Option<Vec<String>>,
    barcode: Option<Vec<String>>,
    master_id: Option<i64>,
}

/// Discogs release details response
#[derive(Debug, Deserialize)]
struct ReleaseResponse {
    id: i64,
    title: Option<String>,
    year: Option<i32>,
    artists: Option<Vec<ReleaseArtist>>,
    labels: Option<Vec<ReleaseLabel>>,
    genres: Option<Vec<String>>,
    styles: Option<Vec<String>>,
    tracklist: Option<Vec<Track>>,
    images: Option<Vec<Image>>,
    country: Option<String>,
    released: Option<String>,
    notes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReleaseArtist {
    name: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseLabel {
    name: String,
    catno: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Track {
    position: Option<String>,
    title: Option<String>,
    duration: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Image {
    #[serde(rename = "type")]
    image_type: String,
    uri: Option<String>,
    uri150: Option<String>,
}

impl DiscogsClient {
    /// Clean Discogs artist name by removing disambiguation numbers
    /// e.g., "Aphex Twin (2)" -> "Aphex Twin"
    fn clean_artist_name(name: &str) -> String {
        // Pattern: ends with " (N)" where N is one or more digits
        let trimmed = name.trim();
        if let Some(paren_pos) = trimmed.rfind(" (") {
            if trimmed.ends_with(')') {
                let inside = &trimmed[paren_pos + 2..trimmed.len() - 1];
                if inside.chars().all(|c| c.is_ascii_digit()) {
                    return trimmed[..paren_pos].to_string();
                }
            }
        }
        trimmed.to_string()
    }

    /// Parse Discogs duration string to milliseconds
    /// e.g., "5:23" -> 323000, "1:02:15" -> 3735000
    fn parse_duration_to_ms(duration: &str) -> Option<i64> {
        let parts: Vec<&str> = duration.split(':').collect();
        match parts.len() {
            2 => {
                // MM:SS
                let mins: i64 = parts[0].parse().ok()?;
                let secs: i64 = parts[1].parse().ok()?;
                Some((mins * 60 + secs) * 1000)
            }
            3 => {
                // HH:MM:SS
                let hours: i64 = parts[0].parse().ok()?;
                let mins: i64 = parts[1].parse().ok()?;
                let secs: i64 = parts[2].parse().ok()?;
                Some((hours * 3600 + mins * 60 + secs) * 1000)
            }
            _ => None,
        }
    }

    /// Create a new Discogs client
    ///
    /// # Arguments
    /// * `http_client` - Shared reqwest client
    pub fn new(http_client: Client) -> Self {
        let token = std::env::var("DISCOGS_TOKEN").ok();
        let user_agent = "Rinse/1.0.0 +https://github.com/rinse-app".to_string();

        if token.is_none() {
            tracing::warn!("[Discogs] DISCOGS_TOKEN not set - API access will be limited");
        }

        Self {
            http_client,
            user_agent,
            token,
        }
    }

    /// Check if the client is configured with an API token
    pub fn is_configured(&self) -> bool {
        self.token.is_some()
    }

    /// Log rate limit info from response headers
    fn log_rate_limit(response: &reqwest::Response) {
        let remaining = response
            .headers()
            .get("X-Discogs-Ratelimit-Remaining")
            .and_then(|v| v.to_str().ok());
        let limit = response
            .headers()
            .get("X-Discogs-Ratelimit")
            .and_then(|v| v.to_str().ok());

        if let (Some(remaining), Some(limit)) = (remaining, limit) {
            tracing::debug!("[Discogs] Rate limit: {}/{} remaining", remaining, limit);
            // Warn if getting low
            if let Ok(rem) = remaining.parse::<i32>() {
                if rem < 10 {
                    tracing::warn!("[Discogs] Rate limit low: {} requests remaining", rem);
                }
            }
        }
    }

    /// Search for a release by query
    ///
    /// Returns TrackMetadata with fields populated from the best match.
    pub async fn search_release(&self, query: &str) -> Result<TrackMetadata> {
        let start = std::time::Instant::now();

        let url = format!(
            "https://api.discogs.com/database/search?q={}&type=release&per_page=10",
            urlencoding::encode(query)
        );

        let mut request = self
            .http_client
            .get(&url)
            .header("User-Agent", &self.user_agent)
            .header("Accept", "application/json");

        if let Some(ref token) = self.token {
            request = request.header("Authorization", format!("Discogs token={}", token));
        }

        let response = request
            .send()
            .await
            .context("Failed to send Discogs request")?;

        let status = response.status();
        Self::log_rate_limit(&response);

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            tracing::error!(
                "[Discogs] Search failed: status={}, elapsed={:.2}s, body={}",
                status,
                start.elapsed().as_secs_f32(),
                body
            );
            anyhow::bail!("Discogs API error: {} - {}", status, body);
        }

        tracing::debug!("[Discogs] Search response received in {:.2}s", start.elapsed().as_secs_f32());

        let search_result: SearchResponse = response
            .json()
            .await
            .context("Failed to parse Discogs response")?;

        let results = search_result
            .results
            .ok_or_else(|| anyhow::anyhow!("No results found for query: {}", query))?;

        if results.is_empty() {
            anyhow::bail!("No results found for query: {}", query);
        }

        tracing::info!("[Discogs] Search: '{}' (authenticated={}) -> {} results",
            query, self.token.is_some(), results.len());
        // Log individual results at debug level
        for (i, result) in results.iter().take(5).enumerate() {
            let score = self.score_result_match(query, result);
            tracing::debug!(
                "[Discogs]   #{}: title='{}', year={:?}, score={}",
                i + 1, result.title.as_deref().unwrap_or("Unknown"), result.year, score
            );
        }

        // Score and select best match
        let best_result = self.select_best_match(query, results)
            .ok_or_else(|| anyhow::anyhow!(
                "No suitable match found for query '{}' - all results scored below threshold",
                query
            ))?;

        // Get full release details if we have a good match
        let metadata = self.get_release_details(best_result.id, query).await?;

        tracing::info!(
            "[Discogs] Selected: '{} - {}' on '{}' ({:?})",
            metadata.artist.as_deref().unwrap_or("?"),
            metadata.title.as_deref().unwrap_or("?"),
            metadata.album.as_deref().unwrap_or("?"),
            metadata.year
        );

        Ok(metadata)
    }

    /// Search with separate artist and title for more precise matching
    pub async fn search_release_precise(
        &self,
        artist: &str,
        title: &str,
    ) -> Result<TrackMetadata> {
        tracing::debug!("[Discogs] Precise search: artist='{}', title='{}'", artist, title);

        // Build query with artist and track
        let query = format!("{} {}", artist, title);

        let url = format!(
            "https://api.discogs.com/database/search?q={}&type=release&per_page=10&artist={}",
            urlencoding::encode(&query),
            urlencoding::encode(artist)
        );

        let mut request = self
            .http_client
            .get(&url)
            .header("User-Agent", &self.user_agent)
            .header("Accept", "application/json");

        if let Some(ref token) = self.token {
            request = request.header("Authorization", format!("Discogs token={}", token));
        }

        let response = request
            .send()
            .await
            .context("Failed to send Discogs request")?;

        let status = response.status();
        Self::log_rate_limit(&response);

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            tracing::error!("[Discogs] Precise search failed: status={}, body={}", status, body);
            anyhow::bail!("Discogs API error: {} - {}", status, body);
        }

        let search_result: SearchResponse = response
            .json()
            .await
            .context("Failed to parse Discogs response")?;

        let results = search_result
            .results
            .and_then(|r| if r.is_empty() { None } else { Some(r) })
            .ok_or_else(|| {
                tracing::warn!("[Discogs] No results for precise search: artist='{}', title='{}'", artist, title);
                anyhow::anyhow!("No results found for artist='{}', title='{}'", artist, title)
            })?;

        tracing::info!("[Discogs] Precise search: '{} {}' -> {} results", artist, title, results.len());

        // Score with combined query
        let best_result = self.select_best_match(&query, results)
            .ok_or_else(|| anyhow::anyhow!(
                "No suitable match found for artist='{}', title='{}'",
                artist, title
            ))?;

        let metadata = self.get_release_details(best_result.id, &query).await?;

        tracing::info!(
            "[Discogs] Selected: '{} - {}' on '{}' ({:?})",
            metadata.artist.as_deref().unwrap_or("?"),
            metadata.title.as_deref().unwrap_or("?"),
            metadata.album.as_deref().unwrap_or("?"),
            metadata.year
        );

        Ok(metadata)
    }

    /// Get full release details by ID
    async fn get_release_details(&self, release_id: i64, query: &str) -> Result<TrackMetadata> {
        let start = std::time::Instant::now();
        tracing::debug!("[Discogs] Fetching release details for ID: {}", release_id);

        let url = format!("https://api.discogs.com/releases/{}", release_id);

        let mut request = self
            .http_client
            .get(&url)
            .header("User-Agent", &self.user_agent)
            .header("Accept", "application/json");

        if let Some(ref token) = self.token {
            request = request.header("Authorization", format!("Discogs token={}", token));
        }

        let response = request
            .send()
            .await
            .context("Failed to fetch Discogs release details")?;

        let status = response.status();
        Self::log_rate_limit(&response);

        if !status.is_success() {
            tracing::error!(
                "[Discogs] Release lookup failed: id={}, status={}, elapsed={:.2}s",
                release_id,
                status,
                start.elapsed().as_secs_f32()
            );
            anyhow::bail!("Discogs release lookup failed: {}", status);
        }

        let release: ReleaseResponse = response
            .json()
            .await
            .context("Failed to parse Discogs release")?;

        tracing::debug!(
            "[Discogs] Release details fetched in {:.2}s: id={}, title={:?}",
            start.elapsed().as_secs_f32(),
            release_id,
            release.title
        );

        self.release_to_metadata(release, query)
    }

    /// Minimum score threshold for accepting a match
    const MIN_MATCH_SCORE: i32 = 15;

    /// Score how well a result matches the query
    fn score_result_match(&self, query: &str, result: &SearchResult) -> i32 {
        let query_lower = query.to_lowercase();
        let title_lower = result.title.as_deref().unwrap_or("").to_lowercase();

        // Extract words from query
        let stop_words = ["the", "a", "an", "and", "or", "of", "in", "on", "at", "to", "for"];
        let query_words: Vec<&str> = query_lower
            .split(|c: char| c.is_whitespace() || c == '-' || c == '_' || c == '\'')
            .filter(|w| !w.is_empty() && w.len() > 1 && !stop_words.contains(w))
            .collect();

        let mut score = 0i32;
        let mut matched_words = 0;

        for word in &query_words {
            if title_lower.contains(word) {
                matched_words += 1;
                score += 10;

                // Bonus for exact word match
                let title_words: Vec<&str> = title_lower
                    .split(|c: char| c.is_whitespace() || c == '-' || c == '_' || c == '\'')
                    .collect();
                if title_words.iter().any(|w| w == word) {
                    score += 5;
                }
            }
        }

        // Bonus if more than half the words matched
        if !query_words.is_empty() && matched_words > query_words.len() / 2 {
            score += 15;
        }

        // Penalty for very long titles (likely compilations)
        if title_lower.len() > 100 {
            score -= 10;
        }

        // Bonus for having a year (indicates real release)
        if result.year.is_some() {
            score += 5;
        }

        score
    }

    /// Select the best matching result
    fn select_best_match(&self, query: &str, results: Vec<SearchResult>) -> Option<SearchResult> {
        let mut best_score = i32::MIN;
        let mut best_idx = 0;

        for (i, result) in results.iter().enumerate() {
            let score = self.score_result_match(query, result);
            if score > best_score {
                best_score = score;
                best_idx = i;
            }
        }

        tracing::debug!(
            "[Discogs] Best match: #{} score={} (threshold={})",
            best_idx + 1, best_score, Self::MIN_MATCH_SCORE
        );

        if best_score < Self::MIN_MATCH_SCORE {
            tracing::warn!(
                "[Discogs] Best score {} is below threshold {}, rejecting all results",
                best_score,
                Self::MIN_MATCH_SCORE
            );
            return None;
        }

        Some(results.into_iter().nth(best_idx).unwrap())
    }

    /// Convert a Discogs Release to TrackMetadata
    fn release_to_metadata(&self, release: ReleaseResponse, query: &str) -> Result<TrackMetadata> {
        let mut metadata = TrackMetadata::default();

        // Album title
        metadata.album = release.title.clone();

        // Year
        metadata.year = release.year;

        // Artist (join multiple artists with " & ", clean disambiguation numbers)
        if let Some(artists) = release.artists {
            let artist_names: Vec<String> = artists
                .into_iter()
                .map(|a| Self::clean_artist_name(&a.name))
                .filter(|n| n != "Various" && !n.is_empty())
                .collect();
            if !artist_names.is_empty() {
                metadata.artist = Some(artist_names.join(" & "));
            }
        }

        // Label (also clean any disambiguation)
        if let Some(labels) = release.labels {
            if let Some(label) = labels.into_iter().next() {
                metadata.label = Some(Self::clean_artist_name(&label.name));
            }
        }

        // Prefer style over genre (more specific for electronic music)
        // Style examples: "Deep House", "Tech House", "Ambient"
        // Genre examples: "Electronic", "Rock", "Jazz"
        if let Some(styles) = release.styles {
            metadata.genre = styles.into_iter().next();
        } else if let Some(genres) = release.genres {
            metadata.genre = genres.into_iter().next();
        }

        // Try to find matching track in tracklist for title
        if let Some(ref tracklist) = release.tracklist {
            let query_lower = query.to_lowercase();

            // Look for track that matches query
            for (i, track) in tracklist.iter().enumerate() {
                if let Some(ref title) = track.title {
                    let title_lower = title.to_lowercase();
                    // Check if track title contains significant query words
                    let query_words: Vec<&str> = query_lower
                        .split_whitespace()
                        .filter(|w| w.len() > 2)
                        .collect();

                    let matches = query_words.iter()
                        .filter(|w| title_lower.contains(*w))
                        .count();

                    if matches > query_words.len() / 2 || title_lower.contains(&query_lower) {
                        metadata.title = Some(title.clone());
                        metadata.track_number = Some((i + 1) as i32);
                        // Parse duration if available
                        if let Some(ref dur) = track.duration {
                            metadata.duration_ms = Self::parse_duration_to_ms(dur);
                        }
                        break;
                    }
                }
            }

            // If no track matched, use album title as fallback
            if metadata.title.is_none() {
                metadata.title = release.title;
                // Use first track's duration as estimate if available
                if let Some(first_track) = tracklist.first() {
                    if let Some(ref dur) = first_track.duration {
                        metadata.duration_ms = Self::parse_duration_to_ms(dur);
                    }
                }
            }
        } else {
            metadata.title = release.title;
        }

        // Album art from images
        if let Some(images) = release.images {
            // Prefer primary image
            let art_url = images.iter()
                .find(|i| i.image_type == "primary")
                .or_else(|| images.first())
                .and_then(|i| i.uri.clone());

            if let Some(url) = art_url {
                metadata.album_art_url = Some(url);
            }
        }

        // Mark source
        metadata.sources.push("discogs".to_string());

        Ok(metadata)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_agent_format() {
        let client = DiscogsClient::new(Client::new());
        assert!(client.user_agent.contains("Rinse/1.0.0"));
    }

    #[test]
    fn test_clean_artist_name() {
        // Should remove disambiguation numbers
        assert_eq!(DiscogsClient::clean_artist_name("Aphex Twin (2)"), "Aphex Twin");
        assert_eq!(DiscogsClient::clean_artist_name("The Beatles (123)"), "The Beatles");

        // Should preserve names without disambiguation
        assert_eq!(DiscogsClient::clean_artist_name("Aphex Twin"), "Aphex Twin");
        assert_eq!(DiscogsClient::clean_artist_name("Boards of Canada"), "Boards of Canada");

        // Should preserve parentheses that aren't disambiguation
        assert_eq!(DiscogsClient::clean_artist_name("Four Tet (Remix)"), "Four Tet (Remix)");
        assert_eq!(DiscogsClient::clean_artist_name("Air (French Band)"), "Air (French Band)");

        // Should handle whitespace
        assert_eq!(DiscogsClient::clean_artist_name("  Aphex Twin (2)  "), "Aphex Twin");
    }

    #[test]
    fn test_parse_duration_to_ms() {
        // Standard MM:SS format
        assert_eq!(DiscogsClient::parse_duration_to_ms("5:23"), Some(323000));
        assert_eq!(DiscogsClient::parse_duration_to_ms("0:30"), Some(30000));
        assert_eq!(DiscogsClient::parse_duration_to_ms("12:00"), Some(720000));

        // HH:MM:SS format
        assert_eq!(DiscogsClient::parse_duration_to_ms("1:02:15"), Some(3735000));
        assert_eq!(DiscogsClient::parse_duration_to_ms("0:05:30"), Some(330000));

        // Invalid formats
        assert_eq!(DiscogsClient::parse_duration_to_ms("invalid"), None);
        assert_eq!(DiscogsClient::parse_duration_to_ms(""), None);
        assert_eq!(DiscogsClient::parse_duration_to_ms("5"), None);
    }
}
