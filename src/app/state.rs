//! Pure UI state and the transitions the widgets perform on it.
//!
//! Everything here is deliberately free of egui and of I/O so it can be unit
//! tested directly. The app shell owns one instance per page and the backend
//! feeds it through the `UiEvent` variants.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::Duration;

use crate::config::Settings;
use crate::database::repository::HistoryQuery;
use crate::models::discovered_link::{DiscoveryCandidate, DiscoveryResult};
use crate::models::download_item::DownloadRecord;
use crate::models::download_status::{DownloadProgress, DownloadState};
use crate::services::url_intake::ParseResult;

/// The six top-level pages, in navigation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Page {
    Dashboard,
    AddDownloads,
    Discover,
    Queue,
    History,
    Settings,
}

impl Page {
    pub const ALL: [Self; 6] = [
        Self::Dashboard,
        Self::AddDownloads,
        Self::Discover,
        Self::Queue,
        Self::History,
        Self::Settings,
    ];

    /// Label shown in the navigation rail.
    pub fn title(self) -> &'static str {
        match self {
            Self::Dashboard => "Dashboard",
            Self::AddDownloads => "Add Downloads",
            Self::Discover => "Discover",
            Self::Queue => "Queue",
            Self::History => "History",
            Self::Settings => "Settings",
        }
    }

    /// Keyboard shortcut hint, also the accelerator implemented by the shell.
    pub fn shortcut(self) -> &'static str {
        match self {
            Self::Dashboard => "Ctrl+1",
            Self::AddDownloads => "Ctrl+2",
            Self::Discover => "Ctrl+3",
            Self::Queue => "Ctrl+4",
            Self::History => "Ctrl+5",
            Self::Settings => "Ctrl+6",
        }
    }
}

/// True for states that occupy a worker slot right now.
pub fn is_active(state: DownloadState) -> bool {
    matches!(
        state,
        DownloadState::Connecting | DownloadState::Downloading
    )
}

/// Everything the queue row needs to draw its progress cell.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgressView {
    pub downloaded_bytes: u64,
    /// `None` when the server never reported a length.
    pub total_bytes: Option<u64>,
    pub speed_bps: f64,
    pub eta_seconds: Option<u64>,
    /// `None` means "indeterminate" (unknown or zero length).
    pub fraction: Option<f32>,
}

/// Activity summary derived from the same live rows the queue page renders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActivityCounts {
    pub active: usize,
    pub queued: usize,
    pub failed: usize,
}

/// Live queue rows plus the ephemeral progress snapshots that decorate them.
///
/// Progress is kept outside [`DownloadRecord`] because the engine throttles it
/// to ~4/s and never persists it; the record only carries the last durable
/// `downloaded_bytes` value.
#[derive(Debug, Default)]
pub struct QueueState {
    records: Vec<DownloadRecord>,
    progress: HashMap<i64, DownloadProgress>,
    /// Progress for a row the manager has not assigned an id to yet.
    ///
    /// The engine emits `download_id: -1` until the record is persisted, so
    /// there is at most one such snapshot; the newest wins.
    unsaved_progress: Option<DownloadProgress>,
    selected: BTreeSet<i64>,
}

impl QueueState {
    pub fn records(&self) -> &[DownloadRecord] {
        &self.records
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Real activity counts for the navigation rail summary.
    pub fn activity_counts(&self) -> ActivityCounts {
        let mut counts = ActivityCounts::default();
        for record in &self.records {
            match record.state {
                DownloadState::Connecting | DownloadState::Downloading => counts.active += 1,
                DownloadState::Queued => counts.queued += 1,
                DownloadState::Failed => counts.failed += 1,
                DownloadState::Paused
                | DownloadState::Completed
                | DownloadState::Cancelled
                | DownloadState::Interrupted => {}
            }
        }
        counts
    }

    /// Replace the whole row set (initial load / refresh).
    ///
    /// Progress and selection entries whose row disappeared are dropped so a
    /// removal never leaves stale bytes behind.
    pub fn replace(&mut self, records: Vec<DownloadRecord>) {
        let live_ids: BTreeSet<i64> = records.iter().filter_map(|record| record.id).collect();
        self.progress.retain(|id, _| live_ids.contains(id));
        self.selected.retain(|id| live_ids.contains(id));
        self.records = records;
    }

    /// Insert a new row or update an existing one in place.
    ///
    /// Order is preserved: engine updates never reshuffle the queue.
    pub fn upsert(&mut self, record: DownloadRecord) {
        if let Some(id) = record.id
            && let Some(existing) = self
                .records
                .iter_mut()
                .find(|candidate| candidate.id == Some(id))
        {
            let settled = record.state.is_terminal();
            *existing = record;
            if settled {
                // The record now owns the final byte count; the snapshot
                // would only go stale.
                self.progress.remove(&id);
            }
            return;
        }

        self.records.push(record);
    }

    /// Drop the given rows, their progress and their selection.
    ///
    /// Exercised by unit tests; the page removes rows through the backend.
    #[allow(dead_code)]
    pub fn remove(&mut self, ids: &[i64]) {
        if ids.is_empty() {
            return;
        }
        let doomed: BTreeSet<i64> = ids.iter().copied().collect();
        self.records
            .retain(|record| record.id.is_none_or(|id| !doomed.contains(&id)));
        for id in &doomed {
            self.progress.remove(id);
            self.selected.remove(id);
        }
    }

    /// Store a throttled progress snapshot.
    pub fn apply_progress(&mut self, progress: DownloadProgress) {
        if progress.download_id < 0 {
            self.unsaved_progress = Some(progress);
        } else {
            self.progress.insert(progress.download_id, progress);
        }
    }

    /// The snapshot that belongs to `record`, if any.
    pub fn progress_for(&self, record: &DownloadRecord) -> Option<&DownloadProgress> {
        match record.id {
            Some(id) => self.progress.get(&id),
            // Only live rows can still be waiting for their id.
            None if record.state.is_live() => self.unsaved_progress.as_ref(),
            None => None,
        }
    }

    /// Merge the persisted row with its live snapshot into one view model.
    ///
    /// Totals prefer the live snapshot (the server's real length) and fall
    /// back to the size the manager stored when the row was created.
    pub fn view_for(&self, record: &DownloadRecord) -> ProgressView {
        let snapshot = self.progress_for(record);

        let downloaded_bytes = snapshot.map_or(record.downloaded_bytes, |p| p.downloaded_bytes);
        let total_bytes = snapshot
            .and_then(|p| p.total_bytes)
            .or(record.expected_size);
        let speed_bps = snapshot.map_or(0.0, |p| p.speed_bps);
        let eta_seconds = snapshot.and_then(|p| p.eta_seconds);

        // A zero or missing length cannot produce a ratio: the bar stays
        // indeterminate rather than pretending the transfer is complete.
        let fraction = if record.state == DownloadState::Completed {
            Some(1.0)
        } else {
            match total_bytes {
                Some(total) if total > 0 => {
                    let ratio = downloaded_bytes as f32 / total as f32;
                    Some(ratio.clamp(0.0, 1.0))
                }
                _ => None,
            }
        };

        ProgressView {
            downloaded_bytes,
            total_bytes,
            speed_bps,
            eta_seconds,
            fraction,
        }
    }

    pub fn selected(&self) -> &BTreeSet<i64> {
        &self.selected
    }

    pub fn selected_ids(&self) -> Vec<i64> {
        self.selected.iter().copied().collect()
    }

    pub fn toggle_selected(&mut self, id: i64) {
        if !self.selected.remove(&id) {
            self.selected.insert(id);
        }
    }

    /// Set one row's tick state.
    ///
    /// The page toggles rows; this is the explicit form used by tests.
    #[allow(dead_code)]
    pub fn set_selected(&mut self, id: i64, selected: bool) {
        if selected {
            self.selected.insert(id);
        } else {
            self.selected.remove(&id);
        }
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
    }

    /// Tick every row currently in the list.
    ///
    /// No page exposes a select-all control yet; the helper stays tested.
    #[allow(dead_code)]
    pub fn select_all(&mut self) {
        self.selected = self.records.iter().filter_map(|record| record.id).collect();
    }

    /// True while at least one row is queued or transferring.
    pub fn has_active(&self) -> bool {
        self.records
            .iter()
            .any(|record| record.state.is_live() && record.state != DownloadState::Paused)
    }
}

/// Paged, filtered history (the immutable side of the queue).
#[derive(Default)]
pub struct HistoryState {
    pub query: HistoryQuery,
    pub records: Vec<DownloadRecord>,
    pub loading: bool,
    pub error: Option<String>,
    /// Free-text box; committed to `query.search` by [`Self::apply_search`].
    pub search_input: String,
    pub selected: BTreeSet<i64>,
    /// Checkbox state for the clear-history dialog. Unchecked by default:
    /// deleting files is the destructive option.
    pub keep_files_on_clear: bool,
}

impl HistoryState {
    /// Commit the search box and jump back to the first page.
    pub fn apply_search(&mut self) {
        let trimmed = self.search_input.trim();
        self.query.search = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        self.query.offset = 0;
    }

    pub fn set_state_filter(&mut self, state: Option<DownloadState>) {
        self.query.state = state.map(|state| state.as_str().to_string());
        self.query.offset = 0;
    }

    /// Sort by `column`, flipping direction when it is already active.
    pub fn toggle_sort(&mut self, column: &str) {
        if self.query.sort_by == column {
            self.query.sort_dir = if self.query.sort_dir.eq_ignore_ascii_case("ASC") {
                "DESC".to_string()
            } else {
                "ASC".to_string()
            };
        } else {
            self.query.sort_by = column.to_string();
            self.query.sort_dir = "DESC".to_string();
        }
        self.query.offset = 0;
    }

    /// True when another page may exist after this one.
    pub fn has_next_page(&self) -> bool {
        self.records.len() >= self.query.limit && self.query.limit > 0
    }

    pub fn next_page(&mut self) {
        if self.has_next_page() {
            self.query.offset += self.query.limit;
        }
    }

    pub fn previous_page(&mut self) {
        self.query.offset = self.query.offset.saturating_sub(self.query.limit);
    }

    /// 1-based page number for the footer.
    pub fn page_number(&self) -> usize {
        self.query
            .offset
            .checked_div(self.query.limit)
            .map_or(1, |page| page + 1)
    }

    pub fn apply_loaded(&mut self, records: Vec<DownloadRecord>, error: Option<String>) {
        self.loading = false;
        self.error = error.clone();
        if error.is_none() {
            self.records = records;
            let present: BTreeSet<i64> =
                self.records.iter().filter_map(|record| record.id).collect();
            self.selected.retain(|id| present.contains(id));
        }
    }

    /// Snapshot the current filter/sort/page as a query for the backend.
    ///
    /// `HistoryQuery` is not `Clone`, so the UI hands over a fresh value each
    /// time it asks for a page.
    pub fn to_query(&self) -> HistoryQuery {
        HistoryQuery {
            state: self.query.state.clone(),
            search: self.query.search.clone(),
            sort_by: self.query.sort_by.clone(),
            sort_dir: self.query.sort_dir.clone(),
            limit: self.query.limit,
            offset: self.query.offset,
        }
    }
}

/// Discover page: the last scan result plus the per-row tick state.
#[derive(Default)]
pub struct DiscoveryState {
    pub page_url: String,
    pub scanning: bool,
    pub status: String,
    pub result: Option<DiscoveryResult>,
    /// Parallel to `result.candidates`; `true` means "add this row".
    pub selected: Vec<bool>,
    /// Show rows that do not look like files.
    pub include_unknown: bool,
    /// Result of the last "Add Selected to Queue" action.
    pub outcome: Option<AddOutcome>,
}

impl DiscoveryState {
    /// Store a finished scan, pre-ticking the likely files.
    pub fn apply_result(&mut self, result: DiscoveryResult) {
        self.selected = result
            .candidates
            .iter()
            .map(|candidate| candidate.is_likely_file && candidate.filtered_reason.is_none())
            .collect();
        self.status = scan_summary(&result);
        self.result = Some(result);
        self.outcome = None;
        self.scanning = false;
    }

    pub fn clear(&mut self) {
        self.result = None;
        self.selected.clear();
        self.status.clear();
        self.outcome = None;
        self.scanning = false;
    }

    pub fn candidates(&self) -> &[DiscoveryCandidate] {
        self.result
            .as_ref()
            .map_or(&[], |result| result.candidates.as_slice())
    }

    /// Row indices to render, honouring the "include unknown" switch.
    pub fn visible_indices(&self) -> Vec<usize> {
        self.candidates()
            .iter()
            .enumerate()
            .filter(|(_, candidate)| self.include_unknown || candidate.is_likely_file)
            .map(|(index, _)| index)
            .collect()
    }

    pub fn is_selected(&self, index: usize) -> bool {
        self.selected.get(index).copied().unwrap_or(false)
    }

    /// Tick or untick one candidate.
    ///
    /// Filtered (ad/tracker) rows can never be selected, so the bulk enqueue
    /// path and the visible checkbox stay in agreement.
    pub fn toggle(&mut self, index: usize) {
        let selectable = self
            .candidates()
            .get(index)
            .is_some_and(|candidate| candidate.filtered_reason.is_none());
        if !selectable {
            return;
        }
        if let Some(slot) = self.selected.get_mut(index) {
            *slot = !*slot;
        }
    }

    pub fn set_all(&mut self, selected: bool) {
        // Precompute which rows the current filter exposes: the bulk action
        // must never touch rows the user cannot see, nor rows the ad/tracker
        // filter rejected.
        let visible: Vec<bool> = self
            .candidates()
            .iter()
            .map(|candidate| {
                (self.include_unknown || candidate.is_likely_file)
                    && candidate.filtered_reason.is_none()
            })
            .collect();

        for (slot, is_visible) in self.selected.iter_mut().zip(visible) {
            if is_visible {
                *slot = selected;
            }
        }
    }

    pub fn selected_count(&self) -> usize {
        self.selected.iter().filter(|ticked| **ticked).count()
    }

    /// Total advertised size of the ticked candidates.
    ///
    /// Covered by unit tests; the Discover page shows per-row sizes only.
    #[allow(dead_code)]
    pub fn selected_size_total(&self) -> u64 {
        self.candidates()
            .iter()
            .zip(&self.selected)
            .filter(|(_, ticked)| **ticked)
            .filter_map(|(candidate, _)| candidate.size)
            .sum()
    }

    /// Candidates ready for the planner.
    ///
    /// `checked_candidates` filters on `is_likely_file`, so the UI's tick state
    /// is folded into that flag on a clone. The scan result itself is left
    /// untouched so the table keeps showing the real classification.
    /// Ad/tracker-filtered rows are never handed to the planner, even if the
    /// user ticks them.
    pub fn candidates_for_planner(&self) -> Vec<DiscoveryCandidate> {
        self.candidates()
            .iter()
            .zip(&self.selected)
            .filter(|(_, ticked)| **ticked)
            .filter(|(candidate, _)| candidate.filtered_reason.is_none())
            .map(|(candidate, _)| DiscoveryCandidate {
                is_likely_file: true,
                ..candidate.clone()
            })
            .collect()
    }
}

/// One-line result banner used by the Discover page.
pub fn scan_summary(result: &DiscoveryResult) -> String {
    let likely = result
        .candidates
        .iter()
        .filter(|candidate| candidate.is_likely_file)
        .count();
    let mut summary = format!(
        "Found {} link(s) ({} likely file(s)) in {:.1}s — tick rows then \"Add Selected to Queue\".",
        result.candidates.len(),
        likely,
        result.elapsed_seconds
    );
    if result.filtered_count > 0 {
        summary.push_str(&format!(
            " {} filtered as ads/trackers.",
            result.filtered_count
        ));
    }
    summary
}

/// Add Downloads form.
#[derive(Debug, Default)]
pub struct AddDownloadsState {
    pub urls_text: String,
    pub destination: Option<PathBuf>,
    /// Only meaningful for a single URL, as in the Python dialog.
    pub filename_override: String,
    pub checksum: String,
    pub start_immediately: bool,
    pub preview: Option<ParseResult>,
    pub error: Option<String>,
    pub outcome: Option<AddOutcome>,
    /// A preview request is in flight; the buttons stay disabled until the
    /// backend answers.
    pub preview_pending: bool,
    /// An add request is in flight; feedback is only shown once it returns.
    pub add_pending: bool,
}

impl AddDownloadsState {
    pub fn new() -> Self {
        Self {
            start_immediately: true,
            ..Self::default()
        }
    }

    /// Destination picked in the dialog, else the configured default.
    pub fn effective_destination(&self, settings: &Settings) -> Option<PathBuf> {
        if let Some(destination) = &self.destination {
            return Some(destination.clone());
        }
        let configured = settings.default_destination_dir.trim();
        if configured.is_empty() {
            None
        } else {
            Some(PathBuf::from(configured))
        }
    }

    pub fn clear_feedback(&mut self) {
        self.preview = None;
        self.error = None;
        self.outcome = None;
        self.preview_pending = false;
        self.add_pending = false;
    }
}

/// Result of one "Add" action.
///
/// Counts rather than records: the UI only needs the headline numbers and the
/// per-URL failure reasons, and the queue refreshes itself from the backend.
#[derive(Debug, Default, Clone)]
pub struct AddOutcome {
    pub added: usize,
    pub failed: Vec<(String, String)>,
    pub started: usize,
}

/// Severity of a transient toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Success,
    Warning,
    Error,
}

/// One toast. Lifetime is a countdown so tests stay deterministic.
#[derive(Debug, Clone)]
pub struct Notification {
    pub text: String,
    pub severity: Severity,
    pub remaining: Duration,
}

/// Bounded toast queue.
#[derive(Debug, Default)]
pub struct Notifications {
    items: Vec<Notification>,
}

/// Newest toasts replace older ones beyond this cap.
const MAX_NOTIFICATIONS: usize = 4;

impl Notifications {
    /// Push a toast; errors linger longer than confirmations.
    pub fn push(&mut self, severity: Severity, text: impl Into<String>) {
        let seconds = match severity {
            Severity::Error | Severity::Warning => 10,
            Severity::Info | Severity::Success => 6,
        };
        self.items.push(Notification {
            text: text.into(),
            severity,
            remaining: Duration::from_secs(seconds),
        });
        while self.items.len() > MAX_NOTIFICATIONS {
            self.items.remove(0);
        }
    }

    /// Advance every countdown and drop expired toasts.
    pub fn tick(&mut self, delta: Duration) {
        for notification in &mut self.items {
            notification.remaining = notification.remaining.saturating_sub(delta);
        }
        self.items
            .retain(|notification| !notification.remaining.is_zero());
    }

    pub fn items(&self) -> &[Notification] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Drop every toast. Toasts already expire on their own.
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.items.clear();
    }
}

/// Settings page draft: a validated [`Settings`] plus the text buffers egui
/// needs for numeric fields.
#[derive(Debug)]
pub struct SettingsState {
    pub draft: Settings,
    /// Serialised snapshot of the last saved state; `Settings` has no
    /// `PartialEq`, and its JSON form is the same string the repository stores.
    saved_json: String,
    pub max_simultaneous_text: String,
    pub connection_timeout_text: String,
    pub retry_count_text: String,
    pub retry_delay_text: String,
    pub buffer_size_text: String,
    /// KB/s (1 KB = 1024 B), matching the Python dialog.
    pub bandwidth_kbps_text: String,
    pub user_agent_text: String,
    pub allowed_extensions_text: String,
    pub error: Option<String>,
}

/// Bytes in a legacy "KB" for the bandwidth field.
const KIB: f64 = 1024.0;

impl Default for SettingsState {
    fn default() -> Self {
        let mut state = Self {
            draft: Settings::default(),
            saved_json: String::new(),
            max_simultaneous_text: String::new(),
            connection_timeout_text: String::new(),
            retry_count_text: String::new(),
            retry_delay_text: String::new(),
            buffer_size_text: String::new(),
            bandwidth_kbps_text: String::new(),
            user_agent_text: String::new(),
            allowed_extensions_text: String::new(),
            error: None,
        };
        state.load(Settings::default());
        state
    }
}

impl SettingsState {
    /// Replace the draft and treat it as the saved baseline.
    pub fn load(&mut self, settings: Settings) {
        self.max_simultaneous_text = settings.max_simultaneous.to_string();
        self.connection_timeout_text = settings.connection_timeout.to_string();
        self.retry_count_text = settings.retry_count.to_string();
        self.retry_delay_text = settings.retry_delay.to_string();
        self.buffer_size_text = settings.buffer_size.to_string();
        self.bandwidth_kbps_text = (settings.bandwidth_limit_bps as f64 / KIB).to_string();
        self.user_agent_text = settings.user_agent.clone();
        self.allowed_extensions_text = settings
            .allowed_extensions
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        self.draft = settings;
        self.saved_json = self.draft.to_json();
        self.error = None;
    }

    /// True when the draft differs from the last saved settings.
    pub fn is_dirty(&self) -> bool {
        self.draft.to_json() != self.saved_json
    }

    pub fn mark_saved(&mut self, settings: Settings) {
        self.load(settings);
    }

    /// Parse the text buffers into the draft, reporting the first bad field.
    pub fn sync_buffers(&mut self) -> Result<(), String> {
        self.draft.max_simultaneous =
            parse_number("Max simultaneous", &self.max_simultaneous_text)?;
        self.draft.connection_timeout =
            parse_number("Connection timeout", &self.connection_timeout_text)?;
        self.draft.retry_count = parse_number("Retry count", &self.retry_count_text)?;
        self.draft.retry_delay = parse_number("Retry delay", &self.retry_delay_text)?;
        self.draft.buffer_size = parse_number("Buffer size", &self.buffer_size_text)?;

        let kbps: f64 = parse_number("Bandwidth limit", &self.bandwidth_kbps_text)?;
        if kbps < 0.0 || !kbps.is_finite() {
            return Err("Bandwidth limit must be 0 or a positive number of KB/s".to_string());
        }
        self.draft.bandwidth_limit_bps = (kbps * KIB).round() as u64;

        self.draft.user_agent = self.user_agent_text.clone();
        self.draft.allowed_extensions = parse_extensions(&self.allowed_extensions_text);
        Ok(())
    }

    /// Full validation pass; returns the settings the backend should persist.
    pub fn validated_settings(&mut self) -> Result<Settings, String> {
        self.sync_buffers()?;
        let validated = self.draft.clone().validated()?;
        // Keep the clamped/normalised values so the UI reflects what is saved.
        self.draft = validated.clone();
        Ok(validated)
    }

    pub fn set_error(&mut self, message: Option<String>) {
        self.error = message;
    }
}

/// Split a comma/whitespace separated extension list into a normalised set.
fn parse_extensions(text: &str) -> BTreeSet<String> {
    text.split(|character: char| character == ',' || character.is_whitespace())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(|token| token.trim_start_matches('.').to_lowercase())
        .collect()
}

/// Parse a numeric buffer, naming the field in the error message.
fn parse_number<T>(label: &str, text: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    text.trim()
        .parse::<T>()
        .map_err(|_| format!("{label} must be a number"))
}

/// A modal awaiting the user's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirmation {
    ResumeInterrupted { count: usize },
    ClearHistory,
    QuitWithActive { count: usize },
    RemoveQueue { ids: Vec<i64> },
    CancelQueue { ids: Vec<i64> },
}

impl Confirmation {
    pub fn title(&self) -> &'static str {
        match self {
            Self::ResumeInterrupted { .. } => "Resume interrupted downloads",
            Self::ClearHistory => "Clear history",
            Self::QuitWithActive { .. } => "Quit with active downloads?",
            Self::RemoveQueue { .. } => "Remove downloads",
            Self::CancelQueue { .. } => "Cancel downloads",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::ResumeInterrupted { count } => format!(
                "{} download(s) were interrupted when the application last closed. Resume them now?",
                count
            ),
            Self::ClearHistory => {
                "Delete ALL completed, failed, cancelled and interrupted records from the history?"
                    .to_string()
            }
            Self::QuitWithActive { count } => format!(
                "{count} download(s) are still active. Quit anyway? Partial files are kept for resume-capable ones."
            ),
            Self::RemoveQueue { ids } => format!(
                "Remove {} download(s) from the queue/history? Files are kept.",
                ids.len()
            ),
            Self::CancelQueue { ids } => format!(
                "Cancel {} download(s)? Partial files are kept for resume-capable ones.",
                ids.len()
            ),
        }
    }

    pub fn accept_label(&self) -> &'static str {
        match self {
            Self::ResumeInterrupted { .. } => "Resume",
            Self::ClearHistory => "Clear history",
            Self::QuitWithActive { .. } => "Quit anyway",
            Self::RemoveQueue { .. } => "Remove",
            Self::CancelQueue { .. } => "Cancel downloads",
        }
    }

    /// False for actions whose default must be the safe choice.
    pub fn is_destructive(&self) -> bool {
        matches!(
            self,
            Self::ClearHistory | Self::RemoveQueue { .. } | Self::CancelQueue { .. }
        )
    }
}

/// Headline counters for the dashboard cards.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DashboardStats {
    pub queued: usize,
    pub active: usize,
    pub completed: usize,
    pub failed: usize,
    pub bytes_downloaded_total: u64,
}

impl DashboardStats {
    pub fn from_records(records: &[DownloadRecord]) -> Self {
        let mut stats = Self::default();
        for record in records {
            match record.state {
                DownloadState::Queued => stats.queued += 1,
                DownloadState::Connecting | DownloadState::Downloading => stats.active += 1,
                DownloadState::Completed => {
                    stats.completed += 1;
                    stats.bytes_downloaded_total += record.downloaded_bytes;
                }
                DownloadState::Failed => stats.failed += 1,
                DownloadState::Paused | DownloadState::Cancelled | DownloadState::Interrupted => {}
            }
        }
        stats
    }
}

/// Dashboard snapshot pushed by the backend.
#[derive(Debug, Default)]
pub struct DashboardState {
    pub stats: DashboardStats,
    pub recent: Vec<DownloadRecord>,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: Option<i64>, state: DownloadState, expected: Option<u64>) -> DownloadRecord {
        DownloadRecord {
            id,
            state,
            expected_size: expected,
            display_name: format!("file-{id:?}"),
            ..DownloadRecord::default()
        }
    }

    fn progress(id: i64, downloaded: u64, total: Option<u64>) -> DownloadProgress {
        DownloadProgress {
            download_id: id,
            downloaded_bytes: downloaded,
            total_bytes: total,
            speed_bps: 1024.0,
            eta_seconds: Some(30),
        }
    }

    #[test]
    fn page_titles_and_shortcuts_cover_all_pages() {
        for (index, page) in Page::ALL.iter().enumerate() {
            assert!(!page.title().is_empty());
            assert_eq!(page.shortcut(), format!("Ctrl+{}", index + 1));
        }
    }

    #[test]
    fn upsert_updates_in_place_without_reordering() {
        let mut queue = QueueState::default();
        queue.replace(vec![
            record(Some(1), DownloadState::Queued, None),
            record(Some(2), DownloadState::Queued, None),
        ]);

        let mut updated = record(Some(1), DownloadState::Downloading, None);
        updated.downloaded_bytes = 512;
        queue.upsert(updated);

        assert_eq!(queue.len(), 2);
        assert_eq!(queue.records()[0].id, Some(1));
        assert_eq!(queue.records()[0].state, DownloadState::Downloading);
        assert_eq!(queue.records()[1].id, Some(2));
    }

    #[test]
    fn upsert_appends_unknown_ids() {
        let mut queue = QueueState::default();
        queue.upsert(record(None, DownloadState::Queued, None));
        queue.upsert(record(None, DownloadState::Queued, None));
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn terminal_update_drops_stale_progress() {
        let mut queue = QueueState::default();
        queue.replace(vec![record(Some(7), DownloadState::Downloading, Some(100))]);
        queue.apply_progress(progress(7, 50, Some(100)));
        assert!(queue.progress_for(&queue.records()[0]).is_some());

        queue.upsert(record(Some(7), DownloadState::Completed, Some(100)));
        assert!(queue.progress_for(&queue.records()[0]).is_none());
    }

    #[test]
    fn replace_prunes_orphaned_progress_and_selection() {
        let mut queue = QueueState::default();
        queue.replace(vec![record(Some(1), DownloadState::Downloading, None)]);
        queue.apply_progress(progress(1, 10, None));
        queue.set_selected(1, true);

        queue.replace(vec![record(Some(2), DownloadState::Queued, None)]);
        assert!(queue.selected().is_empty());
        assert!(queue.progress_for(&queue.records()[0]).is_none());
    }

    #[test]
    fn progress_with_negative_id_is_held_until_the_row_has_an_id() {
        let mut queue = QueueState::default();
        queue.apply_progress(progress(-1, 4096, Some(8192)));

        // A row still awaiting persistence picks up the unsaved snapshot.
        let unsaved = record(None, DownloadState::Downloading, None);
        let view = queue.view_for(&unsaved);
        assert_eq!(view.downloaded_bytes, 4096);
        assert_eq!(view.total_bytes, Some(8192));
        assert_eq!(view.fraction, Some(0.5));
    }

    #[test]
    fn unknown_total_is_indeterminate() {
        let queue = QueueState::default();
        let view = queue.view_for(&record(Some(1), DownloadState::Downloading, None));
        assert_eq!(view.fraction, None);
        assert_eq!(view.total_bytes, None);
    }

    #[test]
    fn zero_total_is_indeterminate_not_complete() {
        let queue = QueueState::default();
        let view = queue.view_for(&record(Some(1), DownloadState::Downloading, Some(0)));
        assert_eq!(view.total_bytes, Some(0));
        assert_eq!(view.fraction, None);
    }

    #[test]
    fn completed_rows_always_show_a_full_bar() {
        let queue = QueueState::default();
        let view = queue.view_for(&record(Some(1), DownloadState::Completed, None));
        assert_eq!(view.fraction, Some(1.0));
    }

    #[test]
    fn fraction_is_clamped_when_the_server_lies_about_the_length() {
        let mut queue = QueueState::default();
        queue.replace(vec![record(Some(1), DownloadState::Downloading, Some(10))]);
        queue.apply_progress(progress(1, 25, Some(10)));
        assert_eq!(queue.view_for(&queue.records()[0]).fraction, Some(1.0));
    }

    #[test]
    fn stored_expected_size_is_the_fallback_total() {
        let queue = QueueState::default();
        let view = queue.view_for(&DownloadRecord {
            downloaded_bytes: 512,
            ..record(Some(1), DownloadState::Downloading, Some(1024))
        });
        assert_eq!(view.total_bytes, Some(1024));
        assert_eq!(view.fraction, Some(0.5));
    }

    #[test]
    fn selection_helpers_track_existing_rows() {
        let mut queue = QueueState::default();
        queue.replace(vec![
            record(Some(1), DownloadState::Queued, None),
            record(Some(2), DownloadState::Queued, None),
        ]);

        queue.toggle_selected(1);
        assert_eq!(queue.selected_ids(), vec![1]);
        queue.toggle_selected(1);
        assert!(queue.selected_ids().is_empty());

        queue.select_all();
        assert_eq!(queue.selected_ids(), vec![1, 2]);
        queue.clear_selection();
        assert!(queue.selected_ids().is_empty());
    }

    #[test]
    fn remove_drops_rows_and_their_state() {
        let mut queue = QueueState::default();
        queue.replace(vec![
            record(Some(1), DownloadState::Queued, None),
            record(Some(2), DownloadState::Queued, None),
        ]);
        queue.set_selected(1, true);
        queue.apply_progress(progress(1, 5, None));

        queue.remove(&[1]);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.records()[0].id, Some(2));
        assert!(queue.selected().is_empty());
    }

    #[test]
    fn paused_rows_do_not_count_as_active() {
        let mut queue = QueueState::default();
        queue.replace(vec![record(Some(1), DownloadState::Paused, None)]);
        assert!(!queue.has_active());
        queue.upsert(record(Some(2), DownloadState::Downloading, None));
        assert!(queue.has_active());
    }

    #[test]
    fn history_search_commits_and_resets_paging() {
        let mut history = HistoryState::default();
        history.query.offset = 200;
        history.search_input = "  report  ".to_string();
        history.apply_search();
        assert_eq!(history.query.search.as_deref(), Some("report"));
        assert_eq!(history.query.offset, 0);

        history.search_input = "   ".to_string();
        history.apply_search();
        assert_eq!(history.query.search, None);
    }

    #[test]
    fn history_sort_toggles_direction_for_the_same_column() {
        let mut history = HistoryState::default();
        assert_eq!(history.query.sort_by, "created_at");
        assert_eq!(history.query.sort_dir, "DESC");

        history.toggle_sort("created_at");
        assert_eq!(history.query.sort_dir, "ASC");
        history.toggle_sort("created_at");
        assert_eq!(history.query.sort_dir, "DESC");

        history.toggle_sort("downloaded_bytes");
        assert_eq!(history.query.sort_by, "downloaded_bytes");
        assert_eq!(history.query.sort_dir, "DESC");
    }

    #[test]
    fn history_paging_uses_the_query_limit() {
        let mut history = HistoryState::default();
        assert_eq!(history.page_number(), 1);
        assert!(!history.has_next_page());

        history.records = vec![record(Some(1), DownloadState::Completed, None); 100];
        assert!(history.has_next_page());
        history.next_page();
        assert_eq!(history.query.offset, 100);
        assert_eq!(history.page_number(), 2);
        history.previous_page();
        assert_eq!(history.query.offset, 0);
    }

    #[test]
    fn history_state_filter_stores_the_database_string() {
        let mut history = HistoryState::default();
        history.set_state_filter(Some(DownloadState::Failed));
        assert_eq!(history.query.state.as_deref(), Some("failed"));
        history.set_state_filter(None);
        assert_eq!(history.query.state, None);
    }

    #[test]
    fn discovery_preselects_likely_files_only() {
        let mut discovery = DiscoveryState::default();
        discovery.apply_result(DiscoveryResult {
            source_url: "http://example.test/page".to_string(),
            candidates: vec![
                DiscoveryCandidate {
                    candidate_url: "http://example.test/a.zip".to_string(),
                    filename: "a.zip".to_string(),
                    extension: Some("zip".to_string()),
                    source_page_url: String::new(),
                    content_type: None,
                    size: Some(2048),
                    is_likely_file: true,
                    anchor_text: None,
                    filtered_reason: None,
                },
                DiscoveryCandidate {
                    candidate_url: "http://example.test/about".to_string(),
                    filename: "about".to_string(),
                    extension: None,
                    source_page_url: String::new(),
                    content_type: None,
                    size: None,
                    is_likely_file: false,
                    anchor_text: None,
                    filtered_reason: None,
                },
            ],
            errors: Vec::new(),
            elapsed_seconds: 0.25,
            filtered_count: 0,
        });

        assert_eq!(discovery.selected_count(), 1);
        assert_eq!(discovery.visible_indices(), vec![0]);
        assert_eq!(discovery.selected_size_total(), 2048);
        assert!(discovery.status.contains("Found 2 link(s)"));

        discovery.include_unknown = true;
        assert_eq!(discovery.visible_indices(), vec![0, 1]);
    }

    #[test]
    fn discovery_bulk_select_respects_the_visibility_filter() {
        let mut discovery = DiscoveryState::default();
        discovery.apply_result(DiscoveryResult {
            candidates: vec![
                DiscoveryCandidate {
                    candidate_url: "http://example.test/a.zip".to_string(),
                    filename: "a.zip".to_string(),
                    extension: Some("zip".to_string()),
                    source_page_url: String::new(),
                    content_type: None,
                    size: None,
                    is_likely_file: true,
                    anchor_text: None,
                    filtered_reason: None,
                },
                DiscoveryCandidate {
                    candidate_url: "http://example.test/b".to_string(),
                    filename: "b".to_string(),
                    extension: None,
                    source_page_url: String::new(),
                    content_type: None,
                    size: None,
                    is_likely_file: false,
                    anchor_text: None,
                    filtered_reason: None,
                },
            ],
            ..DiscoveryResult::default()
        });

        discovery.set_all(true);
        // The hidden unknown row stays unticked while the filter is off.
        assert_eq!(discovery.selected, vec![true, false]);

        discovery.include_unknown = true;
        discovery.set_all(true);
        assert_eq!(discovery.selected, vec![true, true]);

        discovery.set_all(false);
        assert_eq!(discovery.selected_count(), 0);
    }

    #[test]
    fn discovery_filtered_candidates_cannot_be_selected() {
        let mut discovery = DiscoveryState::default();
        discovery.apply_result(DiscoveryResult {
            candidates: vec![
                DiscoveryCandidate {
                    candidate_url: "http://ads.example.test/ad.zip".to_string(),
                    filename: "ad.zip".to_string(),
                    extension: Some("zip".to_string()),
                    source_page_url: String::new(),
                    content_type: None,
                    size: Some(4096),
                    is_likely_file: true,
                    anchor_text: None,
                    filtered_reason: Some("host is on the ad/tracker list".to_string()),
                },
                DiscoveryCandidate {
                    candidate_url: "http://example.test/real.zip".to_string(),
                    filename: "real.zip".to_string(),
                    extension: Some("zip".to_string()),
                    source_page_url: String::new(),
                    content_type: None,
                    size: Some(2048),
                    is_likely_file: true,
                    anchor_text: None,
                    filtered_reason: None,
                },
                DiscoveryCandidate {
                    candidate_url: "http://example.test/unknown".to_string(),
                    filename: "unknown".to_string(),
                    extension: None,
                    source_page_url: String::new(),
                    content_type: None,
                    size: None,
                    is_likely_file: false,
                    anchor_text: None,
                    filtered_reason: None,
                },
            ],
            filtered_count: 1,
            ..DiscoveryResult::default()
        });

        // Preselection skips the filtered row even though it is a likely file.
        assert_eq!(discovery.selected, vec![false, true, false]);

        // A direct toggle cannot select the filtered row.
        discovery.toggle(0);
        assert!(!discovery.is_selected(0));
        assert_eq!(discovery.selected_count(), 1);

        // Bulk selection never reaches a filtered row either, even once the
        // unknown row becomes visible.
        discovery.include_unknown = true;
        discovery.set_all(true);
        assert_eq!(discovery.selected, vec![false, true, true]);
        assert!(!discovery.is_selected(0));

        // The planner refuses filtered rows regardless of tick state.
        let planned = discovery.candidates_for_planner();
        assert_eq!(planned.len(), 2);
        assert!(
            planned
                .iter()
                .all(|candidate| candidate.filename != "ad.zip")
        );
    }

    #[test]
    fn discovery_planner_input_reflects_ticks() {
        let mut discovery = DiscoveryState::default();
        discovery.apply_result(DiscoveryResult {
            candidates: vec![DiscoveryCandidate {
                candidate_url: "http://example.test/page.html".to_string(),
                filename: "page.html".to_string(),
                extension: Some("html".to_string()),
                source_page_url: String::new(),
                content_type: None,
                size: None,
                is_likely_file: false,
                anchor_text: None,
                filtered_reason: None,
            }],
            ..DiscoveryResult::default()
        });

        // Not ticked: nothing reaches the planner.
        assert!(discovery.candidates_for_planner().is_empty());

        // Ticked by hand: the clone is promoted to "likely" for the planner.
        discovery.toggle(0);
        let planned = discovery.candidates_for_planner();
        assert_eq!(planned.len(), 1);
        assert!(planned[0].is_likely_file);
        // The table still shows the real classification.
        assert!(!discovery.candidates()[0].is_likely_file);
    }

    #[test]
    fn notifications_expire_after_their_budget() {
        let mut notifications = Notifications::default();
        notifications.push(Severity::Info, "saved");
        assert_eq!(notifications.items().len(), 1);

        notifications.tick(Duration::from_secs(5));
        assert_eq!(notifications.items().len(), 1);
        notifications.tick(Duration::from_secs(2));
        assert!(notifications.is_empty());
    }

    #[test]
    fn notifications_keep_only_the_newest_four() {
        let mut notifications = Notifications::default();
        for index in 0..6 {
            notifications.push(Severity::Info, format!("toast {index}"));
        }
        assert_eq!(notifications.items().len(), 4);
        assert_eq!(notifications.items()[0].text, "toast 2");
    }

    #[test]
    fn settings_round_trip_through_the_text_buffers() {
        let mut state = SettingsState {
            max_simultaneous_text: "7".to_string(),
            connection_timeout_text: "12.5".to_string(),
            retry_count_text: "2".to_string(),
            retry_delay_text: "0.5".to_string(),
            buffer_size_text: "8192".to_string(),
            bandwidth_kbps_text: "512".to_string(),
            user_agent_text: "agent/1.0".to_string(),
            allowed_extensions_text: "zip, .PDF  mp4".to_string(),
            ..Default::default()
        };

        let settings = state.validated_settings().unwrap();
        assert_eq!(settings.max_simultaneous, 7);
        assert_eq!(settings.connection_timeout, 12.5);
        assert_eq!(settings.bandwidth_limit_bps, 512 * 1024);
        assert_eq!(settings.user_agent, "agent/1.0");
        assert!(settings.allowed_extensions.contains("pdf"));
        assert!(settings.allowed_extensions.contains("mp4"));
        assert_eq!(settings.allowed_extensions.len(), 3);
    }

    #[test]
    fn settings_dirty_detection_tracks_the_draft() {
        let mut state = SettingsState::default();
        assert!(!state.is_dirty());

        state.draft.notifications = !state.draft.notifications;
        assert!(state.is_dirty());

        let saved = state.draft.clone();
        state.mark_saved(saved);
        assert!(!state.is_dirty());
    }

    #[test]
    fn settings_reject_non_numeric_buffers() {
        let mut state = SettingsState {
            max_simultaneous_text: "many".to_string(),
            ..Default::default()
        };
        let error = state.validated_settings().unwrap_err();
        assert!(error.contains("Max simultaneous"), "unexpected: {error}");
    }

    #[test]
    fn settings_surface_validation_errors() {
        let mut state = SettingsState {
            user_agent_text: "   ".to_string(),
            ..Default::default()
        };
        let error = state.validated_settings().unwrap_err();
        assert!(error.contains("user_agent"), "unexpected: {error}");
    }

    #[test]
    fn bandwidth_zero_means_unlimited() {
        let mut state = SettingsState {
            bandwidth_kbps_text: "0".to_string(),
            ..Default::default()
        };
        assert_eq!(state.validated_settings().unwrap().bandwidth_limit_bps, 0);
    }

    #[test]
    fn dashboard_stats_classify_every_state() {
        let records = vec![
            record(Some(1), DownloadState::Queued, None),
            record(Some(2), DownloadState::Connecting, None),
            record(Some(3), DownloadState::Downloading, None),
            record(Some(4), DownloadState::Completed, None),
            record(Some(5), DownloadState::Failed, None),
            record(Some(6), DownloadState::Paused, None),
            record(Some(7), DownloadState::Cancelled, None),
            record(Some(8), DownloadState::Interrupted, None),
        ];
        let stats = DashboardStats::from_records(&records);
        assert_eq!(stats.queued, 1);
        assert_eq!(stats.active, 2);
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.failed, 1);
    }

    #[test]
    fn dashboard_totals_only_count_completed_bytes() {
        let mut done = record(Some(1), DownloadState::Completed, Some(100));
        done.downloaded_bytes = 100;
        let mut partial = record(Some(2), DownloadState::Failed, Some(100));
        partial.downloaded_bytes = 40;
        let stats = DashboardStats::from_records(&[done, partial]);
        assert_eq!(stats.bytes_downloaded_total, 100);
    }

    #[test]
    fn confirmation_copy_matches_the_python_dialogs() {
        assert_eq!(Confirmation::ClearHistory.title(), "Clear history");
        assert_eq!(
            Confirmation::ClearHistory.message(),
            "Delete ALL completed, failed, cancelled and interrupted records from the history?"
        );
        assert_eq!(
            Confirmation::ResumeInterrupted { count: 2 }.message(),
            "2 download(s) were interrupted when the application last closed. Resume them now?"
        );
        assert_eq!(
            Confirmation::RemoveQueue { ids: vec![1, 2] }.message(),
            "Remove 2 download(s) from the queue/history? Files are kept."
        );
        assert!(Confirmation::ClearHistory.is_destructive());
        assert!(!Confirmation::ResumeInterrupted { count: 1 }.is_destructive());
    }

    #[test]
    fn add_downloads_falls_back_to_the_configured_destination() {
        let mut form = AddDownloadsState::new();
        let mut settings = Settings::default();
        assert!(form.effective_destination(&settings).is_none());

        settings.default_destination_dir = "C:/downloads".to_string();
        assert_eq!(
            form.effective_destination(&settings),
            Some(PathBuf::from("C:/downloads"))
        );

        form.destination = Some(PathBuf::from("D:/picked"));
        assert_eq!(
            form.effective_destination(&settings),
            Some(PathBuf::from("D:/picked"))
        );
    }

    #[test]
    fn add_downloads_starts_immediately_by_default() {
        assert!(AddDownloadsState::new().start_immediately);
    }
}
