#!/usr/bin/env bash
# Fetches the jets (the converted Modern Fighter Jets pack) from the private
# assets repo into assets/models/jets/ (gitignored). The pack's licence allows
# shipping it inside the game but not publishing it on its own, so it isn't in
# this public repo; without it the game flies the old F-22.
#   ./scripts/fetch-assets.sh          clone, or pull if it's already there
# ASSETS_REPO overrides the remote (default: the private GitHub repo over ssh).
# To rebuild the jets, run scripts/jets/build.py *into* this checkout (don't
# delete the folder: it's a git clone), then commit and push from inside it.
set -euo pipefail
cd "$(dirname "$0")/.."
repo="${ASSETS_REPO:-git@github.com:mozzius/flight_sim_assets.git}"
dir=assets/models/jets
if [ -d "$dir/.git" ]; then
  git -C "$dir" pull --ff-only
elif [ -e "$dir" ]; then
  echo "$dir exists but isn't a clone of the assets repo; move it aside first" >&2
  exit 1
else
  git clone --depth 1 "$repo" "$dir"
fi
