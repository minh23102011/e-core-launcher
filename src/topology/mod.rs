//! Linux CPU topology collection, grouping, and classification.

mod detector;
mod evidence;
mod parser;
mod sysfs;
mod types;

pub use detector::{CpuTopologyDetector, DetectorPolicy};
pub use evidence::{DetectionEvidence, EvidenceKind, EvidenceSource, EvidenceStrength};
pub use parser::{
    format_cpu_list, interpret_core_type, parse_cpu_list, CoreTypeInterpretation,
    CoreTypeParseError, CpuListParseError,
};
pub use types::{CoreClass, CpuTopology, DetectorError, PhysicalCore, TopologyClass};
