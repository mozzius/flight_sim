#!/usr/bin/env bash
# Builds the web version into dist/ (wasm + JS glue + assets + index.html).
# Needs: rustup target add wasm32-unknown-unknown
#        cargo install wasm-bindgen-cli --version <same as wasm-bindgen in Cargo.lock>
# Optional: wasm-opt (binaryen) for a smaller download.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --target wasm32-unknown-unknown --profile web
rm -rf dist && mkdir -p dist
wasm-bindgen --target web --no-typescript --remove-name-section --remove-producers-section --out-dir dist \
  target/wasm32-unknown-unknown/web/flight_sim.wasm
if command -v wasm-opt >/dev/null; then
  wasm-opt -Os --all-features dist/flight_sim_bg.wasm -o dist/flight_sim_bg.wasm
fi
cp -R assets dist/assets
# the built-in map, pre-baked so the browser doesn't have to generate it
mkdir -p dist/assets/maps
FLIGHT_SIM_BAKE_MAP="$PWD/dist/assets/maps/builtin.map" cargo run --release
cp web/vercel.json dist/
# stamp the build so a stale cached page is easy to spot
BUILD="$(git rev-parse --short HEAD)$(git diff --quiet || echo +)-$(date +%H%M)"
sed "s/__BUILD__/$BUILD/g" web/index.html > dist/index.html
du -sh dist/flight_sim_bg.wasm dist
