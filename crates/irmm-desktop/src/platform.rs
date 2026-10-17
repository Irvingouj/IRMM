//! Native desktops per OS.

use std::sync::Arc;

use crate::Desktop;

/// The desktop of the machine this process runs on, if capture is supported here.
pub fn native() -> Option<Arc<dyn Desktop>> {
    None
}
