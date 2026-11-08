mod cosmic_text_system;

/// The log target of start-up phases ("vulkan-instance", "gpu-device",
/// "fonts-ready", "first-frame"), logged at debug level as each one ends, so
/// an app can time its own start.
pub const STARTUP_LOG_TARGET: &str = "gpui::startup";

pub(crate) fn startup_phase(label: &str) {
    log::debug!(target: STARTUP_LOG_TARGET, "{label}");
}
mod wgpu_atlas;
mod wgpu_context;
mod wgpu_renderer;

pub use cosmic_text_system::*;
pub use wgpu;
pub use wgpu_atlas::*;
pub use wgpu_context::*;
pub use wgpu_renderer::{GpuContext, WgpuRenderer, WgpuSurfaceConfig};
