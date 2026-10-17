//! Native desktops per OS.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod x11;
#[cfg(windows)]
pub mod windows;

use std::sync::Arc;

use crate::Desktop;

/// The desktop of the session this process runs in.
pub fn native() -> Option<Arc<dyn Desktop>> {
    #[cfg(target_os = "macos")]
    {
        Some(Arc::new(macos::XcapDesktop))
    }
    #[cfg(target_os = "linux")]
    {
        Some(Arc::new(x11::X11Desktop))
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
