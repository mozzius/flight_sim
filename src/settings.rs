//! Player settings: GRAPHICS (the main menu's SETTINGS) and the endless
//! DIFFICULTY, plus whether the tutorial's been finished. Kept in `localStorage` on the web and in
//! `<data dir>/flight_sim/settings` (`key=value` lines) natively; screenshot
//! runs keep none. `FLIGHT_SIM_GRAPHICS=auto|low|high` and
//! `FLIGHT_SIM_DIFFICULTY=easy|normal|hard` pin them for a run without saving.
//!
//! GRAPHICS picks the low-memory scene (see `platform::low_memory`). AUTO is
//! low on touch devices on the web and high everywhere else; `?low` / `?high`
//! and `FLIGHT_SIM_LOW_MEMORY` still win. Natively a change applies at once
//! (shadows, clouds and trees live, the terrain from the next round's bake).
//! On the web the page reads it before the game starts (it also caps the
//! canvas resolution), so APPLY saves it and reloads.

use bevy::prelude::*;

use crate::difficulty::Difficulty;

pub struct SettingsPlugin;

impl Plugin for SettingsPlugin {
    fn build(&self, app: &mut App) {
        let settings = Settings::load();
        app.insert_resource(settings).insert_resource(Applied(settings.graphics));
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(switch) = dev_switch() {
            app.add_systems(Update, move |time: Res<Time<Real>>, mut s: ResMut<Settings>, mut a: ResMut<Applied>, mut done: Local<bool>| {
                if !*done && time.elapsed_secs() >= switch.0 {
                    *done = true;
                    s.graphics = switch.1;
                    apply_graphics(&s, &mut a);
                    info!("graphics switched to {}", switch.1.name());
                }
            });
        }
    }
}

/// `FLIGHT_SIM_DEV_GRAPHICS_AT=<secs>:<auto|low|high>` switches GRAPHICS mid-run
/// (natively), to check the live switch in a screenshot run.
#[cfg(not(target_arch = "wasm32"))]
fn dev_switch() -> Option<(f32, Graphics)> {
    let s = std::env::var("FLIGHT_SIM_DEV_GRAPHICS_AT").ok()?;
    let (secs, g) = s.split_once(':')?;
    Some((secs.parse().ok()?, Graphics::parse(g)?))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Graphics {
    #[default]
    Auto,
    Low,
    High,
}

impl Graphics {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Low, Self::High];

    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Low => "low",
            Self::High => "high",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|g| g.name().eq_ignore_ascii_case(s.trim()))
    }
}

#[derive(Resource, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Settings {
    pub graphics: Graphics,
    pub difficulty: Difficulty,
    /// The tutorial has been finished once (the menu stops highlighting it).
    pub tutorial_done: bool,
}

/// The graphics setting the running game uses (on the web, the one the page
/// loaded with: a different pick waits for APPLY).
#[derive(Resource, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Applied(pub Graphics);

const GRAPHICS_KEY: &str = "flight_sim_graphics";
const DIFFICULTY_KEY: &str = "flight_sim_difficulty";
const TUTORIAL_KEY: &str = "flight_sim_tutorial";

impl Settings {
    pub fn load() -> Self {
        let pick = |env: &str, key: &str| crate::environment::setting(env).or_else(|| store::get(key));
        Self {
            graphics: pick("FLIGHT_SIM_GRAPHICS", GRAPHICS_KEY).and_then(|s| Graphics::parse(&s)).unwrap_or_default(),
            difficulty: pick("FLIGHT_SIM_DIFFICULTY", DIFFICULTY_KEY).and_then(|s| Difficulty::parse(&s)).unwrap_or_default(),
            tutorial_done: store::get(TUTORIAL_KEY).is_some_and(|v| v == "done"),
        }
    }
}

/// Saves and applies a graphics pick: live natively; on the web it reloads the page.
pub fn apply_graphics(settings: &Settings, applied: &mut Applied) {
    store::set(GRAPHICS_KEY, settings.graphics.name());
    applied.0 = settings.graphics;
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;
        let _ = (|| {
            let location = js_sys::Reflect::get(&js_sys::global(), &"location".into()).ok()?;
            let reload: js_sys::Function = js_sys::Reflect::get(&location, &"reload".into()).ok()?.dyn_into().ok()?;
            reload.call0(&location).ok()
        })();
    }
    #[cfg(not(target_arch = "wasm32"))]
    crate::platform::set_graphics(settings.graphics);
}

pub fn save_difficulty(difficulty: Difficulty) {
    store::set(DIFFICULTY_KEY, difficulty.name());
}

/// Remembers that the tutorial was finished.
pub fn save_tutorial_done() {
    store::set(TUTORIAL_KEY, "done");
}

/// The stored graphics pick (natively: what `platform::low_memory` starts from).
#[cfg(not(target_arch = "wasm32"))]
pub fn stored_graphics() -> Graphics {
    Settings::load().graphics
}

#[cfg(target_arch = "wasm32")]
mod store {
    fn local_storage() -> Option<(wasm_bindgen::JsValue, impl Fn(&str) -> Option<js_sys::Function>)> {
        use wasm_bindgen::JsCast;
        let ls = js_sys::Reflect::get(&js_sys::global(), &"localStorage".into()).ok().filter(|v| v.is_object())?;
        let ls2 = ls.clone();
        Some((ls, move |name: &str| js_sys::Reflect::get(&ls2, &name.into()).ok()?.dyn_into().ok()))
    }

    pub fn get(key: &str) -> Option<String> {
        let (ls, method) = local_storage()?;
        method("getItem")?.call1(&ls, &key.into()).ok()?.as_string()
    }

    pub fn set(key: &str, value: &str) {
        let _ = (|| {
            let (ls, method) = local_storage()?;
            method("setItem")?.call2(&ls, &key.into(), &value.into()).ok()
        })();
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod store {
    use bevy::log::warn;
    use std::path::PathBuf;

    /// `<data dir>/flight_sim/settings` (screenshot runs don't keep one).
    fn path() -> Option<PathBuf> {
        if crate::dev::headless() {
            return None;
        }
        Some(crate::platform::data_dir()?.join("settings"))
    }

    fn read() -> Vec<(String, String)> {
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else { return Vec::new() };
        text.lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect()
    }

    pub fn get(key: &str) -> Option<String> {
        read().into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn set(key: &str, value: &str) {
        let Some(path) = path() else { return };
        let mut entries = read();
        entries.retain(|(k, _)| k != key);
        entries.push((key.to_string(), value.to_string()));
        let text: String = entries.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, text) {
            warn!("couldn't save settings to {}: {e}", path.display());
        }
    }
}
