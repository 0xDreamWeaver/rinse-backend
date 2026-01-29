use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher as FuzzyMatcherTrait;
use crate::models::Item;

/// Fuzzy matching service for detecting duplicates
pub struct FuzzyMatcher {
    matcher: SkimMatcherV2,
    /// Minimum similarity score (0-100) to consider a match
    threshold: i64,
}

impl FuzzyMatcher {
    /// Create a new fuzzy matcher with a threshold
    pub fn new(threshold: i64) -> Self {
        Self {
            matcher: SkimMatcherV2::default(),
            threshold,
        }
    }

    /// Check if two filenames are similar enough to be duplicates
    pub fn is_duplicate(&self, query: &str, existing: &str) -> bool {
        if let Some(score) = self.matcher.fuzzy_match(existing, query) {
            score >= self.threshold
        } else {
            false
        }
    }

    /// Find duplicate item from a list based on filename similarity
    pub fn find_duplicate<'a>(&self, query: &str, items: &'a [Item]) -> Option<&'a Item> {
        items.iter()
            .filter_map(|item| {
                self.matcher.fuzzy_match(&item.filename, query)
                    .map(|score| (item, score))
            })
            .filter(|(_, score)| *score >= self.threshold)
            .max_by_key(|(_, score)| *score)
            .map(|(item, _)| item)
    }

    /// Check if two items are duplicates based on multiple criteria
    pub fn are_items_similar(&self, item1: &Item, item2: &Item) -> bool {
        // Check filename similarity
        if self.is_duplicate(&item1.filename, &item2.filename) {
            return true;
        }

        // Check if duration and file size are very similar (within 5%)
        if let (Some(dur1), Some(dur2)) = (item1.duration, item2.duration) {
            let size_diff = (item1.file_size as f64 - item2.file_size as f64).abs() / item1.file_size as f64;
            let duration_diff = (dur1 as f64 - dur2 as f64).abs() / dur1 as f64;

            if size_diff < 0.05 && duration_diff < 0.05 {
                return true;
            }
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Item;
    use chrono::Utc;

    #[test]
    fn test_fuzzy_matching() {
        let matcher = FuzzyMatcher::new(60);

        assert!(matcher.is_duplicate("Artist - Song Title.mp3", "Artist - Song Title.mp3"));
        assert!(matcher.is_duplicate("Artist - Song Title.mp3", "Artist - Song Title (320kbps).mp3"));
        assert!(!matcher.is_duplicate("Completely Different Song.mp3", "Artist - Song Title.mp3"));
    }

    #[test]
    fn test_fuzzy_matching_with_variations() {
        let matcher = FuzzyMatcher::new(70);

        // Should match with minor variations
        assert!(matcher.is_duplicate(
            "The Beatles - Hey Jude.mp3",
            "The Beatles - Hey Jude (Remastered).mp3"
        ));

        // Should not match completely different songs
        assert!(!matcher.is_duplicate(
            "The Beatles - Hey Jude.mp3",
            "Queen - Bohemian Rhapsody.mp3"
        ));
    }

    #[test]
    fn test_find_duplicate() {
        let matcher = FuzzyMatcher::new(70);

        let items = vec![
            Item {
                id: 1,
                filename: "Song One.mp3".to_string(),
                original_query: "song one".to_string(),
                file_path: "/tmp/1.mp3".to_string(),
                file_size: 3000000,
                bitrate: Some(320),
                duration: Some(180),
                extension: "mp3".to_string(),
                source_username: "user1".to_string(),
                download_status: "completed".to_string(),
                download_progress: 1.0,
                error_message: None,
                metadata: None,
                created_at: Utc::now(),
                completed_at: Some(Utc::now()),
            },
            Item {
                id: 2,
                filename: "Song Two.mp3".to_string(),
                original_query: "song two".to_string(),
                file_path: "/tmp/2.mp3".to_string(),
                file_size: 2500000,
                bitrate: Some(256),
                duration: Some(200),
                extension: "mp3".to_string(),
                source_username: "user2".to_string(),
                download_status: "completed".to_string(),
                download_progress: 1.0,
                error_message: None,
                metadata: None,
                created_at: Utc::now(),
                completed_at: Some(Utc::now()),
            },
        ];

        // Should find exact match
        let result = matcher.find_duplicate("Song One.mp3", &items);
        assert!(result.is_some());
        assert_eq!(result.unwrap().id, 1);

        // Should find close match
        let result = matcher.find_duplicate("Song One", &items);
        assert!(result.is_some());

        // Should not find match for very different query
        let result = matcher.find_duplicate("Completely Different.mp3", &items);
        assert!(result.is_none());
    }

    #[test]
    fn test_are_items_similar_by_size_and_duration() {
        let matcher = FuzzyMatcher::new(70);

        let item1 = Item {
            id: 1,
            filename: "Song.mp3".to_string(),
            original_query: "query".to_string(),
            file_path: "/tmp/1.mp3".to_string(),
            file_size: 3000000,
            bitrate: Some(320),
            duration: Some(180),
            extension: "mp3".to_string(),
            source_username: "user1".to_string(),
            download_status: "completed".to_string(),
            download_progress: 1.0,
            error_message: None,
            metadata: None,
            created_at: Utc::now(),
            completed_at: Some(Utc::now()),
        };

        let item2 = Item {
            id: 2,
            filename: "Different Name.mp3".to_string(),
            original_query: "query".to_string(),
            file_path: "/tmp/2.mp3".to_string(),
            file_size: 3050000, // Within 5%
            bitrate: Some(320),
            duration: Some(182), // Within 5%
            extension: "mp3".to_string(),
            source_username: "user2".to_string(),
            download_status: "completed".to_string(),
            download_progress: 1.0,
            error_message: None,
            metadata: None,
            created_at: Utc::now(),
            completed_at: Some(Utc::now()),
        };

        assert!(matcher.are_items_similar(&item1, &item2));
    }
}
