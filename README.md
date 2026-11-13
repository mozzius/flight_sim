# Flight Sim

Built using [Bevy](https://bevy.org) 0.20.

Run the game using `cargo run`

## Modes

- **Endless**: waves forever, respawn when shot down. Pick the location, weather and
  time first (each defaults to random).
- **Challenge**: a random map, one life, bigger and sharper waves. Kills and cleared
  waves score points; the best score is kept (`localStorage` on the web, a file in the
  OS data dir natively).

Esc → MENU goes back to the mode menu; every round bakes a fresh map.

The world is procedurally generated eroded mountains (Rune Skovbo Johansen's
erosion filter), and all sound is synthesised at startup.

## Controls

Two control modes, switch with `M` (gamepad: Select):

- **Mouse aim** (default): the mouse trims pitch and rudder toward the aim
  circle; roll with A/D. Left click = gun, right click = missile.
- **Direct**: keyboard / gamepad sticks fly the plane.


- W/S or ↑/↓: pitch
- A/D or ←/→: roll
- Q/E: yaw
- Shift / Ctrl: throttle up / down (hold at the end for afterburner / airbrakes)
- Space or left mouse: gun · F or right mouse: missile (lock on first) · X: flares
- Tab / Shift+Tab: next / previous target · R or middle mouse: target nearest the crosshair
- C: look back · N: mute · Esc: release the mouse

Gamepad: left stick pitch/roll, LB/RB yaw, RT/LT throttle, A gun, B missile,
X flares, D-pad left/right cycle targets, Y or D-pad up target nearest the
crosshair, right stick look around.

Guns: put the boresight cross on the red lead pipper; it turns white with
SHOOT when you have a firing solution.

Every round flies a freshly generated map (mountains, roads and forests) in one of four
biomes (alpine valleys; winter tundra with a low snow line, sparse snowy conifers and
frozen lakes; gentle farmland with a patchwork of fields and hedgerows on the valley
floors; an archipelago of beaches, sea cliffs and green islands, about half the map sea)
under a time of day (dawn, morning, midday, afternoon, golden hour) and weather (clear,
broken cloud, overcast, hazy) rolled from the same seed (the biome tilts the weather:
tundra is never hazy, farmland gets more summer haze, the islands more golden hours). The
deepest valley basins hold lakes; water is coloured by its depth, reflects the sky, glints
in the sun and counts as ground (you crash into it). The seed is in the pause menu and
`FLIGHT_SIM_SEED=<n>` flies it again. The round opens with a short
fly-by shot: any key, click, tap or gamepad A skips it.

On a touchscreen: swipe anywhere to aim and an autopilot flies there (banking into turns,
levelling out once on target); GUN (hold), MSL, FLR and TGT on the left, the throttle slider
on the right edge (top = afterburner, bottom = airbrake), pause top left. Phones and
tablets get a lighter scene (fewer trees, half-res terrain textures).

## Dev

`cargo test` runs headless flight-model tests.

`FLIGHT_SIM_SCREENSHOT=out.png cargo run` saves a screenshot after 5 seconds
(`FLIGHT_SIM_SCREENSHOT_AFTER=<secs>` to change) and exits. Other switches:
`FLIGHT_SIM_LOG=1` (FPS + flight state each second), `FLIGHT_SIM_AUTOPILOT=1`
(the AI flies your jet), `FLIGHT_SIM_NO_HUD=1` (no HUD or player jet), `FLIGHT_SIM_FX_TEST=1` (afterburner + an explosion),
`TERRAIN_DEBUG_CAM=1..4` (terrain viewpoints), `FLIGHT_SIM_FX_TEST=explosion|engines|missile|vapour|...`
(staged effects), `CLOUDS_DEBUG_CAM=above|below|inside|sun|horizon`, `CLOUDS_DIV=2..4` (cloud
resolution divisor: 4 is cheapest, 2 is crispest), `FLIGHT_SIM_NO_CLOUDS=1`, `FLIGHT_SIM_PAUSE_TEST=1`, `FLIGHT_SIM_KILL_TEST=1`,
`FLIGHT_SIM_NO_INTRO=1`, `FLIGHT_SIM_SEED=<n>` (fly that map; screenshot runs default to
seed 1337), `FLIGHT_SIM_LOW_MEMORY=1` (the phone/tablet scene), `FLIGHT_SIM_NO_ATMOSPHERE=1`
(the plain gradient sky), `FLIGHT_SIM_BIOME=alpine|tundra|farmland|archipelago`,
`FLIGHT_SIM_WEATHER=clear|broken|overcast|hazy` and
`FLIGHT_SIM_TIME=dawn|morning|midday|afternoon|golden` (pin the environment; screenshot runs
default to alpine, clear midday), `FLIGHT_SIM_MODE=endless|challenge` (skip the menu;
screenshot runs default to endless), `FLIGHT_SIM_MODE=menu|menu-endless` (show the menu in a
screenshot run), `FLIGHT_SIM_DIE_AT=<secs>` (shoot the player down, for the score screen). Biome parameter sets live in `src/terrain_gen/biome.rs`,
the farmland fields in `src/terrain_gen/fields.rs`, lakes and the sea in `src/terrain_gen/water.rs`
(shader `assets/shaders/water.wesl`).
Screenshot runs are headless (no window) and silent.

## Web build

Live at <https://flight.mozzius.dev> (needs a WebGPU browser). Build and deploy:

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.130  # match wasm-bindgen in Cargo.lock
./scripts/build-web.sh                            # -> dist/
vercel deploy dist --prod --scope samuel-newmans-projects --project flight-sim
```

The web build is single-threaded, so it bakes the terrain textures at half
resolution, a slice per frame under a loading bar, and plants trees a couple of
chunks per frame. `?debug` shows the log and GPU state on the page; `?low` /
`?high` force the phone/tablet scene on or off; `?noatmo` forces the plain sky;
`?biome=`, `?weather=` and `?time=` preselect the environment, `?mode=endless|challenge` skips
the menu (same values as the env vars above).
