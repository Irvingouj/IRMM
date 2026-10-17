//! Native desktops per OS.

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod unix;
#[cfg(windows)]
pub mod windows;

use std::sync::Arc;

use crate::Desktop;

/// The desktop of the session this process runs in.
pub fn native() -> Option<Arc<dyn Desktop>> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        Some(Arc::new(unix::XcapDesktop))
    }
    #[cfg(windows)]
    {
        Some(Arc::new(windows::GdiDesktop))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        None
    }
}
