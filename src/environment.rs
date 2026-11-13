//! Time of day and weather. Each round rolls one of each from the map seed
//! (like the terrain), so every map looks different; [`Environment::look`]
//! turns them into the numbers the rest of the game uses: the sun's direction,
//! colour and illuminance, exposure, distance fog, valley mist, the cloud
//! layer, the atmosphere's aerosols and the fallback sky's gradient.
//!
//! The biome (`Biome`: alpine, tundra, farmland) is rolled first and biases
//! the weather and time dice.
//!
//! Overrides: `FLIGHT_SIM_BIOME=alpine|tundra|farmland`, `FLIGHT_SIM_WEATHER=clear|broken|overcast|hazy` and
//! `FLIGHT_SIM_TIME=dawn|morning|midday|afternoon|golden` natively, and
//! `?biome=` / `?weather=` / `?time=` on the web (the page sets `window.FLIGHT_SIM_BIOME`,
//! `FLIGHT_SIM_WEATHER` and `FLIGHT_SIM_TIME`). Screenshot runs default to alpine, clear midday so they stay
//! comparable.
//!
//! The mode menu's pickers fill in a [`Picks`] (each one "random" or a fixed
//! value; the overrides above preselect them). The chosen environment is
//! inserted as a resource before each map bake, and the sun, sky, camera and
//! clouds follow it whenever it changes.

use bevy::{light::light_consts::lux, prelude::*};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimeOfDay {
    Dawn,
    Morning,
    Midday,
    Afternoon,
    /// Golden hour, the sun a few degrees above the hills.
    Golden,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Weather {
    Clear,
    /// More cumulus, bigger banks with gaps.
    Broken,
    /// A low, dense deck: flat grey light and thick valley mist.
    Overcast,
    /// Warm, low-contrast haze; few clouds.
    Hazy,
}

/// The map's landscape (terrain palette, tree cover, height shaping: see
/// `terrain_gen::biome`). Rolled from the seed like the weather, and it biases
/// the weather and time dice.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Biome {
    /// Forested valleys, alpine meadows, rock and snow-capped peaks (the original look).
    #[default]
    Alpine,
    /// Winter: a low snow line, sparse dark conifers, pale rock.
    Tundra,
    /// Gentler hills, a patchwork of fields and hedgerows on the valley floors.
    Farmland,
}

impl Biome {
    pub const ALL: [Self; 3] = [Self::Alpine, Self::Tundra, Self::Farmland];

    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "alpine" | "mountains" | "default" => Self::Alpine,
            "tundra" | "winter" | "snow" => Self::Tundra,
            "farmland" | "farm" | "fields" => Self::Farmland,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Alpine => "alpine",
            Self::Tundra => "tundra",
            Self::Farmland => "farmland",
        }
    }
}

impl TimeOfDay {
    pub const ALL: [Self; 5] = [Self::Dawn, Self::Morning, Self::Midday, Self::Afternoon, Self::Golden];

    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "dawn" | "sunrise" => Self::Dawn,
            "morning" => Self::Morning,
            "midday" | "noon" | "day" => Self::Midday,
            "afternoon" => Self::Afternoon,
            "golden" | "goldenhour" | "golden-hour" | "evening" | "sunset" => Self::Golden,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Dawn => "dawn",
            Self::Morning => "morning",
            Self::Midday => "midday",
            Self::Afternoon => "afternoon",
            Self::Golden => "golden",
        }
    }

    /// Sun elevation above the horizon, radians.
    fn elevation(self) -> f32 {
        match self {
            Self::Dawn => 6f32,
            Self::Morning => 20.0,
            // the game's original sun, (4, 10, 10)
            Self::Midday => MIDDAY_ELEVATION_DEG,
            Self::Afternoon => 28.0,
            Self::Golden => 10.0,
        }
        .to_radians()
    }
}

impl Weather {
    pub const ALL: [Self; 4] = [Self::Clear, Self::Broken, Self::Overcast, Self::Hazy];

    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "clear" | "sunny" => Self::Clear,
            "broken" | "cloudy" => Self::Broken,
            "overcast" | "grey" | "gray" => Self::Overcast,
            "hazy" | "haze" => Self::Hazy,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Clear => "clear",
            Self::Broken => "broken",
            Self::Overcast => "overcast",
            Self::Hazy => "hazy",
        }
    }
}

/// What the player picked for the round: `None` is "random" (rolled from the
/// seed; screenshot runs use alpine, clear midday instead).
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Picks {
    pub biome: Option<Biome>,
    pub time: Option<TimeOfDay>,
    pub weather: Option<Weather>,
}

impl Picks {
    /// The `FLIGHT_SIM_BIOME` / `_WEATHER` / `_TIME` overrides (or `?biome=` etc.).
    pub fn from_settings() -> Self {
        fn read<T>(name: &str, parse: fn(&str) -> Option<T>) -> Option<T> {
            let s = setting(name)?;
            let v = parse(&s);
            if v.is_none() {
                warn!("unknown {name} {s:?}");
            }
            v
        }
        Self {
            biome: read("FLIGHT_SIM_BIOME", Biome::parse),
            time: read("FLIGHT_SIM_TIME", TimeOfDay::parse),
            weather: read("FLIGHT_SIM_WEATHER", Weather::parse),
        }
    }
}

/// The round's biome, time of day and weather. Inserted before each bake (`loading::begin_round`).
#[derive(Resource, Clone, Copy, Debug)]
pub struct Environment {
    pub biome: Biome,
    pub time: TimeOfDay,
    pub weather: Weather,
    /// Compass bearing of the sun, radians from +Z (behind the spawn, which
    /// faces -Z) towards +X.
    pub sun_azimuth: f32,
}

/// The original sun direction, (4, 10, 10): ~42.9° up, ~21.8° off +Z.
const MIDDAY_ELEVATION_DEG: f32 = 42.876;
const DEFAULT_AZIMUTH: f32 = 0.3805;

impl Default for Environment {
    /// Alpine, clear midday (what screenshot runs fly; also the scene behind the menu).
    fn default() -> Self {
        Self { biome: Biome::Alpine, time: TimeOfDay::Midday, weather: Weather::Clear, sun_azimuth: DEFAULT_AZIMUTH }
    }
}

impl Environment {
    /// The round's environment: the picks, the rest rolled from the map seed
    /// (screenshot runs default to alpine, clear midday).
    pub fn choose(seed: u32, picks: Picks) -> Self {
        let headless = crate::dev::headless();
        let fixed = Self::default();
        let biome = picks.biome.unwrap_or(if headless { fixed.biome } else { Self::roll(seed, None).biome });
        // the weather dice depend on the biome (picked or rolled)
        let rolled = Self::roll(seed, Some(biome));
        let time = picks.time.unwrap_or(if headless { fixed.time } else { rolled.time });
        let weather = picks.weather.unwrap_or(if headless { fixed.weather } else { rolled.weather });
        let sun_azimuth = if headless { DEFAULT_AZIMUTH } else { rolled.sun_azimuth };
        let env = Self { biome, time, weather, sun_azimuth };
        info!("environment: {:?} {:?} {:?}, sun bearing {:.0}°", env.biome, env.time, env.weather, env.sun_azimuth.to_degrees());
        env
    }

    /// Weighted dice from the seed (hashed, so it's independent of the terrain).
    /// `biome` replaces the rolled biome (it still biases the weather and time).
    fn roll(seed: u32, biome: Option<Biome>) -> Self {
        let mut state = seed ^ 0x9e37_79b9;
        let mut next = || {
            // splitmix32
            state = state.wrapping_add(0x9e37_79b9);
            let mut z = state;
            z = (z ^ (z >> 16)).wrapping_mul(0x21f0_aaad);
            z = (z ^ (z >> 15)).wrapping_mul(0x735a_2d97);
            (z ^ (z >> 15)) as f32 / u32::MAX as f32
        };
        let pick = |u: f32, weights: &[f32]| {
            let total: f32 = weights.iter().sum();
            let mut acc = 0.0;
            weights
                .iter()
                .position(|w| {
                    acc += w / total;
                    u < acc
                })
                .unwrap_or(weights.len() - 1)
        };
        // alpine, tundra, farmland
        let rolled_biome = Biome::ALL[pick(next(), &[4.0, 3.0, 3.0])];
        let biome = biome.unwrap_or(rolled_biome);
        let (times, weathers): ([f32; 5], [f32; 4]) = match biome {
            // (dawn, morning, midday, afternoon, golden), (clear, broken, overcast, hazy)
            Biome::Alpine => ([1.0, 2.0, 2.5, 2.5, 2.0], [3.5, 3.0, 1.5, 2.0]),
            // a low winter sun, crisp air: never hazy, often grey
            Biome::Tundra => ([2.0, 2.5, 1.0, 2.0, 2.5], [3.5, 3.0, 2.5, 0.0]),
            // summer: warm haze and long evenings
            Biome::Farmland => ([1.0, 2.0, 2.5, 2.5, 2.5], [3.0, 3.0, 1.0, 2.5]),
        };
        let time = TimeOfDay::ALL[pick(next(), &times)];
        let weather = Weather::ALL[pick(next(), &weathers)];
        // keep the sun off the nose of the spawn heading (-Z): behind or to
        // either side, so the opening fight isn't flown into the glare
        let side = if next() < 0.5 { -1.0 } else { 1.0 };
        let sun_azimuth = side * (0.25 + next() * 1.6);
        Self { biome, time, weather, sun_azimuth }
    }

    /// Unit vector towards the sun.
    pub fn sun_dir(&self) -> Vec3 {
        let el = self.time.elevation();
        Vec3::new(self.sun_azimuth.sin() * el.cos(), el.sin(), self.sun_azimuth.cos() * el.cos())
    }

    pub fn look(&self) -> Look {
        let el = self.time.elevation();
        let w = self.weather;
        let hazy = w == Weather::Hazy;
        let overcast = w == Weather::Overcast;

        // Sunlight through the air relative to the original midday sun: what
        // the atmosphere does to the light on the GPU, approximated for the
        // fallback sky and the clouds (which don't get the atmosphere's
        // transmittance).
        let haze = match w {
            Weather::Hazy => 1.5,
            Weather::Overcast => 1.0,
            _ => 0.0,
        };
        let sun_rel = transmittance(el, haze) / transmittance(MIDDAY_ELEVATION_DEG.to_radians(), 0.0);
        // what's left of the direct sun after the clouds / haze
        let sun_weather = match w {
            Weather::Clear => 1.0,
            Weather::Broken => 0.92,
            Weather::Overcast => 0.2,
            Weather::Hazy => 0.75,
        };
        // overall sky brightness (ambient), relative to midday
        let sky_light = (el.sin() / MIDDAY_ELEVATION_DEG.to_radians().sin()).clamp(0.0, 1.2).powf(0.5).max(0.3);
        // how warm the low sun makes everything (0 = midday white)
        let warmth = match self.time {
            TimeOfDay::Dawn => 0.75,
            TimeOfDay::Golden => 1.0,
            TimeOfDay::Morning => 0.3,
            TimeOfDay::Afternoon => 0.18,
            TimeOfDay::Midday => 0.0,
        } * if overcast { 0.35 } else { 1.0 };
        let warm_tint = match self.time {
            // dawn: rosier and cooler than the evening's gold
            TimeOfDay::Dawn => Vec3::new(1.0, 0.8, 0.78),
            _ => Vec3::new(1.0, 0.8, 0.6),
        };
        let tint = Vec3::ONE.lerp(warm_tint, warmth);
        // brightness of fog / mist / fallback sky (they're mixed in after
        // exposure, so they have to follow the scene's brightness by hand)
        let display = match self.time {
            TimeOfDay::Dawn => 0.6,
            TimeOfDay::Golden => 0.7,
            TimeOfDay::Morning => 0.88,
            TimeOfDay::Afternoon => 0.95,
            TimeOfDay::Midday => 1.0,
        } * match w {
            Weather::Overcast => 0.95,
            _ => 1.0,
        };

        let exposure = match self.time {
            TimeOfDay::Dawn => 14.0,
            TimeOfDay::Golden => 14.3,
            TimeOfDay::Morning => 14.75,
            TimeOfDay::Afternoon => 14.8,
            TimeOfDay::Midday => 15.0,
        } + match w {
            Weather::Clear => 0.0,
            Weather::Broken => -0.1,
            Weather::Overcast => -1.8,
            Weather::Hazy => -0.25,
        };

        // ---- distance fog (colours are post-exposure)
        let (visibility, extinction, inscatter, glare) = match w {
            Weather::Clear => (55_000.0, Vec3::new(0.4, 0.5, 0.62), Vec3::new(0.7, 0.77, 0.86), 0.6),
            Weather::Broken => (42_000.0, Vec3::new(0.42, 0.5, 0.6), Vec3::new(0.68, 0.74, 0.82), 0.5),
            Weather::Overcast => (15_000.0, Vec3::new(0.5, 0.51, 0.53), Vec3::new(0.66, 0.68, 0.71), 0.05),
            Weather::Hazy => (17_000.0, Vec3::new(0.56, 0.56, 0.57), Vec3::new(0.84, 0.83, 0.8), 0.75),
        };
        let fog_tint = tint * display;
        let sun_hue = sun_rel / sun_rel.max_element();

        // ---- valley mist
        let mist_density = match self.biome {
            // the plains sit lower, deeper in the mist layer
            Biome::Farmland => 0.55,
            _ => 1.0,
        } * match w {
            Weather::Clear => 1.0,
            Weather::Broken => 1.2,
            Weather::Overcast => 2.4,
            Weather::Hazy => 1.6,
        } * match self.time {
            // morning mist in the valleys
            TimeOfDay::Dawn => 2.2,
            TimeOfDay::Morning => 1.5,
            _ => 1.0,
        };
        let mist_base = match w {
            Weather::Overcast => Vec3::new(0.6, 0.62, 0.65),
            Weather::Hazy => Vec3::new(0.76, 0.75, 0.72),
            _ => Vec3::new(0.62, 0.68, 0.76),
        } * match self.biome {
            // cold, blue-white mist over the snow
            Biome::Tundra => Vec3::new(1.0, 1.04, 1.1),
            _ => Vec3::ONE,
        };

        // ---- fallback sky gradient (display colours)
        let (horizon, zenith, below) = match w {
            Weather::Clear | Weather::Broken => {
                (Vec3::new(0.72, 0.8, 0.9), Vec3::new(0.2, 0.38, 0.7), Vec3::new(0.45, 0.52, 0.6))
            }
            Weather::Overcast => {
                (Vec3::new(0.66, 0.68, 0.71), Vec3::new(0.5, 0.53, 0.57), Vec3::new(0.5, 0.52, 0.55))
            }
            Weather::Hazy => (Vec3::new(0.86, 0.81, 0.72), Vec3::new(0.38, 0.5, 0.68), Vec3::new(0.6, 0.58, 0.54)),
        };
        // low sun: a warm horizon, a deeper zenith and a glow around the sun
        let low = (1.0 - (el.to_degrees() - 5.0) / 25.0).clamp(0.0, 1.0) * if overcast { 0.3 } else { 1.0 };
        // (the dome is unlit, so exposure doesn't brighten it: dim it less than the fog)
        let sky_scale = 0.45 + 0.55 * display;
        let sky_horizon = horizon.lerp(horizon * warm_tint * 1.05, low) * sky_scale;
        // (a greyer, greener blue overhead rather than a purple blend)
        let sky_zenith = zenith.lerp(Vec3::new(0.27, 0.4, 0.56), low) * sky_scale;
        let sky_below = below * tint * sky_scale;
        let sky_glow = sun_hue * if overcast { 0.08 } else { 0.25 + 0.5 * low } * sky_scale;

        // ---- clouds
        let ambient_col = match w {
            Weather::Overcast => Vec3::new(0.78, 0.8, 0.84),
            Weather::Hazy => Vec3::new(0.82, 0.8, 0.78),
            _ => Vec3::new(0.7, 0.8, 1.0),
        } * tint.lerp(Vec3::ONE, 0.5);
        let cloud = CloudLook {
            coverage_bias: match w {
                Weather::Clear => 0.0,
                Weather::Broken => 0.2,
                Weather::Overcast => 0.62,
                Weather::Hazy => -0.18,
            },
            layer: overcast.then_some((2_150.0, 3_300.0)),
            extinction_mult: if overcast { 1.2 } else { 1.0 },
            // a deck carries on to the horizon; in haze clouds dissolve sooner
            fade_distance: match w {
                Weather::Overcast => 45_000.0,
                Weather::Hazy => 12_000.0,
                _ => 22_000.0,
            },
            ambient_top: ambient_col * 6_500.0 * sky_light * if overcast { 0.75 } else { 1.0 },
            ambient_bottom: ambient_col * 6_500.0 * sky_light * if overcast { 0.62 } else { 1.0 },
            // the light reaching the layer: the low sun's colour, but not the
            // weather's dimming (that happens below the clouds), so undo it
            sun_tint: Vec3::new(1.0, 0.98, 0.94) * sun_rel / sun_weather,
            // the fallback sky's light is already coloured (see `world::setup`)
            fallback_sun_tint: Vec3::new(1.0, 0.98, 0.94) / sun_weather,
        };

        Look {
            sun_dir: self.sun_dir(),
            sun_illuminance: lux::RAW_SUNLIGHT * sun_weather,
            fallback_sun: sun_rel * sun_weather,
            exposure,
            // the fallback sky has no environment light, so a low sun leaves
            // the shadowed slopes darker: open up a little more
            fallback_exposure: exposure - 0.7 * low,
            fog_visibility: visibility,
            fog_extinction: extinction * fog_tint,
            fog_inscatter: inscatter * fog_tint,
            fog_base: (extinction + inscatter) * 0.5 * fog_tint,
            fog_glare: sun_hue.extend(glare),
            mist: (mist_base * tint * display).extend(mist_density),
            sky_horizon,
            sky_zenith,
            sky_below,
            sky_glow,
            ambient_color: (match w {
                Weather::Overcast => Vec3::new(0.85, 0.87, 0.9),
                Weather::Hazy => Vec3::new(0.9, 0.86, 0.8),
                _ => Vec3::new(0.7, 0.8, 1.0),
            } * tint)
                .clamp(Vec3::ZERO, Vec3::ONE),
            // (the fallback has no sky light from the atmosphere, so keep a floor)
            ambient_brightness: 12_000.0 * sky_light.max(0.85) * if overcast { 0.7 } else { 1.0 },
            env_light: if overcast { 0.85 } else { 1.0 },
            mie: match w {
                Weather::Hazy => 30.0,
                Weather::Overcast => 20.0,
                Weather::Broken => 2.0,
                Weather::Clear => 1.0,
            },
            cloud,
            hazy,
        }
    }
}

/// The derived numbers. See where each is used.
#[derive(Clone, Debug)]
pub struct Look {
    pub sun_dir: Vec3,
    /// Directional light illuminance with the atmosphere (which dims and
    /// colours it by elevation itself).
    pub sun_illuminance: f32,
    /// Sunlight colour × strength relative to `RAW_SUNLIGHT`, for the fallback sky.
    pub fallback_sun: Vec3,
    pub exposure: f32,
    /// `exposure` for the fallback sky.
    pub fallback_exposure: f32,
    pub fog_visibility: f32,
    pub fog_extinction: Vec3,
    pub fog_inscatter: Vec3,
    pub fog_base: Vec3,
    /// rgb: sun glare colour in the fog, a: strength
    pub fog_glare: Vec4,
    /// rgb: valley mist colour (post-exposure), a: density multiplier
    pub mist: Vec4,
    pub sky_horizon: Vec3,
    pub sky_zenith: Vec3,
    pub sky_below: Vec3,
    pub sky_glow: Vec3,
    pub ambient_color: Vec3,
    pub ambient_brightness: f32,
    /// `AtmosphereEnvironmentMapLight::intensity`
    pub env_light: f32,
    /// Multiplier on the atmosphere's aerosol (Mie) scattering.
    pub mie: f32,
    pub cloud: CloudLook,
    pub hazy: bool,
}

#[derive(Clone, Debug)]
pub struct CloudLook {
    pub coverage_bias: f32,
    /// (bottom, top) altitude override for the layer.
    pub layer: Option<(f32, f32)>,
    pub extinction_mult: f32,
    pub fade_distance: f32,
    pub ambient_top: Vec3,
    pub ambient_bottom: Vec3,
    /// `CloudSettings::sun_tint` with the atmosphere / with the fallback sky.
    pub sun_tint: Vec3,
    pub fallback_sun_tint: Vec3,
}

impl Look {
    pub fn distance_fog(&self) -> bevy::pbr::DistanceFog {
        // fog colours are written as sRGB, like the original
        let s = |v: Vec3| Color::srgb(v.x, v.y, v.z);
        bevy::pbr::DistanceFog {
            color: s(self.fog_base),
            directional_light_color: Color::srgba(self.fog_glare.x, self.fog_glare.y, self.fog_glare.z, self.fog_glare.w),
            directional_light_exponent: if self.hazy { 10.0 } else { 24.0 },
            falloff: bevy::pbr::FogFalloff::from_visibility_colors(
                self.fog_visibility,
                s(self.fog_extinction),
                s(self.fog_inscatter),
            ),
        }
    }

    /// The atmosphere's medium: Earth's, with more aerosol in haze and under cloud.
    pub fn scattering_medium(&self) -> bevy::light::atmosphere::ScatteringMedium {
        let mut medium = bevy::light::atmosphere::ScatteringMedium::default();
        if self.mie != 1.0
            && let Some(mie) = medium.terms.get_mut(1)
        {
            mie.scattering *= self.mie;
            // whiter, not darker: keep the absorption down
            mie.absorption *= self.mie.sqrt() * 0.5;
        }
        medium
    }
}

/// Approximate sunlight transmittance through the air at elevation `el`
/// (Rayleigh + some aerosol; `haze` adds grey aerosol).
fn transmittance(el: f32, haze: f32) -> Vec3 {
    let deg = el.to_degrees().max(0.0);
    // Kasten & Young air mass
    let air_mass = 1.0 / (el.sin().max(0.0) + 0.50572 * (deg + 6.07995).powf(-1.6364));
    let tau = Vec3::new(0.05, 0.11, 0.27) + Vec3::splat(0.08 * (1.0 + haze));
    Vec3::new((-tau.x * air_mass).exp(), (-tau.y * air_mass).exp(), (-tau.z * air_mass).exp())
}

/// A string setting: an env var natively, a page global on the web.
pub fn setting(name: &str) -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Reflect::get(&js_sys::global(), &name.into())
            .ok()
            .and_then(|v| v.as_string())
            .filter(|s| !s.is_empty())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::env::var(name).ok().filter(|s| !s.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midday_matches_the_original_sun() {
        let env = Environment { biome: Biome::Alpine, time: TimeOfDay::Midday, weather: Weather::Clear, sun_azimuth: DEFAULT_AZIMUTH };
        let original = Vec3::new(4.0, 10.0, 10.0).normalize();
        assert!(env.sun_dir().distance(original) < 1e-3, "{:?}", env.sun_dir());
        let look = env.look();
        assert!((look.fallback_sun - Vec3::ONE).abs().max_element() < 1e-3);
        assert_eq!(look.exposure, 15.0);
    }

    #[test]
    fn rolls_cover_every_preset() {
        let rolls: Vec<_> = (0..400).map(|s| Environment::roll(s, None)).collect();
        for b in Biome::ALL {
            let n = rolls.iter().filter(|e| e.biome == b).count();
            assert!(n > 80, "{b:?} rolled only {n} times in 400");
        }
        assert!(!rolls.iter().any(|e| e.biome == Biome::Tundra && e.weather == Weather::Hazy));
        for t in TimeOfDay::ALL {
            assert!(rolls.iter().any(|e| e.time == t), "{t:?} never rolled");
        }
        for w in Weather::ALL {
            assert!(rolls.iter().any(|e| e.weather == w), "{w:?} never rolled");
        }
    }
}
