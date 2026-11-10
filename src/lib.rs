//! Report task trees through the Zig progress protocol.
//!
//! ```no_run
//! use zprs::Progress;
//!
//! let progress = Progress::new("build", 2)?;
//! for name in ["assets", "code"] {
//!     let task = progress.root().start(name, 10);
//!     for _ in 0..10 {
//!         task.complete_one();
//!     }
//! } // Dropping each task ends it and completes one item in its parent.
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! A missing or invalid `ZIG_PROGRESS` channel disables reporting. This crate
//! never draws a terminal display. Use a supervisor such as `zpg`.
//! Names use 120 bytes by default. Select [`NameSize::Legacy`] for the published
//! 40-byte format. Every process in a tree must use the same format.
//! Pipe transport and fork support are available on Unix.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

mod protocol;
mod reporter;
#[cfg(unix)]
mod unix;

pub use reporter::{Node, Options, Progress};
#[cfg(unix)]
pub use unix::ForkResult;

/// Maximum number of nodes in one report, including the root.
pub const MAX_NODES: usize = 253;
/// Maximum estimated total. Zig reserves `u32::MAX` for internal use.
pub const MAX_TOTAL: u32 = u32::MAX - 1;

/// Name field size. The protocol does not negotiate this setting.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NameSize {
    /// The current Zig format, also used by `zpg`, `zppy`, and `zpgo`.
    #[default]
    Current,
    /// The published protocol and older Zig releases (40 bytes).
    Legacy,
}

impl NameSize {
    pub(crate) fn bytes(self) -> usize {
        match self {
            Self::Current => 120,
            Self::Legacy => 40,
        }
    }
}
