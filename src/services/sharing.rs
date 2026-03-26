//! Sharing service for scanning and indexing shared files.
//!
//! Scans the `storage/` directory on startup, builds an in-memory file index
//! with an inverted word index for fast search, and provides search/lookup
//! capabilities for the Soulseek upload protocol.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use anyhow::Result;
use bytes::{BufMut, BytesMut};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::Write;

use crate::protocol::peer::encode_string;

/// Virtual share root prefix (Soulseek convention uses backslash separators)
const SHARE_ROOT: &str = "@@rinse";

/// Supported audio extensions for sharing
const AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "ogg", "opus", "wav", "aac", "m4a", "wma", "aiff", "alac",
];

/// A single shared file entry
#[derive(Debug, Clone)]
pub struct SharedFile {
    /// Virtual path using Soulseek convention: "@@rinse\\filename.ext"
    pub virtual_path: String,
    /// Actual filesystem path
    pub real_path: PathBuf,
    /// File size in bytes
    pub size: u64,
    /// Audio bitrate in kbps (if readable)
    pub bitrate: Option<u32>,
    /// Duration in seconds (if readable)
    pub duration_secs: Option<u32>,
    /// Sample rate in Hz (if readable)
    pub sample_rate: Option<u32>,
    /// Bit depth (if readable)
    pub bit_depth: Option<u32>,
}

/// In-memory index of all shared files with inverted word index
pub struct ShareIndex {
    /// All shared files
    files: Vec<SharedFile>,
    /// Inverted word index: lowercase word -> indices into `files`
    word_index: HashMap<String, Vec<usize>>,
    /// Path lookup: virtual_path -> index into `files`
    path_index: HashMap<String, usize>,
    /// Pre-compressed SharedFileList response (zlib)
    compressed_file_list: Vec<u8>,
    /// Number of shared folders
    pub folder_count: u32,
    /// Number of shared files
    pub file_count: u32,
}

impl ShareIndex {
    fn new() -> Self {
        Self {
            files: Vec::new(),
            word_index: HashMap::new(),
            path_index: HashMap::new(),
            compressed_file_list: Vec::new(),
            folder_count: 0,
            file_count: 0,
        }
    }
}

/// Service for managing shared files on the Soulseek network
pub struct SharingService {
    index: Arc<RwLock<ShareIndex>>,
    storage_path: PathBuf,
    enabled: bool,
}

impl SharingService {
    /// Create a new SharingService
    pub fn new(storage_path: PathBuf, enabled: bool) -> Self {
        Self {
            index: Arc::new(RwLock::new(ShareIndex::new())),
            storage_path,
            enabled,
        }
    }

    /// Check if sharing is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Full scan of storage directory. Builds word index and compressed file list.
    /// Run via spawn_blocking to avoid blocking the async executor.
    pub async fn scan(&self) -> Result<(u32, u32)> {
        if !self.enabled {
            tracing::info!("[Sharing] Sharing is disabled, skipping scan");
            return Ok((0, 0));
        }

        let storage_path = self.storage_path.clone();
        tracing::info!("[Sharing] Starting scan of: {}", storage_path.display());
        let start = std::time::Instant::now();

        // Do the heavy I/O work on a blocking thread
        let files = tokio::task::spawn_blocking(move || {
            scan_directory(&storage_path)
        }).await??;

        let file_count = files.len() as u32;
        // We use a single virtual folder (@@rinse)
        let folder_count = if file_count > 0 { 1 } else { 0 };

        // Build the index
        let mut word_index: HashMap<String, Vec<usize>> = HashMap::new();
        let mut path_index: HashMap<String, usize> = HashMap::new();

        for (idx, file) in files.iter().enumerate() {
            // Index by virtual path
            path_index.insert(file.virtual_path.clone(), idx);

            // Build word index from filename (not the full path)
            let filename = file.real_path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let words = extract_words(filename);
            for word in words {
                word_index.entry(word).or_default().push(idx);
            }
        }

        // Pre-build compressed file list
        let compressed_file_list = build_compressed_file_list(&files);

        let elapsed = start.elapsed();
        tracing::info!(
            "[Sharing] Scanned {} files in {} folders ({:.2}s)",
            file_count, folder_count, elapsed.as_secs_f64()
        );

        // Update the index
        let mut index = self.index.write().await;
        index.files = files;
        index.word_index = word_index;
        index.path_index = path_index;
        index.compressed_file_list = compressed_file_list;
        index.folder_count = folder_count;
        index.file_count = file_count;

        Ok((folder_count, file_count))
    }

    /// Incrementally add a file after a download completes
    pub async fn add_file(&self, real_path: &Path) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        // Read file info on blocking thread
        let real_path_owned = real_path.to_path_buf();
        let file = tokio::task::spawn_blocking(move || {
            scan_single_file(&real_path_owned)
        }).await??;

        let mut index = self.index.write().await;

        // Check if already indexed
        if index.path_index.contains_key(&file.virtual_path) {
            return Ok(());
        }

        let idx = index.files.len();

        // Build word index for this file
        let filename = file.real_path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let words = extract_words(filename);
        for word in words {
            index.word_index.entry(word).or_default().push(idx);
        }

        index.path_index.insert(file.virtual_path.clone(), idx);

        tracing::debug!("[Sharing] Added file: {}", file.virtual_path);
        index.files.push(file);

        // Update counts
        index.file_count = index.files.len() as u32;
        if index.folder_count == 0 && index.file_count > 0 {
            index.folder_count = 1;
        }

        // Rebuild compressed file list
        index.compressed_file_list = build_compressed_file_list(&index.files);

        Ok(())
    }

    /// Search shared files using word intersection (Nicotine+ algorithm).
    /// Split query into words, find files containing ALL words.
    /// Words prefixed with "-" are excluded.
    pub async fn search(&self, query: &str, max_results: usize) -> Vec<SharedFile> {
        let index = self.index.read().await;

        if index.files.is_empty() {
            return Vec::new();
        }

        let query_lower = query.to_lowercase();
        let words: Vec<&str> = query_lower.split_whitespace().collect();

        if words.is_empty() {
            return Vec::new();
        }

        // Separate include and exclude words
        let mut include_words = Vec::new();
        let mut exclude_words = Vec::new();
        for word in &words {
            if let Some(stripped) = word.strip_prefix('-') {
                if !stripped.is_empty() {
                    exclude_words.push(stripped.to_string());
                }
            } else {
                include_words.push(word.to_string());
            }
        }

        if include_words.is_empty() {
            return Vec::new();
        }

        // Find files matching ALL include words (intersection)
        // Start with the rarest word (fewest matches) for efficiency
        let mut word_sets: Vec<(&str, &Vec<usize>)> = include_words.iter()
            .filter_map(|w| index.word_index.get(w.as_str()).map(|indices| (w.as_str(), indices)))
            .collect();

        if word_sets.len() != include_words.len() {
            // At least one include word has no matches => empty result
            return Vec::new();
        }

        // Sort by fewest matches first
        word_sets.sort_by_key(|(_, indices)| indices.len());

        // Start with the rarest word's file set
        let (_, first_set) = &word_sets[0];
        let mut matching_indices: Vec<usize> = first_set.to_vec();

        // Intersect with remaining word sets
        for (_, other_set) in word_sets.iter().skip(1) {
            let other_set_hash: std::collections::HashSet<usize> = other_set.iter().copied().collect();
            matching_indices.retain(|idx| other_set_hash.contains(idx));
            if matching_indices.is_empty() {
                return Vec::new();
            }
        }

        // Exclude words with "-" prefix
        if !exclude_words.is_empty() {
            let mut exclude_indices: std::collections::HashSet<usize> = std::collections::HashSet::new();
            for word in &exclude_words {
                if let Some(indices) = index.word_index.get(word.as_str()) {
                    exclude_indices.extend(indices);
                }
            }
            matching_indices.retain(|idx| !exclude_indices.contains(idx));
        }

        // Return up to max_results
        matching_indices.truncate(max_results);
        matching_indices.iter()
            .filter_map(|&idx| index.files.get(idx).cloned())
            .collect()
    }

    /// Look up a file by its virtual path
    pub async fn get_file(&self, virtual_path: &str) -> Option<SharedFile> {
        let index = self.index.read().await;
        index.path_index.get(virtual_path)
            .and_then(|&idx| index.files.get(idx))
            .cloned()
    }

    /// Get the pre-compressed SharedFileList for code 5 responses
    pub async fn get_compressed_file_list(&self) -> Vec<u8> {
        let index = self.index.read().await;
        index.compressed_file_list.clone()
    }

    /// Get folder and file counts for server reporting
    pub async fn counts(&self) -> (u32, u32) {
        let index = self.index.read().await;
        (index.folder_count, index.file_count)
    }

    /// Get total number of shared files
    pub async fn file_count(&self) -> u32 {
        let index = self.index.read().await;
        index.file_count
    }

    /// Check if we have any free upload slots (used for search responses)
    pub fn has_free_slots(&self) -> bool {
        // Will be properly checked via UploadService later
        true
    }

    /// Get all shared files (for API responses)
    pub async fn get_all_files(&self) -> Vec<SharedFile> {
        let index = self.index.read().await;
        index.files.clone()
    }
}

// ============================================================================
// Internal helpers
// ============================================================================

/// Scan a directory for audio files and build SharedFile entries
fn scan_directory(storage_path: &Path) -> Result<Vec<SharedFile>> {
    let mut files = Vec::new();

    if !storage_path.exists() {
        tracing::warn!("[Sharing] Storage path does not exist: {}", storage_path.display());
        return Ok(files);
    }

    let entries = std::fs::read_dir(storage_path)?;

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!("[Sharing] Error reading directory entry: {}", e);
                continue;
            }
        };

        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        match scan_single_file(&path) {
            Ok(file) => files.push(file),
            Err(e) => {
                tracing::debug!("[Sharing] Skipping file {}: {}", path.display(), e);
            }
        }
    }

    Ok(files)
}

/// Scan a single file and create a SharedFile entry
fn scan_single_file(path: &Path) -> Result<SharedFile> {
    let extension = path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if !AUDIO_EXTENSIONS.contains(&extension.as_str()) {
        anyhow::bail!("Not an audio file: {}", extension);
    }

    let metadata = std::fs::metadata(path)?;
    let size = metadata.len();

    let filename = path.file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("Invalid filename"))?;

    let virtual_path = format!("{}\\{}", SHARE_ROOT, filename);

    // Try to read audio properties using lofty
    let (bitrate, duration_secs, sample_rate, bit_depth) = read_audio_properties(path);

    Ok(SharedFile {
        virtual_path,
        real_path: path.to_path_buf(),
        size,
        bitrate,
        duration_secs,
        sample_rate,
        bit_depth,
    })
}

/// Read audio properties from a file using lofty
fn read_audio_properties(path: &Path) -> (Option<u32>, Option<u32>, Option<u32>, Option<u32>) {
    use lofty::{AudioFile, Probe};

    match Probe::open(path).and_then(|p| p.read()) {
        Ok(tagged_file) => {
            let props = tagged_file.properties();
            let bitrate = props.audio_bitrate().map(|b| b as u32);
            let duration_secs = Some(props.duration().as_secs() as u32);
            let sample_rate = props.sample_rate();
            let bit_depth = props.bit_depth().map(|b| b as u32);
            (bitrate, duration_secs, sample_rate, bit_depth)
        }
        Err(e) => {
            tracing::debug!("[Sharing] Could not read audio properties for {}: {}", path.display(), e);
            (None, None, None, None)
        }
    }
}

/// Extract search words from a filename.
/// Lowercase, replace non-alphanumeric with spaces, split on whitespace.
/// This follows the Nicotine+ shares.py word extraction algorithm.
fn extract_words(filename: &str) -> Vec<String> {
    // Remove extension first
    let name = if let Some(dot_pos) = filename.rfind('.') {
        &filename[..dot_pos]
    } else {
        filename
    };

    // Replace non-alphanumeric with spaces, lowercase, split
    let cleaned: String = name.chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { ' ' })
        .collect();

    cleaned.split_whitespace()
        .filter(|w| w.len() >= 2)  // Skip single-char words
        .map(|w| w.to_string())
        .collect()
}

/// Build the compressed SharedFileList response for peer code 5.
///
/// Format (per Soulseek protocol):
/// - u32: number of folders
/// - For each folder:
///   - string: folder name
///   - u32: number of files in folder
///   - For each file:
///     - u8: code (1)
///     - string: filename (relative to folder)
///     - u64: file size
///     - string: extension
///     - u32: number of attributes
///     - For each attribute:
///       - u32: attribute type (0=bitrate, 1=duration, 4=sample_rate, 5=bit_depth)
///       - u32: attribute value
fn build_compressed_file_list(files: &[SharedFile]) -> Vec<u8> {
    let mut buf = BytesMut::new();

    if files.is_empty() {
        buf.put_u32_le(0); // 0 folders
    } else {
        buf.put_u32_le(1); // 1 folder (@@rinse)
        encode_string(&mut buf, SHARE_ROOT);
        buf.put_u32_le(files.len() as u32);

        for file in files {
            let is_lossless = file.bit_depth.is_some();

            // Extract just the filename (relative to folder) for the file list.
            // SoulseekQt prepends the folder name to get the full virtual path,
            // so we must NOT include the folder prefix here.
            let filename = file.real_path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&file.virtual_path);

            buf.put_u8(1); // file code
            encode_string(&mut buf, filename);
            buf.put_u64_le(file.size);

            // Extension field (obsolete, always 0 per protocol spec)
            buf.put_u32_le(0);

            // Attributes follow Nicotine+ convention:
            // Lossless: duration (1), samplerate (4), bitdepth (5)
            // Lossy: bitrate (0), duration (1), vbr flag (2)
            if is_lossless {
                let mut attr_count = 0u32;
                if file.duration_secs.is_some() { attr_count += 1; }
                if file.sample_rate.is_some() { attr_count += 1; }
                if file.bit_depth.is_some() { attr_count += 1; }
                buf.put_u32_le(attr_count);

                if let Some(duration) = file.duration_secs {
                    buf.put_u32_le(1); // type: duration
                    buf.put_u32_le(duration);
                }
                if let Some(sample_rate) = file.sample_rate {
                    buf.put_u32_le(4); // type: sample rate
                    buf.put_u32_le(sample_rate);
                }
                if let Some(bit_depth) = file.bit_depth {
                    buf.put_u32_le(5); // type: bit depth
                    buf.put_u32_le(bit_depth);
                }
            } else {
                let mut attr_count = 0u32;
                if file.bitrate.is_some() { attr_count += 1; }
                if file.duration_secs.is_some() { attr_count += 1; }
                if file.bitrate.is_some() { attr_count += 1; } // vbr flag
                buf.put_u32_le(attr_count);

                if let Some(bitrate) = file.bitrate {
                    buf.put_u32_le(0); // type: bitrate
                    buf.put_u32_le(bitrate);
                }
                if let Some(duration) = file.duration_secs {
                    buf.put_u32_le(1); // type: duration
                    buf.put_u32_le(duration);
                }
                if file.bitrate.is_some() {
                    buf.put_u32_le(2); // type: vbr flag
                    buf.put_u32_le(0); // CBR assumed
                }
            }
        }
    }

    // Unknown field (always 0, required by protocol)
    buf.put_u32_le(0);

    // Zlib compress
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    let _ = encoder.write_all(&buf);
    encoder.finish().unwrap_or_default()
}

/// Encode a search response (peer code 9) for our shared files.
/// This is used when responding to FileSearchRequest from peers.
pub fn encode_search_response(
    username: &str,
    token: u32,
    files: &[SharedFile],
    free_slots: bool,
    avg_speed: u32,
    queue_length: u32,
) -> Vec<u8> {
    let mut buf = BytesMut::new();

    encode_string(&mut buf, username);
    buf.put_u32_le(token);
    buf.put_u32_le(files.len() as u32);

    for file in files {
        buf.put_u8(1); // code
        encode_string(&mut buf, &file.virtual_path);
        buf.put_u64_le(file.size);

        let ext = file.real_path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        encode_string(&mut buf, ext);

        // Attributes
        let mut attr_count = 0u32;
        if file.bitrate.is_some() { attr_count += 1; }
        if file.duration_secs.is_some() { attr_count += 1; }
        if file.sample_rate.is_some() { attr_count += 1; }
        if file.bit_depth.is_some() { attr_count += 1; }
        buf.put_u32_le(attr_count);

        if let Some(bitrate) = file.bitrate {
            buf.put_u32_le(0);
            buf.put_u32_le(bitrate);
        }
        if let Some(duration) = file.duration_secs {
            buf.put_u32_le(1);
            buf.put_u32_le(duration);
        }
        if let Some(sample_rate) = file.sample_rate {
            buf.put_u32_le(4);
            buf.put_u32_le(sample_rate);
        }
        if let Some(bit_depth) = file.bit_depth {
            buf.put_u32_le(5);
            buf.put_u32_le(bit_depth);
        }
    }

    // User stats after file list
    buf.put_u8(if free_slots { 1 } else { 0 });
    buf.put_u32_le(avg_speed);
    buf.put_u32_le(queue_length);

    // Zlib compress
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    let _ = encoder.write_all(&buf);
    encoder.finish().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_words() {
        let words = extract_words("Artist - Track Name (Original Mix).flac");
        assert!(words.contains(&"artist".to_string()));
        assert!(words.contains(&"track".to_string()));
        assert!(words.contains(&"name".to_string()));
        assert!(words.contains(&"original".to_string()));
        assert!(words.contains(&"mix".to_string()));
        // Single char words should be filtered
        assert!(!words.iter().any(|w| w.len() < 2));
    }

    #[test]
    fn test_extract_words_no_extension() {
        let words = extract_words("some_track");
        assert!(words.contains(&"some".to_string()));
        assert!(words.contains(&"track".to_string()));
    }

    #[test]
    fn test_virtual_path() {
        let vpath = format!("{}\\{}", SHARE_ROOT, "Artist - Track.flac");
        assert_eq!(vpath, "@@rinse\\Artist - Track.flac");
    }
}
