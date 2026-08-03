use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// Fatal discovery failures which prevent a complete report.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    /// No XDG data root was configured.
    #[error("no desktop application discovery roots were configured")]
    NoDiscoveryRoots,

    /// A caller-required `applications` directory was unavailable.
    #[error("required desktop application directory {path} is unavailable: {source}")]
    RequiredRootUnavailable {
        /// Required directory.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: io::Error,
    },

    /// A configured root was an empty path.
    #[error("desktop application discovery root at priority {priority} is empty")]
    EmptyDiscoveryRoot {
        /// Zero-based source priority.
        priority: usize,
    },
}
