//! Shell execution with process-tree cancellation: process groups on Unix, job objects on Windows.

mod execute;
#[cfg(unix)]
#[path = "unix.rs"]
mod sys;
#[cfg(windows)]
#[path = "windows.rs"]
mod sys;

pub use execute::*;
#[cfg(windows)]
pub use sys::GIT_BASH_ENV;
pub use sys::{Tree, resolve_shell};
