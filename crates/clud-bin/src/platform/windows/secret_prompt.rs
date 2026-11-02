//! Native Windows graphics initialization already returns a typed error.

pub fn configure(options: &mut eframe::NativeOptions) {
    use eframe::egui_wgpu::{WgpuSetup, WgpuSetupCreateNew};

    options.renderer = eframe::Renderer::Wgpu;
    let mut setup = WgpuSetupCreateNew::without_display_handle();
    setup.instance_descriptor.backends = eframe::wgpu::Backends::DX12;
    // The system FXC compiler avoids adding a dxcompiler DLL to the install.
    // wgpu's adapter enumeration includes Windows' software WARP adapter.
    setup
        .instance_descriptor
        .backend_options
        .dx12
        .shader_compiler = eframe::wgpu::Dx12Compiler::Fxc;
    options.wgpu_options.wgpu_setup = WgpuSetup::CreateNew(setup);
}

pub fn graphics_available() -> bool {
    true
}

/// Why no external helper produced a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalFailure {
    Cancelled,
    Unavailable,
}

/// No external prompt helpers exist on Windows.
pub fn external_prompt(
    _title: &str,
    _label: &str,
) -> Result<zeroize::Zeroizing<String>, ExternalFailure> {
    Err(ExternalFailure::Unavailable)
}
