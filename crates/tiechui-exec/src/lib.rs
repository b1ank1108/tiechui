//! Shell execution with process-tree cancellation: process groups on Unix, job objects on Windows.

mod execute;
mod git_bash;
#[cfg(unix)]
#[path = "unix.rs"]
mod sys;
#[cfg(windows)]
#[path = "windows.rs"]
mod sys;

pub use execute::*;
pub use git_bash::{GIT_BASH_ENV, resolve_git_bash, resolve_git_bash_with_env};
#[cfg(windows)]
pub use sys::signal as bash_signal;
pub use sys::{Tree, resolve_shell};
