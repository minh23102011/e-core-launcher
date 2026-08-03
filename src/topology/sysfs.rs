use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::evidence::{DetectionEvidence, EvidenceKind, EvidenceSource, EvidenceStrength};
use super::parser::{format_cpu_list, interpret_core_type, parse_cpu_list, CoreTypeInterpretation};
use super::types::DetectorError;

#[derive(Clone, Debug)]
pub(crate) struct CpuRecord {
    pub id: u32,
    pub package_id: Option<u32>,
    pub core_id: Option<u32>,
    pub siblings: Option<Vec<u32>>,
    pub core_type_metadata_present: bool,
    pub explicit_type: Option<CoreTypeInterpretation>,
    pub max_frequency_khz: Option<u64>,
    pub cache_fingerprint: Option<String>,
    pub evidence: Vec<DetectionEvidence>,
}

#[derive(Debug)]
pub(crate) struct SysfsSnapshot {
    pub online_cpus: Vec<u32>,
    pub records: Vec<CpuRecord>,
    pub evidence: Vec<DetectionEvidence>,
}

pub(crate) struct SysfsCpuSource {
    root: PathBuf,
}

impl SysfsCpuSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn collect(&self) -> Result<SysfsSnapshot, DetectorError> {
        self.validate_root()?;
        let mut evidence = Vec::new();
        let online_cpus = self.resolve_online_cpus(&mut evidence)?;
        if online_cpus.is_empty() {
            return Err(DetectorError::NoCpus {
                path: self.root.clone(),
            });
        }

        let online_set: BTreeSet<u32> = online_cpus.iter().copied().collect();
        let mut records = Vec::with_capacity(online_cpus.len());
        for cpu in &online_cpus {
            records.push(self.collect_cpu(*cpu, &online_set));
        }

        Ok(SysfsSnapshot {
            online_cpus,
            records,
            evidence,
        })
    }

    fn validate_root(&self) -> Result<(), DetectorError> {
        match fs::metadata(&self.root) {
            Ok(metadata) if metadata.is_dir() => Ok(()),
            Ok(_metadata) => Err(DetectorError::SysfsRootUnavailable {
                path: self.root.clone(),
                source: io::Error::new(io::ErrorKind::InvalidInput, "path is not a directory"),
            }),
            Err(source) => Err(DetectorError::SysfsRootUnavailable {
                path: self.root.clone(),
                source,
            }),
        }
    }

    fn resolve_online_cpus(
        &self,
        evidence: &mut Vec<DetectionEvidence>,
    ) -> Result<Vec<u32>, DetectorError> {
        let online_path = self.root.join("online");
        match fs::read_to_string(&online_path) {
            Ok(contents) => {
                let cpus = parse_cpu_list(&contents).map_err(|source| {
                    DetectorError::MalformedRequiredCpuList {
                        path: online_path.clone(),
                        source,
                    }
                })?;
                evidence.push(DetectionEvidence::new(
                    EvidenceSource::OnlineCpuList,
                    cpus.clone(),
                    contents.trim(),
                    format!(
                        "The global online CPU list selected {} active logical CPUs.",
                        cpus.len()
                    ),
                    EvidenceStrength::Explicit,
                    EvidenceKind::Observation,
                ));
                self.inspect_present_consistency(&cpus, evidence);
                Ok(cpus)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.resolve_online_without_global(evidence)
            }
            Err(source) => Err(DetectorError::RequiredFileRead {
                path: online_path,
                source,
            }),
        }
    }

    fn inspect_present_consistency(&self, online: &[u32], evidence: &mut Vec<DetectionEvidence>) {
        let path = self.root.join("present");
        let Some(contents) = read_optional_text(&path, evidence, &[]) else {
            return;
        };
        match parse_cpu_list(&contents) {
            Ok(present) => {
                let present_set: BTreeSet<u32> = present.into_iter().collect();
                let outside: Vec<u32> = online
                    .iter()
                    .copied()
                    .filter(|cpu| !present_set.contains(cpu))
                    .collect();
                if !outside.is_empty() {
                    evidence.push(DetectionEvidence::new(
                        EvidenceSource::OnlineCpuList,
                        outside,
                        contents.trim(),
                        "The online list contains CPUs absent from the present list; the global online list remains authoritative.",
                        EvidenceStrength::Strong,
                        EvidenceKind::Contradiction,
                    ));
                }
            }
            Err(error) => evidence.push(DetectionEvidence::new(
                EvidenceSource::OnlineCpuList,
                Vec::new(),
                contents.trim(),
                format!("The optional present CPU list is malformed: {error}."),
                EvidenceStrength::Moderate,
                EvidenceKind::Warning,
            )),
        }
    }

    fn resolve_online_without_global(
        &self,
        evidence: &mut Vec<DetectionEvidence>,
    ) -> Result<Vec<u32>, DetectorError> {
        let present_path = self.root.join("present");
        let candidates = match fs::read_to_string(&present_path) {
            Ok(contents) => parse_cpu_list(&contents).map_err(|source| {
                DetectorError::MalformedRequiredCpuList {
                    path: present_path.clone(),
                    source,
                }
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => self
                .cpu_directories()
                .map_err(|source| DetectorError::SysfsRootUnavailable {
                    path: self.root.clone(),
                    source,
                })?,
            Err(source) => {
                return Err(DetectorError::RequiredFileRead {
                    path: present_path,
                    source,
                });
            }
        };

        if candidates.is_empty() {
            return Err(DetectorError::NoCpus {
                path: self.root.clone(),
            });
        }

        let mut online = Vec::new();
        for cpu in candidates {
            let path = self.cpu_path(cpu).join("online");
            match fs::read_to_string(&path) {
                Ok(contents) if contents.trim() == "1" => online.push(cpu),
                Ok(contents) if contents.trim() == "0" => {
                    evidence.push(DetectionEvidence::new(
                        EvidenceSource::PerCpuOnline,
                        vec![cpu],
                        "0",
                        format!("CPU {cpu} is offline and was excluded."),
                        EvidenceStrength::Explicit,
                        EvidenceKind::Observation,
                    ));
                }
                Ok(contents) => evidence.push(DetectionEvidence::new(
                    EvidenceSource::PerCpuOnline,
                    vec![cpu],
                    contents.trim(),
                    format!(
                        "CPU {cpu} has a malformed online flag and was conservatively excluded."
                    ),
                    EvidenceStrength::Strong,
                    EvidenceKind::Warning,
                )),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    online.push(cpu);
                    evidence.push(DetectionEvidence::new(
                        EvidenceSource::PerCpuOnline,
                        vec![cpu],
                        "missing",
                        format!(
                            "CPU {cpu} has no per-CPU online flag; Linux omits this for CPUs which cannot be offlined, so it was treated as online."
                        ),
                        EvidenceStrength::Moderate,
                        EvidenceKind::Observation,
                    ));
                }
                Err(error) => evidence.push(DetectionEvidence::new(
                    EvidenceSource::PerCpuOnline,
                    vec![cpu],
                    error.to_string(),
                    format!(
                        "CPU {cpu}'s online flag could not be read and the CPU was conservatively excluded."
                    ),
                    EvidenceStrength::Moderate,
                    EvidenceKind::Warning,
                )),
            }
        }
        online.sort_unstable();
        evidence.push(DetectionEvidence::new(
            EvidenceSource::OnlineCpuList,
            online.clone(),
            format_cpu_list(&online),
            "The global online file was absent; active CPUs were resolved from present CPUs and per-CPU online flags.",
            EvidenceStrength::Strong,
            EvidenceKind::Observation,
        ));
        Ok(online)
    }

    fn cpu_directories(&self) -> io::Result<Vec<u32>> {
        let mut cpus = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(id) = name.strip_prefix("cpu") else {
                continue;
            };
            if !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()) {
                if let Ok(id) = id.parse::<u32>() {
                    cpus.push(id);
                }
            }
        }
        cpus.sort_unstable();
        cpus.dedup();
        Ok(cpus)
    }

    fn collect_cpu(&self, cpu: u32, online: &BTreeSet<u32>) -> CpuRecord {
        let mut evidence = Vec::new();
        let topology = self.cpu_path(cpu).join("topology");
        let package_id = read_topology_id(
            &topology.join("physical_package_id"),
            cpu,
            "physical_package_id",
            &mut evidence,
        );
        let core_id = read_topology_id(&topology.join("core_id"), cpu, "core_id", &mut evidence);
        let siblings = Self::read_siblings(cpu, &topology, online, &mut evidence);
        let (core_type_metadata_present, explicit_type) =
            read_core_type(&topology.join("core_type"), cpu, &mut evidence);
        let max_frequency_khz = self.read_max_frequency(cpu, &mut evidence);
        let cache_fingerprint = self.read_cache_fingerprint(cpu, online, &mut evidence);

        CpuRecord {
            id: cpu,
            package_id,
            core_id,
            siblings,
            core_type_metadata_present,
            explicit_type,
            max_frequency_khz,
            cache_fingerprint,
            evidence,
        }
    }

    fn read_siblings(
        cpu: u32,
        topology: &Path,
        online: &BTreeSet<u32>,
        evidence: &mut Vec<DetectionEvidence>,
    ) -> Option<Vec<u32>> {
        for filename in ["core_cpus_list", "thread_siblings_list"] {
            let path = topology.join(filename);
            let Some(contents) = read_optional_text(&path, evidence, &[cpu]) else {
                continue;
            };
            match parse_cpu_list(&contents) {
                Ok(parsed) => {
                    if !parsed.contains(&cpu) {
                        evidence.push(DetectionEvidence::new(
                            EvidenceSource::ThreadSiblings,
                            vec![cpu],
                            contents.trim(),
                            format!(
                                "{filename} for CPU {cpu} does not contain the CPU itself and was ignored."
                            ),
                            EvidenceStrength::Strong,
                            EvidenceKind::Contradiction,
                        ));
                        continue;
                    }
                    let active: Vec<u32> = parsed
                        .iter()
                        .copied()
                        .filter(|sibling| online.contains(sibling))
                        .collect();
                    let offline: Vec<u32> = parsed
                        .iter()
                        .copied()
                        .filter(|sibling| !online.contains(sibling))
                        .collect();
                    if !offline.is_empty() {
                        evidence.push(DetectionEvidence::new(
                            EvidenceSource::ThreadSiblings,
                            offline,
                            contents.trim(),
                            "Offline siblings were excluded from the active physical-core group.",
                            EvidenceStrength::Strong,
                            EvidenceKind::Observation,
                        ));
                    }
                    return Some(active);
                }
                Err(error) => evidence.push(DetectionEvidence::new(
                    EvidenceSource::ThreadSiblings,
                    vec![cpu],
                    contents.trim(),
                    format!("{filename} for CPU {cpu} is malformed and was ignored: {error}."),
                    EvidenceStrength::Moderate,
                    EvidenceKind::Warning,
                )),
            }
        }
        None
    }

    fn read_max_frequency(&self, cpu: u32, evidence: &mut Vec<DetectionEvidence>) -> Option<u64> {
        let cpufreq = self.cpu_path(cpu).join("cpufreq");
        for filename in ["cpuinfo_max_freq", "scaling_max_freq"] {
            let path = cpufreq.join(filename);
            let Some(contents) = read_optional_text(&path, evidence, &[cpu]) else {
                continue;
            };
            match contents.trim().parse::<u64>() {
                Ok(value) if value > 0 => return Some(value),
                Ok(_) | Err(_) => evidence.push(DetectionEvidence::new(
                    EvidenceSource::MaximumFrequency,
                    vec![cpu],
                    contents.trim(),
                    format!("{filename} for CPU {cpu} is not a positive integer and was ignored."),
                    EvidenceStrength::Weak,
                    EvidenceKind::Warning,
                )),
            }
        }
        None
    }

    fn read_cache_fingerprint(
        &self,
        cpu: u32,
        online: &BTreeSet<u32>,
        evidence: &mut Vec<DetectionEvidence>,
    ) -> Option<String> {
        let path = self.cpu_path(cpu).join("cache");
        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
            Err(error) => {
                evidence.push(DetectionEvidence::new(
                    EvidenceSource::CacheTopology,
                    vec![cpu],
                    error.to_string(),
                    format!("Cache metadata for CPU {cpu} could not be read and was ignored."),
                    EvidenceStrength::Weak,
                    EvidenceKind::Warning,
                ));
                return None;
            }
        };

        let mut indexes = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    evidence.push(DetectionEvidence::new(
                        EvidenceSource::CacheTopology,
                        vec![cpu],
                        error.to_string(),
                        format!(
                            "A cache directory entry for CPU {cpu} could not be read and was ignored."
                        ),
                        EvidenceStrength::Weak,
                        EvidenceKind::Warning,
                    ));
                    continue;
                }
            };
            let name = entry.file_name();
            if name.to_str().is_some_and(|name| name.starts_with("index")) {
                indexes.push(entry.path());
            }
        }
        indexes.sort();

        let mut parts = Vec::new();
        for index in indexes {
            let level = read_optional_text(&index.join("level"), evidence, &[cpu]);
            let cache_type = read_optional_text(&index.join("type"), evidence, &[cpu]);
            let size = read_optional_text(&index.join("size"), evidence, &[cpu]);
            let shared = read_optional_text(&index.join("shared_cpu_list"), evidence, &[cpu]);
            let (Some(level), Some(cache_type), Some(size), Some(shared)) =
                (level, cache_type, size, shared)
            else {
                continue;
            };
            match parse_cpu_list(&shared) {
                Ok(shared_cpus) => {
                    let active_shared: Vec<u32> = shared_cpus
                        .into_iter()
                        .filter(|shared_cpu| online.contains(shared_cpu))
                        .collect();
                    parts.push(format!(
                        "L{}:{}:{}:shared={}",
                        level.trim(),
                        cache_type.trim(),
                        size.trim(),
                        active_shared.len()
                    ));
                }
                Err(error) => evidence.push(DetectionEvidence::new(
                    EvidenceSource::CacheTopology,
                    vec![cpu],
                    shared.trim(),
                    format!(
                        "A shared_cpu_list cache attribute for CPU {cpu} is malformed and was ignored: {error}."
                    ),
                    EvidenceStrength::Weak,
                    EvidenceKind::Warning,
                )),
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("|"))
        }
    }

    fn cpu_path(&self, cpu: u32) -> PathBuf {
        self.root.join(format!("cpu{cpu}"))
    }
}

fn read_topology_id(
    path: &Path,
    cpu: u32,
    name: &str,
    evidence: &mut Vec<DetectionEvidence>,
) -> Option<u32> {
    let contents = read_optional_text(path, evidence, &[cpu])?;
    match contents.trim().parse::<i64>() {
        Ok(-1) => None,
        Ok(value) => match u32::try_from(value) {
            Ok(value) => Some(value),
            Err(_error) => {
                evidence.push(DetectionEvidence::new(
                    EvidenceSource::TopologyIds,
                    vec![cpu],
                    contents.trim(),
                    format!("{name} for CPU {cpu} is malformed or unsupported and was ignored."),
                    EvidenceStrength::Moderate,
                    EvidenceKind::Warning,
                ));
                None
            }
        },
        Err(_) => {
            evidence.push(DetectionEvidence::new(
                EvidenceSource::TopologyIds,
                vec![cpu],
                contents.trim(),
                format!("{name} for CPU {cpu} is malformed or unsupported and was ignored."),
                EvidenceStrength::Moderate,
                EvidenceKind::Warning,
            ));
            None
        }
    }
}

fn read_core_type(
    path: &Path,
    cpu: u32,
    evidence: &mut Vec<DetectionEvidence>,
) -> (bool, Option<CoreTypeInterpretation>) {
    let Some(contents) = read_optional_text(path, evidence, &[cpu]) else {
        return (false, None);
    };
    match interpret_core_type(&contents) {
        Ok(CoreTypeInterpretation::Unsupported(value)) => {
            evidence.push(DetectionEvidence::new(
                EvidenceSource::CoreType,
                vec![cpu],
                value.to_string(),
                format!(
                    "CPU {cpu} exposes an unsupported core_type value; no core class was inferred from it."
                ),
                EvidenceStrength::Explicit,
                EvidenceKind::Warning,
            ));
            (true, None)
        }
        Ok(interpreted) => (true, Some(interpreted)),
        Err(error) => {
            evidence.push(DetectionEvidence::new(
                EvidenceSource::CoreType,
                vec![cpu],
                contents.trim(),
                format!("CPU {cpu} has malformed core_type metadata: {error}."),
                EvidenceStrength::Explicit,
                EvidenceKind::Warning,
            ));
            (true, None)
        }
    }
}

fn read_optional_text(
    path: &Path,
    evidence: &mut Vec<DetectionEvidence>,
    cpus: &[u32],
) -> Option<String> {
    match fs::read_to_string(path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            evidence.push(DetectionEvidence::new(
                EvidenceSource::Classifier,
                cpus.to_vec(),
                error.to_string(),
                format!(
                    "Optional sysfs metadata {} could not be read and was ignored.",
                    path.display()
                ),
                EvidenceStrength::Weak,
                EvidenceKind::Warning,
            ));
            None
        }
    }
}
