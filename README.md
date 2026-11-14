# Flight Sim

Built using [Bevy](https://bevy.org) 0.20.

Run the game using `cargo run`

## Modes

- **Endless**: waves forever, respawn when shot down. Pick the location, weather and
  time first (each defaults to random), and the DIFFICULTY (kept between sessions):
  EASY has smaller waves (3, 4, 5, 6) of mostly rookies, and your missiles pull harder,
  react faster, see wider, mostly ignore flares and lock in half the time; HARD has bigger
  waves (8 up to 16) of mostly aces and slightly weaker missiles. NORMAL is the original
  game (6 up to 12); challenge and multiplayer always play it. The numbers are in
  `src/difficulty.rs`.

**Settings** (main menu): GRAPHICS is AUTO (the lighter scene on phones and tablets on the
web, full elsewhere), LOW or HIGH, kept in `localStorage` / `<data dir>/flight_sim/settings`.
Natively it switches straight away: shadow-map size, cloud resolution and the trees live,
the terrain (near mesh, texture resolution) from the next round. The web reads it before the
game starts (it also caps the canvas at 1.5x DPR instead of 2x), so APPLY saves it and
reloads the page. `?low` / `?high` and `FLIGHT_SIM_LOW_MEMORY=1` override it.
- **Challenge**: a random map, one life, bigger and sharper waves. Kills and cleared
  waves score points; the best score is kept (`localStorage` on the web, a file in the
  OS data dir natively). Signed in, runs also go on the server's leaderboard: the score
  screen shows `BEST <n>  #<rank>` and LEADERBOARD (SIGN IN when signed out), which opens
  GLOBAL / FRIENDS boards (`#  @handle  score  W<wave>`, your row highlighted); the menu
  has it top left too.

- **Multiplayer**: a free-for-all dogfight with respawns, up to 8 players per room. CREATE picks
  the room's location, weather and time (like Endless) and opens the room's lobby: the code,
  who's in (the host marked), FRIENDS with INVITE, START (host only) and LEAVE (Esc / B), with
  the map baking in the background under a thin bar so START is near-instant. JOIN takes the
  4-letter room code (typed, or on the keypad with a gamepad or touch) and lands in the lobby,
  or straight in the game once it's started. If the host leaves, the longest-standing player
  hosts. The code and a kills scoreboard are in the pause menu (which doesn't stop the world).
  Kills read `SPLASH @bob`; the death screen names who got you and your head-to-head record
  against them this room (yours – theirs). Signed out, MULTIPLAYER offers SIGN IN. Errors
  (NO ROOM, FULL, OFFLINE, DECLINED, sign-in) clear after 4 s.

- **Friends**: signed in, the MULTIPLAYER menu and the multiplayer pause menu list your online
  Bluesky mutuals (people you follow who follow you back) with a status dot (green: around,
  amber: in a room, grey: in a full room) and INVITE, or JOIN for a friend in another room.
  JOIN sends a join request to that room's host (the code is never shown): the host gets a
  card, `@handle` with ACCEPT and X (in the lobby or in flight; J / D-pad down accepts in
  flight), and X or 20 s declines (DECLINED). Inviting from the menu makes a room on
  the fly (with the CREATE pickers if they're open, else random) and puts you in its lobby. An invite shows as a card
  wherever you are: JOIN (click, tap, or in menus down to it then A / Enter; in flight J or
  D-pad down) leaves what you're doing, single-player rounds included (no score is saved);
  X or 20 s dismisses it.

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

Flares: you carry four, regaining one every 8 s. Each press drops a pair, and
holding the button keeps them coming. AI pilots sometimes lose their nerve
when you sit locked on their tail and waste theirs before you've fired.

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
tablets get a lighter scene (fewer trees, half-res terrain textures; SETTINGS → GRAPHICS).

## Dev

`cargo test` runs headless flight-model tests.

`FLIGHT_SIM_SCREENSHOT=out.png cargo run` saves a screenshot after 5 seconds
(`FLIGHT_SIM_SCREENSHOT_AFTER=<secs>` to change) and exits;
`FLIGHT_SIM_SCREENSHOT_BURST=<count>,<secs>` saves a numbered sequence instead. Other switches:
`FLIGHT_SIM_LOG=1` (FPS + flight state each second), `FLIGHT_SIM_AUTOPILOT=1`
(the AI flies your jet), `FLIGHT_SIM_NO_HUD=1` (no HUD or player jet), `FLIGHT_SIM_FX_TEST=1` (afterburner + an explosion),
`TERRAIN_DEBUG_CAM=1..4` (terrain viewpoints), `FLIGHT_SIM_FX_TEST=explosion|engines|missile|vapour|...`
(staged effects; `FLIGHT_SIM_CAM=<yaw>,<pitch>,<dist>` parks the camera by the jet, `FLIGHT_SIM_CAM_MISSILE=1` by the newest missile), `FLIGHT_SIM_LOG_MISSILES=1` (how much of each missile's smoke trail fit in the particle budget), `CLOUDS_DEBUG_CAM=above|below|inside|band|sun|horizon` (`CLOUDS_DEBUG_CAM_SPEED=<m/s>` flies it, `CLOUDS_DEBUG_CAM_H=<0..1>` sets the band height), `CLOUDS_DEBUG=1|2` (opacity | march steps), `CLOUDS_DIV=2..4` (cloud
resolution divisor: 4 is cheapest, 2 is crispest), `FLIGHT_SIM_NO_CLOUDS=1`, `FLIGHT_SIM_PAUSE_TEST=1`, `FLIGHT_SIM_KILL_TEST=1`,
`FLIGHT_SIM_NO_INTRO=1`, `FLIGHT_SIM_TEXTURE_DIV=2` (bake the terrain textures at the web's resolution), `FLIGHT_SIM_SEED=<n>` (fly that map; screenshot runs default to
seed 1337), `FLIGHT_SIM_LOW_MEMORY=1` (the phone/tablet scene, whatever GRAPHICS says),
`FLIGHT_SIM_GRAPHICS=auto|low|high` and `FLIGHT_SIM_DIFFICULTY=easy|normal|hard` (pin the
settings for a run without saving them; screenshot runs never read or write the settings file), `FLIGHT_SIM_DEV_GRAPHICS_AT=<secs>:low|high` (switch GRAPHICS mid-run, to check the live switch), `FLIGHT_SIM_NO_ATMOSPHERE=1`
(the plain gradient sky), `FLIGHT_SIM_BIOME=alpine|tundra|farmland|archipelago`,
`FLIGHT_SIM_WEATHER=clear|broken|overcast|hazy` and
`FLIGHT_SIM_TIME=dawn|morning|midday|afternoon|golden` (pin the environment; screenshot runs
default to alpine, clear midday), `FLIGHT_SIM_MODE=endless|challenge` (skip the menu;
screenshot runs default to endless), `FLIGHT_SIM_MODE=menu|menu-endless|menu-settings` (show the menu in a
screenshot run), `FLIGHT_SIM_DIE_AT=<secs>` (shoot the player down, for the score screen). Biome parameter sets live in `src/terrain_gen/biome.rs`,
the farmland fields in `src/terrain_gen/fields.rs` (the terrain shader redraws them per pixel from a small farm-layout texture, so they stay crisp on phones), lakes and the sea in `src/terrain_gen/water.rs`
(shader `assets/shaders/water.wesl`).
Screenshot runs are headless (no window) and silent.

## Multiplayer

The game talks to a small server (`src/server`) over a WebSocket (binary
postcard frames, `src/net/protocol.rs`). Every game flies its own jet and
sends its state 20 times a second; the server relays it and the others draw
it 100 ms in the past, interpolating. Shooters fly their own bullets and
missiles: tracers and flares are relayed as events, missiles as positions (so
the target sees them and can flare), and hits are reported to the server,
which checks them (range, rate of fire, both alive) and decides damage,
kills and score. Each client bakes the room's map from its seed and
environment; the server has no terrain (clients report their own crashes).

The player and their token come from `src/identity.rs` (see Sign-in below):
the client sends a service-auth JWT (`aud=did:web:flight.mozzius.dev`,
`lxm=dev.mozzius.flight.join`) and the server verifies it with
`atproto::verify_service_auth`, naming players by their verified handle.
Native builds sign in too (see Sign-in) and speak `wss://` (tokio-tungstenite with rustls
and bundled webpki roots) as well as `ws://`; `FLIGHT_SIM_DEV_USER=<handle>` still plays as a
dev user, sending a `dev:<handle>` token.

Presence and invites: while signed in the game keeps that one connection open in every mode
(menus and single player too; it just doesn't join a room), and `Leave` takes it out of a room
without hanging up. The server (`src/server/presence.rs`, tied to the rooms in `hub.rs`) keeps
the online DIDs in memory and, when someone comes online, asks the public AppView
(`app.bsky.graph.getRelationships`, 30 others a call; `app.bsky.actor.getProfiles` for display
names) which other online players are mutuals (both follow records, no block either way). It
caches pairs for 5 minutes (a sweep re-checks stale pairs among online players every minute)
and runs at most 5 AppView calls a second. Only mutuals ever hear that you're online; an invite
that fails looks the same whether the target is offline or not a mutual. Invites need an online
mutual, one per friend per 10 s and 10 a minute. Join requests (protocol v3: `RequestJoin` →
the host's `JoinRequest` → `AnswerJoin`, else `JoinDeclined` after 20 s) need the friend and the
room's host to be online mutuals of the requester, one per host per 10 s and 6 a minute, and any
failure is the same decline. MULTIPLAYER offers QUICK PLAY, HOST GAME and JOIN GAME (a code).
Quick play (protocol v5: `QuickPlay { map }`) puts you in a public room that has players and a
seat, preferring rooms with your online mutuals, then the fullest. With none (protocol v6) you
wait in a queue (`Queued`; the menu shows SEARCHING, the online count, CANCEL, and PLAY SOLO
after 30 s, which starts an endless round) while the client bakes the random map it sent. As soon
as a second player queues, the server makes a public room on the first one's map and moves them
in together (up to 8 at once); a waiting player also goes into any public room that gets players
(checked every tick). Players already in rooms are never moved, so the last one left in a public
room keeps flying and the next quick play joins them. `CancelQueue`, `Leave`, going offline, or
joining any room another way (an invite, an accepted join request, a code) takes you out of the
queue. The server also tells every signed-in client how many are online (`Online(n)`: on sign-in,
then on change at most every 5 s), shown as `N ONLINE` under the MULTIPLAYER buttons and on the
queue screen; it's a count only. Public rooms have no host, lobby or visible code, fly from the start (late joiners
drop in) and go 15 s after the last player leaves. A mutual in a public room with a seat shows
JOIN (`FriendStatus::InPublic`), which hops straight in (`JoinFriend { did }`, checked for
mutuals and a seat; any failure is a plain `JoinDeclined`); JOIN on a hosted room (`InRoom`)
is still a request to its host, and a full room shows only its dot. Invites work from either
kind of room; INVITE from the menu makes a hosted room. Hosted rooms start as lobbies (`Joined { host, started }`, `Start` →
`Started`, `Host` on handover); `Killed` carries the pair's head-to-head record, counted per
pair of DIDs for the room's lifetime (a crash credited to an attacker counts). Avatar URLs are sent but not drawn: cdn.bsky.app
sends no CORS header, so the web build can't load them.

Client switches: `FLIGHT_SIM_SERVER=<url>` (`?server=` on the web; default
`wss://mp.flight.mozzius.dev`; `/ws` is appended), `FLIGHT_SIM_MP=quick|create[:CODE]|join:CODE`
(connect straight from the menu; a requested code needs a dev server),
`FLIGHT_SIM_DEV_INVITE=<handle>` (invite that friend as soon as they're online),
`FLIGHT_SIM_DEV_ACCEPT_INVITES=1` (join any invite after 2 s), `FLIGHT_SIM_DEV_START=<n>` (a host
starts its lobby 3 s after n players are in), `FLIGHT_SIM_DEV_REQUEST=<handle>` (press JOIN on that
friend 4 s after they're seen in a room: straight into a public room, a request for a hosted one), `FLIGHT_SIM_DEV_ACCEPT_REQUESTS=1` (let any join
request in after 2 s), `FLIGHT_SIM_DEV_HIT=1` (land gun hits on anyone within range: a quick
kill), `FLIGHT_SIM_SCREENSHOT_EXTRA=<event>:<path>,...` (extra shots without exiting on
`t<secs>`, `lobby<n>`, `room<n>`, `friendjoin`, `request`, `kill`, `death`, `queue`, `online<n>`),
`FLIGHT_SIM_MODE=menu-multiplayer|menu-create|menu-join` (open that panel in a screenshot run).

Run a server locally and two headless clients that fight each other:

```bash
FLIGHT_SIM_DEV_AUTH=1 PORT=8787 cargo run --release --no-default-features --features server --bin flight_sim_server
# in two more shells
FLIGHT_SIM_DEV_USER=alice FLIGHT_SIM_SERVER=ws://127.0.0.1:8787 FLIGHT_SIM_MP=create:TEST FLIGHT_SIM_DEV_START=2 \
  FLIGHT_SIM_AUTOPILOT=1 FLIGHT_SIM_SCREENSHOT=/tmp/a.png FLIGHT_SIM_SCREENSHOT_AFTER=40 cargo run
FLIGHT_SIM_DEV_USER=bob FLIGHT_SIM_SERVER=ws://127.0.0.1:8787 FLIGHT_SIM_MP=join:TEST \
  FLIGHT_SIM_AUTOPILOT=1 FLIGHT_SIM_SCREENSHOT=/tmp/b.png FLIGHT_SIM_SCREENSHOT_AFTER=40 cargo run
```

Server env: `PORT` (default 8080), `FLIGHT_SIM_DEV_AUTH=1` (accept `dev:` tokens and requested
room codes: never in production), `FLIGHT_SIM_DEV_MUTUALS=1` (dev users are all each other's
mutuals; they're never anyone's otherwise), `FLIGHT_SIM_APPVIEW` (default
`https://public.api.bsky.app`), `FLIGHT_SIM_DEV_PROTOCOL=<n>` (a dev server pretends to speak
protocol n, to test version mismatches), `FLIGHT_SIM_DB` (the leaderboard, see below),
`FLIGHT_SIM_METRICS_TOKEN` (turns on `/metrics`, see Metrics below), `RUST_LOG`.

Version mismatches (v6): the server answers a `Hello` with the wrong version with
`ServerVersion(its version)` and then `Error(Version)`. A client older than the server says
"This client is out of date. Update to play multiplayer." in the multiplayer menu, for good
(no fading, no reconnecting; single player is unaffected). A client newer than the server (or
one hearing no `ServerVersion`: servers before v6 don't send it) means the server hasn't
deployed yet: it shows SERVER UPDATING and keeps reconnecting with its usual back-off. Endpoints: `/ws` (the game), `/health` ("ok") and `/metrics`.
`cargo test --no-default-features --features server --bin flight_sim_server` runs the room,
presence and invite tests (against a mock AppView; `real_appview -- --ignored` asks the real one).

Friends and invites locally (alice invites bob from the menu, bob joins):

```bash
FLIGHT_SIM_DEV_AUTH=1 FLIGHT_SIM_DEV_MUTUALS=1 PORT=8787 cargo run --release --no-default-features --features server --bin flight_sim_server
FLIGHT_SIM_DEV_USER=bob FLIGHT_SIM_SERVER=ws://127.0.0.1:8787 FLIGHT_SIM_DEV_ACCEPT_INVITES=1 \
  FLIGHT_SIM_SCREENSHOT=/tmp/b.png FLIGHT_SIM_SCREENSHOT_AFTER=45 cargo run
FLIGHT_SIM_DEV_USER=alice FLIGHT_SIM_SERVER=ws://127.0.0.1:8787 FLIGHT_SIM_DEV_INVITE=bob \
  FLIGHT_SIM_SCREENSHOT=/tmp/a.png FLIGHT_SIM_SCREENSHOT_AFTER=30 cargo run
```

The server is a headless Bevy app (no rendering, windowing or audio: the `server` feature
leaves Bevy's default features off). Docker (multi-stage, Debian slim runtime):

```bash
docker build -t flight-sim-server .
docker run -p 8080:8080 -e FLIGHT_SIM_DEV_AUTH=1 flight-sim-server
```

`railway.json` builds that Dockerfile and health-checks `/health`; Railway sets `PORT`.

### Metrics

`GET /metrics` is a Prometheus text exposition (hand-rolled in `src/server/metrics.rs`, no
crate), served only with `Authorization: Bearer $FLIGHT_SIM_METRICS_TOKEN`: unset, it's a 404;
a wrong token gets a 401. Counters and histograms are process-wide atomics bumped where things
happen; the game loop copies its gauges out once a second. Everything is prefixed `flightsim_`:

- gauges: `players_online` (distinct DIDs), `connections_signed_in`, `websocket_connections`
  (open sockets, signed in or not), `players_in_rooms{kind}`, `rooms{kind,state}` (public/hosted,
  lobby/started), `rooms_by_size{players}`, `room_size_max`, `queue_length`,
  `join_requests_pending`, `leaderboard_players`, `uptime_seconds`, `start_time_seconds`,
  `build_info{git,version,protocol}` (git is Railway's `RAILWAY_GIT_COMMIT_SHA`)
- counters: `auth_attempts_total{result}` (ok, rejected, rejected_dev, bad_hello,
  client_outdated, server_outdated), `ws_messages_{in,out}_total`, `ws_bytes_{in,out}_total`,
  `ws_messages_dropped_total` (rate limit), `rooms_created_total{kind}`, `joins_total{method}`
  (code, invite (a code we invited them to in the last 5 min), friend (hop-in), request
  (accepted), quickplay (straight in), queue), `quickplay_total{result}` (joined, queued),
  `queue_matches_total` (rooms made from the queue), `queue_exits_total{outcome}` (matched,
  cancelled, offline, joined_elsewhere), `invites_total{result}`, `join_requests_total{outcome}`
  (sent, refused, accepted, declined, expired), `kills_total`, `deaths_total{cause}` (shot, crash),
  `challenge_starts_total`, `challenge_runs_total{result,reason}`, `appview_calls_total{method}`,
  `appview_errors_total{method}`
- histograms: `queue_wait_seconds`, `appview_latency_seconds`, `tick_seconds` (one game-loop tick)

Production: the token is a Railway variable on the `server` service; Prometheus on
pds.mozzius.dev scrapes it every 15 s (job `flight_sim`), and Grafana's "Flight Sim" dashboard
(grafana.mozzius.dev, `flight-sim.json` in the box's file-provisioned dashboards) draws it.

```bash
FLIGHT_SIM_METRICS_TOKEN=abc FLIGHT_SIM_DEV_AUTH=1 PORT=8787 target/release/flight_sim_server
curl -H "Authorization: Bearer abc" localhost:8787/metrics
```

### Challenge leaderboard

Protocol v4. When a signed-in challenge round starts the client sends `ChallengeStart { seed }`
(queued until online); when it ends, `SubmitScore(Run { seed, score, wave, kills, secs })`, and
the server answers `Standing(Option<{rank, score, wave}>)` (our best and global rank).
`Leaderboard(Global | Friends)` → `LeaderboardData { kind, rows, you }` (top 20 and where we
stand). Client: `src/leaderboard.rs`; server: `src/server/leaderboard.rs`.

The client is authoritative, so a run only counts if it's sensible (`protocol::check_run`):
a `ChallengeStart` on the same seed came first (one submission per start); the score splits
into 250 × each cleared wave (wave − 1, or all waves if you went down in the breather) plus
100 × kills, with no more kills than hostiles spawned (plus the two wingmen); at least 10 s per
wave after the first plus 2 s per kill, by the client's game clock *and* the server's; the
client's clock no more than 15 s ahead of the server's; caps (score 2,000,000, wave 150, 4 h).
Rate limits: 10 starts a minute, one submission per 5 s, 30 board requests a minute. A rejected
run is dropped quietly (logged; the reply is the same `Standing` as any run that isn't a best).

Storage: SQLite (rusqlite, bundled) at `FLIGHT_SIM_DB` (default `./flight_sim.db`; the Docker
image sets `/data/flight_sim.db`): `players` (DID → handle, refreshed on every connect, so
renames show), `best` (each player's best run) and `runs` (every accepted run). The friends
board is you plus your mutuals among the top 300 players, checked with presence's
relationship cache (`getRelationships`, 5-minute cache, the same 5 calls a second); a request
waits up to 8 s for lookups. On Railway, attach a volume at `/data` and set
`RAILWAY_RUN_UID=0` (Railway mounts volumes root-owned, and the image runs as a non-root user).
If the file can't be opened the server logs it and keeps the board in memory.

Locally (two dev runs with fake but plausible scores, then the boards):

```bash
FLIGHT_SIM_DEV_AUTH=1 FLIGHT_SIM_DEV_MUTUALS=1 FLIGHT_SIM_DB=/tmp/fs/board.db PORT=8787 \
  cargo run --release --no-default-features --features server --bin flight_sim_server
FLIGHT_SIM_DEV_USER=alice FLIGHT_SIM_SERVER=ws://127.0.0.1:8787 FLIGHT_SIM_MODE=challenge FLIGHT_SIM_AUTOPILOT=1 \
  FLIGHT_SIM_DIE_AT=50 FLIGHT_SIM_DEV_RUN=3,2 FLIGHT_SIM_DEV_BOARD=global \
  FLIGHT_SIM_SCREENSHOT_EXTRA=rank:/tmp/score.png,board:/tmp/board.png \
  FLIGHT_SIM_SCREENSHOT=/tmp/end.png FLIGHT_SIM_SCREENSHOT_AFTER=62 cargo run
```

`FLIGHT_SIM_DEV_RUN=<kills>,<wave>` swaps in that run when the challenge ends;
`FLIGHT_SIM_DEV_BOARD=global|friends` opens that board once online in the menu (or 3 s after the
score screen gets its rank); screenshot events `rank` and `board`.

## Sign-in

On the web you can sign in with an Atmosphere (Bluesky) account: SIGN IN in the
menu's top-right corner asks for a handle (leave it empty to start at
bsky.social, no typing) and redirects to the account's server via atproto OAuth.

- `web/auth.js` runs the OAuth client (`@atproto/oauth-client-browser`, pinned on
  esm.sh), keeps `window.FLIGHT_SIM_USER` (`{did, handle, displayName, avatar}` or
  null) and exposes `window.flightSimAuth` (`signIn(handle?)`, `signOut()`,
  `serviceToken()`). `src/identity.rs` is the Rust side.
- Client metadata is `web/oauth-client-metadata.json` (the `client_id` URL on
  flight.mozzius.dev). On `localhost` / `127.0.0.1` the loopback client is used
  instead, so sign-in works against a local `python3 -m http.server --bind 127.0.0.1`
  of `dist/` (it moves `localhost` to `127.0.0.1` first). Other hosts (Vercel
  previews) can't sign in.
- Scope: `atproto rpc:dev.mozzius.flight.join?aud=*`, just enough to mint a service-auth
  token for joining the game server (`com.atproto.server.getServiceAuth`,
  `aud=did:web:flight.mozzius.dev`). The rpc permission only allows `aud` as
  `*` or a `did#service` ref, hence `aud=*`.
- `web/.well-known/did.json` is `did:web:flight.mozzius.dev`; its `#flight_server` service points at the game server, `https://mp.flight.mozzius.dev`.
- The game server checks tokens with `atproto::verify_service_auth`
  (`src/atproto`, native only: ES256K / ES256, DID docs from plc.directory or
  did:web; the `atproto` cargo feature, which `server` turns on, so the client
  doesn't build it). Natively, `FLIGHT_SIM_DEV_USER=<handle>` plays as `did:dev:<handle>`
  with token `dev:<handle>`, which the server accepts only with `FLIGHT_SIM_DEV_AUTH=1`.

Native builds (macOS, Windows, Linux) do the same OAuth from Rust (`src/oauth`, the prompt in
`src/signin.rs`): SIGN IN opens an in-game prompt (handle field, typing and paste; empty starts
at bsky.social), then the system browser on the authorization page.

- A discoverable public "native" client: `web/oauth-native-client-metadata.json`, served at
  `https://flight.mozzius.dev/oauth-native-client-metadata.json` (`application_type: native`,
  `token_endpoint_auth_method: none`, DPoP-bound tokens, the same scope as the web).
- Redirect: a loopback `http://127.0.0.1/callback`, registered without a port. The game listens
  on a free port for one callback and the page says to return to the game. RFC 8252 §7.3 has
  the server accept any port for loopback redirects; the atproto PDS does
  (`compareRedirectUri` in `@atproto/oauth-provider`), and allows loopback redirects for any
  `native` client. (The atproto spec only spells out custom schemes and https for native
  clients; a custom scheme would need per-OS URL handler registration.)
- Handle → DID (the AppView's `resolveHandle`, then `/.well-known/atproto-did`) → DID document
  → PDS → `/.well-known/oauth-protected-resource` → the authorization server's metadata (an
  entryway like bsky.social is its own). PAR with PKCE (S256) and an ES256 DPoP key, nonces
  retried on `use_dpop_nonce`, the `iss` and `state` checked on the callback, and the token's
  `sub` checked to live on a PDS this authorization server protects. Tokens refresh when
  they're about to expire or the PDS refuses them; a refused refresh signs out.
- The game server's token comes from `com.atproto.server.getServiceAuth` on the PDS, as on the web.
- The session (DPoP key, tokens, DID, PDS and server URLs, the cached profile) is
  `<data dir>/flight_sim/session.json`, owner-only (0600). Not the OS keychain: macOS asks
  for the password whenever a differently signed binary reads an item (every rebuild and
  ad-hoc-signed release), Windows caps a secret at 2.5 KB, and Linux needs a Secret Service.
  SIGN OUT revokes the tokens and deletes the file. Screenshot runs never touch it.
- Dev: `FLIGHT_SIM_DEV_SIGN_IN=<handle>|entryway` starts signing in at launch,
  `FLIGHT_SIM_OAUTH_PRINT_URL=1` logs the authorization URL (`FLIGHT_SIM_OAUTH_URL=...` on
  stdout) instead of opening the browser, and `FLIGHT_SIM_MODE=menu-signin` shows the prompt
  in a screenshot run. `cargo test par_against_bsky_social -- --ignored` pushes a real
  authorization request to bsky.social with the deployed metadata and loads the page.

## Desktop builds

Pushing a `v*` tag builds a universal macOS .app, a Windows .exe and a Linux x86_64 tarball in GitHub Actions (`.github/workflows/release.yml`) and attaches them to a GitHub release. The macOS app is ad-hoc signed but not notarised, so the first launch needs right-click → Open (or `xattr -dr com.apple.quarantine "Flight Sim.app"`).

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
`?high` force the phone/tablet scene on or off (over the GRAPHICS setting); `?noatmo` forces the plain sky;
`?biome=`, `?weather=` and `?time=` preselect the environment, `?mode=endless|challenge` skips
the menu (same values as the env vars above).
