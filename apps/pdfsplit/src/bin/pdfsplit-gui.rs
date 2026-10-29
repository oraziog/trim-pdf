//! The window of the splitting app.
//!
//! Built only with the `gui` feature: `cargo build -p pdfsplit --features gui --bin pdfsplit-gui`.
//! The command line stays a separate, small binary that needs no graphics at all.
//!
//! The release build has no console window of its own: this is the file a person double-clicks.
//!
//! # Which renderer, and why not all of them
//!
//! Two stacks are compiled in, because neither alone covers the machines this has to run on:
//!
//! - wgpu, drawing through **Direct3D 12** on Windows and through its own choice elsewhere;
//! - glow, drawing through OpenGL, which is what is left on a machine with no D3D12 adapter.
//!
//! Vulkan is deliberately **not** offered. wgpu's default backends on Windows are Vulkan and
//! D3D12, and on a Windows 11 machine with an Intel graphics driver the Vulkan runtime
//! (`igvk64.dll`) dies with an access violation the moment the instance is created — the process
//! is gone before any fallback can run, and there is no error to catch. Restricting wgpu to D3D12
//! means that driver is never loaded at all.
//!
//! Glow cannot go first: on a bare virtual machine or a remote desktop there is no OpenGL 2.0 and
//! `egui_glow` refuses with `requires opengl 2.0+`. wgpu with D3D12 works there.
//!
//! `PDFSPLIT_RENDERER` overrides the order for a machine that misbehaves: `dx12`, `glow`, `wgpu`
//! (wgpu's own default backends, Vulkan included), `vulkan`, `auto`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[path = "../gui/mod.rs"]
mod gui;

use eframe::wgpu;

/// One way of drawing the window, tried in turn.
#[derive(Clone, Copy)]
struct Attempt {
    /// What this way is called in a message to a person.
    name: &'static str,
    renderer: eframe::Renderer,
    /// The wgpu backends to allow, or `None` for wgpu's own choice.
    backends: Option<wgpu::Backends>,
}

impl Attempt {
    /// wgpu with Direct3D 12 only: the one that works on Windows without loading a Vulkan driver.
    const fn direct3d() -> Self {
        Self { name: "Direct3D 12", renderer: eframe::Renderer::Wgpu, backends: Some(wgpu::Backends::DX12) }
    }

    /// wgpu as it configures itself.
    const fn any_gpu() -> Self {
        Self { name: "wgpu", renderer: eframe::Renderer::Wgpu, backends: None }
    }

    /// wgpu through Vulkan: what crashed on an Intel driver, so it is only ever asked for.
    const fn vulkan() -> Self {
        Self { name: "Vulkan", renderer: eframe::Renderer::Wgpu, backends: Some(wgpu::Backends::VULKAN) }
    }

    /// OpenGL, through glow.
    const fn open_gl() -> Self {
        Self { name: "OpenGL", renderer: eframe::Renderer::Glow, backends: None }
    }
}

fn main() -> eframe::Result<()> {
    let attempts = attempts();
    let mut failures: Vec<String> = Vec::with_capacity(attempts.len());
    for attempt in attempts {
        match open(attempt) {
            Ok(()) => return Ok(()),
            Err(error) => {
                // A release build has no console to print on; the dialog below is what a person
                // sees, and this line is what a terminal in the folder would show.
                eprintln!("pdfsplit: la finestra con {} non si è aperta ({error})", attempt.name);
                failures.push(format!("{}: {error}", attempt.name));
            }
        }
    }
    complain(&failures);
    std::process::exit(1);
}

/// The ways to try, in order.
///
/// A name in `PDFSPLIT_RENDERER` is taken at its word: one attempt, no fallback, so a machine that
/// misbehaves can be told exactly what to do and the failure names what was asked for. A word
/// nobody knows is not worth failing a window over, so it falls back to the default order.
fn attempts() -> Vec<Attempt> {
    let asked = std::env::var("PDFSPLIT_RENDERER").unwrap_or_default().trim().to_lowercase();
    match asked.as_str() {
        "dx12" | "direct3d" | "d3d12" => vec![Attempt::direct3d()],
        "glow" | "opengl" | "gl" => vec![Attempt::open_gl()],
        "wgpu" => vec![Attempt::any_gpu()],
        "vulkan" | "vk" => vec![Attempt::vulkan()],
        _ => default_order(),
    }
}

/// The order to try when nothing was asked for.
fn default_order() -> Vec<Attempt> {
    vec![
        #[cfg(target_os = "windows")]
        Attempt::direct3d(),
        #[cfg(not(target_os = "windows"))]
        Attempt::any_gpu(),
        Attempt::open_gl(),
    ]
}

/// Open the window with one way of drawing.
fn open(attempt: Attempt) -> eframe::Result<()> {
    let mut options = eframe::NativeOptions {
        renderer: attempt.renderer,
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([900.0, 700.0]).with_min_inner_size([620.0, 520.0]).with_title(concat!(
            "pdfsplit ",
            env!("CARGO_PKG_VERSION"),
            " — taglia un PDF"
        )),
        ..Default::default()
    };
    if let Some(backends) = attempt.backends
        && let eframe::WgpuConfiguration { wgpu_setup: eframe::egui_wgpu::WgpuSetup::CreateNew(setup), .. } = &mut options.wgpu_options
    {
        setup.instance_descriptor.backends = backends;
    }
    eframe::run_native("pdfsplit", options, Box::new(|cc| Ok(Box::new(gui::app::App::new(cc)))))
}

/// Tell a person that the window will not open, since a release build has no console.
fn complain(failures: &[String]) {
    let message = format!(
        "Nessun modo di disegnare la finestra ha funzionato:\n\n{}\n\n\
         È un problema di driver video, non del file da tagliare: il comando pdfsplit.exe \
         funziona lo stesso da una finestra del prompt.",
        failures.join("\n")
    );
    for line in failures {
        eprintln!("pdfsplit: {line}");
    }
    let _ = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("pdfsplit — la finestra non si apre")
        .set_description(message)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}
