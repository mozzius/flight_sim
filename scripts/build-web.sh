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
cp web/index.html web/vercel.json dist/
du -sh dist/flight_sim_bg.wasm dist
