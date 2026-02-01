//! File selection and scoring for search results
//!
//! This module provides query parsing, fuzzy matching, and file scoring
//! to select the best file from search results.

use std::collections::HashSet;

use super::messages::SearchFile;
use super::client::SearchResult;

/// Remix/alternate version indicators that should be penalized
/// (unless the query explicitly contains them)
const REMIX_INDICATORS: &[&str] = &[
    "remix",
    "mix)",      // Catches "(... Mix)" patterns
    "edit",
    "version)",  // Catches "(... Version)" patterns
    "instrumental",
    "acapella",
    "a cappella",
    "radio edit",
    "live",
    "demo",
    "vip",
    "bootleg",
    "cover",
    "acoustic",
];

/// Bonus indicator - extended versions are preferred
const EXTENDED_INDICATOR: &str = "extended";

/// Penalty applied for remix/alternate versions (when not requested)
const REMIX_PENALTY: f64 = 30.0;

/// Bonus applied for extended versions
const EXTENDED_BONUS: f64 = 10.0;

/// Check if a string contains any remix indicators that are NOT present in the query
/// Returns the penalty to apply (0.0 if no penalty, REMIX_PENALTY if remix indicator found)
#[cfg_attr(test, allow(dead_code))]
pub fn calculate_remix_penalty(filename: &str, query: &str) -> f64 {
    let filename_lower = filename.to_lowercase();
    let query_lower = query.to_lowercase();

    for indicator in REMIX_INDICATORS {
        // Check if filename contains this indicator
        if filename_lower.contains(indicator) {
            // Only penalize if the query does NOT contain this indicator
            if !query_lower.contains(indicator) {
                tracing::debug!(
                    "[FileSelection] Remix penalty applied: '{}' contains '{}' but query doesn't",
                    filename, indicator
                );
                return REMIX_PENALTY;
            }
        }
    }

    0.0
}

/// Check if filename contains "extended" and query doesn't explicitly exclude it
/// Returns bonus points if extended version found
#[cfg_attr(test, allow(dead_code))]
pub fn calculate_extended_bonus(filename: &str, query: &str) -> f64 {
    let filename_lower = filename.to_lowercase();
    let query_lower = query.to_lowercase();

    // Give bonus if filename has "extended"
    // (user searching for extended will get even higher match via word overlap)
    if filename_lower.contains(EXTENDED_INDICATOR) {
        // Don't double-bonus if user explicitly searched for extended
        // (they'll already get word overlap bonus)
        if !query_lower.contains(EXTENDED_INDICATOR) {
            return EXTENDED_BONUS;
        }
    }

    0.0
}

/// Parse a search query in "{artist} - {title}" format
/// Returns (artist, title) where either may be None
pub fn parse_query(query: &str) -> (Option<String>, Option<String>) {
    if let Some(idx) = query.find(" - ") {
        let artist = query[..idx].trim().to_lowercase();
        let title = query[idx + 3..].trim().to_lowercase();
        (Some(artist), Some(title))
    } else {
        // No separator found, treat entire query as title
        (None, Some(query.trim().to_lowercase()))
    }
}

/// Normalize a string for fuzzy matching
/// Lowercases, removes non-alphanumeric chars, normalizes whitespace
pub fn normalize_for_matching(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extract the track name from a filename (strip path, number prefix, extension)
pub fn extract_track_name(filename: &str) -> String {
    // Get just the filename part (after last / or \)
    let basename = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(filename);

    // Remove extension
    let without_ext = basename
        .rsplit_once('.')
        .map(|(name, _)| name)
        .unwrap_or(basename);

    // Remove common track number prefixes like "01 - ", "01. ", "01_"
    let track_name = without_ext
        .trim_start_matches(|c: char| c.is_numeric())
        .trim_start_matches([' ', '-', '.', '_'])
        .trim();

    normalize_for_matching(track_name)
}

/// Score how well a filename matches the expected title (0.0 = no match, 1.0 = perfect)
pub fn score_title_match(filename: &str, expected_title: &str) -> f64 {
    let track_name = extract_track_name(filename);
    let expected = normalize_for_matching(expected_title);

    if track_name.is_empty() || expected.is_empty() {
        return 0.0;
    }

    // Exact match
    if track_name == expected {
        return 1.0;
    }

    // Track name contains expected title
    if track_name.contains(&expected) {
        return 0.9;
    }

    // Expected title contains track name (partial match)
    if expected.contains(&track_name) {
        return 0.7;
    }

    // Check word overlap
    let track_words: HashSet<_> = track_name.split_whitespace().collect();
    let expected_words: HashSet<_> = expected.split_whitespace().collect();

    let intersection = track_words.intersection(&expected_words).count();
    if intersection > 0 {
        let union = track_words.union(&expected_words).count();
        return (intersection as f64 / union as f64) * 0.6;
    }

    0.0
}

/// Score a file for selection (higher is better)
/// Considers: title match, format quality, bitrate, remix penalties, extended bonus
///
/// Parameters:
/// - filename: The full filename/path from the search result
/// - size: File size in bytes (currently unused, kept for future use)
/// - bitrate: Optional bitrate for lossy formats
/// - expected_title: The title portion of the search query
/// - full_query: The complete original search query (for remix detection)
pub fn score_file(filename: &str, _size: u64, bitrate: Option<u32>, expected_title: &str, full_query: &str) -> f64 {
    let mut score = 0.0;

    // Title match is most important (0-100 points)
    let title_score = score_title_match(filename, expected_title);
    score += title_score * 100.0;

    // Format preference (0-30 points)
    let lower = filename.to_lowercase();
    if lower.ends_with(".flac") {
        score += 30.0;  // Lossless - strongly preferred
    } else if lower.ends_with(".wav") {
        score += 25.0;  // Uncompressed lossless
    } else if lower.ends_with(".mp3") {
        // MP3 base score depends heavily on bitrate
        if let Some(br) = bitrate {
            if br >= 320 {
                score += 20.0;  // 320kbps MP3 - excellent
            } else if br >= 256 {
                score += 15.0;  // 256kbps - good
            } else if br >= 192 {
                score += 8.0;   // 192kbps - acceptable
            } else if br >= 128 {
                score += 2.0;   // 128kbps - poor quality, barely acceptable
            }
            // Below 128kbps gets 0 points - strongly discouraged
        } else {
            // Unknown bitrate MP3 - assume worst case
            score += 5.0;
        }
    } else if lower.ends_with(".m4a") || lower.ends_with(".aac") {
        // AAC is generally better than MP3 at same bitrate
        score += 12.0;
    } else if lower.ends_with(".aiff") || lower.ends_with(".aif") {
        // AIFF is uncompressed lossless like WAV
        score += 25.0;
    } else if lower.ends_with(".ogg") {
        // OGG Vorbis - similar quality to MP3 at same bitrate
        if let Some(br) = bitrate {
            if br >= 320 {
                score += 18.0;
            } else if br >= 256 {
                score += 14.0;
            } else if br >= 192 {
                score += 8.0;
            } else if br >= 128 {
                score += 3.0;
            }
        } else {
            score += 10.0;  // Unknown bitrate OGG
        }
    }

    // Remix penalty: penalize remixes/edits/etc. unless query explicitly asks for them
    let remix_penalty = calculate_remix_penalty(filename, full_query);
    score -= remix_penalty;

    // Extended bonus: extended versions are generally preferred
    let extended_bonus = calculate_extended_bonus(filename, full_query);
    score += extended_bonus;

    score
}

/// Check if a file is an audio file based on extension
pub fn is_audio_file(filename: &str) -> bool {
    let lower = filename.to_lowercase();
    lower.ends_with(".mp3")
        || lower.ends_with(".flac")
        || lower.ends_with(".wav")
        || lower.ends_with(".ogg")
        || lower.ends_with(".m4a")
        || lower.ends_with(".aac")
        || lower.ends_with(".aiff")
        || lower.ends_with(".aif")
}

/// Check if a file matches the requested format filter
/// format should be lowercase: "mp3", "flac", "m4a", "wav", "aiff", "ogg"
pub fn matches_format(filename: &str, format: Option<&str>) -> bool {
    match format {
        None => true, // No filter, accept all audio files
        Some(fmt) => {
            let lower = filename.to_lowercase();
            match fmt {
                "mp3" => lower.ends_with(".mp3"),
                "flac" => lower.ends_with(".flac"),
                "m4a" => lower.ends_with(".m4a"),
                "wav" => lower.ends_with(".wav"),
                "aiff" => lower.ends_with(".aiff") || lower.ends_with(".aif"),
                "ogg" => lower.ends_with(".ogg"),
                _ => true, // Unknown format, accept all
            }
        }
    }
}

/// Scored file with all relevant metadata for selection
#[derive(Debug, Clone)]
pub struct ScoredFile {
    pub username: String,
    pub filename: String,
    pub size: u64,
    pub bitrate: Option<u32>,
    pub score: f64,
    pub slot_free: bool,
    pub avg_speed: u32,
    pub queue_length: u64,
    pub peer_ip: String,
    pub peer_port: u32,
}

/// Find the best file from search results based on scoring
///
/// This considers:
/// - Title match score
/// - File format (FLAC > WAV > MP3)
/// - Bitrate for lossy formats
/// - User's upload speed and queue length
/// - Whether user has free slots
///
/// format_filter: Optional format filter ("mp3", "flac", "m4a", "wav")
pub fn find_best_file(
    results: &[SearchResult],
    query: &str,
    format_filter: Option<&str>,
) -> Option<ScoredFile> {
    let (_, expected_title) = parse_query(query);
    let title_to_match = expected_title.unwrap_or_default();

    if title_to_match.is_empty() {
        return None;
    }

    // Collect all candidates first for logging
    let mut candidates: Vec<ScoredFile> = Vec::new();

    // First pass: only consider peers with free slots
    for result in results {
        if !result.has_slots {
            continue;
        }

        for file in &result.files {
            if !is_audio_file(&file.filename) || !matches_format(&file.filename, format_filter) {
                continue;
            }

            let base_score = score_file(&file.filename, file.size, file.bitrate(), &title_to_match, query);

            // Add speed bonus (0-25 points based on upload speed)
            // 1 MB/s (1024 KB/s) = full bonus, scale linearly
            let speed_kbps = result.avg_speed as f64 / 1024.0;
            let speed_bonus = (speed_kbps / 1024.0).min(1.0) * 25.0;

            // Small penalty for longer queues (0-5 points)
            let queue_penalty = (result.queue_length as f64 / 10.0).min(5.0);

            let score = base_score + speed_bonus - queue_penalty;

            // Only consider files with some title match (score > 50 means title_score > 0.5)
            if score > 50.0 {
                candidates.push(ScoredFile {
                    username: result.username.clone(),
                    filename: file.filename.clone(),
                    size: file.size,
                    bitrate: file.bitrate(),
                    score,
                    slot_free: result.has_slots,
                    avg_speed: result.avg_speed,
                    queue_length: result.queue_length,
                    peer_ip: result.peer_ip.clone(),
                    peer_port: result.peer_port,
                });
            }
        }
    }

    // Sort candidates by score (highest first) and log top candidates
    candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    if !candidates.is_empty() {
        tracing::info!("[FileSelection] Found {} candidates matching '{}'", candidates.len(), query);

        // Log top 5 candidates
        for (i, c) in candidates.iter().take(5).enumerate() {
            let ext = c.filename.rsplit('.').next().unwrap_or("?");
            let size_mb = c.size as f64 / 1_048_576.0;
            let bitrate_str = c.bitrate.map(|b| format!("{}kbps", b)).unwrap_or_else(|| "?".to_string());
            tracing::info!(
                "[FileSelection] #{}: {:.1} pts - {} ({}, {:.1}MB, {}) from '{}' ({:.0}KB/s)",
                i + 1,
                c.score,
                ext.to_uppercase(),
                bitrate_str,
                size_mb,
                c.filename.rsplit(['/', '\\']).next().unwrap_or(&c.filename),
                c.username,
                c.avg_speed as f64 / 1024.0
            );
        }
    }

    let best = candidates.into_iter().next();

    // If no good match from free peers, fall back to any audio file
    if best.is_none() {
        tracing::warn!("No good title match found from free peers, falling back to any audio file");

        for result in results {
            // Still prefer free peers in fallback
            if !result.has_slots && results.iter().any(|r| r.has_slots) {
                continue;
            }

            for file in &result.files {
                if !is_audio_file(&file.filename) || !matches_format(&file.filename, format_filter) {
                    continue;
                }

                let size_mb = file.size as f64 / 1_048_576.0;
                if size_mb >= 1.0 && size_mb <= 50.0 {
                    return Some(ScoredFile {
                        username: result.username.clone(),
                        filename: file.filename.clone(),
                        size: file.size,
                        bitrate: file.bitrate(),
                        score: 0.0,
                        slot_free: result.has_slots,
                        avg_speed: result.avg_speed,
                        queue_length: result.queue_length,
                        peer_ip: result.peer_ip.clone(),
                        peer_port: result.peer_port,
                    });
                }
            }
        }
    }

    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_query_with_artist() {
        let (artist, title) = parse_query("Skrillex - Bangarang");
        assert_eq!(artist, Some("skrillex".to_string()));
        assert_eq!(title, Some("bangarang".to_string()));
    }

    #[test]
    fn test_parse_query_without_artist() {
        let (artist, title) = parse_query("Bangarang");
        assert_eq!(artist, None);
        assert_eq!(title, Some("bangarang".to_string()));
    }

    #[test]
    fn test_normalize_for_matching() {
        assert_eq!(normalize_for_matching("Hello  World!"), "hello world");
        assert_eq!(normalize_for_matching("test-file_name.mp3"), "testfilenammp3");
    }

    #[test]
    fn test_extract_track_name() {
        assert_eq!(extract_track_name("01 - Bangarang.mp3"), "bangarang");
        assert_eq!(extract_track_name("/path/to/02. Song Title.flac"), "song title");
        assert_eq!(extract_track_name("03_Track Name.wav"), "track name");
    }

    #[test]
    fn test_score_title_match_exact() {
        assert_eq!(score_title_match("Bangarang.mp3", "bangarang"), 1.0);
    }

    #[test]
    fn test_score_title_match_contains() {
        let score = score_title_match("01 - Bangarang (Original Mix).mp3", "bangarang");
        assert!(score >= 0.9);
    }

    #[test]
    fn test_is_audio_file() {
        assert!(is_audio_file("song.mp3"));
        assert!(is_audio_file("track.FLAC"));
        assert!(is_audio_file("audio.wav"));
        assert!(!is_audio_file("cover.jpg"));
        assert!(!is_audio_file("readme.txt"));
    }

    #[test]
    fn test_remix_penalty_applied() {
        // Query doesn't mention remix, file has remix indicator
        let penalty = calculate_remix_penalty("Block Rockin Beats (Micronauts Mix).mp3", "Block Rockin Beats");
        assert_eq!(penalty, REMIX_PENALTY);
    }

    #[test]
    fn test_remix_penalty_not_applied_when_query_has_indicator() {
        // Query mentions "mix", file has "mix" - no penalty
        let penalty = calculate_remix_penalty("Block Rockin Beats (Micronauts Mix).mp3", "Block Rockin Beats Micronauts Mix");
        assert_eq!(penalty, 0.0);
    }

    #[test]
    fn test_remix_penalty_not_applied_to_original() {
        // No remix indicator in filename
        let penalty = calculate_remix_penalty("Block Rockin Beats.mp3", "Block Rockin Beats");
        assert_eq!(penalty, 0.0);
    }

    #[test]
    fn test_extended_bonus_applied() {
        // Extended version gets bonus when query doesn't ask for it
        let bonus = calculate_extended_bonus("Track Name (Extended).mp3", "Track Name");
        assert_eq!(bonus, EXTENDED_BONUS);
    }

    #[test]
    fn test_extended_bonus_not_doubled_when_query_asks() {
        // No extra bonus when user explicitly searches for extended
        let bonus = calculate_extended_bonus("Track Name (Extended).mp3", "Track Name Extended");
        assert_eq!(bonus, 0.0);
    }

    #[test]
    fn test_original_beats_remix() {
        // Original should score higher than remix for basic query
        let query = "Block Rockin Beats";
        let original_score = score_file("Block Rockin Beats.mp3", 5_000_000, Some(320), "Block Rockin Beats", query);
        let remix_score = score_file("Block Rockin Beats (Micronauts Mix).mp3", 5_000_000, Some(320), "Block Rockin Beats", query);
        assert!(original_score > remix_score, "Original ({}) should beat remix ({})", original_score, remix_score);
    }

    #[test]
    fn test_correct_remix_beats_wrong_remix() {
        // When searching for specific remix, correct one should win
        let query = "Block Rockin Beats Micronauts Mix";
        let correct_remix = score_file("Block Rockin Beats (Micronauts Mix).mp3", 5_000_000, Some(320), "Block Rockin Beats Micronauts Mix", query);
        let wrong_remix = score_file("Block Rockin Beats (Souberian Edit).mp3", 5_000_000, Some(320), "Block Rockin Beats Micronauts Mix", query);
        assert!(correct_remix > wrong_remix, "Correct remix ({}) should beat wrong remix ({})", correct_remix, wrong_remix);
    }
}
