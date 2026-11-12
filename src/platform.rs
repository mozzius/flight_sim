//! Device tiers. Phones and tablets get a low-memory mode: iOS kills a tab
//! whose GPU process goes over its budget, and the full scene doesn't fit.
//!
//! On the web the page decides (`window.FLIGHT_SIM_LOW_MEMORY`: touch
//! devices, or `?low`; `?high` forces it off). Natively,
//! `FLIGHT_SIM_LOW_MEMORY=1` turns it on for testing.

use std::sync::OnceLock;

/// Fewer and nearer trees, half-res terrain textures, no 32 m near terrain
/// mesh, smaller shadow maps and cheaper clouds.
pub fn low_memory() -> bool {
    static LOW: OnceLock<bool> = OnceLock::new();
    *LOW.get_or_init(detect)
}

#[cfg(target_arch = "wasm32")]
fn detect() -> bool {
    js_sys::Reflect::get(&js_sys::global(), &"FLIGHT_SIM_LOW_MEMORY".into())
        .map(|v| v.is_truthy())
        .unwrap_or(false)
}

#[cfg(not(target_arch = "wasm32"))]
fn detect() -> bool {
    std::env::var("FLIGHT_SIM_LOW_MEMORY").is_ok()
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
