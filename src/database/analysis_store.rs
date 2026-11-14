//! Persistence for AI File Finder analysis results.
//!
//! Stores the bounded, non-sensitive fields of the latest analysis runs:
//! display filename, archive type, score, classification, multipart group
//! identifier and part number, plus a JSON evidence summary. URLs are not
//! stored (only the safe display name), so restarting the app shows what was
//! found without keeping link tokens or query strings on disk.

use std::sync::Arc;

use rusqlite::params;

use crate::file_intelligence::archive::{ArchiveFormat, SignatureOutcome};
use crate::file_intelligence::multipart::GroupStatus;
use crate::file_intelligence::report::{
    AnalysisReport, AnalysisSource, CandidateReport, GroupReport, PartReport,
};
use crate::file_intelligence::scoring::{Confidence, Evidence};

use super::connection::Database;
use super::repository::RepositoryError;

/// How many recent analysis runs are kept; older runs are pruned on write.
const KEEP_RUNS: usize = 10;

const INSERT_ANALYSIS_SQL: &str = "
INSERT INTO file_analyses (
    analysis_id, created_at, display_name, archive_type, score,
    classification, group_id, part_number, evidence_json, status
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
";

const LATEST_ANALYSIS_ID_SQL: &str = "
SELECT analysis_id FROM file_analyses
GROUP BY analysis_id
ORDER BY MIN(created_at) DESC, MIN(id) DESC
";

const DELETE_RUN_SQL: &str = "DELETE FROM file_analyses WHERE analysis_id = ?1";

const LOAD_ROWS_SQL: &str = "
SELECT analysis_id, created_at, display_name, archive_type, score, classification,
       group_id, part_number, evidence_json, status
FROM file_analyses
WHERE analysis_id = ?1
ORDER BY id
";

/// Reads and writes the latest analysis runs. Cheap to clone.
#[derive(Clone)]
pub struct AnalysisStore {
    db: Arc<Database>,
}

impl AnalysisStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Test and caller access to the underlying database.
    pub fn database(&self) -> Arc<Database> {
        Arc::clone(&self.db)
    }

    /// Persist one analysis run and prune older ones.
    pub fn save_report(&self, report: &AnalysisReport) -> Result<(), RepositoryError> {
        self.db.transaction(|conn| {
            // Remove an existing copy of this run first (a re-scan with the
            // same timestamp never duplicates rows).
            conn.execute(DELETE_RUN_SQL, params![report.created_at])?;

            for candidate in &report.candidates {
                let evidence = serde_json::to_string(&candidate.evidence)
                    .map_err(|error| RepositoryError::CorruptSettings(error.to_string()))?;
                conn.execute(
                    INSERT_ANALYSIS_SQL,
                    params![
                        report.created_at,
                        report.created_at,
                        candidate.filename,
                        candidate.archive_type,
                        i64::from(candidate.score),
                        format!("{:?}", candidate.confidence),
                        candidate.group_key,
                        candidate.part_number.map(i64::from),
                        evidence,
                        candidate.status,
                    ],
                )?;
            }

            // Prune runs beyond the retention limit.
            let mut statement = conn.prepare(LATEST_ANALYSIS_ID_SQL)?;
            let ids: Vec<String> = statement
                .query_map([], |row| row.get(0))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(statement);
            for id in ids.into_iter().skip(KEEP_RUNS) {
                conn.execute(DELETE_RUN_SQL, params![id])?;
            }
            Ok(())
        })
    }

    /// Load the most recent run, when any is stored.
    pub fn latest_report(&self) -> Result<Option<AnalysisReport>, RepositoryError> {
        let latest: Option<String> = self.db.with_conn(|conn| {
            let mut statement = conn.prepare(&format!("{LATEST_ANALYSIS_ID_SQL} LIMIT 1"))?;
            let mut ids = statement.query_map([], |row| row.get::<_, String>(0))?;
            match ids.next() {
                Some(id) => Ok(Some(id?)),
                None => Ok(None),
            }
        })?;
        let Some(analysis_id) = latest else {
            return Ok(None);
        };
        self.load_report(&analysis_id)
    }

    /// Rebuild a report from its stored rows.
    fn load_report(&self, analysis_id: &str) -> Result<Option<AnalysisReport>, RepositoryError> {
        let rows = self.db.with_conn(|conn| {
            let mut statement = conn.prepare(LOAD_ROWS_SQL)?;
            let rows = statement
                .query_map(params![analysis_id], |row| {
                    Ok(StoredRow {
                        created_at: row.get(1)?,
                        display_name: row.get(2)?,
                        archive_type: row.get(3)?,
                        score: row.get(4)?,
                        classification: row.get(5)?,
                        group_id: row.get(6)?,
                        part_number: row.get(7)?,
                        evidence_json: row.get(8)?,
                        status: row.get(9)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;

        if rows.is_empty() {
            return Ok(None);
        }
        let created_at = rows[0].created_at.clone();
        let analyzed_count = rows.len();

        let mut candidates = Vec::with_capacity(rows.len());
        for row in rows {
            candidates.push(row.into_candidate()?);
        }
        // Score order, matching how the report was produced.
        candidates.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.filename.cmp(&right.filename))
        });

        // Rebuild the groups from the stored group ids and part numbers.
        let groups = rebuild_groups(&candidates);
        let candidate_count = candidates
            .iter()
            .filter(|candidate| {
                candidate.score >= crate::file_intelligence::engine::CANDIDATE_THRESHOLD
            })
            .count();
        let filtered_count = candidates
            .iter()
            .filter(|candidate| candidate.filtered_reason.is_some())
            .count();
        let unverified_count = candidates
            .iter()
            .filter(|candidate| candidate.signature.is_none_or(is_unavailable))
            .filter(|candidate| candidate.format.is_some())
            .count();
        let complete_groups = groups
            .iter()
            .filter(|group| group.status == GroupStatus::Complete)
            .count();
        let incomplete_groups = groups
            .iter()
            .filter(|group| {
                matches!(
                    group.status,
                    GroupStatus::Incomplete | GroupStatus::Duplicate | GroupStatus::Inconsistent
                )
            })
            .count();

        Ok(Some(AnalysisReport {
            created_at,
            source: AnalysisSource::Pasted,
            candidates,
            groups,
            analyzed_count,
            candidate_count,
            filtered_count,
            unverified_count,
            complete_groups,
            incomplete_groups,
            errors: Vec::new(),
            elapsed_seconds: 0.0,
            probed: false,
        }))
    }
}

struct StoredRow {
    created_at: String,
    display_name: String,
    archive_type: String,
    score: i64,
    classification: String,
    group_id: Option<String>,
    part_number: Option<i64>,
    evidence_json: String,
    status: String,
}

impl StoredRow {
    fn into_candidate(self) -> Result<CandidateReport, RepositoryError> {
        let evidence: Vec<Evidence> =
            serde_json::from_str(&self.evidence_json).map_err(|error| {
                RepositoryError::CorruptRow {
                    id: 0,
                    message: format!("bad analysis evidence: {error}"),
                }
            })?;
        Ok(CandidateReport {
            url: String::new(),
            filename: self.display_name.clone(),
            source_page_url: String::new(),
            extension: None,
            archive_type: self.archive_type.clone(),
            format: ArchiveFormat::parse(&self.archive_type_to_key()),
            score: self.score.clamp(0, 100) as u8,
            confidence: confidence_from_stored(&self.classification),
            status: self.status.clone(),
            size: None,
            content_type: None,
            signature: None,
            group_key: self.group_id.clone(),
            part_number: self.part_number.map(|value| value.clamp(0, 9999) as u32),
            evidence,
            missing_evidence: Vec::new(),
            filtered_reason: None,
            anchor_text: None,
        })
    }

    /// The stored `archive_type` is a label like "ZIP archive"; the row's
    /// `classification` column also stores the score band, so the format key
    /// is derived from the label.
    fn archive_type_to_key(&self) -> String {
        match self.archive_type.as_str() {
            "ZIP archive" => "zip",
            "RAR archive" => "rar",
            "7-Zip archive" => "seven_zip",
            "TAR archive" => "tar",
            "GZIP archive" => "gzip",
            "BZIP2 archive" => "bzip2",
            "XZ archive" => "xz",
            "Split archive" => "split",
            _ => "",
        }
        .to_string()
    }
}

/// The `classification` column stores `{:?}` of the confidence band.
fn confidence_from_stored(value: &str) -> Confidence {
    match value {
        "VeryHigh" => Confidence::VeryHigh,
        "High" => Confidence::High,
        "Possible" => Confidence::Possible,
        "Low" => Confidence::Low,
        _ => Confidence::Unlikely,
    }
}

fn is_unavailable(outcome: SignatureOutcome) -> bool {
    outcome == SignatureOutcome::Unavailable
}

/// Rebuild [`GroupReport`] values from candidates that carry a group key.
///
/// The grouping rules in [`crate::file_intelligence::multipart`] are
/// deterministic, so the status is recomputed from the part numbers that were
/// persisted with each row.
fn rebuild_groups(candidates: &[CandidateReport]) -> Vec<GroupReport> {
    use std::collections::BTreeMap;

    let mut buckets: BTreeMap<String, Vec<(u32, usize)>> = BTreeMap::new();
    for (index, candidate) in candidates.iter().enumerate() {
        if let Some(key) = &candidate.group_key
            && let Some(number) = candidate.part_number
        {
            buckets
                .entry(key.clone())
                .or_default()
                .push((number, index));
        }
    }

    let mut reports = Vec::with_capacity(buckets.len());
    for (key, mut members) in buckets {
        members.sort_by_key(|member| member.0);
        let parts: Vec<PartReport> = members
            .iter()
            .map(|(number, index)| PartReport {
                filename: candidates[*index].filename.clone(),
                label: String::new(),
                number: *number,
                candidate_index: *index,
            })
            .collect();

        let numbers: Vec<u32> = members.iter().map(|(number, _)| *number).collect();
        let (status, missing) = judge_stored(numbers);

        let score = members
            .iter()
            .map(|(_, index)| candidates[*index].score)
            .max()
            .unwrap_or(0);

        reports.push(GroupReport {
            display_name: key.clone(),
            key,
            archive_type: candidates
                .get(members.first().map(|(_, index)| *index).unwrap_or(0))
                .map_or_else(
                    || "Archive".to_string(),
                    |candidate| candidate.archive_type.clone(),
                ),
            format: candidates
                .get(members.first().map(|(_, index)| *index).unwrap_or(0))
                .and_then(|candidate| candidate.format),
            parts,
            status,
            missing,
            duplicates: Vec::new(),
            score,
            candidate_indexes: members.iter().map(|(_, index)| *index).collect(),
        });
    }
    reports
}

/// A conservative completeness judgement for reloaded groups: a contiguous
/// run starting at 1 counts as complete-looking, gaps read as incomplete,
/// everything else is "cannot determine".
fn judge_stored(numbers: Vec<u32>) -> (GroupStatus, Vec<String>) {
    if numbers.is_empty() {
        return (GroupStatus::Undetermined, Vec::new());
    }
    let mut missing = Vec::new();
    let first = numbers[0];
    let max = *numbers.last().expect("never empty");
    for expected in first..=max {
        if !numbers.contains(&expected) {
            missing.push(expected.to_string());
        }
    }
    if missing.is_empty() {
        if first == 1 && numbers.len() > 1 {
            (GroupStatus::Complete, missing)
        } else {
            (GroupStatus::Undetermined, missing)
        }
    } else {
        (GroupStatus::Incomplete, missing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_intelligence::multipart::GroupStatus;
    use crate::file_intelligence::scoring::WEIGHT_ARCHIVE_EXTENSION;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_store(label: &str) -> std::path::PathBuf {
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "bdm-analysis-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("downloads.db")
    }

    fn sample_report(created_at: &str) -> AnalysisReport {
        let evidence = vec![Evidence::new(
            WEIGHT_ARCHIVE_EXTENSION,
            "the link ends in .zip",
        )];
        let candidate = CandidateReport {
            url: "https://files.example.com/bundle.zip".to_string(),
            filename: "bundle.zip".to_string(),
            source_page_url: String::new(),
            extension: Some("zip".to_string()),
            archive_type: "ZIP archive".to_string(),
            format: Some(ArchiveFormat::Zip),
            score: 80,
            confidence: Confidence::High,
            status: "Archive candidate".to_string(),
            size: None,
            content_type: None,
            signature: None,
            group_key: None,
            part_number: None,
            evidence,
            missing_evidence: Vec::new(),
            filtered_reason: None,
            anchor_text: None,
        };
        let member = CandidateReport {
            filename: "demo.part01.rar".to_string(),
            archive_type: "RAR archive".to_string(),
            format: Some(ArchiveFormat::Rar),
            score: 70,
            confidence: Confidence::High,
            group_key: Some("demo".to_string()),
            part_number: Some(1),
            ..CandidateReport::default()
        };
        let member2 = CandidateReport {
            filename: "demo.part02.rar".to_string(),
            archive_type: "RAR archive".to_string(),
            format: Some(ArchiveFormat::Rar),
            score: 70,
            confidence: Confidence::High,
            group_key: Some("demo".to_string()),
            part_number: Some(2),
            ..CandidateReport::default()
        };
        let group = GroupReport {
            display_name: "demo".to_string(),
            key: "demo".to_string(),
            archive_type: "RAR archive".to_string(),
            format: Some(ArchiveFormat::Rar),
            parts: vec![
                PartReport {
                    filename: "demo.part01.rar".to_string(),
                    label: "part01".to_string(),
                    number: 1,
                    candidate_index: 1,
                },
                PartReport {
                    filename: "demo.part02.rar".to_string(),
                    label: "part02".to_string(),
                    number: 2,
                    candidate_index: 2,
                },
            ],
            status: GroupStatus::Complete,
            missing: Vec::new(),
            duplicates: Vec::new(),
            score: 70,
            candidate_indexes: vec![1, 2],
        };
        AnalysisReport {
            created_at: created_at.to_string(),
            source: AnalysisSource::Pasted,
            candidates: vec![candidate, member, member2],
            groups: vec![group],
            analyzed_count: 3,
            candidate_count: 1,
            filtered_count: 0,
            unverified_count: 0,
            complete_groups: 1,
            incomplete_groups: 0,
            errors: Vec::new(),
            elapsed_seconds: 1.0,
            probed: false,
        }
    }

    #[test]
    fn empty_database_has_no_report() {
        let path = temp_store("empty");
        let database = Database::init_db(&path).expect("initialise");
        let store = AnalysisStore::new(Arc::new(database));
        assert!(store.latest_report().expect("query").is_none());
    }

    #[test]
    fn a_saved_report_round_trips() {
        let path = temp_store("roundtrip");
        let database = Database::init_db(&path).expect("initialise");
        let store = AnalysisStore::new(Arc::new(database));

        let report = sample_report("2026-01-01T00:00:00Z");
        store.save_report(&report).expect("save");

        let loaded = store
            .latest_report()
            .expect("load")
            .expect("report present");
        assert_eq!(loaded.created_at, "2026-01-01T00:00:00Z");
        assert_eq!(loaded.analyzed_count, 3);
        // Highest score first, like the original report.
        assert_eq!(loaded.candidates[0].filename, "bundle.zip");
        assert_eq!(loaded.candidates[0].score, 80);
        assert_eq!(loaded.candidates[0].confidence, Confidence::High);
        assert_eq!(loaded.candidates[0].format, Some(ArchiveFormat::Zip));
        assert_eq!(loaded.candidates[0].evidence.len(), 1);
        // Groups are rebuilt with deterministic statuses.
        assert_eq!(loaded.groups.len(), 1);
        assert_eq!(loaded.groups[0].status, GroupStatus::Complete);
        assert_eq!(loaded.groups[0].parts.len(), 2);
        assert_eq!(loaded.complete_groups, 1);
    }

    #[test]
    fn only_the_latest_run_survives_pruning() {
        let path = temp_store("prune");
        let database = Database::init_db(&path).expect("initialise");
        let store = AnalysisStore::new(Arc::new(database));

        for index in 0..12 {
            let created_at = format!("2026-01-{index:02}T00:00:00Z");
            let report = AnalysisReport {
                candidates: vec![CandidateReport {
                    filename: format!("file-{index}.zip"),
                    ..CandidateReport::default()
                }],
                ..sample_report(&created_at)
            };
            store.save_report(&report).expect("save");
        }

        let db = store.database();
        let rows: i64 = db
            .with_conn(|conn| {
                Ok(conn.query_row("SELECT COUNT(*) FROM file_analyses", [], |row| row.get(0))?)
            })
            .expect("count rows");
        assert_eq!(rows, 10, "only the newest {KEEP_RUNS} rows are kept");

        let loaded = store
            .latest_report()
            .expect("load")
            .expect("report present");
        assert_eq!(loaded.created_at, "2026-01-11T00:00:00Z");
        assert_eq!(loaded.candidates[0].filename, "file-11.zip");
    }
}
