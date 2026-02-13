//! MusicBrainz API client for track metadata lookup
//!
//! MusicBrainz provides comprehensive music metadata including:
//! - Artist, Album, Title
//! - Year, Label, Genre
//! - Track number
//! - MusicBrainz Recording ID (MBID) for linking to other services
//!
//! Rate limit: 1 request per second (enforced externally by MetadataService)

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use tracing;

use crate::models::TrackMetadata;

/// MusicBrainz API client
pub struct MusicBrainzClient {
    http_client: Client,
    user_agent: String,
}

/// MusicBrainz recording search response
#[derive(Debug, Deserialize)]
struct RecordingSearchResponse {
    recordings: Option<Vec<Recording>>,
}

#[derive(Debug, Deserialize)]
struct Recording {
    id: String,
    title: Option<String>,
    #[serde(rename = "artist-credit")]
    artist_credit: Option<Vec<ArtistCredit>>,
    #[serde(rename = "first-release-date")]
    first_release_date: Option<String>,
    releases: Option<Vec<Release>>,
    tags: Option<Vec<MbTag>>,
}

#[derive(Debug, Deserialize)]
struct ArtistCredit {
    artist: Artist,
}

#[derive(Debug, Deserialize)]
struct Artist {
    name: String,
}

#[derive(Debug, Deserialize)]
struct Release {
    id: String,
    title: Option<String>,
    date: Option<String>,
    #[serde(rename = "label-info")]
    label_info: Option<Vec<LabelInfo>>,
    #[serde(rename = "track-count")]
    track_count: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct LabelInfo {
    label: Option<Label>,
}

#[derive(Debug, Deserialize)]
struct Label {
    name: String,
}

#[derive(Debug, Deserialize)]
struct MbTag {
    name: String,
    count: i32,
}

impl MusicBrainzClient {
    /// Create a new MusicBrainz client
    ///
    /// # Arguments
    /// * `http_client` - Shared reqwest client
    /// * `contact_email` - Contact email for User-Agent (required by MusicBrainz)
    pub fn new(http_client: Client, contact_email: &str) -> Self {
        let user_agent = format!("Rinse/1.0.0 ({})", contact_email);
        Self {
            http_client,
            user_agent,
        }
    }

    /// Search for a recording by artist and title
    ///
    /// Returns TrackMetadata with fields populated from the best match.
    /// Note: Caller is responsible for rate limiting (1 req/sec).
    pub async fn search_recording(&self, query: &str) -> Result<TrackMetadata> {
        tracing::info!("[MusicBrainz] Searching for: {}", query);

        // Build the search URL
        // Using recording search with the full query
        let url = format!(
            "https://musicbrainz.org/ws/2/recording?query={}&fmt=json&limit=10",
            urlencoding::encode(query)
        );

        let response = self
            .http_client
            .get(&url)
            .header("User-Agent", &self.user_agent)
            .header("Accept", "application/json")
            .send()
            .await
            .context("Failed to send MusicBrainz request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("MusicBrainz API error: {} - {}", status, body);
        }

        let search_result: RecordingSearchResponse = response
            .json()
            .await
            .context("Failed to parse MusicBrainz response")?;

        let recordings = search_result
            .recordings
            .ok_or_else(|| anyhow::anyhow!("No recordings found for query: {}", query))?;

        if recordings.is_empty() {
            anyhow::bail!("No recordings found for query: {}", query);
        }

        // Log all results for debugging
        tracing::info!("[MusicBrainz] === {} results for query: '{}' ===", recordings.len(), query);
        for (i, rec) in recordings.iter().enumerate() {
            let artist = rec.artist_credit
                .as_ref()
                .and_then(|credits| credits.first())
                .map(|c| c.artist.name.as_str())
                .unwrap_or("Unknown");
            let title = rec.title.as_deref().unwrap_or("Unknown");
            let album = rec.releases
                .as_ref()
                .and_then(|r| r.first())
                .and_then(|r| r.title.as_deref())
                .unwrap_or("Unknown");

            let score = self.score_result_match(query, artist, title);
            tracing::info!(
                "[MusicBrainz]   #{}: artist='{}', title='{}', album='{}', match_score={}",
                i + 1, artist, title, album, score
            );
        }

        // Score all results and pick the best match
        let best_recording = self.select_best_match(query, recordings)
            .ok_or_else(|| anyhow::anyhow!(
                "No suitable match found for query '{}' - all results scored below threshold",
                query
            ))?;

        let metadata = self.recording_to_metadata(best_recording);

        tracing::info!(
            "[MusicBrainz] Selected: artist={:?}, title={:?}, album={:?}",
            metadata.artist,
            metadata.title,
            metadata.album
        );

        Ok(metadata)
    }

    /// Minimum score threshold for accepting a match
    /// If best match scores below this, we reject it as "no match found"
    const MIN_MATCH_SCORE: i32 = 20;

    /// Score how well a result matches the query
    /// Higher score = better match
    ///
    /// IMPORTANT: At least one query word MUST match the artist name,
    /// otherwise the result is heavily penalized. This prevents selecting
    /// wrong tracks that happen to have matching titles.
    fn score_result_match(&self, query: &str, artist: &str, title: &str) -> i32 {
        let query_lower = query.to_lowercase();
        let artist_lower = artist.to_lowercase();
        let title_lower = title.to_lowercase();

        // Extract words from query (split on spaces, hyphens, underscores, apostrophes)
        // Filter out common words and short words that don't help matching
        let stop_words = ["the", "a", "an", "and", "or", "of", "in", "on", "at", "to", "for", "it", "is"];
        let query_words: Vec<&str> = query_lower
            .split(|c: char| c.is_whitespace() || c == '-' || c == '_' || c == '\'')
            .filter(|w| !w.is_empty() && w.len() > 1 && !stop_words.contains(w))
            .collect();

        let mut score = 0i32;
        let mut artist_matched = false;
        let mut title_matched = false;

        for word in &query_words {
            // Check if word appears in artist name
            if artist_lower.contains(word) {
                artist_matched = true;
                score += 10;
                // Bonus if it's an exact word match (not substring)
                let artist_words: Vec<&str> = artist_lower
                    .split(|c: char| c.is_whitespace() || c == '-' || c == '_' || c == '\'')
                    .collect();
                if artist_words.iter().any(|w| w == word) {
                    score += 5;
                }
            }

            // Check if word appears in title
            if title_lower.contains(word) {
                title_matched = true;
                score += 10;
                // Bonus if it's an exact word match
                let title_words: Vec<&str> = title_lower
                    .split(|c: char| c.is_whitespace() || c == '-' || c == '_' || c == '\'')
                    .collect();
                if title_words.iter().any(|w| w == word) {
                    score += 5;
                }
            }
        }

        // Bonus for exact matches
        if artist_lower == query_lower || title_lower == query_lower {
            score += 50;
        }

        // CRITICAL: If no query word matched the artist name, this is likely wrong
        // Apply heavy penalty to prevent selecting tracks with matching titles
        // but completely wrong artists (e.g., "Takin' It Back" by Kingspade when
        // searching for "Headhunterz Takin' It Back")
        if !artist_matched {
            score -= 50;
            tracing::debug!(
                "[MusicBrainz] No artist match for '{}' in artist='{}' - applying -50 penalty",
                query, artist
            );
        }

        // Bonus if both artist AND title matched - high confidence
        if artist_matched && title_matched {
            score += 20;
        }

        // Penalty if artist/title contains words NOT in query (likely wrong track)
        let combined = format!("{} {}", artist_lower, title_lower);
        let combined_words: Vec<&str> = combined
            .split(|c: char| c.is_whitespace() || c == '-' || c == '_' || c == '\'')
            .filter(|w| !w.is_empty() && w.len() > 2 && !stop_words.contains(w))
            .collect();

        for word in combined_words {
            if !query_words.iter().any(|qw| word.contains(qw) || qw.contains(word)) {
                // Word in result not found in query - small penalty
                score -= 2;
            }
        }

        score
    }

    /// Select the best matching recording from a list of results
    /// Returns None if no result meets the minimum score threshold
    fn select_best_match(&self, query: &str, recordings: Vec<Recording>) -> Option<Recording> {
        let mut best_score = i32::MIN;
        let mut best_idx = 0;

        for (i, rec) in recordings.iter().enumerate() {
            let artist = rec.artist_credit
                .as_ref()
                .and_then(|credits| credits.first())
                .map(|c| c.artist.name.as_str())
                .unwrap_or("");
            let title = rec.title.as_deref().unwrap_or("");

            let score = self.score_result_match(query, artist, title);

            if score > best_score {
                best_score = score;
                best_idx = i;
            }
        }

        tracing::info!(
            "[MusicBrainz] Best match is #{} with score {} (threshold: {})",
            best_idx + 1,
            best_score,
            Self::MIN_MATCH_SCORE
        );

        // Reject if best score is below threshold
        if best_score < Self::MIN_MATCH_SCORE {
            tracing::warn!(
                "[MusicBrainz] Best score {} is below threshold {}, rejecting all results",
                best_score,
                Self::MIN_MATCH_SCORE
            );
            return None;
        }

        // Return the best match
        Some(recordings.into_iter().nth(best_idx).unwrap())
    }

    /// Search with separate artist and title for more precise matching
    pub async fn search_recording_precise(
        &self,
        artist: &str,
        title: &str,
    ) -> Result<TrackMetadata> {
        tracing::info!(
            "[MusicBrainz] Precise search: artist='{}', title='{}'",
            artist,
            title
        );

        // Build Lucene query with artist and recording name
        let query = format!(
            "recording:\"{}\" AND artist:\"{}\"",
            title.replace('"', ""),
            artist.replace('"', "")
        );

        let url = format!(
            "https://musicbrainz.org/ws/2/recording?query={}&fmt=json&limit=5",
            urlencoding::encode(&query)
        );

        let response = self
            .http_client
            .get(&url)
            .header("User-Agent", &self.user_agent)
            .header("Accept", "application/json")
            .send()
            .await
            .context("Failed to send MusicBrainz request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("MusicBrainz API error: {} - {}", status, body);
        }

        let search_result: RecordingSearchResponse = response
            .json()
            .await
            .context("Failed to parse MusicBrainz response")?;

        let recording = search_result
            .recordings
            .and_then(|r| r.into_iter().next())
            .ok_or_else(|| {
                anyhow::anyhow!("No recordings found for artist='{}', title='{}'", artist, title)
            })?;

        let metadata = self.recording_to_metadata(recording);

        tracing::info!(
            "[MusicBrainz] Found: artist={:?}, title={:?}, album={:?}, year={:?}",
            metadata.artist,
            metadata.title,
            metadata.album,
            metadata.year
        );

        Ok(metadata)
    }

    /// Convert a MusicBrainz Recording to TrackMetadata
    fn recording_to_metadata(&self, recording: Recording) -> TrackMetadata {
        let mut metadata = TrackMetadata::default();

        // Recording ID (MBID)
        metadata.musicbrainz_id = Some(recording.id);

        // Title
        metadata.title = recording.title;

        // Artist (join multiple artists with " & ")
        if let Some(credits) = recording.artist_credit {
            let artists: Vec<String> = credits.into_iter().map(|c| c.artist.name).collect();
            if !artists.is_empty() {
                metadata.artist = Some(artists.join(" & "));
            }
        }

        // Get album info from first release
        if let Some(releases) = recording.releases {
            if let Some(release) = releases.into_iter().next() {
                metadata.album = release.title;

                // Year from release date (take first 4 chars)
                if let Some(date) = release.date {
                    if date.len() >= 4 {
                        if let Ok(year) = date[..4].parse::<i32>() {
                            metadata.year = Some(year);
                        }
                    }
                }

                // Label from label-info
                if let Some(label_infos) = release.label_info {
                    if let Some(li) = label_infos.into_iter().next() {
                        if let Some(label) = li.label {
                            metadata.label = Some(label.name);
                        }
                    }
                }
            }
        }

        // Fallback: year from first-release-date
        if metadata.year.is_none() {
            if let Some(date) = recording.first_release_date {
                if date.len() >= 4 {
                    if let Ok(year) = date[..4].parse::<i32>() {
                        metadata.year = Some(year);
                    }
                }
            }
        }

        // Genre from tags (take the most popular one)
        if let Some(mut tags) = recording.tags {
            tags.sort_by(|a, b| b.count.cmp(&a.count));
            if let Some(top_tag) = tags.into_iter().next() {
                metadata.genre = Some(top_tag.name);
            }
        }

        // Mark source
        metadata.sources.push("musicbrainz".to_string());

        metadata
    }

    /// Get the release MBID from a recording search result (for Cover Art Archive lookup)
    /// Uses the same scoring logic as search_recording to ensure consistency
    pub async fn get_release_mbid(&self, query: &str) -> Result<Option<String>> {
        let url = format!(
            "https://musicbrainz.org/ws/2/recording?query={}&fmt=json&limit=10",
            urlencoding::encode(query)
        );

        let response = self
            .http_client
            .get(&url)
            .header("User-Agent", &self.user_agent)
            .header("Accept", "application/json")
            .send()
            .await
            .context("Failed to send MusicBrainz request")?;

        if !response.status().is_success() {
            return Ok(None);
        }

        let search_result: RecordingSearchResponse = response
            .json()
            .await
            .context("Failed to parse MusicBrainz response")?;

        let recordings = match search_result.recordings {
            Some(r) if !r.is_empty() => r,
            _ => return Ok(None),
        };

        // Use same scoring logic to pick best match
        let best_recording = match self.select_best_match(query, recordings) {
            Some(rec) => rec,
            None => return Ok(None),
        };

        // Get first release ID from the best recording
        Ok(best_recording
            .releases
            .and_then(|releases| releases.into_iter().next())
            .map(|release| release.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_agent_format() {
        let client = MusicBrainzClient::new(Client::new(), "test@example.com");
        assert!(client.user_agent.contains("Rinse/1.0.0"));
        assert!(client.user_agent.contains("test@example.com"));
    }
}
