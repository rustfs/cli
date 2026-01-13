//! Multipart upload support
//!
//! Implements multipart upload for large files with resume capability.

use std::path::{Path, PathBuf};

use rc_core::Result;

/// Default part size: 64 MiB
pub const DEFAULT_PART_SIZE: u64 = 64 * 1024 * 1024;

/// Minimum part size: 5 MiB (S3 requirement)
pub const MIN_PART_SIZE: u64 = 5 * 1024 * 1024;

/// Maximum part size: 5 GiB
pub const MAX_PART_SIZE: u64 = 5 * 1024 * 1024 * 1024;

/// Maximum number of parts: 10,000 (S3 limit)
pub const MAX_PARTS: usize = 10_000;

/// Multipart upload configuration
#[derive(Debug, Clone)]
pub struct MultipartConfig {
    /// Part size in bytes
    pub part_size: u64,

    /// Number of concurrent uploads
    pub concurrency: usize,

    /// Path for state file (for resume support)
    pub state_dir: Option<PathBuf>,
}

impl Default for MultipartConfig {
    fn default() -> Self {
        Self {
            part_size: DEFAULT_PART_SIZE,
            concurrency: 4,
            state_dir: None,
        }
    }
}

impl MultipartConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn part_size(mut self, size: u64) -> Self {
        self.part_size = size.clamp(MIN_PART_SIZE, MAX_PART_SIZE);
        self
    }

    pub fn concurrency(mut self, n: usize) -> Self {
        self.concurrency = n.max(1);
        self
    }

    pub fn state_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.state_dir = Some(path.into());
        self
    }

    /// Calculate appropriate part size for a file
    pub fn calculate_part_size(&self, file_size: u64) -> u64 {
        // If file fits in one part, use minimum
        if file_size <= MIN_PART_SIZE {
            return MIN_PART_SIZE;
        }

        // Calculate parts needed with current size
        let parts = file_size.div_ceil(self.part_size);

        if parts <= MAX_PARTS as u64 {
            self.part_size
        } else {
            // Need larger parts to fit within 10,000 limit
            let required_size = file_size.div_ceil(MAX_PARTS as u64);
            required_size.clamp(MIN_PART_SIZE, MAX_PART_SIZE)
        }
    }
}

/// State of a multipart upload (for resume)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UploadState {
    /// Upload ID from S3
    pub upload_id: String,

    /// Target path
    pub target: String,

    /// Source file path (if local)
    pub source: Option<String>,

    /// Total file size
    pub total_size: u64,

    /// Part size used
    pub part_size: u64,

    /// Completed parts (part_number, etag)
    pub completed_parts: Vec<CompletedPart>,

    /// Timestamp of last update
    pub last_updated: jiff::Timestamp,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompletedPart {
    pub part_number: i32,
    pub etag: String,
}

impl UploadState {
    /// Create a new upload state
    pub fn new(
        upload_id: impl Into<String>,
        target: impl Into<String>,
        total_size: u64,
        part_size: u64,
    ) -> Self {
        Self {
            upload_id: upload_id.into(),
            target: target.into(),
            source: None,
            total_size,
            part_size,
            completed_parts: Vec::new(),
            last_updated: jiff::Timestamp::now(),
        }
    }

    /// Set source file path
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Add a completed part
    pub fn add_completed_part(&mut self, part_number: i32, etag: String) {
        self.completed_parts
            .push(CompletedPart { part_number, etag });
        self.last_updated = jiff::Timestamp::now();
    }

    /// Get the next part number to upload
    pub fn next_part_number(&self) -> i32 {
        self.completed_parts
            .iter()
            .map(|p| p.part_number)
            .max()
            .map(|n| n + 1)
            .unwrap_or(1)
    }

    /// Calculate progress percentage
    pub fn progress_percent(&self) -> f64 {
        let completed_bytes = self.completed_parts.len() as u64 * self.part_size;
        (completed_bytes as f64 / self.total_size as f64 * 100.0).min(100.0)
    }

    /// State file path for this upload
    pub fn state_file_path(state_dir: &Path, upload_id: &str) -> PathBuf {
        // Create safe filename from upload_id
        let safe_id: String = upload_id
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        state_dir.join(format!("upload_{safe_id}.json"))
    }

    /// Save state to file
    pub fn save(&self, state_dir: &Path) -> Result<()> {
        let path = Self::state_file_path(state_dir, &self.upload_id);

        // Create directory if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    /// Load state from file
    pub fn load(state_dir: &Path, upload_id: &str) -> Result<Self> {
        let path = Self::state_file_path(state_dir, upload_id);
        let content = std::fs::read_to_string(&path)?;
        let state: Self = serde_json::from_str(&content)?;
        Ok(state)
    }

    /// Delete state file
    pub fn delete(state_dir: &Path, upload_id: &str) -> Result<()> {
        let path = Self::state_file_path(state_dir, upload_id);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// Find pending uploads for a target
    pub fn find_pending(state_dir: &Path, target: &str) -> Result<Vec<Self>> {
        let mut pending = Vec::new();

        if !state_dir.exists() {
            return Ok(pending);
        }

        for entry in std::fs::read_dir(state_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(state) = serde_json::from_str::<Self>(&content) {
                        if state.target == target {
                            pending.push(state);
                        }
                    }
                }
            }
        }

        Ok(pending)
    }
}

/// Calculate number of parts for a file
pub fn calculate_parts(file_size: u64, part_size: u64) -> usize {
    file_size.div_ceil(part_size) as usize
}

/// Get byte range for a part
pub fn part_byte_range(part_number: i32, part_size: u64, total_size: u64) -> (u64, u64) {
    let start = (part_number as u64 - 1) * part_size;
    let end = (start + part_size).min(total_size);
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = MultipartConfig::default();
        assert_eq!(config.part_size, DEFAULT_PART_SIZE);
        assert_eq!(config.concurrency, 4);
    }

    #[test]
    fn test_config_builder() {
        let config = MultipartConfig::new()
            .part_size(128 * 1024 * 1024)
            .concurrency(8);

        assert_eq!(config.part_size, 128 * 1024 * 1024);
        assert_eq!(config.concurrency, 8);
    }

    #[test]
    fn test_part_size_clamping() {
        // Too small
        let config = MultipartConfig::new().part_size(1024);
        assert_eq!(config.part_size, MIN_PART_SIZE);

        // Too large
        let config = MultipartConfig::new().part_size(10 * 1024 * 1024 * 1024);
        assert_eq!(config.part_size, MAX_PART_SIZE);
    }

    #[test]
    fn test_calculate_part_size_small_file() {
        let config = MultipartConfig::default();
        let size = config.calculate_part_size(1024 * 1024); // 1 MiB
        assert_eq!(size, MIN_PART_SIZE);
    }

    #[test]
    fn test_calculate_part_size_large_file() {
        let config = MultipartConfig::default();
        // File that would need more than 10,000 parts with default size
        let huge_file = DEFAULT_PART_SIZE * 20_000;
        let size = config.calculate_part_size(huge_file);
        let parts = calculate_parts(huge_file, size);
        assert!(parts <= MAX_PARTS);
    }

    #[test]
    fn test_upload_state() {
        let mut state = UploadState::new("upload-123", "bucket/key", 1000, 100);
        assert_eq!(state.next_part_number(), 1);

        state.add_completed_part(1, "etag1".to_string());
        assert_eq!(state.next_part_number(), 2);

        state.add_completed_part(2, "etag2".to_string());
        assert_eq!(state.next_part_number(), 3);
    }

    #[test]
    fn test_progress_percent() {
        let mut state = UploadState::new("upload-123", "bucket/key", 1000, 100);
        assert_eq!(state.progress_percent(), 0.0);

        state.add_completed_part(1, "etag1".to_string());
        assert_eq!(state.progress_percent(), 10.0);

        state.add_completed_part(2, "etag2".to_string());
        assert_eq!(state.progress_percent(), 20.0);
    }

    #[test]
    fn test_calculate_parts() {
        assert_eq!(calculate_parts(100, 10), 10);
        assert_eq!(calculate_parts(101, 10), 11);
        assert_eq!(calculate_parts(99, 10), 10);
    }

    #[test]
    fn test_part_byte_range() {
        // First part
        let (start, end) = part_byte_range(1, 100, 250);
        assert_eq!(start, 0);
        assert_eq!(end, 100);

        // Middle part
        let (start, end) = part_byte_range(2, 100, 250);
        assert_eq!(start, 100);
        assert_eq!(end, 200);

        // Last part (smaller)
        let (start, end) = part_byte_range(3, 100, 250);
        assert_eq!(start, 200);
        assert_eq!(end, 250);
    }
}
