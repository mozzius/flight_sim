//! The multiplayer server: a headless Bevy app (no rendering, windowing or
//! audio) running the rooms, with an axum WebSocket front end on a tokio
//! thread.
//!
//! - `GET /ws`: the game's WebSocket (see `src/net/protocol.rs`)
//! - `GET /health`: "ok"
//! - `GET /metrics`: Prometheus metrics (see `metrics`), with
//!   `Authorization: Bearer $FLIGHT_SIM_METRICS_TOKEN`; a 404 if that's unset
//!
//! Env: `PORT` (default 8080), `FLIGHT_SIM_DEV_AUTH=1` (accept `dev:<handle>`
//! tokens and honour requested room codes, for local testing),
//! `FLIGHT_SIM_DEV_MUTUALS=1` (dev users are all each other's mutuals),
//! `FLIGHT_SIM_DEV_PROTOCOL=<n>` (a dev server pretends to speak version n,
//! to test the client's version-mismatch handling),
//! `FLIGHT_SIM_APPVIEW` (default the public Bluesky AppView),
//! `FLIGHT_SIM_DB` (the leaderboard's SQLite file, default `./flight_sim.db`;
//! on Railway a path on the attached volume, `/data/flight_sim.db`).
//!
//! Presence and invites: see `presence` and `hub`. AppView lookups run on a
//! tokio task, at most [`LOOKUPS_PER_SEC`], and come back to the game loop as
//! events.
//!
//! Phase 1 needs no terrain: clients handle their own crashes. Phase 2's
//! server-flown AI would add the flight model and terrain here (see README).

#[path = "../atproto/mod.rs"]
mod atproto;
#[allow(dead_code)]
#[path = "../net/protocol.rs"]
mod protocol;
mod hub;
mod leaderboard;
mod metrics;
mod presence;
mod rooms;

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::Bytes,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use bevy::{app::ScheduleRunnerPlugin, log::LogPlugin, prelude::*};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use protocol::{ClientMsg, ErrorCode, PROTOCOL_VERSION, ServerMsg, decode, encode};
use rooms::ConnId;

/// From the sockets to the game loop.
enum NetEvent {
    Connected { conn: ConnId, did: String, handle: String, out: UnboundedSender<Bytes> },
    Message(ConnId, ClientMsg),
    Disconnected(ConnId),
    /// An AppView lookup came back.
    Lookup(presence::JobResult),
}

#[derive(Resource)]
struct Inbox(Mutex<UnboundedReceiver<NetEvent>>);

#[derive(Resource)]
struct Game {
    hub: hub::Hub,
    outs: HashMap<ConnId, UnboundedSender<Bytes>>,
    jobs: UnboundedSender<presence::Job>,
    last_report: Instant,
    last_gauges: Instant,
}

#[derive(Clone)]
struct Shared {
    events: UnboundedSender<NetEvent>,
    next_conn: std::sync::Arc<AtomicU64>,
    /// The protocol version we speak (`FLIGHT_SIM_DEV_PROTOCOL` fakes another on dev servers).
    version: u16,
    /// `/metrics` needs this bearer token (None: no `/metrics`).
    metrics_token: Option<Arc<str>>,
}

/// Game loop rate: relays wait at most this long.
const TICK_HZ: f64 = 100.0;
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Clients ping every few seconds when they've nothing else to say.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_FRAME: usize = 16 * 1024;
/// Per-connection message allowance (a flying client sends ~70/s).
const MAX_MSGS_PER_SEC: f32 = 200.0;
/// AppView calls (relationships and profiles), server-wide.
const LOOKUPS_PER_SEC: f64 = 5.0;

/// The metrics snapshot is copied out this often.
const GAUGE_GAP: Duration = Duration::from_secs(1);

fn main() {
    metrics::start();
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080);
    let dev = std::env::var("FLIGHT_SIM_DEV_AUTH").as_deref() == Ok("1");
    let dev_mutuals = std::env::var("FLIGHT_SIM_DEV_MUTUALS").as_deref() == Ok("1");
    let appview = std::env::var("FLIGHT_SIM_APPVIEW").unwrap_or_else(|_| presence::PUBLIC_APPVIEW.to_string());
    let db_path = std::env::var("FLIGHT_SIM_DB").unwrap_or_else(|_| leaderboard::DEFAULT_DB.to_string());
    let store = match leaderboard::Store::open(std::path::Path::new(&db_path)) {
        Ok(store) => store,
        Err(e) => {
            // (the rest of the game still works; scores just don't last)
            eprintln!("can't open the leaderboard at {db_path}: {e}; keeping it in memory");
            leaderboard::Store::memory()
        }
    };
    let (events, inbox) = unbounded_channel();
    let (jobs, jobs_rx) = unbounded_channel();
    let version = std::env::var("FLIGHT_SIM_DEV_PROTOCOL").ok().filter(|_| dev).and_then(|v| v.parse().ok()).unwrap_or(PROTOCOL_VERSION);
    let metrics_token = std::env::var("FLIGHT_SIM_METRICS_TOKEN").ok().filter(|t| !t.trim().is_empty()).map(|t| Arc::from(t.trim()));
    let metrics_on = metrics_token.is_some();
    let shared = Shared { events: events.clone(), next_conn: Default::default(), version, metrics_token };

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
        let source: Arc<dyn presence::RelationshipSource> =
            Arc::new(presence::DevAware { inner: presence::AppView::new(&appview), dev_mutuals });
        rt.spawn(lookups(jobs_rx, source, events));
        rt.block_on(serve(port, shared));
        // (serve only returns if it can't listen)
        std::process::exit(1);
    });

    App::new()
        .add_plugins(MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(1.0 / TICK_HZ))))
        .add_plugins(LogPlugin::default())
        .insert_resource(Inbox(Mutex::new(inbox)))
        .insert_resource(Game { hub: hub::Hub::with_store(dev, store), outs: HashMap::new(), jobs, last_report: Instant::now(), last_gauges: Instant::now() })
        .add_systems(Startup, move || {
            let flags = [(dev, " (dev auth)"), (dev_mutuals, " (dev mutuals)")];
            info!("flight sim server on :{port}{}", flags.iter().filter(|f| f.0).map(|f| f.1).collect::<String>());
            info!("leaderboard: {db_path}");
            info!("metrics: {}", if metrics_on { "/metrics (bearer token)" } else { "off (no FLIGHT_SIM_METRICS_TOKEN)" });
        })
        .add_systems(Update, pump)
        .run();
}

/// Runs the rooms on everything the sockets received, and sends the replies.
fn pump(inbox: Res<Inbox>, mut game: ResMut<Game>) {
    let now = Instant::now();
    let _timer = TickTimer(now);
    let mut step = hub::Step::default();
    let mut rx = inbox.0.lock().unwrap();
    while let Ok(event) = rx.try_recv() {
        let s = match event {
            NetEvent::Connected { conn, did, handle, out } => {
                info!("conn {conn}: {handle} ({did})");
                game.outs.insert(conn, out);
                game.hub.connect(conn, did, handle, now)
            }
            NetEvent::Message(conn, msg) => {
                if let ClientMsg::Create { .. }
                | ClientMsg::Join { .. }
                | ClientMsg::Leave
                | ClientMsg::Invite { .. }
                | ClientMsg::Start
                | ClientMsg::RequestJoin { .. }
                | ClientMsg::AnswerJoin { .. }
                | ClientMsg::ChallengeStart { .. }
                | ClientMsg::QuickPlay { .. }
                | ClientMsg::CancelQueue
                | ClientMsg::JoinFriend { .. }
                | ClientMsg::Leaderboard(_) = &msg
                {
                    info!("conn {conn}: {msg:?}");
                }
                game.hub.handle(conn, msg, now)
            }
            NetEvent::Disconnected(conn) => {
                info!("conn {conn}: gone");
                game.outs.remove(&conn);
                game.hub.disconnect(conn, now)
            }
            NetEvent::Lookup(result) => game.hub.apply(result, now),
        };
        step.out.extend(s.out);
        step.jobs.extend(s.jobs);
    }
    drop(rx);
    let s = game.hub.tick(now);
    step.out.extend(s.out);
    step.jobs.extend(s.jobs);
    for job in step.jobs {
        let _ = game.jobs.send(job);
    }
    for o in step.out {
        match &o.msg {
            ServerMsg::Joined { code, you, players, started, public, .. } => {
                info!("room {code}: player {you} joined ({} in the room, started {started}, public {public})", players.len())
            }
            ServerMsg::Killed { victim, killer, record } => info!("player {victim} down, killer {killer:?} {record:?}"),
            ServerMsg::Started => info!("room started -> conns {:?}", o.to),
            ServerMsg::Host(id) => info!("player {id} hosts now"),
            ServerMsg::JoinRequest(f) => info!("join request from {} -> conn {:?}", f.handle, o.to),
            ServerMsg::JoinDeclined(_) => info!("join request declined (conn {:?})", o.to),
            ServerMsg::Respawned(id) => info!("player {id} respawned"),
            ServerMsg::Invited { from, code } => info!("invite from {} to room {code} -> conns {:?}", from.handle, o.to),
            ServerMsg::InviteFailed(_) => info!("invite failed (conn {:?})", o.to),
            ServerMsg::Standing(s) => info!("standing {s:?} -> conn {:?}", o.to),
            ServerMsg::LeaderboardData { kind, rows, you } => info!("{kind:?} board: {} rows, you {you:?} -> conn {:?}", rows.len(), o.to),
            ServerMsg::Queued => info!("queued (conn {:?}, {} waiting)", o.to, game.hub.queued()),
            ServerMsg::Online(n) => info!("{n} online -> {} conns", o.to.len()),
            ServerMsg::FriendUpdate(f) => info!("friend {} ({:?}) -> conns {:?}", f.handle, f.status, o.to),
            _ => {}
        }
        let bytes = Bytes::from(encode(&o.msg));
        for conn in o.to {
            if let Some(tx) = game.outs.get(&conn) {
                let _ = tx.send(bytes.clone());
            }
        }
    }
    if now.duration_since(game.last_gauges) >= GAUGE_GAP {
        game.last_gauges = now;
        *metrics::GAUGES.lock().unwrap() = game.hub.gauges();
    }
    if now.duration_since(game.last_report) > Duration::from_secs(60) {
        game.last_report = now;
        info!(
            "{} rooms, {} players, {} online, {} queued",
            game.hub.rooms.room_count(),
            game.hub.rooms.player_count(),
            game.hub.presence.online_count(),
            game.hub.queued()
        );
    }
}

/// Times a game-loop tick (when dropped).
struct TickTimer(Instant);

impl Drop for TickTimer {
    fn drop(&mut self) {
        metrics::M.tick.observe(self.0.elapsed());
    }
}

/// Runs AppView lookups one at a time, at most `LOOKUPS_PER_SEC`.
async fn lookups(
    mut jobs: UnboundedReceiver<presence::Job>,
    source: Arc<dyn presence::RelationshipSource>,
    events: UnboundedSender<NetEvent>,
) {
    let gap = Duration::from_secs_f64(1.0 / LOOKUPS_PER_SEC);
    let mut next = tokio::time::Instant::now();
    while let Some(job) = jobs.recv().await {
        tokio::time::sleep_until(next).await;
        next = tokio::time::Instant::now() + gap;
        let result = job.run(&*source).await;
        if events.send(NetEvent::Lookup(result)).is_err() {
            break;
        }
    }
}

async fn serve(port: u16, shared: Shared) {
    let app = Router::new()
        .route("/", get(|| async { "flight sim server" }))
        .route("/health", get(|| async { "ok" }))
        .route("/ws", get(upgrade))
        .route("/metrics", get(metrics_page))
        .with_state(shared);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            if let Err(e) = axum::serve(listener, app).await {
                error!("server stopped: {e}");
            }
        }
        Err(e) => error!("can't listen on {addr}: {e}"),
    }
}

async fn metrics_page(headers: HeaderMap, State(shared): State<Shared>) -> axum::response::Response {
    let Some(token) = &shared.metrics_token else { return StatusCode::NOT_FOUND.into_response() };
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if !metrics::authorized(auth, token) {
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")]).into_response();
    }
    let gauges = metrics::GAUGES.lock().unwrap().clone();
    let body = metrics::render(&gauges, &metrics::M);
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], body).into_response()
}

async fn upgrade(ws: WebSocketUpgrade, State(shared): State<Shared>) -> impl IntoResponse {
    ws.max_message_size(MAX_FRAME).max_frame_size(MAX_FRAME).on_upgrade(move |socket| connection(socket, shared))
}

async fn connection(socket: WebSocket, shared: Shared) {
    let _open = metrics::OpenSocket::new();
    let m = &metrics::M;
    let (mut sink, mut stream) = socket.split();
    let reply = |msg: ServerMsg| {
        let bytes = Bytes::from(encode(&msg));
        m.ws_messages_out.inc();
        m.ws_bytes_out.add(bytes.len() as u64);
        Message::Binary(bytes)
    };

    // first frame: Hello with a service-auth token
    let hello = match tokio::time::timeout(HELLO_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(Message::Binary(b)))) => {
            m.ws_messages_in.inc();
            m.ws_bytes_in.add(b.len() as u64);
            decode::<ClientMsg>(&b)
        }
        _ => None,
    };
    let Some(ClientMsg::Hello { version, token }) = hello else {
        m.auth.inc(&["bad_hello"]);
        let _ = sink.send(reply(ServerMsg::Error(ErrorCode::BadRequest))).await;
        return;
    };
    if version != shared.version {
        m.auth.inc(&[if version < shared.version { "client_outdated" } else { "server_outdated" }]);
        // (our version first: older clients can't read it and skip it)
        let _ = sink.send(reply(ServerMsg::ServerVersion(shared.version))).await;
        let _ = sink.send(reply(ServerMsg::Error(ErrorCode::Version))).await;
        return;
    }
    let user = match atproto::verify_service_auth(&token, protocol::AUTH_AUDIENCE, protocol::AUTH_LXM).await {
        Ok(user) => user,
        Err(e) => {
            m.auth.inc(&[if token.starts_with("dev:") { "rejected_dev" } else { "rejected" }]);
            info!("auth refused: {e}");
            let _ = sink.send(reply(ServerMsg::Error(ErrorCode::Auth))).await;
            return;
        }
    };
    m.auth.inc(&["ok"]);
    let conn = shared.next_conn.fetch_add(1, Ordering::Relaxed) + 1;
    // only a verified handle (the DID doc's alsoKnownAs, resolving back to the
    // DID); the client never gets to name itself. No handle: part of the DID.
    let handle = user.handle.clone().unwrap_or_else(|| short_did(&user.did));
    let (out, mut out_rx) = unbounded_channel::<Bytes>();
    if shared.events.send(NetEvent::Connected { conn, did: user.did, handle, out }).is_err() {
        return;
    }

    let writer = tokio::spawn(async move {
        while let Some(bytes) = out_rx.recv().await {
            metrics::M.ws_messages_out.inc();
            metrics::M.ws_bytes_out.add(bytes.len() as u64);
            if sink.send(Message::Binary(bytes)).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let mut allowance = MAX_MSGS_PER_SEC;
    let mut last = Instant::now();
    loop {
        let frame = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(frame))) => frame,
            _ => break,
        };
        let bytes = match frame {
            Message::Binary(b) => {
                m.ws_messages_in.inc();
                m.ws_bytes_in.add(b.len() as u64);
                b
            }
            Message::Close(_) => break,
            // (axum answers pings itself)
            _ => continue,
        };
        let now = Instant::now();
        allowance = (allowance + now.duration_since(last).as_secs_f32() * MAX_MSGS_PER_SEC).min(MAX_MSGS_PER_SEC);
        last = now;
        if allowance < 1.0 {
            m.ws_dropped.inc();
            continue;
        }
        allowance -= 1.0;
        match decode::<ClientMsg>(&bytes) {
            Some(msg) => {
                if shared.events.send(NetEvent::Message(conn, msg)).is_err() {
                    break;
                }
            }
            None => break,
        }
    }
    let _ = shared.events.send(NetEvent::Disconnected(conn));
    writer.abort();
}

/// `did:plc:abcdef…` → `abcdef` (a stand-in when a user has no handle).
fn short_did(did: &str) -> String {
    did.rsplit(':').next().unwrap_or(did).chars().take(12).collect()
}
