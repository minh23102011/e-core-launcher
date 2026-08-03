use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::evidence::DetectionEvidence;
use super::parser::CpuListParseError;

/// Classification assigned to one physical core.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum CoreClass {
    /// A performance-oriented core in a confirmed hybrid topology.
    Performance,
    /// An efficiency-oriented core in a confirmed hybrid topology.
    Efficiency,
    /// A core in a topology with no corroborated heterogeneous distinction.
    Uniform,
    /// A core which could not be classified reliably.
    Unknown,
}

impl std::fmt::Display for CoreClass {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

/// Classification of the complete active CPU topology.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum TopologyClass {
    /// Both performance and efficiency physical cores were identified.
    Hybrid,
    /// Visible physical cores appear equivalent.
    Uniform,
    /// Available metadata cannot support a safe overall classification.
    Unknown,
}

impl std::fmt::Display for TopologyClass {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

/// Logical CPUs which the detector believes belong to one physical core.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PhysicalCore {
    /// Kernel physical package ID, when consistently available.
    pub package_id: Option<u32>,
    /// Kernel core ID, when consistently available.
    pub core_id: Option<u32>,
    /// Sorted active logical CPUs belonging to this physical core.
    pub logical_cpus: Vec<u32>,
    /// Conservative class assigned to this physical core.
    pub core_class: CoreClass,
    /// Detector confidence in `0.0..=1.0`.
    pub confidence: f32,
    /// Grouping and classification evidence local to this core.
    pub evidence: Vec<DetectionEvidence>,
}

/// A deterministic snapshot of the active Linux CPU topology.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CpuTopology {
    /// Sorted online logical CPU IDs.
    pub online_cpus: Vec<u32>,
    /// Deterministically ordered active physical-core groups.
    pub physical_cores: Vec<PhysicalCore>,
    /// Sorted logical CPUs on confirmed performance cores.
    pub performance_cpus: Vec<u32>,
    /// Sorted logical CPUs on confirmed efficiency cores.
    pub efficiency_cpus: Vec<u32>,
    /// Overall conservative topology classification.
    pub classification: TopologyClass,
    /// Detector confidence in `0.0..=1.0`.
    pub confidence: f32,
    /// Ordered observations, warnings, contradictions, and conclusions.
    pub evidence: Vec<DetectionEvidence>,
}

/// Fatal errors which prevent a usable topology snapshot.
///
/// Missing or malformed optional per-CPU metadata is represented as evidence
/// instead of an error.
#[derive(Debug, Error)]
pub enum DetectorError {
    /// The configured root is missing, inaccessible, or not a directory.
    #[error("CPU sysfs root {path} is unavailable: {source}")]
    SysfsRootUnavailable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// A required file exists but cannot be read.
    #[error("failed to read required CPU sysfs file {path}: {source}")]
    RequiredFileRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The authoritative global or fallback CPU list is malformed.
    #[error("required CPU list in {path} is malformed: {source}")]
    MalformedRequiredCpuList {
        path: PathBuf,
        #[source]
        source: CpuListParseError,
    },

    /// No logical CPU could be found below the configured root.
    #[error("CPU sysfs root {path} contains no discoverable CPUs")]
    NoCpus { path: PathBuf },
}
