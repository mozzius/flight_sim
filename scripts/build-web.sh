#!/usr/bin/env bash
# Builds the web version into dist/ (wasm + JS glue + assets + index.html).
# Needs: rustup target add wasm32-unknown-unknown
#        cargo install wasm-bindgen-cli --version <same as wasm-bindgen in Cargo.lock>
# Optional: wasm-opt (binaryen) for a smaller download.
set -euo pipefail
cd "$(dirname "$0")/.."
# SITE_HOST: the host this build is served from (staging.flight.mozzius.dev for staging)
SITE_HOST="${SITE_HOST:-flight.mozzius.dev}"

cargo build --target wasm32-unknown-unknown --profile web
rm -rf dist && mkdir -p dist
wasm-bindgen --target web --no-typescript --remove-name-section --remove-producers-section --out-dir dist \
  target/wasm32-unknown-unknown/web/flight_sim.wasm
if command -v wasm-opt >/dev/null; then
  wasm-opt -Os --all-features dist/flight_sim_bg.wasm -o dist/flight_sim_bg.wasm
fi
cp -R assets dist/assets
cp web/vercel.json web/manifest.json web/auth.js web/oauth-client-metadata.json web/oauth-native-client-metadata.json dist/
cp -R web/.well-known dist/.well-known
# the web OAuth client is per host: its client_id and redirect URI must match where it is served
sed -i.bak "s#https://flight.mozzius.dev#https://$SITE_HOST#g" dist/oauth-client-metadata.json && rm dist/oauth-client-metadata.json.bak
# stamp the build so a stale cached page is easy to spot
BUILD="$(git rev-parse --short HEAD)$(git diff --quiet || echo +)-$(date +%H%M)"
sed "s/__BUILD__/$BUILD/g" web/index.html > dist/index.html
if [ "$SITE_HOST" != "flight.mozzius.dev" ]; then
  # keep staging out of the prod analytics
  sed -i.bak "/plausible.mozzius.dev/d" dist/index.html && rm dist/index.html.bak
fi
du -sh dist/flight_sim_bg.wasm dist
