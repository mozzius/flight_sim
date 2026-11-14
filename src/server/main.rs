//! The multiplayer server: a headless Bevy app (no rendering, windowing or
//! audio) running the rooms, with an axum WebSocket front end on a tokio
//! runtime.
//!
//! - `GET /ws`: the game's WebSocket (see `src/net/protocol.rs`)
//! - `GET /health`: "ok" (a 503 while shutting down)
//! - `GET /metrics`: Prometheus metrics (see `metrics`), with
//!   `Authorization: Bearer $FLIGHT_SIM_METRICS_TOKEN`; a 404 if that's unset
//!
//! Env: `PORT` (default 8080), `FLIGHT_SIM_DEV_AUTH=1` (accept `dev:<handle>`
//! tokens and honour requested room codes, for local testing),
//! `FLIGHT_SIM_DEV_MUTUALS=1` (dev users are all each other's mutuals),
//! `FLIGHT_SIM_DEV_PROTOCOL=<n>` (a dev server pretends to speak version n,
//! to test the client's version-mismatch handling),
//! `FLIGHT_SIM_APPVIEW` (default the public Bluesky AppView).
//!
//! Scaling (see README and `cluster`): `REDIS_URL` makes this one replica of
//! many sharing state in Redis (keys under `FLIGHT_SIM_REDIS_PREFIX`,
//! default `fs:`); without it, everything's in-process (one server).
//! `FLIGHT_SIM_REPLICA` names the replica (default Railway's replica id),
//! plus a random suffix per process.
//!
//! The leaderboard: Postgres at `DATABASE_URL` (`FLIGHT_SIM_PG_SCHEMA`: a
//! schema of its own, for tests), importing the SQLite file at
//! `FLIGHT_SIM_DB` once if Postgres is empty; without `DATABASE_URL`, that
//! SQLite file (default `./flight_sim.db`) is the leaderboard.
//!
//! On SIGTERM (Railway's scale-down or redeploy) or ctrl-c, the replica
//! drains: no new sockets, its rooms end (players back to the menu with NO
//! ROOM), it says goodbye to the others, and exits within [`DRAIN`].
//!
//! Presence and invites: see `presence` and `hub`. AppView lookups run on a
//! tokio task, at most [`LOOKUPS_PER_SEC`] across the cluster, and come back
//! to the game loop as events, like database answers and other replicas'
//! messages.
//!
//! Phase 1 needs no terrain: clients handle their own crashes. Phase 2's
//! server-flown AI would add the flight model and terrain here (see README).

#[path = "../atproto/mod.rs"]
mod atproto;
#[allow(dead_code)]
#[path = "../net/protocol.rs"]
mod protocol;
mod cluster;
mod hub;
mod leaderboard;
mod metrics;
mod presence;
mod redis_backend;
mod rooms;

use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
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

use cluster::Backend;
use leaderboard::{Db, DbJob, DbResult};
use protocol::{ClientMsg, ErrorCode, PROTOCOL_VERSION, ServerMsg, decode, encode};
use rooms::ConnId;

/// From the sockets (and the workers) to the game loop.
enum NetEvent {
    Connected { conn: ConnId, did: String, handle: String, out: UnboundedSender<Bytes> },
    Message(ConnId, ClientMsg),
    Disconnected(ConnId),
    /// An AppView lookup came back.
    Lookup(presence::JobResult),
    /// A database answer.
    Db(DbResult),
    /// An envelope from another replica.
    Cluster(Vec<u8>),
    /// Every live replica's view (the periodic re-read).
    Views(Vec<Vec<u8>>),
    /// SIGTERM: drain and exit.
    Shutdown,
}

#[derive(Resource)]
struct Inbox(Mutex<UnboundedReceiver<NetEvent>>);

#[derive(Resource)]
struct Game {
    hub: hub::Hub,
    outs: HashMap<ConnId, UnboundedSender<Bytes>>,
    jobs: UnboundedSender<presence::Job>,
    db: UnboundedSender<DbJob>,
    last_report: Instant,
    last_gauges: Instant,
    /// Draining since.
    closing: Option<Instant>,
    backend: Arc<dyn Backend>,
    rt: tokio::runtime::Handle,
}

#[derive(Clone)]
struct Shared {
    events: UnboundedSender<NetEvent>,
    next_conn: std::sync::Arc<AtomicU64>,
    /// The protocol version we speak (`FLIGHT_SIM_DEV_PROTOCOL` fakes another on dev servers).
    version: u16,
    /// `/metrics` needs this bearer token (None: no `/metrics`).
    metrics_token: Option<Arc<str>>,
    /// Shutting down: no new sockets, `/health` fails.
    draining: Arc<AtomicBool>,
    backend: Arc<dyn Backend>,
}

/// Game loop rate: relays wait at most this long.
const TICK_HZ: f64 = 100.0;
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Clients ping every few seconds when they've nothing else to say.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_FRAME: usize = 16 * 1024;
/// Per-connection message allowance (a flying client sends ~70/s).
const MAX_MSGS_PER_SEC: f32 = 200.0;
/// AppView calls (relationships and profiles), across the cluster.
const LOOKUPS_PER_SEC: u32 = 5;
/// A lookup another replica has claimed is waited for this long.
const LOOKUP_WAIT: Duration = Duration::from_secs(3);

/// The metrics snapshot is copied out this often.
const GAUGE_GAP: Duration = Duration::from_secs(1);
/// From SIGTERM to exit (Railway's `drainingSeconds` is longer).
const DRAIN: Duration = Duration::from_millis(1500);

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// This process's replica id: Railway's replica (or `FLIGHT_SIM_REPLICA`), and a random suffix.
fn replica_id() -> String {
    let base = env("FLIGHT_SIM_REPLICA")
        .or_else(|| env("RAILWAY_REPLICA_ID").map(|r| r.chars().take(8).collect()))
        .unwrap_or_else(|| "local".into());
    let base: String = base.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    format!("{base}-{:04x}", rand::random::<u16>())
}

fn main() {
    metrics::start();
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080);
    let dev = std::env::var("FLIGHT_SIM_DEV_AUTH").as_deref() == Ok("1");
    let dev_mutuals = std::env::var("FLIGHT_SIM_DEV_MUTUALS").as_deref() == Ok("1");
    let appview = std::env::var("FLIGHT_SIM_APPVIEW").unwrap_or_else(|_| presence::PUBLIC_APPVIEW.to_string());
    let db_path = std::env::var("FLIGHT_SIM_DB").unwrap_or_else(|_| leaderboard::DEFAULT_DB.to_string());
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    // (Bevy's logger isn't up yet: plain stderr until it is)
    let id = replica_id();

    // the shared state: Redis, or this process alone
    let mut redis = None;
    let backend: Arc<dyn Backend> = match env("REDIS_URL") {
        Some(url) => {
            let prefix = env("FLIGHT_SIM_REDIS_PREFIX").unwrap_or_else(|| "fs:".into());
            match rt.block_on(redis_backend::Redis::connect(&url, &prefix, &id)) {
                Ok(r) => {
                    eprintln!("cluster: redis (prefix {prefix}), replica {id}");
                    redis = Some(r.clone());
                    r
                }
                Err(e) => {
                    eprintln!("can't reach redis: {e}");
                    std::process::exit(1);
                }
            }
        }
        None => {
            eprintln!("cluster: none (no REDIS_URL), one server");
            cluster::Memory::single()
        }
    };

    // the leaderboard: Postgres, or SQLite
    let db_note;
    let db = match env("DATABASE_URL") {
        Some(url) => {
            let schema = env("FLIGHT_SIM_PG_SCHEMA");
            let db = match rt.block_on(async {
                let mut tries = 0;
                loop {
                    match Db::postgres(&url, schema.as_deref()).await {
                        Ok(db) => break Ok(db),
                        Err(e) if tries < 5 => {
                            eprintln!("postgres: {e}; retrying");
                            tries += 1;
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                        Err(e) => break Err(e),
                    }
                }
            }) {
                Ok(db) => db,
                Err(e) => {
                    eprintln!("can't open the leaderboard in postgres: {e}");
                    std::process::exit(1);
                }
            };
            match rt.block_on(db.import_sqlite(std::path::Path::new(&db_path))) {
                Ok(Some((from, to))) => eprintln!(
                    "leaderboard: imported {db_path} into postgres: sqlite had {} players, {} best, {} runs; postgres now has {} players, {} best, {} runs",
                    from.0, from.1, from.2, to.0, to.1, to.2
                ),
                Ok(None) => {}
                Err(e) => eprintln!("leaderboard: importing {db_path} failed: {e}"),
            }
            match rt.block_on(db.counts()) {
                Ok(c) => eprintln!("leaderboard: postgres{} ({} players, {} best, {} runs)", schema.map(|s| format!(" schema {s}")).unwrap_or_default(), c.0, c.1, c.2),
                Err(e) => eprintln!("leaderboard: postgres (can't count: {e})"),
            }
            db_note = "postgres".to_string();
            db
        }
        None => {
            db_note = format!("sqlite {db_path}");
            match Db::sqlite(std::path::Path::new(&db_path)) {
                Ok(db) => db,
                Err(e) => {
                    // (the rest of the game still works; scores just don't last)
                    eprintln!("can't open the leaderboard at {db_path}: {e}; keeping it in memory");
                    Db::memory()
                }
            }
        }
    };

    let (events, inbox) = unbounded_channel();
    let (jobs, jobs_rx) = unbounded_channel();
    let (db_jobs, db_rx) = unbounded_channel();
    let version = std::env::var("FLIGHT_SIM_DEV_PROTOCOL").ok().filter(|_| dev).and_then(|v| v.parse().ok()).unwrap_or(PROTOCOL_VERSION);
    let metrics_token = std::env::var("FLIGHT_SIM_METRICS_TOKEN").ok().filter(|t| !t.trim().is_empty()).map(|t| Arc::from(t.trim()));
    let metrics_on = metrics_token.is_some();
    let draining = Arc::new(AtomicBool::new(false));
    let shared = Shared { events: events.clone(), next_conn: Default::default(), version, metrics_token, draining: draining.clone(), backend: backend.clone() };

    let source: Arc<dyn presence::RelationshipSource> = Arc::new(presence::DevAware { inner: presence::AppView::new(&appview), dev_mutuals });
    rt.spawn(lookups(jobs_rx, source, backend.clone(), events.clone()));
    rt.spawn(database(db_rx, db, events.clone()));
    rt.spawn(push_metrics(backend.clone()));
    if backend.clustered() {
        let refresh = Arc::new(tokio::sync::Notify::new());
        rt.spawn(read_views(backend.clone(), refresh.clone(), events.clone()));
        if let Some(r) = redis {
            let ev = events.clone();
            rt.spawn(r.subscribe(move |m| drop(ev.send(NetEvent::Cluster(m))), move || refresh.notify_one()));
        }
    }
    rt.spawn(signals(draining, events.clone()));
    rt.spawn(async move {
        serve(port, shared).await;
        // (serve only returns if it can't listen)
        std::process::exit(1);
    });

    let handle = rt.handle().clone();
    let hub = hub::Hub::with(dev, backend.clone());
    App::new()
        .add_plugins(MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(1.0 / TICK_HZ))))
        .add_plugins(LogPlugin::default())
        .insert_resource(Inbox(Mutex::new(inbox)))
        .insert_resource(Game {
            hub,
            outs: HashMap::new(),
            jobs,
            db: db_jobs,
            last_report: Instant::now(),
            last_gauges: Instant::now(),
            closing: None,
            backend,
            rt: handle,
        })
        .add_systems(Startup, move || {
            let flags = [(dev, " (dev auth)"), (dev_mutuals, " (dev mutuals)")];
            info!("flight sim server on :{port}{}", flags.iter().filter(|f| f.0).map(|f| f.1).collect::<String>());
            info!("leaderboard: {db_note}");
            info!("metrics: {}", if metrics_on { "/metrics (bearer token)" } else { "off (no FLIGHT_SIM_METRICS_TOKEN)" });
        })
        .add_systems(Update, pump)
        .run();
    drop(rt);
}

/// SIGTERM or ctrl-c: drain.
async fn signals(draining: Arc<AtomicBool>, events: UnboundedSender<NetEvent>) {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
    draining.store(true, Ordering::Relaxed);
    let _ = events.send(NetEvent::Shutdown);
}

/// Runs the rooms on everything the sockets received, and sends the replies.
fn pump(inbox: Res<Inbox>, mut game: ResMut<Game>) {
    let now = Instant::now();
    let _timer = TickTimer(now);
    let mut step = hub::Step::default();
    let mut rx = inbox.0.lock().unwrap();
    let mut shutdown = false;
    while let Ok(event) = rx.try_recv() {
        let s = match event {
            NetEvent::Connected { conn, did, handle, out } => {
                if game.closing.is_some() {
                    // (dropping `out` closes the socket)
                    continue;
                }
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
            NetEvent::Db(result) => game.hub.apply_db(result, now),
            NetEvent::Cluster(bytes) => game.hub.on_cluster(&bytes, now),
            NetEvent::Views(views) => game.hub.on_views(views, now),
            NetEvent::Shutdown => {
                if game.closing.is_some() {
                    continue;
                }
                info!("shutting down: ending {} rooms", game.hub.rooms.room_count());
                shutdown = true;
                game.closing = Some(now);
                game.hub.shutdown(now)
            }
        };
        step.extend(s);
    }
    drop(rx);
    step.extend(game.hub.tick(now));
    for job in step.jobs {
        let _ = game.jobs.send(job);
    }
    for job in step.db {
        let _ = game.db.send(job);
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
            ServerMsg::Queued => info!("queued (conn {:?}, {} waiting here)", o.to, game.hub.queued()),
            ServerMsg::Online(n) => info!("{n} online -> {} conns", o.to.len()),
            ServerMsg::FriendUpdate(f) => info!("friend {} ({:?}) -> conns {:?}", f.handle, f.status, o.to),
            ServerMsg::Error(e) => info!("error {e:?} -> conns {:?}", o.to),
            _ => {}
        }
        let bytes = Bytes::from(encode(&o.msg));
        for conn in o.to {
            if let Some(tx) = game.outs.get(&conn) {
                let _ = tx.send(bytes.clone());
            }
        }
    }
    for (to, bytes) in step.raw {
        if let Some(msg) = decode::<ServerMsg>(&bytes) {
            match &msg {
                ServerMsg::Joined { code, you, players, .. } => {
                    info!("room {code} (another replica's): player {you} joined ({} in the room) -> conns {to:?}", players.len())
                }
                ServerMsg::Killed { victim, killer, .. } => info!("player {victim} down (another replica's room), killer {killer:?}"),
                ServerMsg::Error(e) => info!("error {e:?} (another replica's room) -> conns {to:?}"),
                _ => {}
            }
        }
        let bytes = Bytes::from(bytes);
        for conn in to {
            if let Some(tx) = game.outs.get(&conn) {
                let _ = tx.send(bytes.clone());
            }
        }
    }
    game.hub.flush(now);
    if shutdown {
        // (our goodbye, then the sockets close once their writers have sent what's queued)
        game.hub.farewell();
        game.outs.clear();
        let (backend, rt) = (game.backend.clone(), game.rt.clone());
        std::thread::spawn(move || {
            std::thread::sleep(DRAIN);
            let _ = rt.block_on(async { tokio::time::timeout(Duration::from_secs(2), push_once(&*backend)).await });
            info!("bye");
            std::process::exit(0);
        });
    }
    if now.duration_since(game.last_gauges) >= GAUGE_GAP {
        game.last_gauges = now;
        *metrics::GAUGES.lock().unwrap() = game.hub.snapshot();
    }
    if now.duration_since(game.last_report) > Duration::from_secs(60) {
        game.last_report = now;
        info!(
            "{} rooms, {} players, {} online, {} queued, {} other replicas",
            game.hub.rooms.room_count(),
            game.hub.rooms.player_count(),
            game.hub.presence.online_count(),
            game.hub.queued(),
            game.hub.others()
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

/// Runs AppView lookups one at a time, within the cluster's budget, through
/// the cluster's cache: a pair another replica has (or is) looking up isn't
/// looked up again.
async fn lookups(
    mut jobs: UnboundedReceiver<presence::Job>,
    source: Arc<dyn presence::RelationshipSource>,
    backend: Arc<dyn Backend>,
    events: UnboundedSender<NetEvent>,
) {
    while let Some(job) = jobs.recv().await {
        let results = match job {
            presence::Job::Mutuals { actor, others } => mutuals(actor, others, &*source, &*backend).await,
            job @ presence::Job::Profiles(_) => {
                backend.lookup_slot(LOOKUPS_PER_SEC).await;
                vec![job.run(&*source).await]
            }
        };
        for r in results {
            if events.send(NetEvent::Lookup(r)).is_err() {
                return;
            }
        }
    }
}

async fn mutuals(actor: String, others: Vec<String>, source: &dyn presence::RelationshipSource, backend: &dyn Backend) -> Vec<presence::JobResult> {
    use presence::JobResult;
    let cached = backend.cached_pairs(actor.clone(), others.clone()).await;
    let mut known: Vec<(String, bool)> = vec![];
    let mut unknown = vec![];
    for (o, c) in others.into_iter().zip(cached.into_iter().chain(std::iter::repeat(None))) {
        match c {
            Some(m) => known.push((o, m)),
            None => unknown.push(o),
        }
    }
    let locks = backend.lock_pairs(actor.clone(), unknown.clone()).await;
    let (mine, theirs): (Vec<(String, bool)>, Vec<(String, bool)>) = unknown.into_iter().zip(locks).partition(|(_, l)| *l);
    let mine: Vec<String> = mine.into_iter().map(|(o, _)| o).collect();
    let mut failed: Vec<String> = vec![];
    if !mine.is_empty() {
        backend.lookup_slot(LOOKUPS_PER_SEC).await;
        match source.mutuals(&actor, &mine).await {
            Ok(m) => {
                let found: Vec<(String, bool)> = mine.iter().map(|o| (o.clone(), m.contains(o))).collect();
                backend.store_pairs(actor.clone(), found.clone(), presence::PAIR_TTL).await;
                known.extend(found);
            }
            Err(e) => {
                // (their locks lapse in a few seconds; presence tries again in a minute)
                warn!("relationships for {actor}: {e}");
                failed.extend(mine);
            }
        }
    }
    // another replica's on these: wait for its answer in the cache
    let mut waiting: Vec<String> = theirs.into_iter().map(|(o, _)| o).collect();
    let until = tokio::time::Instant::now() + LOOKUP_WAIT;
    while !waiting.is_empty() && tokio::time::Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let got = backend.cached_pairs(actor.clone(), waiting.clone()).await;
        let mut still = vec![];
        for (o, c) in waiting.into_iter().zip(got.into_iter().chain(std::iter::repeat(None))) {
            match c {
                Some(m) => known.push((o, m)),
                None => still.push(o),
            }
        }
        waiting = still;
    }
    failed.extend(waiting);
    let mut out = vec![];
    if !known.is_empty() {
        let mutual = known.iter().filter(|(_, m)| *m).map(|(o, _)| o.clone()).collect();
        out.push(JobResult::Mutuals { actor: actor.clone(), others: known.into_iter().map(|(o, _)| o).collect(), result: Ok(mutual) });
    }
    if !failed.is_empty() {
        out.push(JobResult::Mutuals { actor, others: failed, result: Err("lookup failed or timed out".into()) });
    }
    out
}

/// Runs database jobs (each on its own task: Postgres has a pool).
async fn database(mut jobs: UnboundedReceiver<DbJob>, db: Db, events: UnboundedSender<NetEvent>) {
    while let Some(job) = jobs.recv().await {
        let (db, events) = (db.clone(), events.clone());
        tokio::spawn(async move {
            let _ = events.send(NetEvent::Db(db.run(job).await));
        });
    }
}

/// Re-reads every replica's view now and then (and when the subscription
/// comes back, having maybe missed some).
async fn read_views(backend: Arc<dyn Backend>, now: Arc<tokio::sync::Notify>, events: UnboundedSender<NetEvent>) {
    loop {
        if let Some(views) = backend.read_views().await
            && events.send(NetEvent::Views(views)).is_err()
        {
            return;
        }
        let _ = tokio::time::timeout(cluster::REFRESH, now.notified()).await;
    }
}

/// What the cluster's totals have of ours so far (the periodic push and the
/// last one on the way out share it, or the last would add it all again).
static PUSHED: tokio::sync::Mutex<BTreeMap<String, u64>> = tokio::sync::Mutex::const_new(BTreeMap::new());

/// Adds what the counters have grown by to the cluster's totals.
async fn push_once(backend: &dyn Backend) {
    if !backend.clustered() {
        return;
    }
    let mut last = PUSHED.lock().await;
    let now = metrics::flat(&metrics::M);
    if backend.push_metrics(metrics::deltas(&now, &last)).await {
        *last = now;
    }
}

async fn push_metrics(backend: Arc<dyn Backend>) {
    loop {
        push_once(&*backend).await;
        tokio::time::sleep(metrics::PUSH_EVERY).await;
    }
}

async fn serve(port: u16, shared: Shared) {
    let draining = shared.draining.clone();
    let app = Router::new()
        .route("/", get(|| async { "flight sim server" }))
        .route(
            "/health",
            get(move || {
                let draining = draining.load(Ordering::Relaxed);
                async move { if draining { (StatusCode::SERVICE_UNAVAILABLE, "draining") } else { (StatusCode::OK, "ok") } }
            }),
        )
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
        // (stderr too: this can happen before the logger's up)
        Err(e) => eprintln!("can't listen on {addr}: {e}"),
    }
}

async fn metrics_page(headers: HeaderMap, State(shared): State<Shared>) -> axum::response::Response {
    let Some(token) = &shared.metrics_token else { return StatusCode::NOT_FOUND.into_response() };
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if !metrics::authorized(auth, token) {
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")]).into_response();
    }
    let totals = match shared.backend.metric_totals().await {
        Ok(Some(t)) => t,
        Ok(None) => metrics::flat(&metrics::M),
        // (better no scrape than numbers that go backwards)
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("metrics store: {e}")).into_response(),
    };
    let snapshot = metrics::GAUGES.lock().unwrap().clone();
    let body = metrics::render(&snapshot, &totals);
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], body).into_response()
}

async fn upgrade(ws: WebSocketUpgrade, State(shared): State<Shared>) -> axum::response::Response {
    if shared.draining.load(Ordering::Relaxed) {
        return (StatusCode::SERVICE_UNAVAILABLE, "draining").into_response();
    }
    ws.max_message_size(MAX_FRAME).max_frame_size(MAX_FRAME).on_upgrade(move |socket| connection(socket, shared)).into_response()
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
