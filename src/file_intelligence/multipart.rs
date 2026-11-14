//! Multipart archive grouping.
//!
//! Turns a flat list of filenames into groups of related archive parts,
//! following the naming conventions of each family:
//!
//! - `Example.part01.rar` … (RAR multi-part; first part is `part01`)
//! - `Example.rar`, `Example.r00`, `Example.r01` … (RAR volumes)
//! - `Example.7z.001`, `Example.7z.002` … (7-Zip split)
//! - `Example.zip`, `Example.z01`, `Example.z02` … (split ZIP; `.zip` last)
//! - `Example.001`, `Example.002` … (generic split; family unknown)
//! - `Example.part1`, `Example.part2` … (generic split, family unknown)
//!
//! Grouping requires the *exact* base stem (case-insensitive) plus a single
//! part style, so unrelated files that merely look similar are never merged.
//! Completeness is judged from the detected naming sequence only; a group
//! marked complete is "complete-looking", never "verified".

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::archive::{ArchiveFormat, MultipartInfo, NameAnalysis, PartStyle, analyse_filename};

/// Completeness of a multipart group according to its detected naming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupStatus {
    /// The numbering is contiguous and starts where the format convention
    /// says it must. Still naming-based, never verified byte-wise.
    Complete,
    /// A gap or a required marker is missing.
    Incomplete,
    /// The same part number appears more than once.
    Duplicate,
    /// Mixed part styles or mixed archive families within one base stem.
    Inconsistent,
    /// The naming scheme does not allow a reliable judgement (e.g. a single
    /// `part01` with no way to know whether more parts exist).
    Undetermined,
}

impl GroupStatus {
    /// Short UI label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Complete => "Complete (per naming)",
            Self::Incomplete => "Incomplete",
            Self::Duplicate => "Duplicate parts",
            Self::Inconsistent => "Inconsistent naming",
            Self::Undetermined => "Cannot determine",
        }
    }
}

/// One member of a multipart group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMember {
    /// Original filename (percent-decoded).
    pub filename: String,
    /// URL the member was found at.
    pub url: String,
    /// Part number used for ordering; see [`ArchiveGroup::part_numbers`].
    pub number: u32,
    /// Suffix label as written (`part01`, `r00`, `001`, `z01`, `base`).
    pub label: String,
}

/// A detected multipart archive group.
#[derive(Debug, Clone)]
pub struct ArchiveGroup {
    /// Lowercased base stem; stable identifier for persistence.
    pub key: String,
    /// Base stem in the case the files actually use.
    pub display_name: String,
    pub format: ArchiveFormat,
    pub style: PartStyle,
    /// Members sorted by part number, ties by filename.
    pub members: Vec<GroupMember>,
    pub status: GroupStatus,
    /// Missing part labels, listed only where the naming convention makes
    /// the determination reliable.
    pub missing: Vec<String>,
    /// Duplicate part labels.
    pub duplicates: Vec<String>,
}

impl ArchiveGroup {
    /// Part numbers as detected, in member order.
    pub fn part_numbers(&self) -> Vec<u32> {
        self.members.iter().map(|member| member.number).collect()
    }
}

/// Internal grouping bucket.
#[derive(Debug)]
struct Bucket {
    display_name: String,
    format: ArchiveFormat,
    style: PartStyle,
    entries: Vec<(MultipartInfo, String, String)>,
    /// Families seen among the members (for inconsistency detection).
    formats: Vec<ArchiveFormat>,
}

/// Group filenames (already percent-decoded) with their URLs into multipart
/// archives.
///
/// Filenames that are not multipart members are skipped; single-file archives
/// (a lone `Example.zip`) are *not* groups.
pub fn group_multipart<'a, I>(items: I) -> Vec<ArchiveGroup>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut buckets: BTreeMap<(String, PartStyle), Bucket> = BTreeMap::new();

    for (filename, url) in items {
        let Some(analysis) = analyse_filename(filename) else {
            continue;
        };
        let Some(multipart) = analysis.multipart.clone() else {
            continue;
        };
        let key = (analysis.base.clone(), multipart.style);
        let bucket = buckets.entry(key).or_insert_with(|| Bucket {
            display_name: base_display_name(&analysis),
            format: analysis.format,
            style: multipart.style,
            entries: Vec::new(),
            formats: Vec::new(),
        });
        bucket
            .entries
            .push((multipart, filename.to_string(), url.to_string()));
        if !bucket.formats.contains(&analysis.format) {
            bucket.formats.push(analysis.format);
        }
    }

    let mut groups: Vec<ArchiveGroup> = buckets
        .into_iter()
        .map(|((key, style), bucket)| build_group(key, style, bucket))
        .collect();
    groups.sort_by(|left, right| left.display_name.cmp(&right.display_name));
    groups.retain(|group| group.members.len() > 1 || group_requires_single(group));

    // The same base stem grouped under two styles (e.g. `Example.part01.rar`
    // next to `Example.r00`) is a naming conflict: mark both sides
    // inconsistent unless duplicates already carry stronger evidence.
    let mut style_count: BTreeMap<String, usize> = BTreeMap::new();
    for group in &groups {
        *style_count.entry(group.key.clone()).or_default() += 1;
    }
    for group in &mut groups {
        if style_count.get(group.key.as_str()).copied().unwrap_or(1) > 1
            && group.status != GroupStatus::Duplicate
        {
            group.status = GroupStatus::Inconsistent;
            group.missing.clear();
        }
    }
    groups
}

/// A one-member group is only meaningful when the naming itself says the
/// group is incomplete (e.g. a lone `part01` or a lone `.r00` volume without
/// its base). A lone `Example.rar` is a single archive, not a group.
fn group_requires_single(group: &ArchiveGroup) -> bool {
    match group.style {
        PartStyle::RarPart
        | PartStyle::SevenZNumeric
        | PartStyle::SplitNumeric
        | PartStyle::ZipNumeric => group
            .members
            .first()
            .is_some_and(|member| member.number > 1),
        PartStyle::RarVolume => group
            .members
            .first()
            .is_some_and(|member| member.number > 0),
        // A lone `.zip` marker is a plain single-file ZIP, but a lone
        // `.zNN` volume without its marker is a real incomplete group.
        PartStyle::ZipSplit => group
            .members
            .first()
            .is_some_and(|member| member.number != u32::MAX),
    }
}

/// The base stem spelled the way the files spell it.
///
/// Every naming rule above slices the base from the front of the filename, so
/// the display name is simply that prefix before lowercasing.
fn base_display_name(analysis: &NameAnalysis) -> String {
    if analysis
        .filename
        .get(..analysis.base.len())
        .is_some_and(|prefix| prefix.to_ascii_lowercase() == analysis.base)
    {
        analysis.filename[..analysis.base.len()].to_string()
    } else {
        analysis.base.clone()
    }
}

fn build_group(key: String, style: PartStyle, bucket: Bucket) -> ArchiveGroup {
    let mut entries = bucket.entries;
    // Numeric order, ties by filename, so `.part10` never sorts before
    // `.part02` alphabetically.
    entries.sort_by(|left, right| {
        left.0
            .number
            .cmp(&right.0.number)
            .then_with(|| left.1.cmp(&right.1))
    });

    let members: Vec<GroupMember> = entries
        .iter()
        .map(|(info, filename, url)| GroupMember {
            filename: filename.clone(),
            url: url.clone(),
            number: info.number,
            label: info.label.clone(),
        })
        .collect();

    let mut missing = Vec::new();
    let mut duplicates: Vec<String> = Vec::new();
    let status = judge_group(&style, &entries, &mut missing, &mut duplicates);

    ArchiveGroup {
        key,
        display_name: bucket.display_name,
        format: bucket.format,
        style,
        members,
        status,
        missing,
        duplicates,
    }
}

/// The suffix label a missing part number would carry under the group's
/// naming convention (widths follow the detected members).
fn label_for(style: &PartStyle, number: u32, digits: usize) -> String {
    match style {
        PartStyle::RarPart => format!("part{:0>width$}", number, width = digits),
        // Volume `rNN` is tracked as `number = NN + 1`.
        PartStyle::RarVolume => format!("r{:0>width$}", number - 1, width = digits),
        PartStyle::ZipSplit => format!("z{:0>width$}", number, width = digits),
        PartStyle::SevenZNumeric | PartStyle::ZipNumeric | PartStyle::SplitNumeric => {
            format!("{:0>width$}", number, width = digits)
        }
    }
}

/// Judge a bucket whose scan numbers are empty (marker-only or empty).
fn judge_marker_only(style: &PartStyle, count: usize) -> GroupStatus {
    let _ = count;
    match style {
        // A lone `.zip` is a plain single-file ZIP; never a group here.
        PartStyle::ZipSplit => GroupStatus::Undetermined,
        _ => GroupStatus::Undetermined,
    }
}

/// Decide completeness from the detected numbering.
///
/// Format-specific assumptions, spelled out:
/// - `partNN.rar`: the sequence starts at part 1; a run `1..max` with no gaps
///   and more than one member is complete-looking. A single `part01` cannot
///   be judged (`Undetermined`) because the total count is unknowable.
/// - `rar` + `rNN`: the `.rar` base (number 0) plus contiguous `r00..rmax`.
/// - `7z.NNN`: split 7z requires `.001`; contiguous `1..max` is
///   complete-looking.
/// - `zip` + `zNN`: the `.zip` marker is the last volume; complete-looking
///   only when the marker and a contiguous `z01..zmax` are all present.
/// - `NNN` / `partN` generic: contiguous from 1 is complete-looking, with
///   the same single-member caveat.
fn judge_group(
    style: &PartStyle,
    entries: &[(MultipartInfo, String, String)],
    missing: &mut Vec<String>,
    duplicates: &mut Vec<String>,
) -> GroupStatus {
    // Duplicate detection first; a duplicated number makes the sequence
    // unreliable regardless of gaps.
    let mut seen: BTreeMap<u32, usize> = BTreeMap::new();
    for (info, _, _) in entries {
        *seen.entry(info.number).or_default() += 1;
    }
    let duplicated: Vec<u32> = seen
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(number, _)| number)
        .collect();
    if !duplicated.is_empty() {
        for (info, _, _) in entries {
            if duplicated.contains(&info.number) && !duplicates.contains(&info.label) {
                duplicates.push(info.label.clone());
            }
        }
        return GroupStatus::Duplicate;
    }

    // A single style per bucket is enforced by construction, but different
    // archive families sharing one style (e.g. `.001` from two tools) are
    // still consistent; family mixing is only meaningful within the same
    // style, which cannot happen by construction here.

    let numbers: Vec<u32> = entries.iter().map(|(info, _, _)| info.number).collect();

    // The `.zip` final marker carries the sentinel number u32::MAX; it is a
    // presence marker, never part of a numeric scan.
    let scan: Vec<u32> = numbers.iter().copied().filter(|n| *n != u32::MAX).collect();
    let Some(first) = scan.first().copied() else {
        // Only the marker is present: nothing to scan (handled per style).
        return judge_marker_only(style, entries.len());
    };
    let last = scan.last().copied().expect("scan is never empty");
    // Guard against absurd ranges from hostile names.
    if u64::from(last) - u64::from(first) > 4096 {
        return GroupStatus::Undetermined;
    }

    // Contiguity check between the lowest and highest detected numbers.
    // Width follows the widest member (the base `.rar` writes 0 digits).
    let digits = entries
        .iter()
        .map(|(info, _, _)| info.digits)
        .max()
        .unwrap_or(2)
        .max(2);
    let mut contiguous = true;
    for expected in first..=last {
        if !scan.contains(&expected) {
            missing.push(label_for(style, expected, digits));
            contiguous = false;
        }
    }

    match style {
        PartStyle::RarPart => {
            let starts_at_one = numbers.first() == Some(&1);
            match (starts_at_one, contiguous, entries.len()) {
                (true, true, 1) => GroupStatus::Undetermined,
                (true, true, _) => GroupStatus::Complete,
                (false, _, _) => {
                    // Convention says part1 is first; its absence is a gap.
                    missing.push("part1".to_string());
                    GroupStatus::Incomplete
                }
                (true, false, _) => GroupStatus::Incomplete,
            }
        }
        PartStyle::SevenZNumeric | PartStyle::SplitNumeric | PartStyle::ZipNumeric => {
            let starts_at_one = numbers.first() == Some(&1);
            match (starts_at_one, contiguous, entries.len()) {
                (true, true, 1) => GroupStatus::Undetermined,
                (true, true, _) => GroupStatus::Complete,
                (false, _, _) => {
                    // The first volume is required by the format; without it
                    // the set cannot be extracted even if the rest is there.
                    missing.push("001".to_string());
                    GroupStatus::Incomplete
                }
                (true, false, _) => GroupStatus::Incomplete,
            }
        }
        PartStyle::RarVolume => {
            // Number 0 is the `.rar` base volume; it must be present.
            let has_base = numbers.first() == Some(&0);
            if !has_base {
                missing.push("base .rar".to_string());
                return GroupStatus::Incomplete;
            }
            if contiguous && entries.len() > 1 {
                GroupStatus::Complete
            } else if entries.len() > 1 {
                // Volumes with gaps between them.
                GroupStatus::Incomplete
            } else {
                GroupStatus::Undetermined
            }
        }
        PartStyle::ZipSplit => {
            // u32::MAX is the `.zip` final marker.
            let has_marker = numbers.contains(&u32::MAX);
            let volumes: Vec<u32> = numbers.iter().copied().filter(|n| *n != u32::MAX).collect();
            if !has_marker {
                missing.push("base .zip".to_string());
                return GroupStatus::Incomplete;
            }
            let volume_contiguous = volumes.is_empty()
                || (volumes.first() == Some(&1)
                    && volumes
                        .iter()
                        .zip(volumes.iter().skip(1))
                        .all(|(previous, next)| *next == previous + 1));
            if volume_contiguous {
                GroupStatus::Complete
            } else {
                GroupStatus::Incomplete
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_intelligence::archive::ArchiveFormat;

    fn urls(names: &[&str]) -> Vec<(String, String)> {
        names
            .iter()
            .map(|name| {
                (
                    name.to_string(),
                    format!("https://files.example.com/files/{name}"),
                )
            })
            .collect()
    }

    fn run(names: &[&str]) -> Vec<ArchiveGroup> {
        let items = urls(names);
        let refs: Vec<(&str, &str)> = items
            .iter()
            .map(|(name, url)| (name.as_str(), url.as_str()))
            .collect();
        group_multipart(refs)
    }

    #[test]
    fn zero_padded_rar_parts_group_numerically() {
        let groups = run(&["Demo.part001.rar", "Demo.part002.rar", "Demo.part003.rar"]);
        assert_eq!(groups.len(), 1);
        let group = &groups[0];
        assert_eq!(group.status, GroupStatus::Complete);
        let numbers = group.part_numbers();
        assert_eq!(numbers, vec![1, 2, 3]);
        assert!(group.missing.is_empty());
    }

    #[test]
    fn gaps_in_the_sequence_are_incomplete() {
        // Numeric order matters: .part10 after .part02 leaves a real gap,
        // and the convention says part1 is first.
        let groups = run(&["Demo.part02.rar", "Demo.part10.rar"]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].status, GroupStatus::Incomplete);
        assert_eq!(groups[0].missing.len(), 8);
        assert!(groups[0].missing.contains(&"part1".to_string()));
        assert!(groups[0].missing.contains(&"part03".to_string()));
    }

    #[test]
    fn short_part_numbers_group_too() {
        let groups = run(&["Demo.part1.rar", "Demo.part2.rar", "Demo.part3.rar"]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].status, GroupStatus::Complete);
    }

    #[test]
    fn rar_volumes_group_with_their_base() {
        let groups = run(&["Movie.rar", "Movie.r00", "Movie.r01", "Movie.r02"]);
        assert_eq!(groups.len(), 1);
        let group = &groups[0];
        assert_eq!(group.format, ArchiveFormat::Rar);
        assert_eq!(group.status, GroupStatus::Complete);
        assert_eq!(group.part_numbers(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn seven_zip_splits_group_from_001() {
        let groups = run(&["Set.7z.001", "Set.7z.002", "Set.7z.003"]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].status, GroupStatus::Complete);
    }

    #[test]
    fn zip_splits_group_with_the_zip_marker_last() {
        let groups = run(&["Pack.zip", "Pack.z01", "Pack.z02"]);
        assert_eq!(groups.len(), 1);
        let group = &groups[0];
        assert_eq!(group.status, GroupStatus::Complete);
        // The `.zip` marker sorts last, after the volumes.
        assert_eq!(group.members.last().expect("member").label, "base");
    }

    #[test]
    fn missing_volume_is_reported() {
        let groups = run(&["Movie.rar", "Movie.r01", "Movie.r02"]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].status, GroupStatus::Incomplete);
        assert!(groups[0].missing.contains(&"r00".to_string()));
    }

    #[test]
    fn missing_first_rar_part_is_reported() {
        let groups = run(&["Demo.part02.rar", "Demo.part03.rar"]);
        assert_eq!(groups[0].status, GroupStatus::Incomplete);
        assert!(groups[0].missing.contains(&"part1".to_string()));
    }

    #[test]
    fn single_part01_cannot_be_judged() {
        // A lone part01 is unknowable: it is not presented as a group card.
        let groups = run(&["Solo.part01.rar"]);
        assert!(groups.is_empty());
        // A lone later part is a real incomplete group.
        let groups = run(&["Solo.part02.rar"]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].status, GroupStatus::Incomplete);
    }

    #[test]
    fn duplicate_parts_are_detected() {
        // The same filename discovered at two different URLs.
        let groups = run(&["Demo.part01.rar", "Demo.part01.rar", "Demo.part02.rar"]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].status, GroupStatus::Duplicate);
        assert!(!groups[0].duplicates.is_empty());
    }

    #[test]
    fn mixed_styles_for_one_base_are_inconsistent() {
        let groups = run(&["Demo.part01.rar", "Demo.part02.rar", "Demo.r00"]);
        assert_eq!(groups.len(), 2);
        for group in groups {
            assert_eq!(group.status, GroupStatus::Inconsistent);
        }
    }

    #[test]
    fn similar_but_unrelated_names_stay_separate() {
        let groups = run(&["a1.part2.rar", "a11.part2.rar", "Pack.z01", "Pack2.z01"]);
        assert_eq!(groups.len(), 4);
    }

    #[test]
    fn a_lone_zip_is_not_a_group() {
        let groups = run(&["Single.zip", "readme.txt"]);
        assert!(groups.is_empty());
    }

    #[test]
    fn missing_base_volume_is_incomplete() {
        let groups = run(&["Movie.r00", "Movie.r01"]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].status, GroupStatus::Incomplete);
        assert!(groups[0].missing.contains(&"base .rar".to_string()));
    }

    #[test]
    fn zip_split_without_the_zip_marker_is_incomplete() {
        let groups = run(&["Pack.z01", "Pack.z02"]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].status, GroupStatus::Incomplete);
        assert!(groups[0].missing.contains(&"base .zip".to_string()));
    }

    #[test]
    fn out_of_order_input_sorts_by_number() {
        let groups = run(&["Demo.7z.003", "Demo.7z.001", "Demo.7z.002"]);
        assert_eq!(groups[0].part_numbers(), vec![1, 2, 3]);
    }

    #[test]
    fn group_members_keep_the_original_url() {
        let groups = run(&["Demo.7z.001", "Demo.7z.002"]);
        assert_eq!(
            groups[0].members[0].url,
            "https://files.example.com/files/Demo.7z.001"
        );
    }

    #[test]
    fn generic_numeric_splits_start_at_001() {
        let groups = run(&["Data.001", "Data.002"]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].status, GroupStatus::Complete);
        let groups = run(&["Data.002", "Data.003"]);
        assert_eq!(groups[0].status, GroupStatus::Incomplete);
    }
}
