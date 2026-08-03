use std::collections::BTreeSet;
use std::num::ParseIntError;

use thiserror::Error;

const MAX_EXPANDED_CPU_IDS: usize = 1_048_576;

/// Errors returned by the Linux CPU-list parser.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum CpuListParseError {
    /// The input contains no syntax after trimming.
    #[error("CPU list is empty")]
    Empty,

    /// Two commas are adjacent, or a comma is leading/trailing.
    #[error("CPU list contains an empty item at position {position}")]
    EmptyItem { position: usize },

    /// A list member is not an unsigned decimal CPU ID.
    #[error("invalid CPU ID `{value}`")]
    InvalidCpuId { value: String },

    /// A range has missing or extra delimiters.
    #[error("invalid CPU range `{value}`")]
    InvalidRange { value: String },

    /// The range end is smaller than its start.
    #[error("descending CPU range {start}-{end} is not allowed")]
    DescendingRange { start: u32, end: u32 },

    /// One range is unreasonably large to materialize.
    #[error("CPU range {start}-{end} expands to too many IDs")]
    RangeTooLarge { start: u32, end: u32 },

    /// The union of all list members is unreasonably large.
    #[error("CPU list contains more than {limit} unique IDs")]
    TooManyCpuIds { limit: usize },
}

/// Interpretation of the Linux x86 `topology/core_type` value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoreTypeInterpretation {
    /// Intel Core (`0x40`).
    Performance,
    /// Intel Atom (`0x20`).
    Efficiency,
    /// A syntactically valid value with no supported mapping.
    Unsupported(u32),
}

/// Syntax errors in a `topology/core_type` value.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum CoreTypeParseError {
    /// The value contains no syntax after trimming.
    #[error("core_type is empty")]
    Empty,

    /// The value is not a supported integer syntax.
    #[error("invalid core_type value `{value}`")]
    Invalid { value: String },
}

/// Parse Linux CPU-list syntax into sorted, unique logical CPU IDs.
///
/// Valid inputs include `0`, `0-3`, and `0-3,8-11`. Whitespace around the
/// complete list is ignored; whitespace inside the syntax is rejected.
///
/// # Errors
///
/// Returns [`CpuListParseError`] when the input is empty, contains malformed
/// IDs or ranges, descends, or would expand to an unreasonable number of IDs.
pub fn parse_cpu_list(input: &str) -> Result<Vec<u32>, CpuListParseError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(CpuListParseError::Empty);
    }

    let mut result = BTreeSet::new();
    for (position, item) in input.split(',').enumerate() {
        if item.is_empty() {
            return Err(CpuListParseError::EmptyItem { position });
        }

        let hyphen_count = item.bytes().filter(|byte| *byte == b'-').count();
        match hyphen_count {
            0 => {
                result.insert(parse_cpu_id(item)?);
            }
            1 => {
                let (start, end) =
                    item.split_once('-')
                        .ok_or_else(|| CpuListParseError::InvalidRange {
                            value: item.to_owned(),
                        })?;
                if start.is_empty() || end.is_empty() {
                    return Err(CpuListParseError::InvalidRange {
                        value: item.to_owned(),
                    });
                }
                let start = parse_cpu_id(start)?;
                let end = parse_cpu_id(end)?;
                if start > end {
                    return Err(CpuListParseError::DescendingRange { start, end });
                }
                let count = u64::from(end) - u64::from(start) + 1;
                if usize::try_from(count).map_or(true, |count| count > MAX_EXPANDED_CPU_IDS) {
                    return Err(CpuListParseError::RangeTooLarge { start, end });
                }
                result.extend(start..=end);
            }
            _ => {
                return Err(CpuListParseError::InvalidRange {
                    value: item.to_owned(),
                });
            }
        }
        if result.len() > MAX_EXPANDED_CPU_IDS {
            return Err(CpuListParseError::TooManyCpuIds {
                limit: MAX_EXPANDED_CPU_IDS,
            });
        }
    }

    Ok(result.into_iter().collect())
}

fn parse_cpu_id(value: &str) -> Result<u32, CpuListParseError> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || value.starts_with('+')
    {
        return Err(CpuListParseError::InvalidCpuId {
            value: value.to_owned(),
        });
    }
    value
        .parse::<u32>()
        .map_err(|_error: ParseIntError| CpuListParseError::InvalidCpuId {
            value: value.to_owned(),
        })
}

/// Interpret the x86 kernel's raw `topology/core_type` value.
///
/// Linux exposes the Intel CPUID leaf 0x1A core-type byte as a decimal sysfs
/// value on kernels which provide this optional attribute. Intel defines
/// `0x40` (64) as Core and `0x20` (32) as Atom. No other numeric value is
/// inferred here: future or vendor-specific values remain unsupported.
///
/// # Errors
///
/// Returns [`CoreTypeParseError`] when the value is empty or is neither a
/// decimal integer nor a `0x`-prefixed hexadecimal integer.
pub fn interpret_core_type(input: &str) -> Result<CoreTypeInterpretation, CoreTypeParseError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(CoreTypeParseError::Empty);
    }

    let parsed = if let Some(hex) = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16)
    } else {
        input.parse::<u32>()
    }
    .map_err(|_error| CoreTypeParseError::Invalid {
        value: input.to_owned(),
    })?;

    Ok(match parsed {
        0x40 => CoreTypeInterpretation::Performance,
        0x20 => CoreTypeInterpretation::Efficiency,
        value => CoreTypeInterpretation::Unsupported(value),
    })
}

/// Format sorted or unsorted CPU IDs using compact Linux CPU-list syntax.
#[must_use]
pub fn format_cpu_list(cpus: &[u32]) -> String {
    let sorted: Vec<u32> = cpus
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut output = Vec::new();
    let mut index = 0;
    while index < sorted.len() {
        let start = sorted[index];
        let mut end = start;
        while index + 1 < sorted.len() && sorted[index + 1] == end.saturating_add(1) {
            index += 1;
            end = sorted[index];
        }
        if start == end {
            output.push(start.to_string());
        } else {
            output.push(format!("{start}-{end}"));
        }
        index += 1;
    }
    output.join(",")
}

#[cfg(test)]
mod tests {
    use super::{
        format_cpu_list, interpret_core_type, parse_cpu_list, CoreTypeInterpretation,
        CoreTypeParseError, CpuListParseError,
    };

    #[test]
    fn parses_supported_cpu_list_forms() {
        let cases = [
            ("0", vec![0]),
            ("0-3", vec![0, 1, 2, 3]),
            ("0-3,8-11", vec![0, 1, 2, 3, 8, 9, 10, 11]),
            ("0,2,4,6", vec![0, 2, 4, 6]),
            ("0-1,4,8-10", vec![0, 1, 4, 8, 9, 10]),
            ("  2,0-2,2  \n", vec![0, 1, 2]),
        ];

        for (input, expected) in cases {
            assert_eq!(parse_cpu_list(input), Ok(expected), "input: {input}");
        }
    }

    #[test]
    fn rejects_malformed_cpu_lists() {
        assert_eq!(parse_cpu_list(""), Err(CpuListParseError::Empty));
        assert!(matches!(
            parse_cpu_list("0,,2"),
            Err(CpuListParseError::EmptyItem { .. })
        ));
        assert!(matches!(
            parse_cpu_list("3-1"),
            Err(CpuListParseError::DescendingRange { start: 3, end: 1 })
        ));
        assert!(matches!(
            parse_cpu_list("0-1-2"),
            Err(CpuListParseError::InvalidRange { .. })
        ));
        assert!(matches!(
            parse_cpu_list("0, 2"),
            Err(CpuListParseError::InvalidCpuId { .. })
        ));
        assert!(matches!(
            parse_cpu_list("cpu0"),
            Err(CpuListParseError::InvalidCpuId { .. })
        ));
        assert!(matches!(
            parse_cpu_list("0-2000000"),
            Err(CpuListParseError::RangeTooLarge { .. })
        ));
    }

    #[test]
    fn formats_cpu_lists_compactly_and_deterministically() {
        assert_eq!(format_cpu_list(&[8, 2, 1, 0, 2, 10, 9, 4]), "0-2,4,8-10");
        assert_eq!(format_cpu_list(&[]), "");
    }

    #[test]
    fn interprets_only_documented_intel_core_types() {
        assert_eq!(
            interpret_core_type("64"),
            Ok(CoreTypeInterpretation::Performance)
        );
        assert_eq!(
            interpret_core_type("0x20\n"),
            Ok(CoreTypeInterpretation::Efficiency)
        );
        assert_eq!(
            interpret_core_type("1"),
            Ok(CoreTypeInterpretation::Unsupported(1))
        );
        assert!(matches!(
            interpret_core_type("performance"),
            Err(CoreTypeParseError::Invalid { .. })
        ));
    }
}
