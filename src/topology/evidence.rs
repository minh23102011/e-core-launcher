use serde::{Deserialize, Serialize};

/// The sysfs or detector component which produced an observation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    /// Global or fallback active CPU resolution.
    OnlineCpuList,
    /// One logical CPU's hotplug state.
    PerCpuOnline,
    /// Package and physical-core identifiers.
    TopologyIds,
    /// `core_cpus_list` or `thread_siblings_list`.
    ThreadSiblings,
    /// Explicit `topology/core_type`.
    CoreType,
    /// `cpuinfo_max_freq` or `scaling_max_freq`.
    MaximumFrequency,
    /// Per-CPU cache index metadata.
    CacheTopology,
    /// A conclusion produced by the classifier.
    Classifier,
}

/// How an evidence item contributes to the result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// A neutral input observation.
    Observation,
    /// Evidence which directly explains a class assignment.
    Classification,
    /// Incomplete or unsupported optional data.
    Warning,
    /// Two inputs which cannot both describe the same topology.
    Contradiction,
}

/// Qualitative reliability of a piece of evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStrength {
    /// Useful context which cannot classify a core alone.
    Weak,
    /// Repeatable but indirect evidence.
    Moderate,
    /// Reliable topology evidence.
    Strong,
    /// Directly reported kernel or platform metadata.
    Explicit,
}

/// A structured, human-readable observation used by topology detection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DetectionEvidence {
    /// Component which produced this item.
    pub source: EvidenceSource,
    /// Sorted logical CPUs to which the item applies.
    pub affected_cpus: Vec<u32>,
    /// Stable textual representation of the raw or summarized observation.
    pub observed_value: String,
    /// Human-readable meaning assigned to the observation.
    pub interpretation: String,
    /// Qualitative reliability.
    pub strength: EvidenceStrength,
    /// How the evidence contributes to the result.
    pub kind: EvidenceKind,
}

impl DetectionEvidence {
    pub(crate) fn new(
        source: EvidenceSource,
        mut affected_cpus: Vec<u32>,
        observed_value: impl Into<String>,
        interpretation: impl Into<String>,
        strength: EvidenceStrength,
        kind: EvidenceKind,
    ) -> Self {
        affected_cpus.sort_unstable();
        affected_cpus.dedup();
        Self {
            source,
            affected_cpus,
            observed_value: observed_value.into(),
            interpretation: interpretation.into(),
            strength,
            kind,
        }
    }
}
