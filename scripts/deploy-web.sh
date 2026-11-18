#!/usr/bin/env bash
# Build and deploy the web client.
#   ./scripts/deploy-web.sh staging   -> https://staging.flight.mozzius.dev (Vercel project flight-sim-staging)
#   ./scripts/deploy-web.sh prod      -> https://flight.mozzius.dev         (Vercel project flight-sim)
# Both talk to the same game server (wss://mp.flight.mozzius.dev) unless you add ?server=...
set -euo pipefail
cd "$(dirname "$0")/.."
case "${1:-}" in
  staging) host=staging.flight.mozzius.dev; project=flight-sim-staging ;;
  prod) host=flight.mozzius.dev; project=flight-sim ;;
  *) echo "usage: $0 staging|prod" >&2; exit 1 ;;
esac
# the jets come from the private assets repo; without them the site would fly the F-22
if [ ! -f assets/models/jets/f16.bin ]; then
  echo "assets/models/jets is missing: run ./scripts/fetch-assets.sh first" >&2
  exit 1
fi
SITE_HOST="$host" ./scripts/build-web.sh
vercel deploy dist --prod --yes --scope samuel-newmans-projects --project "$project"
echo "deployed to https://$host"
