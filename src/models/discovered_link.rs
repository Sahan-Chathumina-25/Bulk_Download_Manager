//! Discovery results and the plan that turns checked rows into queue items.
//!
//! Port of the Python link_discovery result dataclasses and the
//! discovery_queue_planner outcome types.

/// One link the scanner reports as a plausible download candidate
/// (port of DiscoveryCandidate).
#[derive(Debug, Clone)]
pub struct DiscoveryCandidate {
    pub candidate_url: String,
    pub filename: String,
    pub extension: Option<String>,
    pub source_page_url: String,
    pub content_type: Option<String>,
    pub size: Option<u64>,
    pub is_likely_file: bool,
}

/// Outcome of one page scan; scan problems land in errors, never panic
/// (port of DiscoveryResult).
#[derive(Debug, Clone, Default)]
pub struct DiscoveryResult {
    pub source_url: String,
    pub candidates: Vec<DiscoveryCandidate>,
    pub errors: Vec<String>,
    pub elapsed_seconds: f64,
}

/// The result of planning which checked candidates may join the queue
/// (port of the planner QueuePlan).
#[derive(Debug, Clone, Default)]
pub struct QueuePlan {
    /// Candidates eligible for the queue (first occurrence of each URL
    /// wins; later repeats land in duplicates).
    pub to_add: Vec<DiscoveryCandidate>,
    /// (label, reason) pairs for URLs already present in the queue/history
    /// or repeated within the same selection.
    pub duplicates: Vec<(String, String)>,
    /// (label, reason) pairs for unusable URLs.
    pub rejected: Vec<(String, String)>,
}

impl QueuePlan {
    /// True when there is nothing to report at all.
    pub fn is_empty(&self) -> bool {
        self.to_add.is_empty() && self.duplicates.is_empty() && self.rejected.is_empty()
    }
}
