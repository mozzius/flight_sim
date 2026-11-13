//! The multiplayer server: a headless Bevy app (no rendering, windowing or
//! audio) running the rooms, with an axum WebSocket front end on a tokio
//! thread.
//!
//! - `GET /ws`: the game's WebSocket (see `src/net/protocol.rs`)
//! - `GET /health`: "ok"
//!
//! Env: `PORT` (default 8080), `FLIGHT_SIM_DEV_AUTH=1` (accept `dev:<handle>`
//! tokens and honour requested room codes, for local testing).
//!
//! Phase 1 needs no terrain: clients handle their own crashes. Phase 2's
//! server-flown AI would add the flight model and terrain here (see README).

#[path = "../atproto/mod.rs"]
mod atproto;
#[allow(dead_code)]
#[path = "../net/protocol.rs"]
mod protocol;
mod rooms;

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Mutex,
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
}

#[derive(Resource)]
struct Inbox(Mutex<UnboundedReceiver<NetEvent>>);

#[derive(Resource)]
struct Game {
    server: rooms::Server,
    outs: HashMap<ConnId, UnboundedSender<Bytes>>,
    last_report: Instant,
}

#[derive(Clone)]
struct Shared {
    events: UnboundedSender<NetEvent>,
    next_conn: std::sync::Arc<AtomicU64>,
}

/// Game loop rate: relays wait at most this long.
const TICK_HZ: f64 = 100.0;
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Clients ping every few seconds when they've nothing else to say.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_FRAME: usize = 16 * 1024;
/// Per-connection message allowance (a flying client sends ~70/s).
const MAX_MSGS_PER_SEC: f32 = 200.0;

fn main() {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080);
    let dev = std::env::var("FLIGHT_SIM_DEV_AUTH").as_deref() == Ok("1");
    let (events, inbox) = unbounded_channel();
    let shared = Shared { events, next_conn: Default::default() };

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
        rt.block_on(serve(port, shared));
        // (serve only returns if it can't listen)
        std::process::exit(1);
    });

    App::new()
        .add_plugins(MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(1.0 / TICK_HZ))))
        .add_plugins(LogPlugin::default())
        .insert_resource(Inbox(Mutex::new(inbox)))
        .insert_resource(Game { server: rooms::Server::new(dev), outs: HashMap::new(), last_report: Instant::now() })
        .add_systems(Startup, move || info!("flight sim server on :{port}{}", if dev { " (dev auth)" } else { "" }))
        .add_systems(Update, pump)
        .run();
}

/// Runs the rooms on everything the sockets received, and sends the replies.
fn pump(inbox: Res<Inbox>, mut game: ResMut<Game>) {
    let now = Instant::now();
    let mut outgoing = vec![];
    let mut rx = inbox.0.lock().unwrap();
    while let Ok(event) = rx.try_recv() {
        match event {
            NetEvent::Connected { conn, did, handle, out } => {
                info!("conn {conn}: {handle} ({did})");
                game.outs.insert(conn, out);
                outgoing.extend(game.server.connect(conn, did, handle));
            }
            NetEvent::Message(conn, msg) => {
                if let ClientMsg::Create { .. } | ClientMsg::Join { .. } = &msg {
                    info!("conn {conn}: {msg:?}");
                }
                outgoing.extend(game.server.handle(conn, msg, now));
            }
            NetEvent::Disconnected(conn) => {
                info!("conn {conn}: gone");
                game.outs.remove(&conn);
                outgoing.extend(game.server.disconnect(conn, now));
            }
        }
    }
    drop(rx);
    game.server.tick(now);
    for o in outgoing {
        match &o.msg {
            ServerMsg::Joined { code, you, players, .. } => info!("room {code}: player {you} joined ({} in the room)", players.len()),
            ServerMsg::Killed { victim, killer } => info!("player {victim} down, killer {killer:?}"),
            ServerMsg::Respawned(id) => info!("player {id} respawned"),
            _ => {}
        }
        let bytes = Bytes::from(encode(&o.msg));
        for conn in o.to {
            if let Some(tx) = game.outs.get(&conn) {
                let _ = tx.send(bytes.clone());
            }
        }
    }
    if now.duration_since(game.last_report) > Duration::from_secs(60) {
        game.last_report = now;
        info!("{} rooms, {} players", game.server.room_count(), game.server.player_count());
    }
}

async fn serve(port: u16, shared: Shared) {
    let app = Router::new()
        .route("/", get(|| async { "flight sim server" }))
        .route("/health", get(|| async { "ok" }))
        .route("/ws", get(upgrade))
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

async fn upgrade(ws: WebSocketUpgrade, State(shared): State<Shared>) -> impl IntoResponse {
    ws.max_message_size(MAX_FRAME).max_frame_size(MAX_FRAME).on_upgrade(move |socket| connection(socket, shared))
}

async fn connection(socket: WebSocket, shared: Shared) {
    let (mut sink, mut stream) = socket.split();
    let reply = |msg: ServerMsg| Message::Binary(Bytes::from(encode(&msg)));

    // first frame: Hello with a service-auth token
    let hello = match tokio::time::timeout(HELLO_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(Message::Binary(b)))) => decode::<ClientMsg>(&b),
        _ => None,
    };
    let Some(ClientMsg::Hello { version, token }) = hello else {
        let _ = sink.send(reply(ServerMsg::Error(ErrorCode::BadRequest))).await;
        return;
    };
    if version != PROTOCOL_VERSION {
        let _ = sink.send(reply(ServerMsg::Error(ErrorCode::Version))).await;
        return;
    }
    let user = match atproto::verify_service_auth(&token, protocol::AUTH_AUDIENCE, protocol::AUTH_LXM).await {
        Ok(user) => user,
        Err(e) => {
            info!("auth refused: {e}");
            let _ = sink.send(reply(ServerMsg::Error(ErrorCode::Auth))).await;
            return;
        }
    };
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
            Message::Binary(b) => b,
            Message::Close(_) => break,
            // (axum answers pings itself)
            _ => continue,
        };
        let now = Instant::now();
        allowance = (allowance + now.duration_since(last).as_secs_f32() * MAX_MSGS_PER_SEC).min(MAX_MSGS_PER_SEC);
        last = now;
        if allowance < 1.0 {
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
