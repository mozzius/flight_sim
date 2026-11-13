//! Device tiers. Phones and tablets get a low-memory mode: iOS kills a tab
//! whose GPU process goes over its budget, and the full scene doesn't fit.
//!
//! On the web the page decides before the game starts
//! (`window.FLIGHT_SIM_LOW_MEMORY`: the GRAPHICS setting, AUTO meaning touch
//! devices; `?low` / `?high` override it). Natively it's the GRAPHICS setting
//! (AUTO is high), switchable while running, and `FLIGHT_SIM_LOW_MEMORY=1`
//! forces it on.

use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
};

static LOW: AtomicBool = AtomicBool::new(false);
static INIT: OnceLock<()> = OnceLock::new();

/// Fewer and nearer trees, half-res terrain textures, no 32 m near terrain
/// mesh, smaller shadow maps and cheaper clouds.
pub fn low_memory() -> bool {
    INIT.get_or_init(|| LOW.store(detect(), Ordering::Relaxed));
    LOW.load(Ordering::Relaxed)
}

#[cfg(target_arch = "wasm32")]
fn detect() -> bool {
    js_sys::Reflect::get(&js_sys::global(), &"FLIGHT_SIM_LOW_MEMORY".into())
        .map(|v| v.is_truthy())
        .unwrap_or(false)
}

#[cfg(not(target_arch = "wasm32"))]
fn detect() -> bool {
    std::env::var("FLIGHT_SIM_LOW_MEMORY").is_ok() || crate::settings::stored_graphics() == crate::settings::Graphics::Low
}

/// Switches the scene for a new GRAPHICS pick (natively; the web reloads instead).
#[cfg(not(target_arch = "wasm32"))]
pub fn set_graphics(graphics: crate::settings::Graphics) {
    low_memory();
    let low = std::env::var("FLIGHT_SIM_LOW_MEMORY").is_ok() || graphics == crate::settings::Graphics::Low;
    LOW.store(low, Ordering::Relaxed);
}

/// `<OS data dir>/flight_sim`.
#[cfg(not(target_arch = "wasm32"))]
pub fn data_dir() -> Option<std::path::PathBuf> {
    let home = |rest: &str| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(rest));
    let dir = if cfg!(target_os = "macos") {
        home("Library/Application Support")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(Into::into)
    } else {
        std::env::var_os("XDG_DATA_HOME").map(Into::into).or_else(|| home(".local/share"))
    }?;
    Some(dir.join("flight_sim"))
}

/// Whether to use Bevy's physical atmosphere. It samples 32-bit float
/// lookup textures with filtering, which needs the `float32-filterable` GPU
/// feature; many phones lack it, so the page checks
/// (`window.FLIGHT_SIM_NO_ATMOSPHERE`, or `?noatmo`) and they get a plain
/// gradient sky instead. Natively: `FLIGHT_SIM_NO_ATMOSPHERE=1`.
pub fn atmosphere() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        #[cfg(target_arch = "wasm32")]
        {
            !js_sys::Reflect::get(&js_sys::global(), &"FLIGHT_SIM_NO_ATMOSPHERE".into())
                .map(|v| v.is_truthy())
                .unwrap_or(false)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            std::env::var("FLIGHT_SIM_NO_ATMOSPHERE").is_err()
        }
    })
}

/// Screen-edge insets (left, top, right, bottom) in logical pixels that
/// controls should keep clear of: the notch and the home indicator. From the
/// page's `env(safe-area-inset-*)` on the web, zero elsewhere.
pub fn safe_area() -> bevy::math::Vec4 {
    #[cfg(target_arch = "wasm32")]
    {
        let get = || -> Option<bevy::math::Vec4> {
            let v = js_sys::Reflect::get(&js_sys::global(), &"FLIGHT_SIM_SAFE_AREA".into()).ok()?;
            let a: js_sys::Array = v.dyn_into().ok()?;
            let n = |i| a.get(i).as_f64().unwrap_or(0.0) as f32;
            Some(bevy::math::Vec4::new(n(0), n(1), n(2), n(3)))
        };
        use wasm_bindgen::JsCast;
        get().unwrap_or_default()
    }
    #[cfg(not(target_arch = "wasm32"))]
    bevy::math::Vec4::ZERO
}
