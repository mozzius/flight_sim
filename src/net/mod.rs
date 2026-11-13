//! Multiplayer, phase 1: a free-for-all dogfight in rooms of up to eight.
//!
//! Each game flies its own jet and sends its state ~20 times a second
//! (`send`); the server (`src/server`) relays it, and everyone else draws
//! that jet ~100 ms in the past, interpolating between states
//! (`interpolate`). Shooters simulate their own bullets and missiles: tracers
//! and flares go out as events, missiles as positions (so victims see them
//! coming and can flare), and hits on other players are reported to the
//! server, which decides damage, kills and score.
//!
//! Other players' jets are ordinary aircraft entities with a [`Remote`]
//! marker (Team::Red: everyone's a hostile), without physics or a pilot;
//! their missiles carry [`RemoteMissile`] and aren't simulated here.
//!
//! Presence: while signed in the game stays connected in every mode, and
//! the server keeps us up to date on which Bluesky mutuals are online
//! ([`Net::friends`]); INVITE asks one to our room (made on the fly if we're
//! not in one), and an invite to theirs waits in [`Net::pending_invite`]
//! (the card in `friends`). JOIN on a friend who's in a room asks that
//! room's host ([`Net::request_join`]); a host's incoming requests wait in
//! [`Net::pending_request`] (the same card, ACCEPT / X).
//!
//! The challenge leaderboard rides the same connection: `crate::leaderboard`
//! calls [`Net::challenge_start`], [`Net::submit_score`] and
//! [`Net::request_board`] (queued until we're online), and reads the replies
//! back ([`Net::standing`], [`Net::board`]).
//!
//! The lobby: a new room waits (in the menu, `menu`'s lobby panel) until its
//! host starts it, baking the map meanwhile (`loading::Prebake`); joiners
//! wait there too, and once it's started they go straight to the game.
//!
//! Dev: `FLIGHT_SIM_MP=create[:CODE]|join:CODE` connects straight from the
//! menu (natively with `FLIGHT_SIM_DEV_USER`; requested codes need a dev
//! server); `FLIGHT_SIM_DEV_INVITE=<handle>` and
//! `FLIGHT_SIM_DEV_ACCEPT_INVITES=1`, `FLIGHT_SIM_DEV_START=<n>`,
//! `FLIGHT_SIM_DEV_REQUEST=<handle>`, `FLIGHT_SIM_DEV_ACCEPT_REQUESTS=1` and
//! `FLIGHT_SIM_DEV_HIT=1` (see `dev_hooks` and `send`). See `auth` for
//! sign-in and the server URL.

pub mod auth;
pub mod protocol;
mod transport;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU8, Ordering};

use bevy::prelude::*;
use rand::RngExt;

use crate::{
    environment::{Biome, Environment, Picks, TimeOfDay, Weather},
    flight::{ControlInput, Disabled, FlightState},
    game::{Explosion, Health, Models, Player, spawn_remote_aircraft},
    loading::{GameState, begin_map, round_seed},
    modes::GameMode,
    weapons::{Bullet, Flare, Missile, ProjectileAssets, Weapons, decoy_missiles, spawn_flare, spawn_missile, spawn_tracer},
};
use protocol::*;
use transport::{Transport, TransportEvent};

/// States per second.
const SEND_HZ: f64 = 20.0;
/// How far in the past other players are drawn (absorbs jitter).
const INTERP_DELAY: f64 = 0.1;
/// Past the newest state, keep going on its velocity for at most this long.
const MAX_EXTRAPOLATE: f64 = 0.25;
const PING_SECS: f64 = 5.0;

pub struct NetPlugin;

impl Plugin for NetPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Net>()
            .add_message::<NetHit>()
            .add_systems(OnEnter(GameState::Menu), (leave_room, dev_auto_start).chain())
            .add_systems(OnExit(GameState::Playing), forget_entities)
            .add_systems(
                Update,
                (
                    pump,
                    (apply, interpolate, tag_own_missiles, send)
                        .chain()
                        .run_if(in_state(GameState::Playing))
                        // (not a single-player round we're leaving for a room)
                        .run_if(|mode: Res<GameMode>| *mode == GameMode::Multiplayer),
                )
                    .chain(),
            );
    }
}

/// Another player's jet.
#[derive(Component, Clone, Copy)]
pub struct Remote {
    pub id: PlayerId,
}

/// Their handle (the HUD's name tag).
#[derive(Component)]
pub struct RemoteName(pub String);

/// Another player's missile: drawn where their game says it is.
#[derive(Component)]
pub struct RemoteMissile;

/// One of ours, with the id the others know it by.
#[derive(Component)]
pub struct OwnMissile(pub u16);

/// Our bullet or missile hit another player's jet (sent to the server).
#[derive(Message, Clone, Copy)]
pub struct NetHit {
    pub target: Entity,
    pub weapon: Weapon,
    pub damage: f32,
}

/// The connection. While signed in the game stays connected in every mode
/// (presence: the server tells us which mutuals are online, and passes on
/// invites); rooms come and go over the same connection.
#[derive(Default, PartialEq, Eq, Debug, Clone, Copy)]
enum Phase {
    #[default]
    Idle,
    /// Waiting for the page's service-auth token.
    Token,
    Connecting,
    /// Sent Hello, waiting for Welcome.
    Hello,
    /// Signed in.
    Online,
}

enum Intent {
    Create(RoomMap, Option<String>),
    Join(String),
}

/// An invite to a friend's room, waiting for JOIN or ✕.
#[derive(Clone, Debug)]
pub struct PendingInvite {
    pub from: Friend,
    pub code: String,
    at: f64,
}

/// Someone asking to join our room (we host it), waiting for ACCEPT or ✕.
#[derive(Clone, Debug)]
pub struct PendingRequest {
    pub from: Friend,
    at: f64,
}

/// Who shot us down last (multiplayer), for the death screen.
#[derive(Clone, Debug, PartialEq)]
pub struct Death {
    pub killer: String,
    /// Our kills on them, theirs on us (this room).
    pub record: (u16, u16),
}

#[derive(Resource, Default)]
pub struct Net {
    phase: Phase,
    transport: Option<Transport>,
    token_request: Option<auth::TokenRequest>,
    token: String,
    /// Who the connection is signed in as (a sign-out or a new account reconnects).
    did: Option<String>,
    /// Waiting to go once we're online.
    intent: Option<Intent>,
    /// Create / Join sent, waiting for the room.
    awaiting: bool,
    /// The menu's one-word status (an error), and since when (it clears after `LABEL_SECS`).
    label: Option<&'static str>,
    label_seen: Option<(&'static str, f64)>,
    /// Reconnect no sooner than this (real seconds), and the next wait.
    retry_at: f64,
    backoff: f64,
    /// The server wants a newer game: stop trying.
    outdated: bool,
    /// Online mutuals, by handle.
    friends: Vec<Friend>,
    /// Bumped whenever `friends` changes (the menus rebuild their rows).
    friends_version: u32,
    /// When we last invited each friend (DID → real seconds).
    invited: HashMap<String, f64>,
    invite: Option<PendingInvite>,
    /// When we last asked to join each friend's room (DID → real seconds).
    requested: HashMap<String, f64>,
    requests: VecDeque<PendingRequest>,
    dev_invited: bool,
    dev_requested: bool,
    /// (dev) when the friend to ask was first seen in a room / the lobby filled up.
    dev_since: Option<f64>,
    dev_start_since: Option<f64>,
    last_dev_hit: f64,

    // ---- the challenge leaderboard ----
    /// Leaderboard messages waiting for us to be online.
    outbox: Vec<ClientMsg>,
    /// Our best run and rank, as of our last submission.
    standing: Option<Standing>,
    /// The boards as last received: (rows, where we stand).
    boards: HashMap<BoardKind, (Vec<BoardRow>, Option<Standing>)>,
    /// Bumped whenever a board arrives.
    board_version: u32,

    // ---- the room ----
    in_room: bool,
    code: Option<String>,
    me: Option<PlayerId>,
    host: Option<PlayerId>,
    /// Flying (else the lobby).
    started: bool,
    room_map: Option<RoomMap>,
    /// Joined a room that's in the lobby: to the menu.
    to_lobby: bool,
    death: Option<Death>,
    peers: HashMap<PlayerId, Peer>,
    scores: Vec<(PlayerId, u16, u16)>,
    /// Gameplay messages for `apply`.
    inbox: Vec<ServerMsg>,
    /// Joined a room: start the round on its map.
    joined: Option<RoomMap>,
    /// Out of the room while flying in it (the connection dropped, or a
    /// join failed): back to the menu.
    lost: bool,
    last_send: f64,
    last_msg: f64,
    next_missile: u16,
    /// Our tracers / flares since the last batch (with when they were fired).
    shots: Vec<(f64, Shot)>,
    flares: Vec<(f64, Shot)>,
    had_missiles: bool,
    /// We've had a jet this round (so "no jet" means dead, not loading).
    flown: bool,
    last_pose: Option<(Vec3, Quat)>,
    /// The server has us down; ignore damage until it confirms the respawn.
    dead_on_server: bool,
    awaiting_respawn: bool,
}

struct Peer {
    did: String,
    handle: String,
    entity: Option<Entity>,
    buffer: VecDeque<PlayerState>,
    /// Our clock minus theirs (plus the quickest transit seen).
    offset: Option<f64>,
    /// (their time it was fired, is a flare, shot)
    fire: VecDeque<(f64, bool, Shot)>,
    missiles: HashMap<u16, PeerMissile>,
}

struct PeerMissile {
    entity: Entity,
    /// (their time, position, velocity)
    buffer: VecDeque<(f64, Vec3, Vec3)>,
    target: Option<PlayerId>,
    born: f64,
    /// Their time it went (detonated or expired).
    gone: Option<f64>,
}

static ME: AtomicU8 = AtomicU8::new(0);

/// Invites disappear after this long.
pub const INVITE_SECS: f64 = 20.0;
/// An invited friend's button says SENT this long (the server's per-friend limit).
const SENT_SECS: f64 = 10.0;
/// Errors clear after this long.
const LABEL_SECS: f64 = 4.0;
const FIRST_RETRY: f64 = 2.0;
const MAX_RETRY: f64 = 60.0;

/// The room map for a new room with these picks (random where unpicked).
fn new_map(picks: Picks) -> RoomMap {
    let seed = round_seed();
    let env = Environment::choose(seed, picks);
    let index = |i: Option<usize>| i.unwrap_or(0) as u8;
    RoomMap {
        seed,
        biome: index(Biome::ALL.iter().position(|b| *b == env.biome)),
        time: index(TimeOfDay::ALL.iter().position(|t| *t == env.time)),
        weather: index(Weather::ALL.iter().position(|w| *w == env.weather)),
    }
}

/// A room map's picks (all set: the creator resolved them).
pub fn room_picks(map: RoomMap) -> Picks {
    Picks {
        biome: Biome::ALL.get(map.biome as usize).copied(),
        time: TimeOfDay::ALL.get(map.time as usize).copied(),
        weather: Weather::ALL.get(map.weather as usize).copied(),
    }
}

impl Net {
    /// Creates a room on a fresh map with these picks.
    pub fn create(&mut self, picks: Picks) {
        self.start(Intent::Create(new_map(picks), None));
    }

    pub fn join(&mut self, code: &str) {
        self.start(Intent::Join(normalize_code(code)));
    }

    /// The menu's status word: what went wrong, or that we're on our way.
    pub fn label(&self) -> Option<&'static str> {
        if self.intent.is_some() || self.awaiting { Some("CONNECTING") } else { self.label }
    }

    pub fn room_code(&self) -> Option<&str> {
        self.code.as_deref().filter(|_| self.in_room)
    }

    /// In a room that hasn't started: its map (being baked meanwhile).
    pub fn lobby_map(&self) -> Option<RoomMap> {
        self.room_map.filter(|_| self.in_room && !self.started)
    }

    pub fn in_lobby(&self) -> bool {
        self.lobby_map().is_some()
    }

    pub fn is_host(&self) -> bool {
        self.in_room && self.host.is_some() && self.host == self.me
    }

    /// Everyone in the room: (handle, hosts it, is us), in id order.
    pub fn players(&self) -> Vec<(String, bool, bool)> {
        let Some(me) = self.me.filter(|_| self.in_room) else { return vec![] };
        let my_handle = crate::identity::current_user().map(|u| u.handle).unwrap_or_else(|| "you".into());
        let mut rows: Vec<(PlayerId, String)> = self.peers.iter().map(|(id, p)| (*id, p.handle.clone())).collect();
        rows.push((me, my_handle));
        rows.sort_by_key(|r| r.0);
        rows.into_iter().map(|(id, h)| (h, Some(id) == self.host, id == me)).collect()
    }

    /// Whether a friend (by DID) is in our room.
    pub fn in_my_room(&self, did: &str) -> bool {
        self.in_room && self.peers.values().any(|p| p.did == did)
    }

    /// START (the host, in the lobby).
    pub fn start_game(&mut self) {
        if self.is_host() && !self.started {
            info!("multiplayer: starting room {:?}", self.code);
            self.send(&ClientMsg::Start);
        }
    }

    /// LEAVE (the lobby): out of the room, still online.
    pub fn leave(&mut self) {
        if self.in_room {
            self.send(&ClientMsg::Leave);
            self.clear_room();
        }
    }

    /// JOIN on a friend who's in a room: asks its host to let us in.
    pub fn request_join(&mut self, did: &str, now: f64) {
        if self.phase != Phase::Online || self.request_sent(did, now) {
            return;
        }
        info!("multiplayer: asking to join {did}'s room");
        self.requested.insert(did.to_string(), now);
        self.send(&ClientMsg::RequestJoin { to: did.to_string() });
        self.friends_version += 1;
    }

    /// Whether a friend's JOIN button says SENT (waiting for their host).
    pub fn request_sent(&self, did: &str, now: f64) -> bool {
        self.requested.get(did).is_some_and(|t| now - t < JOIN_REQUEST_SECS as f64)
    }

    /// The oldest join request waiting for us (the host).
    pub fn pending_request(&self) -> Option<&PendingRequest> {
        self.requests.front()
    }

    /// ACCEPT / ✕ on a join request.
    pub fn answer_request(&mut self, accept: bool) {
        if let Some(r) = self.requests.pop_front() {
            info!("multiplayer: {} {}", if accept { "letting in" } else { "turning down" }, r.from.handle);
            self.send(&ClientMsg::AnswerJoin { did: r.from.did, accept });
        }
    }

    /// A challenge round began on this map (the server times the run).
    pub fn challenge_start(&mut self, seed: u32) {
        self.standing = None;
        self.queue(ClientMsg::ChallengeStart { seed });
    }

    /// The challenge round ended.
    pub fn submit_score(&mut self, run: Run) {
        info!("leaderboard: submitting {run:?}");
        self.queue(ClientMsg::SubmitScore(run));
    }

    pub fn request_board(&mut self, kind: BoardKind) {
        let msg = ClientMsg::Leaderboard(kind);
        if !self.outbox.contains(&msg) {
            self.queue(msg);
        }
    }

    /// Our best challenge run and its global rank (after this round's submission).
    pub fn standing(&self) -> Option<Standing> {
        self.standing
    }

    /// A board, once it's arrived: (rows, where we stand).
    pub fn board(&self, kind: BoardKind) -> Option<&(Vec<BoardRow>, Option<Standing>)> {
        self.boards.get(&kind)
    }

    pub fn board_version(&self) -> u32 {
        self.board_version
    }

    pub fn online(&self) -> bool {
        self.phase == Phase::Online
    }

    /// Sends now if we're online, else once we are (a few at most).
    fn queue(&mut self, msg: ClientMsg) {
        if self.phase == Phase::Online {
            self.send(&msg);
        } else if self.outbox.len() < 8 {
            self.outbox.push(msg);
        }
    }

    /// Who shot us down (until we fly again).
    pub fn death(&self) -> Option<&Death> {
        self.death.as_ref()
    }

    pub fn friends(&self) -> &[Friend] {
        &self.friends
    }

    pub fn friends_version(&self) -> u32 {
        self.friends_version
    }

    /// Asks a friend (by DID) to our room; not in one, the server makes one
    /// with these picks first.
    pub fn invite(&mut self, did: &str, picks: Picks, now: f64) {
        if self.phase != Phase::Online || self.invited.get(did).is_some_and(|t| now - t < SENT_SECS) {
            return;
        }
        info!("multiplayer: inviting {did}");
        self.invited.insert(did.to_string(), now);
        self.send(&ClientMsg::Invite { to: did.to_string(), map: new_map(picks) });
        self.friends_version += 1;
    }

    /// Whether a friend's button says SENT.
    pub fn invite_sent(&self, did: &str, now: f64) -> bool {
        self.invited.get(did).is_some_and(|t| now - t < SENT_SECS)
    }

    pub fn pending_invite(&self) -> Option<&PendingInvite> {
        self.invite.as_ref()
    }

    /// JOIN: leaves whatever we're doing for the friend's room.
    pub fn accept_invite(&mut self) {
        if let Some(invite) = self.invite.take() {
            info!("multiplayer: joining {}'s room {}", invite.from.handle, invite.code);
            self.join(&invite.code);
        }
    }

    pub fn dismiss_invite(&mut self) {
        self.invite = None;
    }

    /// (handle, kills, deaths, is us), most kills first.
    pub fn scoreboard(&self) -> Vec<(String, u16, u16, bool)> {
        let mut rows: Vec<_> = self
            .scores
            .iter()
            .map(|(id, k, d)| {
                let me = Some(*id) == self.me;
                let name = if me {
                    crate::identity::current_user().map(|u| u.handle).unwrap_or_else(|| "you".into())
                } else {
                    self.peers.get(id).map(|p| p.handle.clone()).unwrap_or_default()
                };
                (name, *k, *d, me)
            })
            .collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
        rows
    }

    fn start(&mut self, intent: Intent) {
        self.label = None;
        if self.phase == Phase::Online {
            self.send_intent(intent);
            return;
        }
        self.intent = Some(intent);
        if self.phase == Phase::Idle {
            self.connect();
        }
    }

    fn send_intent(&mut self, intent: Intent) {
        self.awaiting = true;
        match intent {
            Intent::Create(map, code) => self.send(&ClientMsg::Create { map, code }),
            Intent::Join(code) => self.send(&ClientMsg::Join { code }),
        }
    }

    fn connect(&mut self) {
        self.phase = Phase::Token;
        self.did = crate::identity::current_user().map(|u| u.did);
        self.token_request = Some(auth::request_token());
    }

    /// Drops the connection (and the room), and schedules a reconnect.
    fn disconnect(&mut self, label: Option<&'static str>, now: f64) {
        if let Some(l) = label {
            warn!("multiplayer: {l}");
        }
        let had_intent = self.intent.take().is_some() || self.awaiting;
        if self.in_room || had_intent {
            self.label = label;
        }
        self.lost |= self.in_room;
        self.clear_room();
        self.phase = Phase::Idle;
        self.transport = None;
        self.token_request = None;
        self.token.clear();
        self.did = None;
        self.awaiting = false;
        self.friends.clear();
        self.friends_version += 1;
        self.invite = None;
        self.requested.clear();
        self.backoff = if self.backoff == 0.0 { FIRST_RETRY } else { (self.backoff * 2.0).min(MAX_RETRY) };
        self.retry_at = now + self.backoff;
    }

    /// Forgets the room (the connection stays).
    fn clear_room(&mut self) {
        self.in_room = false;
        self.code = None;
        self.me = None;
        self.host = None;
        self.started = false;
        self.room_map = None;
        self.to_lobby = false;
        self.death = None;
        self.requests.clear();
        self.peers.clear();
        self.scores.clear();
        self.inbox.clear();
        self.joined = None;
        self.next_missile = 0;
        self.shots.clear();
        self.flares.clear();
        self.had_missiles = false;
        self.flown = false;
        self.last_pose = None;
        self.dead_on_server = false;
        self.awaiting_respawn = false;
        ME.store(0, Ordering::Relaxed);
    }

    fn send(&self, msg: &ClientMsg) {
        if let Some(t) = &self.transport {
            t.send(encode(msg));
        }
    }

    fn set_friend(&mut self, friend: Friend) {
        match self.friends.iter_mut().find(|f| f.did == friend.did) {
            Some(f) => *f = friend,
            None => self.friends.push(friend),
        }
        self.friends.sort_by(|a, b| a.handle.cmp(&b.handle));
        self.friends_version += 1;
    }

    fn receive(&mut self, msg: ServerMsg, commands: &mut Commands, now: f64) {
        match msg {
            ServerMsg::Welcome { handle } => {
                info!("multiplayer: online as {handle}");
                self.phase = Phase::Online;
                self.backoff = 0.0;
                for msg in std::mem::take(&mut self.outbox) {
                    self.send(&msg);
                }
                if let Some(intent) = self.intent.take() {
                    self.send_intent(intent);
                }
            }
            // before Welcome: the server's closing on us
            ServerMsg::Error(code) if self.phase != Phase::Online => {
                self.outdated |= code == ErrorCode::Version;
                self.disconnect(Some(code.label()), now);
            }
            // a room we couldn't get into (the server has us in none now)
            ServerMsg::Error(code) => {
                warn!("multiplayer: {}", code.label());
                self.awaiting = false;
                self.label = Some(code.label());
                self.lost |= self.in_room;
                self.clear_room();
            }
            ServerMsg::Joined { code, map, you, players, host, started } => {
                info!("multiplayer: room {code}, player {you}, {} here{}", players.len(), if started { "" } else { " (lobby)" });
                self.clear_room();
                self.awaiting = false;
                self.in_room = true;
                self.code = Some(code);
                self.me = Some(you);
                self.host = Some(host);
                self.started = started;
                self.room_map = Some(map);
                ME.store(you, Ordering::Relaxed);
                self.peers = players.into_iter().filter(|p| p.id != you).map(|p| (p.id, Peer::new(p.did, p.handle))).collect();
                self.label = None;
                if started {
                    self.joined = Some(map);
                } else {
                    self.to_lobby = true;
                }
                self.lost = false;
                self.friends_version += 1;
            }
            ServerMsg::Host(id) if self.in_room => self.host = Some(id),
            ServerMsg::Started if self.in_room && !self.started => {
                info!("multiplayer: the room's started");
                self.started = true;
                self.joined = self.room_map;
            }
            ServerMsg::PlayerJoined(info) if Some(info.id) != self.me => {
                self.peers.insert(info.id, Peer::new(info.did, info.handle));
                self.friends_version += 1;
            }
            ServerMsg::PlayerLeft(id) => {
                if let Some(p) = self.peers.remove(&id) {
                    p.despawn(commands);
                }
                self.friends_version += 1;
            }
            ServerMsg::Scores(scores) => self.scores = scores,
            ServerMsg::Friends(mut list) => {
                list.sort_by(|a, b| a.handle.cmp(&b.handle));
                info!("multiplayer: {} friends online", list.len());
                self.friends = list;
                self.friends_version += 1;
            }
            ServerMsg::FriendUpdate(f) => self.set_friend(f),
            ServerMsg::FriendGone(did) => {
                self.friends.retain(|f| f.did != did);
                self.friends_version += 1;
            }
            ServerMsg::Invited { from, code } => {
                info!("multiplayer: {} invites us to {code}", from.handle);
                self.invite = Some(PendingInvite { from, code, at: now });
            }
            ServerMsg::InviteFailed(did) => {
                info!("multiplayer: invite to {did} failed");
                self.invited.remove(&did);
                self.friends_version += 1;
            }
            ServerMsg::JoinRequest(from) if self.is_host() => {
                info!("multiplayer: {} asks to join", from.handle);
                self.requests.retain(|r| r.from.did != from.did);
                self.requests.push_back(PendingRequest { from, at: now });
            }
            ServerMsg::JoinDeclined(did) => {
                info!("multiplayer: join request to {did} declined");
                self.requested.remove(&did);
                self.label = Some("DECLINED");
                self.friends_version += 1;
            }
            ServerMsg::Standing(standing) => {
                info!("leaderboard: standing {standing:?}");
                self.standing = standing;
            }
            ServerMsg::LeaderboardData { kind, rows, you } => {
                info!("leaderboard: {kind:?} board, {} rows", rows.len());
                self.boards.insert(kind, (rows, you));
                self.board_version += 1;
            }
            other if self.in_room => self.inbox.push(other),
            _ => {}
        }
    }

    fn entity_of(&self, id: PlayerId, me: Option<Entity>) -> Option<Entity> {
        if Some(id) == self.me { me } else { self.peers.get(&id).and_then(|p| p.entity) }
    }
}

impl Peer {
    fn new(did: String, handle: String) -> Self {
        Self { did, handle, entity: None, buffer: default(), offset: None, fire: default(), missiles: default() }
    }

    fn despawn(&self, commands: &mut Commands) {
        if let Some(e) = self.entity {
            commands.entity(e).try_despawn();
        }
        for m in self.missiles.values() {
            commands.entity(m.entity).try_despawn();
        }
    }

    /// Their clock's offset from ours: the quickest arrival seen, creeping
    /// up slowly in case the route got longer.
    fn clock(&mut self, their_t: f64, now: f64) {
        let sample = now - their_t;
        self.offset = Some(match self.offset {
            Some(o) if sample >= o => o + (sample - o) * 0.01,
            _ => sample,
        });
    }

    fn push_state(&mut self, state: PlayerState, now: f64) {
        self.clock(state.t, now);
        if self.buffer.back().is_some_and(|b| b.t >= state.t) {
            return;
        }
        self.buffer.push_back(state);
        while self.buffer.front().is_some_and(|f| f.t < state.t - 2.0) {
            self.buffer.pop_front();
        }
    }
}

/// Where we (re)spawn: around the middle of the map by player id, facing in.
pub fn spawn_point() -> (Vec3, Vec3) {
    let id = ME.load(Ordering::Relaxed) as f32;
    // (screenshot runs: the same spots every time)
    let jitter = if crate::dev::headless() { 0.0 } else { 1.0 };
    let mut rng = rand::rng();
    let angle = id * 2.4 + rng.random_range(-0.4..0.4) * jitter;
    let r = 3_500.0 + rng.random_range(0.0..2_000.0) * jitter;
    let pos = Vec3::new(angle.sin() * r, 1_800.0, angle.cos() * r);
    let inward = Vec3::new(-pos.x, 0.0, -pos.z).normalize_or(Vec3::NEG_Z);
    let heading = Quat::from_rotation_y(rng.random_range(-0.35..0.35) * jitter) * inward;
    (pos, heading)
}

pub enum DevAuto {
    Create(Option<String>),
    Join(String),
}

/// `FLIGHT_SIM_MP=create[:CODE]|join:CODE`.
pub fn dev_auto() -> Option<DevAuto> {
    let s = crate::environment::setting("FLIGHT_SIM_MP")?;
    let (verb, code) = s.split_once(':').map_or((s.as_str(), None), |(v, c)| (v, Some(c.to_string())));
    match verb.to_ascii_lowercase().as_str() {
        "create" => Some(DevAuto::Create(code)),
        "join" => code.map(DevAuto::Join),
        _ => {
            warn!("unknown FLIGHT_SIM_MP {s:?}");
            None
        }
    }
}

fn dev_auto_start(mut net: ResMut<Net>, mut done: Local<bool>) {
    if std::mem::replace(&mut *done, true) {
        return;
    }
    match dev_auto() {
        Some(DevAuto::Create(code)) => net.start(Intent::Create(new_map(Picks::from_settings()), code)),
        Some(DevAuto::Join(code)) => net.join(&code),
        None => {}
    }
}

/// Back at the menu from a game: out of the room (still online). (A room
/// that's in the lobby is why we're here: it stays.)
fn leave_room(mut net: ResMut<Net>) {
    if net.in_room && net.started {
        net.leave();
    }
}

/// The connection: presence while signed in, sign-in, the socket, invites,
/// and the room's comings and goings.
fn pump(
    mut net: ResMut<Net>,
    time: Res<Time<Real>>,
    mut commands: Commands,
    state: Res<State<GameState>>,
    mut next: ResMut<NextState<GameState>>,
) {
    let now = time.elapsed_secs_f64();
    // online whenever signed in (a sign-out or another account: reconnect)
    let user = crate::identity::current_user().map(|u| u.did);
    if net.phase != Phase::Idle && net.did != user {
        net.disconnect(None, now);
        net.retry_at = now;
        // (someone else's leaderboard)
        net.standing = None;
        net.boards.clear();
        net.outbox.clear();
        net.board_version += 1;
    }
    if net.phase == Phase::Idle && user.is_some() && !net.outdated && now >= net.retry_at {
        net.connect();
    }
    if net.phase == Phase::Token
        && let Some(result) = net.token_request.as_mut().and_then(|t| t.poll())
    {
        match result {
            Ok(token) => {
                let url = auth::server_url();
                info!("multiplayer: connecting to {url}");
                net.token = token;
                net.transport = Some(Transport::connect(&url));
                net.phase = Phase::Connecting;
            }
            Err(e) => {
                warn!("multiplayer: no token: {e}");
                net.disconnect(Some("SIGN IN"), now);
            }
        }
    }
    let events = net.transport.as_ref().map(|t| t.poll()).unwrap_or_default();
    for event in events {
        match event {
            TransportEvent::Open => {
                let token = std::mem::take(&mut net.token);
                net.send(&ClientMsg::Hello { version: PROTOCOL_VERSION, token });
                net.phase = Phase::Hello;
                net.last_msg = now;
            }
            TransportEvent::Message(bytes) => match decode::<ServerMsg>(&bytes) {
                Some(msg) => net.receive(msg, &mut commands, now),
                None => warn!("multiplayer: unreadable message"),
            },
            TransportEvent::Closed => net.disconnect(Some("OFFLINE"), now),
        }
        if net.phase == Phase::Idle {
            break;
        }
    }
    // (gameplay messages only mean something while flying)
    if *state.get() != GameState::Playing {
        net.inbox.clear();
    }
    if net.invite.as_ref().is_some_and(|i| now - i.at > INVITE_SECS) {
        net.invite = None;
    }
    while net.requests.front().is_some_and(|r| now - r.at > JOIN_REQUEST_SECS as f64) {
        net.requests.pop_front();
    }
    // errors clear after a few seconds
    match (net.label, net.label_seen) {
        (Some(l), Some((seen, at))) if l == seen => {
            if now - at > LABEL_SECS {
                net.label = None;
                net.label_seen = None;
            }
        }
        (Some(l), _) => net.label_seen = Some((l, now)),
        (None, _) => net.label_seen = None,
    }
    dev_hooks(&mut net, now);
    if std::mem::take(&mut net.to_lobby) && *state.get() != GameState::Menu {
        next.set(GameState::Menu);
    }
    if let Some(map) = net.joined.take() {
        begin_map(&mut commands, &mut next, GameMode::Multiplayer, map.seed, room_picks(map));
    }
    if std::mem::take(&mut net.lost) && *state.get() != GameState::Menu {
        next.set(GameState::Menu);
    }
    if net.phase == Phase::Online && now - net.last_msg > PING_SECS {
        net.send(&ClientMsg::Ping);
        net.last_msg = now;
    }
}

/// Dev: `FLIGHT_SIM_DEV_INVITE=<handle>` invites that friend once they're
/// online; `FLIGHT_SIM_DEV_ACCEPT_INVITES=1` joins any invite after 2 s;
/// `FLIGHT_SIM_DEV_START=<n>` (a host) starts the room 3 s after `n` players
/// are in its lobby; `FLIGHT_SIM_DEV_REQUEST=<handle>` asks to join that
/// friend's room 4 s after they're seen in one; `FLIGHT_SIM_DEV_ACCEPT_REQUESTS=1`
/// lets in any join request after 2 s.
fn dev_hooks(net: &mut Net, now: f64) {
    use crate::environment::setting;
    if let Some(n) = setting("FLIGHT_SIM_DEV_START").and_then(|n| n.parse::<usize>().ok())
        && net.is_host()
        && net.in_lobby()
    {
        if net.players().len() < n {
            net.dev_start_since = None;
        } else if now - *net.dev_start_since.get_or_insert(now) > 3.0 {
            net.dev_start_since = None;
            net.start_game();
        }
    }
    if !net.dev_requested
        && let Some(handle) = setting("FLIGHT_SIM_DEV_REQUEST")
    {
        let friend = net.friends.iter().find(|f| f.handle == handle && f.status == FriendStatus::InRoom).map(|f| f.did.clone());
        match friend {
            Some(did) if !net.in_my_room(&did) => {
                if now - *net.dev_since.get_or_insert(now) > 4.0 {
                    net.dev_requested = true;
                    net.request_join(&did, now);
                }
            }
            _ => net.dev_since = None,
        }
    }
    if net.requests.front().is_some_and(|r| now - r.at > 2.0) && setting("FLIGHT_SIM_DEV_ACCEPT_REQUESTS").as_deref() == Some("1") {
        net.answer_request(true);
    }
    if !net.dev_invited
        && let Some(handle) = crate::environment::setting("FLIGHT_SIM_DEV_INVITE")
        && let Some(did) = net.friends.iter().find(|f| f.handle == handle).map(|f| f.did.clone())
    {
        net.dev_invited = true;
        net.invite(&did, Picks::from_settings(), now);
    }
    if net.invite.as_ref().is_some_and(|i| now - i.at > 2.0)
        && crate::environment::setting("FLIGHT_SIM_DEV_ACCEPT_INVITES").as_deref() == Some("1")
    {
        net.accept_invite();
    }
}

/// The round's entities are gone (`game::end_round`).
fn forget_entities(mut net: ResMut<Net>) {
    for p in net.peers.values_mut() {
        p.entity = None;
        p.missiles.clear();
        p.fire.clear();
    }
    net.shots.clear();
    net.flares.clear();
    net.flown = false;
    net.last_pose = None;
    net.had_missiles = false;
}

/// Damage, kills and respawns from the server; other players' states and events into their buffers.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn apply(
    mut net: ResMut<Net>,
    time: Res<Time<Real>>,
    mut commands: Commands,
    assets: Res<ProjectileAssets>,
    mut player: Query<(Entity, &mut Health), With<Player>>,
    mut remotes: Query<&mut Health, (With<Remote>, Without<Player>)>,
) {
    let now = time.elapsed_secs_f64();
    let me = player.single().ok().map(|(e, _)| e);
    for msg in std::mem::take(&mut net.inbox) {
        match msg {
            ServerMsg::State { id, state } => {
                if let Some(p) = net.peers.get_mut(&id) {
                    p.push_state(state, now);
                }
            }
            ServerMsg::Fire { id, t, shots, flares } => {
                if let Some(p) = net.peers.get_mut(&id) {
                    p.clock(t, now);
                    let shots = shots.into_iter().map(|s| (false, s));
                    let flares = flares.into_iter().map(|s| (true, s));
                    for (flare, s) in shots.chain(flares) {
                        p.fire.push_back((t - s.age as f64, flare, s));
                    }
                }
            }
            ServerMsg::Missiles { id, t, missiles } => {
                let Some(p) = net.peers.get_mut(&id) else { continue };
                p.clock(t, now);
                for m in &missiles {
                    let (pos, vel) = (Vec3::from(m.pos), Vec3::from(m.vel));
                    let entry = p.missiles.entry(m.id).or_insert_with(|| {
                        let missile = Missile {
                            velocity: vel,
                            target: None,
                            owner: p.entity.unwrap_or(Entity::PLACEHOLDER),
                            team: crate::game::Team::Red,
                            age: 0.0,
                            lateral: Vec3::ZERO,
                            last_target_velocity: None,
                            target_accel: Vec3::ZERO,
                            seeker: default(),
                        };
                        let e = spawn_missile(&mut commands, &assets, missile, missile_transform(pos, vel));
                        commands.entity(e).insert(RemoteMissile);
                        PeerMissile { entity: e, buffer: default(), target: None, born: t, gone: None }
                    });
                    entry.buffer.push_back((t, pos, vel));
                    while entry.buffer.len() > 40 {
                        entry.buffer.pop_front();
                    }
                    entry.target = m.target;
                }
                for (mid, m) in p.missiles.iter_mut() {
                    if m.gone.is_none() && !missiles.iter().any(|x| x.id == *mid) {
                        m.gone = Some(t);
                    }
                }
            }
            ServerMsg::Damage { target, attacker, health } => {
                let attacker = net.entity_of(attacker, me);
                if Some(target) == net.me {
                    if !net.awaiting_respawn
                        && let Ok((_, mut h)) = player.single_mut()
                    {
                        h.current = health;
                        h.last_attacker = attacker;
                    }
                } else if let Some(mut h) = net.entity_of(target, me).and_then(|e| remotes.get_mut(e).ok()) {
                    h.current = health;
                    h.last_attacker = attacker;
                }
            }
            ServerMsg::Killed { victim, killer, record } => {
                info!("multiplayer: player {victim} down (killer {killer:?}, {record:?})");
                if Some(victim) == net.me {
                    let handle = killer.and_then(|k| net.peers.get(&k)).map(|p| p.handle.clone());
                    // (theirs on us, ours on them) → ours first
                    net.death = handle.map(|killer| Death { killer, record: (record.1, record.0) });
                }
                let killer = killer.and_then(|k| net.entity_of(k, me));
                if Some(victim) == net.me {
                    if !net.awaiting_respawn {
                        net.dead_on_server = true;
                        if let Ok((_, mut h)) = player.single_mut() {
                            h.current = 0.0;
                            h.last_attacker = killer.or(h.last_attacker);
                        }
                    }
                } else if let Some(mut h) = net.entity_of(victim, me).and_then(|e| remotes.get_mut(e).ok()) {
                    // (`game::handle_damage` takes it from here: the kill banner if it was us)
                    h.current = 0.0;
                    h.last_attacker = killer.or(h.last_attacker);
                }
            }
            ServerMsg::Respawned(id) => {
                info!("multiplayer: player {id} respawned");
                if Some(id) == net.me {
                    net.awaiting_respawn = false;
                    net.dead_on_server = false;
                } else if let Some(p) = net.peers.get_mut(&id) {
                    if let Some(e) = p.entity.take() {
                        commands.entity(e).try_despawn();
                    }
                    p.buffer.clear();
                }
            }
            _ => {}
        }
    }
}

fn missile_transform(pos: Vec3, vel: Vec3) -> Transform {
    Transform::from_translation(pos).looking_to(vel.normalize_or(Vec3::NEG_Z), Vec3::Y)
        * Transform::from_rotation(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2))
}

/// A state between two received ones.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Sample {
    pos: Vec3,
    rot: Quat,
    vel: Vec3,
    throttle: f32,
    g: f32,
    flags: u8,
    health: f32,
    status: Status,
}

impl From<&PlayerState> for Sample {
    fn from(s: &PlayerState) -> Self {
        Sample {
            pos: s.pos.into(),
            rot: Quat::from_array(s.rot).normalize(),
            vel: s.vel.into(),
            throttle: s.throttle,
            g: s.g,
            flags: s.flags,
            health: s.health,
            status: s.status,
        }
    }
}

/// Cubic Hermite between two positions with their velocities.
fn hermite(p0: Vec3, v0: Vec3, p1: Vec3, v1: Vec3, span: f32, u: f32) -> Vec3 {
    let (u2, u3) = (u * u, u * u * u);
    p0 * (2.0 * u3 - 3.0 * u2 + 1.0) + v0 * span * (u3 - 2.0 * u2 + u) + p1 * (3.0 * u2 - 2.0 * u3) + v1 * span * (u3 - u2)
}

fn sample(buffer: &VecDeque<PlayerState>, t: f64) -> Option<Sample> {
    let first = buffer.front()?;
    if t <= first.t {
        return Some(first.into());
    }
    match buffer.iter().position(|s| s.t > t) {
        None => {
            let mut s: Sample = buffer.back()?.into();
            let dt = (t - buffer.back()?.t).min(MAX_EXTRAPOLATE) as f32;
            s.pos += s.vel * dt;
            Some(s)
        }
        Some(i) => {
            let (a, b): (Sample, Sample) = ((&buffer[i - 1]).into(), (&buffer[i]).into());
            let span = (buffer[i].t - buffer[i - 1].t) as f32;
            let u = ((t - buffer[i - 1].t) as f32 / span.max(1e-4)).clamp(0.0, 1.0);
            Some(Sample {
                pos: hermite(a.pos, a.vel, b.pos, b.vel, span, u),
                rot: a.rot.slerp(b.rot, u),
                vel: a.vel.lerp(b.vel, u),
                throttle: a.throttle + (b.throttle - a.throttle) * u,
                g: a.g + (b.g - a.g) * u,
                ..a
            })
        }
    }
}

/// (position, velocity) along a missile's track.
fn sample_track(buffer: &VecDeque<(f64, Vec3, Vec3)>, t: f64) -> Option<(Vec3, Vec3)> {
    let first = buffer.front()?;
    if t <= first.0 {
        return Some((first.1, first.2));
    }
    match buffer.iter().position(|s| s.0 > t) {
        None => {
            let (lt, p, v) = *buffer.back()?;
            Some((p + v * (t - lt).min(MAX_EXTRAPOLATE) as f32, v))
        }
        Some(i) => {
            let ((t0, p0, v0), (t1, p1, v1)) = (buffer[i - 1], buffer[i]);
            let span = (t1 - t0) as f32;
            let u = ((t - t0) as f32 / span.max(1e-4)).clamp(0.0, 1.0);
            Some((hermite(p0, v0, p1, v1, span, u), v0.lerp(v1, u)))
        }
    }
}

/// Draws the other players (and their tracers, flares and missiles) where they were `INTERP_DELAY` ago.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn interpolate(
    mut net: ResMut<Net>,
    time: Res<Time<Real>>,
    mut commands: Commands,
    models: Res<Models>,
    assets: Res<ProjectileAssets>,
    me: Query<Entity, With<Player>>,
    mut remotes: Query<(&mut Transform, &mut FlightState, &mut Weapons, &mut Health, Has<Disabled>), With<Remote>>,
    mut their_missiles: Query<(&mut Transform, &mut Missile), (With<RemoteMissile>, Without<Remote>)>,
    mut our_missiles: Query<&mut Missile, (With<OwnMissile>, Without<RemoteMissile>)>,
    mut explosions: MessageWriter<Explosion>,
) {
    let now = time.elapsed_secs_f64();
    let me = me.single().ok();
    let my_id = net.me;
    let entities: HashMap<PlayerId, Entity> =
        net.peers.iter().filter_map(|(id, p)| p.entity.map(|e| (*id, e))).chain(my_id.zip(me)).collect();
    for (id, peer) in net.peers.iter_mut() {
        let Some(offset) = peer.offset else { continue };
        let render = now - offset - INTERP_DELAY;

        // the jet
        if let Some(s) = sample(&peer.buffer, render) {
            match s.status {
                Status::Dead => {
                    // crashed
                    if let Some(e) = peer.entity.take() {
                        if let Ok((t, ..)) = remotes.get(e) {
                            explosions.write(Explosion { position: t.translation, size: 25.0 });
                        }
                        commands.entity(e).try_despawn();
                    }
                }
                Status::Flying | Status::Down => match peer.entity {
                    None if s.status == Status::Flying => {
                        let transform = Transform::from_translation(s.pos).with_rotation(s.rot);
                        let e = spawn_remote_aircraft(&mut commands, &models, transform, s.vel, Remote { id: *id }, &peer.handle);
                        if s.health < MAX_HEALTH {
                            let mut health = Health::new(MAX_HEALTH);
                            health.current = s.health.max(1.0);
                            commands.entity(e).insert(health);
                        }
                        peer.entity = Some(e);
                    }
                    None => {}
                    Some(e) => {
                        if let Ok((mut t, mut fs, mut weapons, mut health, disabled)) = remotes.get_mut(e) {
                            t.translation = s.pos;
                            t.rotation = s.rot;
                            fs.velocity = s.vel;
                            fs.airspeed = s.vel.length();
                            fs.mach = fs.airspeed / 340.0;
                            fs.spool = s.throttle;
                            fs.afterburner_on = s.flags & PlayerState::AFTERBURNER != 0;
                            fs.g_load = s.g;
                            weapons.trigger_gun = s.flags & PlayerState::GUN != 0 && s.status == Status::Flying;
                            // (missed the Killed: still goes down in flames)
                            if s.status == Status::Down && !disabled && health.current > 0.0 {
                                health.current = 0.0;
                            }
                        }
                    }
                },
            }
        }

        // tracers and flares, when the jet that fired them gets there
        let owner = peer.entity.unwrap_or(Entity::PLACEHOLDER);
        let mut decoy = None;
        while let Some(&(t, flare, shot)) = peer.fire.front() {
            if t > render {
                break;
            }
            peer.fire.pop_front();
            let late = (render - t) as f32;
            if late > 1.0 {
                continue;
            }
            let vel = Vec3::from(shot.vel);
            let pos = Vec3::from(shot.pos) + vel * late;
            if flare {
                let f = spawn_flare(&mut commands, &assets, pos, vel, owner);
                decoy.get_or_insert(f);
            } else {
                spawn_tracer(&mut commands, &assets, pos, vel, owner, crate::game::Team::Red, true);
            }
        }
        // their flares can pull our missiles off them (we fly our missiles)
        if let (Some(flare), Some(target)) = (decoy, peer.entity) {
            decoy_missiles(target, flare, our_missiles.iter_mut().map(|m| m.into_inner()));
        }

        // their missiles
        peer.missiles.retain(|_, m| {
            if m.gone.is_some_and(|g| render >= g) {
                if let Some((p, _)) = m.buffer.back().map(|(_, p, v)| (*p, *v)) {
                    explosions.write(Explosion { position: p, size: 7.0 });
                }
                commands.entity(m.entity).try_despawn();
                return false;
            }
            if let (Some((pos, vel)), Ok((mut t, mut missile))) = (sample_track(&m.buffer, render), their_missiles.get_mut(m.entity)) {
                *t = missile_transform(pos, vel);
                missile.velocity = vel;
                missile.age = (render - m.born).max(0.0) as f32;
                missile.owner = owner;
                missile.target = m.target.and_then(|id| entities.get(&id).copied());
            }
            true
        });
    }
}

/// Numbers the missiles we fire.
fn tag_own_missiles(
    mut commands: Commands,
    mut net: ResMut<Net>,
    player: Query<Entity, With<Player>>,
    new: Query<(Entity, &Missile), (Added<Missile>, Without<RemoteMissile>)>,
) {
    let me = player.single().ok();
    for (e, m) in &new {
        if Some(m.owner) == me {
            let id = net.next_missile;
            net.next_missile = net.next_missile.wrapping_add(1);
            commands.entity(e).insert(OwnMissile(id));
        }
    }
}

/// Our state, shots, missiles and hits.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn send(
    mut net: ResMut<Net>,
    time: Res<Time<Real>>,
    player: Query<(Entity, &Transform, &FlightState, &ControlInput, &Weapons, Has<Disabled>), With<Player>>,
    spawned: Query<(), Added<Player>>,
    bullets: Query<(&Bullet, &Transform), Added<Bullet>>,
    flares: Query<(&Flare, &Transform), Added<Flare>>,
    missiles: Query<(&OwnMissile, &Transform, &Missile)>,
    remotes: Query<(&Remote, &Transform, Has<Disabled>)>,
    mut hits: MessageReader<NetHit>,
) {
    if !net.in_room {
        hits.clear();
        return;
    }
    let now = time.elapsed_secs_f64();
    if !spawned.is_empty() {
        net.flown = true;
        net.death = None;
        if net.dead_on_server {
            net.awaiting_respawn = true;
        }
        net.send(&ClientMsg::Respawn);
    }
    for hit in hits.read() {
        if let Ok((r, ..)) = remotes.get(hit.target) {
            net.send(&ClientMsg::Hit { target: r.id, weapon: hit.weapon, damage: hit.damage });
        }
    }
    let me = player.single().ok();
    // dev: `FLIGHT_SIM_DEV_HIT=1` lands gun hits on anyone in range (a quick, sure kill for tests)
    if let Some((_, t, ..)) = me
        && now - net.last_dev_hit > 0.05
        && crate::environment::setting("FLIGHT_SIM_DEV_HIT").as_deref() == Some("1")
    {
        net.last_dev_hit = now;
        for (r, rt, down) in &remotes {
            if !down && rt.translation.distance(t.translation) < GUN_MAX_RANGE * 0.9 {
                net.send(&ClientMsg::Hit { target: r.id, weapon: Weapon::Gun, damage: GUN_DAMAGE });
            }
        }
    }
    let mine = |owner: Entity| me.is_some_and(|m| m.0 == owner);
    for (b, t) in &bullets {
        if mine(b.owner) {
            net.shots.push((now, Shot { pos: t.translation.into(), vel: b.velocity.into(), age: 0.0 }));
        }
    }
    for (f, t) in &flares {
        if mine(f.owner) {
            net.flares.push((now, Shot { pos: t.translation.into(), vel: f.velocity.into(), age: 0.0 }));
        }
    }

    if now - net.last_send < 1.0 / SEND_HZ {
        return;
    }
    net.last_send = now;
    net.last_msg = now;

    let state = match me {
        Some((_, t, fs, input, weapons, disabled)) => {
            net.last_pose = Some((t.translation, t.rotation));
            let mut flags = 0;
            if fs.afterburner_on {
                flags |= PlayerState::AFTERBURNER;
            }
            if input.airbrake {
                flags |= PlayerState::AIRBRAKE;
            }
            if weapons.trigger_gun && weapons.gun_ammo > 0 {
                flags |= PlayerState::GUN;
            }
            Some(PlayerState {
                t: now,
                pos: t.translation.into(),
                rot: t.rotation.to_array(),
                vel: fs.velocity.into(),
                throttle: fs.spool,
                g: fs.g_load,
                flags,
                health: 0.0,
                status: if disabled { Status::Down } else { Status::Flying },
            })
        }
        // crashed (and not yet back): the server takes this as a death
        None if net.flown => net.last_pose.map(|(p, r)| PlayerState {
            t: now,
            pos: p.into(),
            rot: r.to_array(),
            vel: [0.0; 3],
            throttle: 0.0,
            g: 1.0,
            flags: 0,
            health: 0.0,
            status: Status::Dead,
        }),
        None => None,
    };
    if let Some(state) = state {
        net.send(&ClientMsg::State(state));
    }

    if !net.shots.is_empty() || !net.flares.is_empty() {
        let aged = |v: Vec<(f64, Shot)>| v.into_iter().map(|(at, s)| Shot { age: (now - at) as f32, ..s }).collect();
        let shots = aged(std::mem::take(&mut net.shots));
        let flares = aged(std::mem::take(&mut net.flares));
        net.send(&ClientMsg::Fire { t: now, shots, flares });
    }

    let list: Vec<MissileState> = missiles
        .iter()
        .map(|(id, t, m)| MissileState {
            id: id.0,
            pos: t.translation.into(),
            vel: m.velocity.into(),
            target: m.target.and_then(|e| remotes.get(e).ok()).map(|(r, ..)| r.id),
        })
        .collect();
    if !list.is_empty() || net.had_missiles {
        net.had_missiles = !list.is_empty();
        net.send(&ClientMsg::Missiles { t: now, missiles: list });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(t: f64, x: f32, vx: f32) -> PlayerState {
        PlayerState {
            t,
            pos: [x, 1000.0, 0.0],
            rot: [0.0, 0.0, 0.0, 1.0],
            vel: [vx, 0.0, 0.0],
            throttle: 0.5,
            g: 1.0,
            flags: 0,
            health: 100.0,
            status: Status::Flying,
        }
    }

    #[test]
    fn interpolates_between_states_and_extrapolates_a_little() {
        let buf: VecDeque<_> = (0..5).map(|i| state(i as f64 * 0.05, i as f32 * 10.0, 200.0)).collect();
        // constant velocity: Hermite is exact
        let s = sample(&buf, 0.075).unwrap();
        assert!((s.pos.x - 15.0).abs() < 1e-3, "{}", s.pos.x);
        // before the first: the first
        assert_eq!(sample(&buf, -1.0).unwrap().pos.x, 0.0);
        // past the last: on its velocity, but not forever
        assert!((sample(&buf, 0.25).unwrap().pos.x - 50.0).abs() < 1e-3);
        assert!((sample(&buf, 10.0).unwrap().pos.x - (40.0 + 200.0 * MAX_EXTRAPOLATE as f32)).abs() < 1e-3);
        assert!(sample(&VecDeque::new(), 0.0).is_none());
    }

    #[test]
    fn clock_offset_tracks_the_quickest_arrival() {
        let mut p = Peer::new("did:dev:a".into(), "a".into());
        p.push_state(state(10.0, 0.0, 0.0), 100.2);
        p.push_state(state(10.05, 0.0, 0.0), 100.21);
        assert!((p.offset.unwrap() - 90.16).abs() < 1e-9);
        // a slow one only nudges it
        p.push_state(state(10.1, 0.0, 0.0), 100.5);
        assert!(p.offset.unwrap() < 90.17);
        // out-of-order states are dropped
        p.push_state(state(10.0, 0.0, 0.0), 100.6);
        assert_eq!(p.buffer.len(), 3);
    }

    #[test]
    fn missile_tracks_interpolate() {
        let buf: VecDeque<_> = [(0.0, Vec3::ZERO, Vec3::X * 100.0), (0.1, Vec3::X * 10.0, Vec3::X * 100.0)].into();
        let (p, v) = sample_track(&buf, 0.05).unwrap();
        assert!((p.x - 5.0).abs() < 1e-4 && v.x == 100.0);
    }
}
