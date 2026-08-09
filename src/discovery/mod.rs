//! Safe, deterministic Linux desktop-application discovery.
//!
//! The scanner reads only `applications` directories under configured XDG
//! data roots. It never executes desktop-entry content and never invokes a
//! shell. Desktop action groups are outside the supported core scope.

mod desktop_entry;
mod error;
mod exec_parser;
mod resolver;
mod scanner;
mod types;

pub use desktop_entry::DesktopEntryParseError;
pub use error::DiscoveryError;
pub use exec_parser::{parse_exec, ExecParseError, ParsedExec};
pub use resolver::{ExecutableResolutionError, ExecutableResolver};
pub use scanner::{DesktopApplicationScanner, DiscoveryOptions};
pub use types::{
    DiscoveredApplication, DiscoveryReport, DiscoveryWarning, DiscoveryWarningCategory,
    DiscoveryWarningSeverity,
};
