mod cosmic_text_system;
/// FIL-65 timing build only: prints a start-up mark to stderr as
/// `FIL65 <unix ms> <label>`. Never merged.
pub fn fil65_mark(label: &str) {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or_default();
    eprintln!("FIL65 {ms:.1} {label}");
}
mod wgpu_atlas;
mod wgpu_context;
mod wgpu_renderer;

pub use cosmic_text_system::*;
pub use wgpu;
pub use wgpu_atlas::*;
pub use wgpu_context::*;
pub use wgpu_renderer::{GpuContext, WgpuRenderer, WgpuSurfaceConfig};
