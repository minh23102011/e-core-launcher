use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::evidence::{DetectionEvidence, EvidenceKind, EvidenceSource, EvidenceStrength};
use super::parser::{format_cpu_list, CoreTypeInterpretation};
use super::sysfs::{CpuRecord, SysfsCpuSource, SysfsSnapshot};
use super::types::{CoreClass, CpuTopology, DetectorError, PhysicalCore, TopologyClass};

/// Tunable, conservative thresholds used by heuristic classification.
#[derive(Clone, Debug)]
pub struct DetectorPolicy {
    /// Minimum high/low maximum-frequency ratio.
    pub minimum_frequency_ratio: f64,
    /// Maximum relative spread permitted inside either frequency cluster.
    pub maximum_cluster_spread: f64,
    /// Minimum physical cores required in each heuristic cluster.
    pub minimum_cores_per_cluster: usize,
    /// Confidence assigned to explicit hybrid metadata.
    pub explicit_hybrid_confidence: f32,
    /// Confidence assigned to a fully corroborated heuristic result.
    pub heuristic_hybrid_confidence: f32,
}

impl Default for DetectorPolicy {
    fn default() -> Self {
        Self {
            minimum_frequency_ratio: 1.25,
            maximum_cluster_spread: 0.08,
            minimum_cores_per_cluster: 2,
            explicit_hybrid_confidence: 0.98,
            heuristic_hybrid_confidence: 0.72,
        }
    }
}

impl DetectorPolicy {
    fn conservative(mut self) -> Self {
        let defaults = Self::default();
        self.minimum_frequency_ratio = if self.minimum_frequency_ratio.is_finite() {
            self.minimum_frequency_ratio
                .max(defaults.minimum_frequency_ratio)
        } else {
            defaults.minimum_frequency_ratio
        };
        self.maximum_cluster_spread = if self.maximum_cluster_spread.is_finite() {
            self.maximum_cluster_spread
                .clamp(0.0, defaults.maximum_cluster_spread)
        } else {
            defaults.maximum_cluster_spread
        };
        self.minimum_cores_per_cluster = self
            .minimum_cores_per_cluster
            .max(defaults.minimum_cores_per_cluster);
        self.explicit_hybrid_confidence = clamp_confidence(self.explicit_hybrid_confidence);
        self.heuristic_hybrid_confidence = clamp_confidence(self.heuristic_hybrid_confidence)
            .min((self.explicit_hybrid_confidence - f32::EPSILON).max(0.0));
        self
    }
}

/// Read-only Linux CPU topology detector with a configurable sysfs root.
#[derive(Clone, Debug)]
pub struct CpuTopologyDetector {
    sysfs_root: PathBuf,
    policy: DetectorPolicy,
}

impl Default for CpuTopologyDetector {
    fn default() -> Self {
        Self::new("/sys/devices/system/cpu")
    }
}

impl CpuTopologyDetector {
    /// Create a detector which reads from `sysfs_root`.
    #[must_use]
    pub fn new(sysfs_root: impl Into<PathBuf>) -> Self {
        Self {
            sysfs_root: sysfs_root.into(),
            policy: DetectorPolicy::default(),
        }
    }

    /// Create a detector using the production Linux CPU sysfs root.
    #[must_use]
    pub fn system() -> Self {
        Self::default()
    }

    /// Replace the classification thresholds.
    ///
    /// Values are normalized so callers can make detection stricter, but
    /// cannot weaken the built-in separation and repetition safeguards.
    #[must_use]
    pub fn with_policy(mut self, policy: DetectorPolicy) -> Self {
        self.policy = policy.conservative();
        self
    }

    /// Return the configured CPU sysfs root.
    #[must_use]
    pub fn sysfs_root(&self) -> &Path {
        &self.sysfs_root
    }

    /// Inspect online CPUs and return a deterministic topology snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`DetectorError`] only when the sysfs root or required online
    /// CPU data cannot be read. Optional metadata problems become evidence.
    pub fn detect(&self) -> Result<CpuTopology, DetectorError> {
        let snapshot = SysfsCpuSource::new(&self.sysfs_root).collect()?;
        let mut grouped = group_physical_cores(&snapshot);
        let classification = classify(&snapshot, &mut grouped, &self.policy);
        let cache_evidence = summarize_cache_evidence(&grouped);

        let mut evidence = snapshot.evidence;
        for record in &snapshot.records {
            evidence.extend(record.evidence.clone());
        }
        evidence.extend(cache_evidence);
        evidence.extend(classification.evidence);

        let mut physical_cores: Vec<PhysicalCore> =
            grouped.into_iter().map(|group| group.output).collect();
        physical_cores.sort_by(core_sort_key);

        let mut performance_cpus = Vec::new();
        let mut efficiency_cpus = Vec::new();
        if classification.classification == TopologyClass::Hybrid {
            for core in &physical_cores {
                match core.core_class {
                    CoreClass::Performance => {
                        performance_cpus.extend(core.logical_cpus.iter().copied());
                    }
                    CoreClass::Efficiency => {
                        efficiency_cpus.extend(core.logical_cpus.iter().copied());
                    }
                    CoreClass::Uniform | CoreClass::Unknown => {}
                }
            }
        }
        performance_cpus.sort_unstable();
        performance_cpus.dedup();
        efficiency_cpus.sort_unstable();
        efficiency_cpus.dedup();

        Ok(CpuTopology {
            online_cpus: snapshot.online_cpus,
            physical_cores,
            performance_cpus,
            efficiency_cpus,
            classification: classification.classification,
            confidence: clamp_confidence(classification.confidence),
            evidence,
        })
    }
}

#[derive(Debug)]
struct GroupedCore {
    output: PhysicalCore,
    exact_grouping: bool,
    core_type_metadata_present: bool,
    explicit_type: Option<CoreTypeInterpretation>,
    complete_explicit_type: bool,
    max_frequency_khz: Option<u64>,
    complete_frequency: bool,
    cache_fingerprint: Option<String>,
}

fn group_physical_cores(snapshot: &SysfsSnapshot) -> Vec<GroupedCore> {
    let mut disjoint = DisjointSet::new(&snapshot.online_cpus);
    let records: BTreeMap<u32, &CpuRecord> = snapshot
        .records
        .iter()
        .map(|record| (record.id, record))
        .collect();

    for record in &snapshot.records {
        if let Some(siblings) = &record.siblings {
            for sibling in siblings {
                disjoint.union(record.id, *sibling);
            }
        }
    }

    let sibling_known: BTreeSet<u32> = snapshot
        .records
        .iter()
        .filter(|record| record.siblings.is_some())
        .map(|record| record.id)
        .collect();
    let mut by_ids: BTreeMap<(u32, u32), Vec<u32>> = BTreeMap::new();
    for record in &snapshot.records {
        if !sibling_known.contains(&record.id) {
            if let (Some(package), Some(core)) = (record.package_id, record.core_id) {
                by_ids.entry((package, core)).or_default().push(record.id);
            }
        }
    }
    for cpus in by_ids.values() {
        if let Some(first) = cpus.first() {
            for cpu in &cpus[1..] {
                disjoint.union(*first, *cpu);
            }
        }
    }

    let mut groups: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for cpu in &snapshot.online_cpus {
        let root = disjoint.find(*cpu);
        groups.entry(root).or_default().push(*cpu);
    }

    groups
        .into_values()
        .map(|mut cpus| {
            cpus.sort_unstable();
            build_group(&cpus, &records)
        })
        .collect()
}

fn build_group(cpus: &[u32], records: &BTreeMap<u32, &CpuRecord>) -> GroupedCore {
    let group_records: Vec<&CpuRecord> = cpus
        .iter()
        .filter_map(|cpu| records.get(cpu).copied())
        .collect();
    let package_id = common_value(group_records.iter().map(|record| record.package_id));
    let core_id = common_value(group_records.iter().map(|record| record.core_id));

    let sibling_sets_consistent = group_records.iter().all(|record| match &record.siblings {
        Some(siblings) => siblings == cpus,
        None => true,
    });
    let has_strong_group_key = group_records.iter().any(|record| record.siblings.is_some())
        || (package_id.is_some() && core_id.is_some());
    let exact_grouping = sibling_sets_consistent && has_strong_group_key;

    let mut evidence = Vec::new();
    let grouping_kind = if exact_grouping {
        EvidenceKind::Observation
    } else {
        EvidenceKind::Warning
    };
    evidence.push(DetectionEvidence::new(
        EvidenceSource::ThreadSiblings,
        cpus.to_vec(),
        format_cpu_list(cpus),
        if exact_grouping {
            "Logical CPUs were grouped using consistent sibling masks or package/core identifiers."
        } else {
            "Physical-core grouping is incomplete or inconsistent; this group remains usable but is not classification-grade."
        },
        if exact_grouping {
            EvidenceStrength::Strong
        } else {
            EvidenceStrength::Moderate
        },
        grouping_kind,
    ));
    add_id_uncertainty_evidence(
        cpus,
        "physical_package_id",
        group_records.iter().map(|record| record.package_id),
        &mut evidence,
    );
    add_id_uncertainty_evidence(
        cpus,
        "core_id",
        group_records.iter().map(|record| record.core_id),
        &mut evidence,
    );

    let explicit_values: Vec<CoreTypeInterpretation> = group_records
        .iter()
        .filter_map(|record| record.explicit_type)
        .collect();
    let explicit_type = all_same(&explicit_values);
    let complete_explicit_type =
        explicit_values.len() == group_records.len() && explicit_type.is_some();
    if !explicit_values.is_empty() && explicit_type.is_none() {
        evidence.push(DetectionEvidence::new(
            EvidenceSource::CoreType,
            cpus.to_vec(),
            format!("{explicit_values:?}"),
            "Logical CPUs grouped into one physical core report contradictory core_type values.",
            EvidenceStrength::Explicit,
            EvidenceKind::Contradiction,
        ));
    }

    let frequencies: Vec<u64> = group_records
        .iter()
        .filter_map(|record| record.max_frequency_khz)
        .collect();
    let frequency_consistent = match frequencies.first() {
        Some(first) => frequencies.iter().all(|frequency| frequency == first),
        None => true,
    };
    let complete_frequency = frequencies.len() == group_records.len() && frequency_consistent;
    let max_frequency_khz = if complete_frequency {
        frequencies.first().copied()
    } else {
        None
    };
    if !frequencies.is_empty() && !frequency_consistent {
        evidence.push(DetectionEvidence::new(
            EvidenceSource::MaximumFrequency,
            cpus.to_vec(),
            format!("{frequencies:?}"),
            "Logical CPUs in one physical core report contradictory maximum frequencies.",
            EvidenceStrength::Moderate,
            EvidenceKind::Contradiction,
        ));
    }

    let cache_values: Vec<String> = group_records
        .iter()
        .filter_map(|record| record.cache_fingerprint.clone())
        .collect();
    let cache_fingerprint = if cache_values.len() == group_records.len() {
        all_same(&cache_values)
    } else {
        None
    };
    if !cache_values.is_empty() && cache_fingerprint.is_none() {
        evidence.push(DetectionEvidence::new(
            EvidenceSource::CacheTopology,
            cpus.to_vec(),
            format!("{cache_values:?}"),
            "Logical CPUs in one physical core report inconsistent cache topology.",
            EvidenceStrength::Weak,
            EvidenceKind::Contradiction,
        ));
    }

    GroupedCore {
        output: PhysicalCore {
            package_id,
            core_id,
            logical_cpus: cpus.to_vec(),
            core_class: CoreClass::Unknown,
            confidence: 0.0,
            evidence,
        },
        exact_grouping,
        core_type_metadata_present: group_records
            .iter()
            .any(|record| record.core_type_metadata_present),
        explicit_type,
        complete_explicit_type,
        max_frequency_khz,
        complete_frequency,
        cache_fingerprint,
    }
}

fn common_value<I>(values: I) -> Option<u32>
where
    I: IntoIterator<Item = Option<u32>>,
{
    let values: Vec<Option<u32>> = values.into_iter().collect();
    let first = values.first().copied().flatten()?;
    values
        .iter()
        .all(|value| *value == Some(first))
        .then_some(first)
}

fn add_id_uncertainty_evidence<I>(
    cpus: &[u32],
    name: &str,
    values: I,
    evidence: &mut Vec<DetectionEvidence>,
) where
    I: IntoIterator<Item = Option<u32>>,
{
    let values: Vec<Option<u32>> = values.into_iter().collect();
    let distinct: BTreeSet<u32> = values.iter().flatten().copied().collect();
    let has_missing = values.iter().any(Option::is_none);
    if distinct.len() > 1 || (has_missing && !distinct.is_empty()) {
        evidence.push(DetectionEvidence::new(
            EvidenceSource::TopologyIds,
            cpus.to_vec(),
            format!("{name} values {values:?}"),
            format!(
                "The {name} values in this sibling group are incomplete or contradictory; the identifier was left unknown."
            ),
            EvidenceStrength::Moderate,
            EvidenceKind::Warning,
        ));
    }
}

fn all_same<T>(values: &[T]) -> Option<T>
where
    T: Clone + PartialEq,
{
    let first = values.first()?;
    values
        .iter()
        .all(|value| value == first)
        .then(|| first.clone())
}

#[derive(Debug)]
struct Classification {
    classification: TopologyClass,
    confidence: f32,
    evidence: Vec<DetectionEvidence>,
}

fn classify(
    snapshot: &SysfsSnapshot,
    cores: &mut [GroupedCore],
    policy: &DetectorPolicy,
) -> Classification {
    let mut evidence = Vec::new();
    if let Some(explicit) = classify_explicit(cores, policy, &mut evidence) {
        return explicit;
    }
    if let Some(heuristic) = classify_heuristic(cores, policy, &mut evidence) {
        return heuristic;
    }

    let exact = cores.iter().all(|core| core.exact_grouping);
    let unusable_explicit_metadata = cores.iter().any(|core| core.core_type_metadata_present)
        && !cores.iter().all(|core| core.complete_explicit_type);
    let thread_counts: BTreeSet<usize> = cores
        .iter()
        .map(|core| core.output.logical_cpus.len())
        .collect();
    if exact && thread_counts.len() == 1 && !unusable_explicit_metadata {
        set_all_cores(cores, CoreClass::Uniform, 0.82);
        evidence.push(DetectionEvidence::new(
            EvidenceSource::Classifier,
            snapshot.online_cpus.clone(),
            format!("{} physical cores; SMT widths {thread_counts:?}", cores.len()),
            "All visible physical cores have an equivalent, consistently grouped topology and no corroborated hybrid distinction was found.",
            EvidenceStrength::Strong,
            EvidenceKind::Classification,
        ));
        Classification {
            classification: TopologyClass::Uniform,
            confidence: 0.82,
            evidence,
        }
    } else {
        set_all_cores(cores, CoreClass::Unknown, 0.25);
        evidence.push(DetectionEvidence::new(
            EvidenceSource::Classifier,
            snapshot.online_cpus.clone(),
            format!(
                "exact_grouping={exact}, SMT widths={thread_counts:?}, physical_cores={}",
                cores.len()
            ),
            "Available metadata is incomplete, inconsistent, or shows uncorroborated asymmetry; no efficiency CPU set was produced.",
            EvidenceStrength::Moderate,
            EvidenceKind::Warning,
        ));
        Classification {
            classification: TopologyClass::Unknown,
            confidence: 0.25,
            evidence,
        }
    }
}

fn classify_explicit(
    cores: &mut [GroupedCore],
    policy: &DetectorPolicy,
    evidence: &mut Vec<DetectionEvidence>,
) -> Option<Classification> {
    let any_explicit = cores.iter().any(|core| core.core_type_metadata_present);
    if !any_explicit {
        return None;
    }
    let complete = cores
        .iter()
        .all(|core| core.complete_explicit_type && core.explicit_type.is_some());
    if !complete {
        evidence.push(DetectionEvidence::new(
            EvidenceSource::CoreType,
            all_core_cpus(cores),
            "partial",
            "core_type metadata was present but incomplete or contradictory, so it was not used to create an E-core mask.",
            EvidenceStrength::Explicit,
            EvidenceKind::Warning,
        ));
        return None;
    }

    let types: BTreeSet<u8> = cores
        .iter()
        .filter_map(|core| core.explicit_type)
        .map(|core_type| match core_type {
            CoreTypeInterpretation::Performance => 1,
            CoreTypeInterpretation::Efficiency => 2,
            CoreTypeInterpretation::Unsupported(_) => 3,
        })
        .collect();
    if types == BTreeSet::from([1, 2]) {
        for core in cores.iter_mut() {
            core.output.core_class = match core.explicit_type {
                Some(CoreTypeInterpretation::Performance) => CoreClass::Performance,
                Some(CoreTypeInterpretation::Efficiency) => CoreClass::Efficiency,
                Some(CoreTypeInterpretation::Unsupported(_)) | None => CoreClass::Unknown,
            };
            core.output.confidence = clamp_confidence(policy.explicit_hybrid_confidence);
            core.output.evidence.push(DetectionEvidence::new(
                EvidenceSource::CoreType,
                core.output.logical_cpus.clone(),
                match core.explicit_type {
                    Some(CoreTypeInterpretation::Performance) => "64",
                    Some(CoreTypeInterpretation::Efficiency) => "32",
                    Some(CoreTypeInterpretation::Unsupported(_)) | None => "unavailable",
                },
                format!(
                    "Complete explicit core_type metadata classified this physical core as {}.",
                    core.output.core_class
                ),
                EvidenceStrength::Explicit,
                EvidenceKind::Classification,
            ));
        }
        let efficiency: Vec<u32> = cores
            .iter()
            .filter(|core| core.output.core_class == CoreClass::Efficiency)
            .flat_map(|core| core.output.logical_cpus.iter().copied())
            .collect();
        evidence.push(DetectionEvidence::new(
            EvidenceSource::CoreType,
            efficiency.clone(),
            format_cpu_list(&efficiency),
            format!(
                "Explicit core_type metadata identified CPUs {} as efficiency cores.",
                format_cpu_list(&efficiency)
            ),
            EvidenceStrength::Explicit,
            EvidenceKind::Classification,
        ));
        return Some(Classification {
            classification: TopologyClass::Hybrid,
            confidence: policy.explicit_hybrid_confidence,
            evidence: std::mem::take(evidence),
        });
    }

    set_all_cores(cores, CoreClass::Uniform, 0.95);
    add_core_classification_evidence(
        cores,
        EvidenceSource::CoreType,
        "single supported core type",
        "Complete explicit core_type metadata reports the same type for this physical core as for every other active core.",
        EvidenceStrength::Explicit,
    );
    evidence.push(DetectionEvidence::new(
        EvidenceSource::CoreType,
        all_core_cpus(cores),
        "single supported core type",
        "Complete core_type metadata reports one core type across all active physical cores.",
        EvidenceStrength::Explicit,
        EvidenceKind::Classification,
    ));
    Some(Classification {
        classification: TopologyClass::Uniform,
        confidence: 0.95,
        evidence: std::mem::take(evidence),
    })
}

fn classify_heuristic(
    cores: &mut [GroupedCore],
    policy: &DetectorPolicy,
    evidence: &mut Vec<DetectionEvidence>,
) -> Option<Classification> {
    if cores.len() < policy.minimum_cores_per_cluster.saturating_mul(2)
        || cores
            .iter()
            .any(|core| !core.exact_grouping || !core.complete_frequency)
    {
        if cores.iter().any(|core| core.max_frequency_khz.is_some())
            && cores.iter().any(|core| !core.complete_frequency)
        {
            evidence.push(DetectionEvidence::new(
                EvidenceSource::MaximumFrequency,
                all_core_cpus(cores),
                "incomplete",
                "Maximum-frequency metadata was incomplete or inconsistent and was not used for classification.",
                EvidenceStrength::Weak,
                EvidenceKind::Warning,
            ));
        }
        return None;
    }

    let mut ordered: Vec<(usize, u64)> = cores
        .iter()
        .enumerate()
        .filter_map(|(index, core)| core.max_frequency_khz.map(|frequency| (index, frequency)))
        .collect();
    ordered.sort_by_key(|(index, frequency)| (*frequency, *index));

    let (split, ratio) = strongest_frequency_split(&ordered)?;
    let low = &ordered[..split];
    let high = &ordered[split..];
    if low.len() < policy.minimum_cores_per_cluster
        || high.len() < policy.minimum_cores_per_cluster
        || ratio < policy.minimum_frequency_ratio
        || relative_spread(low) > policy.maximum_cluster_spread
        || relative_spread(high) > policy.maximum_cluster_spread
    {
        evidence.push(DetectionEvidence::new(
            EvidenceSource::MaximumFrequency,
            all_core_cpus(cores),
            format!("strongest cluster ratio {ratio:.3}"),
            "Maximum-frequency groups were not sufficiently separated and internally consistent for hybrid classification.",
            EvidenceStrength::Moderate,
            EvidenceKind::Observation,
        ));
        return None;
    }

    let low_widths: BTreeSet<usize> = low
        .iter()
        .map(|(index, _frequency)| cores[*index].output.logical_cpus.len())
        .collect();
    let high_widths: BTreeSet<usize> = high
        .iter()
        .map(|(index, _frequency)| cores[*index].output.logical_cpus.len())
        .collect();
    let (Some(low_width), Some(high_width)) = (
        single_set_value(&low_widths),
        single_set_value(&high_widths),
    ) else {
        evidence.push(DetectionEvidence::new(
            EvidenceSource::MaximumFrequency,
            all_core_cpus(cores),
            format!("low SMT widths {low_widths:?}; high SMT widths {high_widths:?}"),
            "Frequency clusters exist, but their SMT layouts are internally inconsistent.",
            EvidenceStrength::Moderate,
            EvidenceKind::Contradiction,
        ));
        return None;
    };
    if high_width <= low_width {
        evidence.push(DetectionEvidence::new(
            EvidenceSource::MaximumFrequency,
            all_core_cpus(cores),
            format!("low SMT width {low_width}; high SMT width {high_width}"),
            "Frequency and SMT distinctions do not form the repeated performance/efficiency pattern required by policy.",
            EvidenceStrength::Moderate,
            EvidenceKind::Observation,
        ));
        return None;
    }

    for (index, _frequency) in low {
        cores[*index].output.core_class = CoreClass::Efficiency;
        cores[*index].output.confidence = clamp_confidence(policy.heuristic_hybrid_confidence);
        add_heuristic_core_evidence(&mut cores[*index], "lower-frequency");
    }
    for (index, _frequency) in high {
        cores[*index].output.core_class = CoreClass::Performance;
        cores[*index].output.confidence = clamp_confidence(policy.heuristic_hybrid_confidence);
        add_heuristic_core_evidence(&mut cores[*index], "higher-frequency");
    }
    let efficiency: Vec<u32> = low
        .iter()
        .flat_map(|(index, _frequency)| cores[*index].output.logical_cpus.iter().copied())
        .collect();
    let cache_groups: BTreeSet<&str> = cores
        .iter()
        .filter_map(|core| core.cache_fingerprint.as_deref())
        .collect();
    let cache_summary = if cache_groups.is_empty() {
        "unavailable".to_owned()
    } else {
        cache_groups.len().to_string()
    };
    evidence.push(DetectionEvidence::new(
        EvidenceSource::MaximumFrequency,
        efficiency,
        format!(
            "frequency ratio {ratio:.3}; SMT widths low={low_width}, high={high_width}; cache patterns={cache_summary}"
        ),
        "Two repeated, well-separated maximum-frequency groups were corroborated by a consistent SMT-width distinction. This remains heuristic.",
        EvidenceStrength::Moderate,
        EvidenceKind::Classification,
    ));
    Some(Classification {
        classification: TopologyClass::Hybrid,
        confidence: policy.heuristic_hybrid_confidence,
        evidence: std::mem::take(evidence),
    })
}

fn strongest_frequency_split(ordered: &[(usize, u64)]) -> Option<(usize, f64)> {
    ordered
        .windows(2)
        .enumerate()
        .filter_map(|(index, pair)| {
            let low = pair[0].1;
            let high = pair[1].1;
            (low > 0).then_some((index + 1, high as f64 / low as f64))
        })
        .max_by(|left, right| left.1.total_cmp(&right.1))
}

fn relative_spread(cluster: &[(usize, u64)]) -> f64 {
    let Some(minimum) = cluster.iter().map(|item| item.1).min() else {
        return f64::INFINITY;
    };
    let Some(maximum) = cluster.iter().map(|item| item.1).max() else {
        return f64::INFINITY;
    };
    if maximum == 0 {
        f64::INFINITY
    } else {
        (maximum - minimum) as f64 / maximum as f64
    }
}

fn single_set_value(values: &BTreeSet<usize>) -> Option<usize> {
    (values.len() == 1)
        .then(|| values.first().copied())
        .flatten()
}

fn all_core_cpus(cores: &[GroupedCore]) -> Vec<u32> {
    let mut cpus: Vec<u32> = cores
        .iter()
        .flat_map(|core| core.output.logical_cpus.iter().copied())
        .collect();
    cpus.sort_unstable();
    cpus.dedup();
    cpus
}

fn summarize_cache_evidence(cores: &[GroupedCore]) -> Option<DetectionEvidence> {
    let patterns: BTreeSet<&str> = cores
        .iter()
        .filter_map(|core| core.cache_fingerprint.as_deref())
        .collect();
    if patterns.is_empty() {
        return None;
    }

    Some(DetectionEvidence::new(
        EvidenceSource::CacheTopology,
        all_core_cpus(cores),
        format!("{} complete cache-layout pattern(s)", patterns.len()),
        if patterns.len() > 1 {
            "Consistent cache-layout differences were observed between physical cores; cache layout is supporting evidence, not standalone proof of core type."
        } else {
            "Available cache metadata showed one complete layout pattern across physical cores."
        },
        EvidenceStrength::Weak,
        EvidenceKind::Observation,
    ))
}

fn add_heuristic_core_evidence(core: &mut GroupedCore, cluster: &str) {
    core.output.evidence.push(DetectionEvidence::new(
        EvidenceSource::MaximumFrequency,
        core.output.logical_cpus.clone(),
        core.max_frequency_khz
            .map_or_else(|| "unavailable".to_owned(), |value| value.to_string()),
        format!(
            "This physical core belongs to the repeated {cluster} cluster; the classification is heuristic and SMT-corroborated."
        ),
        EvidenceStrength::Moderate,
        EvidenceKind::Classification,
    ));
}

fn add_core_classification_evidence(
    cores: &mut [GroupedCore],
    source: EvidenceSource,
    observed_value: &str,
    interpretation: &str,
    strength: EvidenceStrength,
) {
    for core in cores {
        core.output.evidence.push(DetectionEvidence::new(
            source.clone(),
            core.output.logical_cpus.clone(),
            observed_value,
            interpretation,
            strength,
            EvidenceKind::Classification,
        ));
    }
}

fn set_all_cores(cores: &mut [GroupedCore], class: CoreClass, confidence: f32) {
    let confidence = clamp_confidence(confidence);
    for core in cores {
        core.output.core_class = class;
        core.output.confidence = confidence;
    }
}

fn clamp_confidence(confidence: f32) -> f32 {
    if confidence.is_finite() {
        confidence.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn core_sort_key(left: &PhysicalCore, right: &PhysicalCore) -> std::cmp::Ordering {
    left.package_id
        .unwrap_or(u32::MAX)
        .cmp(&right.package_id.unwrap_or(u32::MAX))
        .then_with(|| {
            left.core_id
                .unwrap_or(u32::MAX)
                .cmp(&right.core_id.unwrap_or(u32::MAX))
        })
        .then_with(|| left.logical_cpus.cmp(&right.logical_cpus))
}

#[derive(Debug)]
struct DisjointSet {
    parents: BTreeMap<u32, u32>,
}

impl DisjointSet {
    fn new(cpus: &[u32]) -> Self {
        Self {
            parents: cpus.iter().copied().map(|cpu| (cpu, cpu)).collect(),
        }
    }

    fn find(&mut self, cpu: u32) -> u32 {
        let parent = self.parents.get(&cpu).copied().unwrap_or(cpu);
        if parent == cpu {
            cpu
        } else {
            let root = self.find(parent);
            self.parents.insert(cpu, root);
            root
        }
    }

    fn union(&mut self, first: u32, second: u32) {
        if !self.parents.contains_key(&first) || !self.parents.contains_key(&second) {
            return;
        }
        let first_root = self.find(first);
        let second_root = self.find(second);
        if first_root != second_root {
            let (root, child) = if first_root < second_root {
                (first_root, second_root)
            } else {
                (second_root, first_root)
            };
            self.parents.insert(child, root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{clamp_confidence, DetectorPolicy};

    #[test]
    fn confidence_is_always_bounded() {
        assert_eq!(clamp_confidence(-1.0), 0.0);
        assert_eq!(clamp_confidence(0.5), 0.5);
        assert_eq!(clamp_confidence(2.0), 1.0);
        assert_eq!(clamp_confidence(f32::NAN), 0.0);
    }

    #[test]
    fn custom_policy_cannot_weaken_conservative_safeguards() {
        let policy = DetectorPolicy {
            minimum_frequency_ratio: f64::NAN,
            maximum_cluster_spread: 0.5,
            minimum_cores_per_cluster: 1,
            explicit_hybrid_confidence: 0.4,
            heuristic_hybrid_confidence: 0.9,
        }
        .conservative();

        assert!(policy.minimum_frequency_ratio >= 1.25);
        assert!(policy.maximum_cluster_spread <= 0.08);
        assert!(policy.minimum_cores_per_cluster >= 2);
        assert!(policy.heuristic_hybrid_confidence < policy.explicit_hybrid_confidence);
    }
}
