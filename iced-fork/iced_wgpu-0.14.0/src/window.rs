//! Display rendering results on windows.
pub mod compositor;
#[cfg(windows)]
pub mod dcomp_surface;

pub use compositor::Compositor;
pub use wgpu::Surface;
