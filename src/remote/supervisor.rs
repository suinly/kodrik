#[cfg(windows)]
#[path = "supervisor_windows.rs"]
mod platform;
#[cfg(unix)]
#[path = "supervisor_unix.rs"]
mod platform;
pub(crate) use platform::*;
