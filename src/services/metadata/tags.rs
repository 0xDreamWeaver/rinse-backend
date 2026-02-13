//! File tag reading and writing using the lofty crate
//!
//! This module handles:
//! - Reading duration from audio files
//! - Writing metadata tags to audio files (ID3v2, Vorbis, FLAC, etc.)

use anyhow::{Context, Result};
use lofty::{Accessor, AudioFile, ItemKey, Probe, Tag, TaggedFileExt};
use std::path::Path;
use tracing;

use crate::models::TrackMetadata;

/// Read duration from an audio file in milliseconds
pub fn read_duration<P: AsRef<Path>>(file_path: P) -> Result<i64> {
    let file_path = file_path.as_ref();
    tracing::debug!("[Tags] Reading duration from: {}", file_path.display());

    let tagged_file = Probe::open(file_path)
        .context("Failed to open audio file")?
        .read()
        .context("Failed to read audio file")?;

    let properties = tagged_file.properties();
    let duration_ms = properties.duration().as_millis() as i64;

    tracing::debug!("[Tags] Duration: {}ms", duration_ms);
    Ok(duration_ms)
}

/// Read existing tags from an audio file into TrackMetadata
pub fn read_tags<P: AsRef<Path>>(file_path: P) -> Result<TrackMetadata> {
    let file_path = file_path.as_ref();
    tracing::debug!("[Tags] Reading tags from: {}", file_path.display());

    let tagged_file = Probe::open(file_path)
        .context("Failed to open audio file")?
        .read()
        .context("Failed to read audio file")?;

    let mut metadata = TrackMetadata::default();

    // Get duration from properties
    let properties = tagged_file.properties();
    metadata.duration_ms = Some(properties.duration().as_millis() as i64);

    // Try to get tags (primary tag first, then any tag)
    if let Some(tag) = tagged_file.primary_tag().or_else(|| tagged_file.first_tag()) {
        metadata.title = tag.title().map(|s| s.to_string());
        metadata.artist = tag.artist().map(|s| s.to_string());
        metadata.album = tag.album().map(|s| s.to_string());
        metadata.genre = tag.genre().map(|s| s.to_string());
        metadata.year = tag.year().map(|y| y as i32);
        metadata.track_number = tag.track().map(|t| t as i32);

        // Try to get BPM (stored in different ways depending on format)
        if let Some(bpm_item) = tag.get(&ItemKey::Bpm) {
            if let Some(bpm_str) = bpm_item.value().text() {
                if let Ok(bpm) = bpm_str.parse::<f32>() {
                    metadata.bpm = Some(bpm.round() as i32);
                }
            }
        }

        // Try to get key (Initial Key in ID3)
        if let Some(key_item) = tag.get(&ItemKey::InitialKey) {
            if let Some(key_str) = key_item.value().text() {
                metadata.key = Some(key_str.to_string());
            }
        }

        // Try to get label (Publisher in ID3)
        if let Some(label_item) = tag.get(&ItemKey::Label) {
            if let Some(label_str) = label_item.value().text() {
                metadata.label = Some(label_str.to_string());
            }
        }

        // Try to get MusicBrainz Recording ID
        if let Some(mbid_item) = tag.get(&ItemKey::MusicBrainzRecordingId) {
            if let Some(mbid_str) = mbid_item.value().text() {
                metadata.musicbrainz_id = Some(mbid_str.to_string());
            }
        }
    }

    if metadata.artist.is_some() || metadata.title.is_some() {
        metadata.sources.push("file_tags".to_string());
    }

    tracing::debug!(
        "[Tags] Read tags: artist={:?}, title={:?}, bpm={:?}, key={:?}",
        metadata.artist,
        metadata.title,
        metadata.bpm,
        metadata.key
    );

    Ok(metadata)
}

/// Write metadata tags to an audio file
///
/// This function writes all available metadata fields to the audio file's tags.
/// It preserves existing tags and only overwrites fields that are present in the metadata.
pub fn write_tags<P: AsRef<Path>>(file_path: P, metadata: &TrackMetadata) -> Result<()> {
    let file_path = file_path.as_ref();
    tracing::info!("[Tags] Writing tags to: {}", file_path.display());

    let mut tagged_file = Probe::open(file_path)
        .context("Failed to open audio file for writing")?
        .read()
        .context("Failed to read audio file")?;

    // Get or create primary tag
    let tag = match tagged_file.primary_tag_mut() {
        Some(t) => t,
        None => {
            // If no primary tag exists, try to get any tag or we can't write
            if let Some(tag) = tagged_file.first_tag_mut() {
                tag
            } else {
                // Create a new tag based on file type
                let file_type = tagged_file.file_type();
                let tag_type = file_type.primary_tag_type();
                tagged_file.insert_tag(Tag::new(tag_type));
                tagged_file.primary_tag_mut().unwrap()
            }
        }
    };

    // Write basic fields
    if let Some(ref title) = metadata.title {
        tag.set_title(title.clone());
    }
    if let Some(ref artist) = metadata.artist {
        tag.set_artist(artist.clone());
    }
    if let Some(ref album) = metadata.album {
        tag.set_album(album.clone());
    }
    if let Some(ref genre) = metadata.genre {
        tag.set_genre(genre.clone());
    }
    if let Some(year) = metadata.year {
        tag.set_year(year as u32);
    }
    if let Some(track_num) = metadata.track_number {
        tag.set_track(track_num as u32);
    }

    // Write BPM
    if let Some(bpm) = metadata.bpm {
        tag.insert_text(ItemKey::Bpm, bpm.to_string());
    }

    // Write Key (Initial Key)
    if let Some(ref key) = metadata.key {
        tag.insert_text(ItemKey::InitialKey, key.clone());
    }

    // Write Label (Publisher)
    if let Some(ref label) = metadata.label {
        tag.insert_text(ItemKey::Label, label.clone());
    }

    // Write MusicBrainz Recording ID
    if let Some(ref mbid) = metadata.musicbrainz_id {
        tag.insert_text(ItemKey::MusicBrainzRecordingId, mbid.clone());
    }

    // Save the file
    tagged_file
        .save_to_path(file_path)
        .context("Failed to save tags to file")?;

    tracing::info!(
        "[Tags] Successfully wrote tags: artist={:?}, title={:?}, bpm={:?}, key={:?}",
        metadata.artist,
        metadata.title,
        metadata.bpm,
        metadata.key
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_track_metadata_default() {
        let metadata = TrackMetadata::default();
        assert!(metadata.artist.is_none());
        assert!(metadata.title.is_none());
        assert!(metadata.sources.is_empty());
    }
}
