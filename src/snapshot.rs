use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotDiff {
    pub files: Vec<FileDiff>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub status: FileDiffStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FileDiffStatus {
    Added,
    Removed,
    Changed,
    Matched,
}

impl SnapshotDiff {
    pub fn has_differences(&self) -> bool {
        !self.files.is_empty()
    }
}

/// Extract files from a tar.gz archive into a sorted map of path -> content.
pub fn extract_tarball(data: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let gz = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(gz);
    let mut files = BTreeMap::new();

    let entries = archive.entries().map_err(|e| format!("Failed to read tarball: {}", e))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("Failed to read tarball entry: {}", e))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|e| format!("Invalid path in tarball: {}", e))?
            .to_string_lossy()
            .to_string();
        // Normalize: strip leading "./"
        let path = path.strip_prefix("./").unwrap_or(&path).to_string();
        if path.is_empty() {
            continue;
        }
        let mut content = Vec::new();
        entry
            .read_to_end(&mut content)
            .map_err(|e| format!("Failed to read file {}: {}", path, e))?;
        files.insert(path, content);
    }

    Ok(files)
}

/// Extract a single file from a tar.gz archive by path.
pub fn extract_file_from_tarball(data: &[u8], target_path: &str) -> Result<Option<Vec<u8>>, String> {
    let gz = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(gz);

    let entries = archive.entries().map_err(|e| format!("Failed to read tarball: {}", e))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("Failed to read tarball entry: {}", e))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|e| format!("Invalid path: {}", e))?
            .to_string_lossy()
            .to_string();
        let path = path.strip_prefix("./").unwrap_or(&path);
        if path == target_path {
            let mut content = Vec::new();
            entry
                .read_to_end(&mut content)
                .map_err(|e| format!("Failed to read file: {}", e))?;
            return Ok(Some(content));
        }
    }

    Ok(None)
}

/// Compare current snapshot files against a baseline.
/// Baseline is a map of path -> content (from the snapshot_baseline table).
/// Current is a map of path -> content (extracted from the job's tarball).
pub fn compare(
    baseline: &BTreeMap<String, Vec<u8>>,
    current: &BTreeMap<String, Vec<u8>>,
) -> SnapshotDiff {
    let mut files = Vec::new();

    // Check for removed and changed files
    for (path, baseline_content) in baseline {
        match current.get(path) {
            None => files.push(FileDiff {
                path: path.clone(),
                status: FileDiffStatus::Removed,
            }),
            Some(current_content) => {
                if baseline_content != current_content {
                    files.push(FileDiff {
                        path: path.clone(),
                        status: FileDiffStatus::Changed,
                    });
                }
            }
        }
    }

    // Check for added files
    for path in current.keys() {
        if !baseline.contains_key(path) {
            files.push(FileDiff {
                path: path.clone(),
                status: FileDiffStatus::Added,
            });
        }
    }

    // Sort by path for consistent ordering
    files.sort_by(|a, b| a.path.cmp(&b.path));

    SnapshotDiff { files }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "text")]
#[serde(rename_all = "snake_case")]
pub enum DiffLine {
    Context(String),
    Added(String),
    Removed(String),
}

/// Pretty-print two JSON byte slices and produce a line-by-line unified diff.
/// Returns None if either side isn't valid JSON.
pub fn json_diff(old: &[u8], new: &[u8]) -> Option<Vec<DiffLine>> {
    let old_val: Value = serde_json::from_slice(old).ok()?;
    let new_val: Value = serde_json::from_slice(new).ok()?;
    let old_pretty = serde_json::to_string_pretty(&old_val).ok()?;
    let new_pretty = serde_json::to_string_pretty(&new_val).ok()?;
    Some(unified_diff(&old_pretty, &new_pretty, 3))
}

/// Produce a unified diff with context lines between two texts.
fn unified_diff(old: &str, new: &str, context: usize) -> Vec<DiffLine> {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(old, new);
    let mut lines = Vec::new();

    for hunk in diff.unified_diff().context_radius(context).iter_hunks() {
        for change in hunk.iter_changes() {
            let text = change.value().trim_end_matches('\n').to_string();
            match change.tag() {
                ChangeTag::Equal => lines.push(DiffLine::Context(text)),
                ChangeTag::Insert => lines.push(DiffLine::Added(text)),
                ChangeTag::Delete => lines.push(DiffLine::Removed(text)),
            }
        }
    }

    lines
}
