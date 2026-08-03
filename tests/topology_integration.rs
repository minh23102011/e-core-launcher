use std::path::{Path, PathBuf};

use ecore_launcher::{
    CoreClass, CpuTopology, CpuTopologyDetector, DetectorError, EvidenceKind, EvidenceSource,
    TopologyClass,
};

type CoreSummary = (Option<u32>, Option<u32>, Vec<u32>, CoreClass);

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/sysfs")
        .join(name)
}

fn detect(name: &str) -> CpuTopology {
    CpuTopologyDetector::new(fixture(name))
        .detect()
        .unwrap_or_else(|error| panic!("fixture {name} should detect successfully: {error}"))
}

#[test]
fn intel_hybrid_uses_explicit_metadata_not_cpu_numbering() {
    let topology = detect("intel-hybrid");

    assert_eq!(topology.classification, TopologyClass::Hybrid);
    assert_eq!(topology.online_cpus, (0..=7).collect::<Vec<_>>());
    assert_eq!(topology.performance_cpus, vec![0, 2, 4, 6]);
    assert_eq!(topology.efficiency_cpus, vec![1, 3, 5, 7]);
    assert!(topology.confidence > 0.95);
    assert!(topology.evidence.iter().any(|item| {
        item.source == EvidenceSource::CoreType
            && item.kind == EvidenceKind::Classification
            && item.affected_cpus == vec![1, 3, 5, 7]
    }));
    assert!(topology.evidence.iter().any(|item| {
        item.source == EvidenceSource::CacheTopology
            && item.observed_value == "2 complete cache-layout pattern(s)"
    }));
}

#[test]
fn physical_cores_are_grouped_and_ordered_by_topology_metadata() {
    let topology = detect("intel-hybrid");
    let groups: Vec<CoreSummary> = topology
        .physical_cores
        .iter()
        .map(|core| {
            (
                core.package_id,
                core.core_id,
                core.logical_cpus.clone(),
                core.core_class,
            )
        })
        .collect();

    assert_eq!(
        groups,
        vec![
            (Some(0), Some(0), vec![0, 4], CoreClass::Performance),
            (Some(0), Some(1), vec![2, 6], CoreClass::Performance),
            (Some(0), Some(2), vec![1], CoreClass::Efficiency),
            (Some(0), Some(3), vec![3], CoreClass::Efficiency),
            (Some(0), Some(4), vec![5], CoreClass::Efficiency),
            (Some(0), Some(5), vec![7], CoreClass::Efficiency),
        ]
    );
}

#[test]
fn non_hybrid_intel_is_uniform_without_efficiency_mask() {
    let topology = detect("intel-uniform");

    assert_eq!(topology.classification, TopologyClass::Uniform);
    assert!(topology.performance_cpus.is_empty());
    assert!(topology.efficiency_cpus.is_empty());
    assert!(topology
        .physical_cores
        .iter()
        .all(|core| core.core_class == CoreClass::Uniform));
}

#[test]
fn ordinary_amd_frequency_variation_is_not_called_hybrid() {
    let topology = detect("amd-uniform");

    assert_eq!(topology.classification, TopologyClass::Uniform);
    assert!(topology.performance_cpus.is_empty());
    assert!(topology.efficiency_cpus.is_empty());
}

#[test]
fn repeated_frequency_and_smt_clusters_can_form_a_low_confidence_hybrid_result() {
    let topology = detect("heuristic-hybrid");

    assert_eq!(topology.classification, TopologyClass::Hybrid);
    assert_eq!(topology.performance_cpus, vec![0, 2, 4, 6]);
    assert_eq!(topology.efficiency_cpus, vec![1, 3, 5, 7]);
    assert!(topology.confidence < detect("intel-hybrid").confidence);
    assert!(topology.evidence.iter().any(|item| {
        item.source == EvidenceSource::MaximumFrequency
            && item.kind == EvidenceKind::Classification
            && item.interpretation.contains("remains heuristic")
    }));
}

#[test]
fn missing_cpufreq_is_optional() {
    let topology = detect("missing-cpufreq");

    assert_eq!(topology.classification, TopologyClass::Uniform);
    assert_eq!(topology.physical_cores.len(), 2);
    assert!(topology.efficiency_cpus.is_empty());
}

#[test]
fn malformed_optional_metadata_becomes_evidence() {
    let topology = detect("malformed-optional");

    assert_eq!(topology.online_cpus, vec![0, 1, 2]);
    assert_eq!(topology.classification, TopologyClass::Unknown);
    assert!(topology.efficiency_cpus.is_empty());
    assert!(
        topology
            .evidence
            .iter()
            .filter(|item| matches!(
                item.kind,
                EvidenceKind::Warning | EvidenceKind::Contradiction
            ))
            .count()
            >= 4
    );
    assert!(topology.evidence.iter().any(|item| {
        item.source == EvidenceSource::CoreType
            && item.interpretation.contains("unsupported core_type")
    }));
}

#[test]
fn offline_present_cpu_is_excluded_everywhere() {
    let topology = detect("offline-cpu");

    assert_eq!(topology.online_cpus, vec![0, 1, 2]);
    assert!(topology
        .physical_cores
        .iter()
        .flat_map(|core| &core.logical_cpus)
        .all(|cpu| *cpu != 3));
    assert!(!topology.performance_cpus.contains(&3));
    assert!(!topology.efficiency_cpus.contains(&3));
}

#[test]
fn missing_global_online_uses_per_cpu_flags_and_accepts_cpu0_without_one() {
    let topology = detect("per-cpu-online");

    assert_eq!(topology.online_cpus, vec![0, 1]);
    assert!(topology.evidence.iter().any(|item| {
        item.source == EvidenceSource::PerCpuOnline
            && item.affected_cpus == vec![0]
            && item.observed_value == "missing"
    }));
    assert!(topology.evidence.iter().any(|item| {
        item.source == EvidenceSource::PerCpuOnline
            && item.affected_cpus == vec![2]
            && item.observed_value == "0"
    }));
}

#[test]
fn conflicting_heuristic_signals_produce_unknown() {
    let topology = detect("ambiguous");

    assert_eq!(topology.classification, TopologyClass::Unknown);
    assert!(topology.performance_cpus.is_empty());
    assert!(topology.efficiency_cpus.is_empty());
    assert!(topology.evidence.iter().any(|item| {
        item.source == EvidenceSource::MaximumFrequency && item.kind == EvidenceKind::Contradiction
    }));
}

#[test]
fn output_ordering_and_evidence_are_deterministic() {
    let first = detect("intel-hybrid");
    let second = detect("intel-hybrid");

    assert_eq!(first, second);
    assert!(first.online_cpus.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(first
        .performance_cpus
        .windows(2)
        .all(|pair| pair[0] < pair[1]));
    assert!(first
        .efficiency_cpus
        .windows(2)
        .all(|pair| pair[0] < pair[1]));
    assert!(first
        .physical_cores
        .iter()
        .all(|core| core.logical_cpus.windows(2).all(|pair| pair[0] < pair[1])));
}

#[test]
fn confidence_values_are_bounded() {
    for name in [
        "intel-hybrid",
        "intel-uniform",
        "amd-uniform",
        "heuristic-hybrid",
        "missing-cpufreq",
        "malformed-optional",
        "offline-cpu",
        "ambiguous",
    ] {
        let topology = detect(name);
        assert!((0.0..=1.0).contains(&topology.confidence), "{name}");
        assert!(
            topology
                .physical_cores
                .iter()
                .all(|core| (0.0..=1.0).contains(&core.confidence)),
            "{name}"
        );
    }
}

#[test]
fn topology_json_is_stable_and_machine_readable() {
    let topology = detect("intel-hybrid");
    let first = serde_json::to_string_pretty(&topology).expect("serialize topology");
    let second = serde_json::to_string_pretty(&topology).expect("serialize topology again");
    let parsed: serde_json::Value = serde_json::from_str(&first).expect("valid JSON");

    assert_eq!(first, second);
    assert_eq!(parsed["classification"], "Hybrid");
    assert_eq!(
        parsed["online_cpus"],
        serde_json::json!([0, 1, 2, 3, 4, 5, 6, 7])
    );
    assert_eq!(parsed["efficiency_cpus"], serde_json::json!([1, 3, 5, 7]));
    assert!(parsed["evidence"].is_array());
}

#[test]
fn malformed_required_online_list_is_fatal() {
    let error = CpuTopologyDetector::new(fixture("malformed-online"))
        .detect()
        .expect_err("malformed global online list must be fatal");

    assert!(matches!(
        error,
        DetectorError::MalformedRequiredCpuList { .. }
    ));
}

#[test]
fn missing_sysfs_root_is_fatal() {
    let error = CpuTopologyDetector::new(fixture("does-not-exist"))
        .detect()
        .expect_err("missing root must be fatal");

    assert!(matches!(error, DetectorError::SysfsRootUnavailable { .. }));
}
