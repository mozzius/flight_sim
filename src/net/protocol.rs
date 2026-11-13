//! The multiplayer wire protocol, shared by the game and the server
//! (`src/server/main.rs` includes this file too, so it only uses serde and
//! plain types, no Bevy).
//!
//! Binary WebSocket frames, one postcard-encoded [`ClientMsg`] or
//! [`ServerMsg`] each. The first client frame is [`ClientMsg::Hello`] with a
//! service-auth token. Once welcomed the connection counts as online
//! (presence): the server pushes the player's online mutuals ([`Friend`]s)
//! whether or not they're in a room. Then `Create` / `Join` / `Invite` put
//! them in one, and `Leave` takes them out again (the connection stays up).
//!
//! Authority: each client flies its own jet and simulates its own bullets and
//! missiles; the server relays states and events, and decides damage, kills
//! and score from the hits shooters report ([`ClientMsg::Hit`]).

// (shared with the server: each side uses part of it)
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// Bumped whenever the messages change shape.
pub const PROTOCOL_VERSION: u16 = 2;

/// Service-auth audience and method the server checks tokens against.
pub const AUTH_AUDIENCE: &str = "did:web:flight.mozzius.dev";
pub const AUTH_LXM: &str = "dev.mozzius.flight.join";

pub const MAX_PLAYERS: usize = 8;
/// Room codes: four of these (no I, L or O: they read as 1 and 0).
pub const CODE_ALPHABET: &[u8; 23] = b"ABCDEFGHJKMNPQRSTUVWXYZ";
pub const CODE_LEN: usize = 4;

// ---- combat rules (the game's weapons use the same numbers) ----

pub const MAX_HEALTH: f32 = 100.0;
/// Per tracer hit.
pub const GUN_DAMAGE: f32 = 5.0;
/// Most a single missile can do to one aircraft.
pub const MISSILE_DAMAGE: f32 = 95.0;
/// Tracers per second (each stands for a few rounds).
pub const GUN_RATE: f32 = 25.0;
/// Farthest a gun hit is believed (bullets live 2.2 s at ~1 km/s).
pub const GUN_MAX_RANGE: f32 = 3_000.0;
/// How far from its last reported position a missile may claim a hit (latency slack).
pub const MISSILE_HIT_SLACK: f32 = 400.0;
/// A crash within this long of being hit credits the attacker.
pub const CRASH_CREDIT_SECS: f32 = 10.0;

/// Player ids are per room. Phase 2's server-flown AI would take ids from
/// [`AI_ID_START`] up, so they never clash with players.
pub type PlayerId = u8;
pub const AI_ID_START: PlayerId = 128;

pub fn valid_code(code: &str) -> bool {
    code.len() == CODE_LEN && code.bytes().all(|b| CODE_ALPHABET.contains(&b))
}

/// Upper-cases and drops anything that can't be in a code.
pub fn normalize_code(code: &str) -> String {
    code.trim().to_ascii_uppercase()
}

/// The map: the seed plus the environment, resolved by the room's creator
/// (indices into `Biome::ALL`, `TimeOfDay::ALL`, `Weather::ALL`), so every
/// client bakes and lights the same world.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoomMap {
    pub seed: u32,
    pub biome: u8,
    pub time: u8,
    pub weather: u8,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Flying,
    /// Shot down, falling.
    Down,
    /// No aircraft (crashed, waiting to respawn).
    Dead,
}

/// One aircraft's state, sent by its owner ~20 times a second.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct PlayerState {
    /// The sender's clock (seconds), for interpolation.
    pub t: f64,
    pub pos: [f32; 3],
    pub rot: [f32; 4],
    pub vel: [f32; 3],
    /// Engine spool 0..1.
    pub throttle: f32,
    pub g: f32,
    pub flags: u8,
    /// Relayed with the server's value (clients' own numbers are ignored).
    pub health: f32,
    pub status: Status,
}

impl PlayerState {
    pub const AFTERBURNER: u8 = 1;
    pub const AIRBRAKE: u8 = 2;
    pub const GUN: u8 = 4;
}

/// A tracer or a flare: where it was fired and how fast it's going.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Shot {
    pub pos: [f32; 3],
    pub vel: [f32; 3],
    /// Seconds before the batch's `t` it was fired.
    pub age: f32,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct MissileState {
    /// The shooter's id for it (unique per shooter).
    pub id: u16,
    pub pos: [f32; 3],
    pub vel: [f32; 3],
    /// Whom it's guiding on (None: a flare, or lost).
    pub target: Option<PlayerId>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weapon {
    Gun,
    /// The shooter's missile id.
    Missile(u16),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ClientMsg {
    Hello { version: u16, token: String },
    /// New room. `code` is only honoured on dev servers (for automated tests).
    Create { map: RoomMap, code: Option<String> },
    Join { code: String },
    State(PlayerState),
    /// Tracers and flares fired since the last batch.
    Fire { t: f64, shots: Vec<Shot>, flares: Vec<Shot> },
    /// Every live missile of the sender's (an empty list once the last is gone).
    Missiles { t: f64, missiles: Vec<MissileState> },
    /// Our bullet or missile hit someone; the server decides if it counts.
    Hit { target: PlayerId, weapon: Weapon, damage: f32 },
    /// Back in the air after going down.
    Respawn,
    /// Keeps the connection alive while not flying (menus, loading).
    Ping,
    /// Out of the room, still online.
    Leave,
    /// Invite an online mutual (by DID) to our room. Not in one: the server
    /// creates one on `map` first and puts us in it.
    Invite { to: String, map: RoomMap },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PlayerInfo {
    pub id: PlayerId,
    pub did: String,
    pub handle: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FriendStatus {
    /// Online, not in a room (menus, single player).
    Online,
    InRoom,
    /// In a room with no seat left.
    RoomFull,
}

/// An online mutual (they follow us and we follow them on Bluesky).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Friend {
    pub did: String,
    pub handle: String,
    pub display_name: Option<String>,
    pub avatar: Option<String>,
    pub status: FriendStatus,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    Version,
    Auth,
    NoRoom,
    Full,
    BadRequest,
}

impl ErrorCode {
    /// The one-word menu label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Version => "UPDATE",
            Self::Auth => "SIGN IN",
            Self::NoRoom => "NO ROOM",
            Self::Full => "FULL",
            Self::BadRequest => "ERROR",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ServerMsg {
    /// Signed in; send `Create` or `Join`.
    Welcome { handle: String },
    Error(ErrorCode),
    Joined { code: String, map: RoomMap, you: PlayerId, players: Vec<PlayerInfo> },
    PlayerJoined(PlayerInfo),
    PlayerLeft(PlayerId),
    State { id: PlayerId, state: PlayerState },
    Fire { id: PlayerId, t: f64, shots: Vec<Shot>, flares: Vec<Shot> },
    Missiles { id: PlayerId, t: f64, missiles: Vec<MissileState> },
    Damage { target: PlayerId, attacker: PlayerId, health: f32 },
    Killed { victim: PlayerId, killer: Option<PlayerId> },
    Respawned(PlayerId),
    /// (id, kills, deaths) for everyone in the room.
    Scores(Vec<(PlayerId, u16, u16)>),
    /// Every online mutual (sent on sign-in; then updates).
    Friends(Vec<Friend>),
    /// A mutual came online or changed status.
    FriendUpdate(Friend),
    /// A mutual went offline (or isn't a mutual any more), by DID.
    FriendGone(String),
    /// A mutual asks us to their room.
    Invited { from: Friend, code: String },
    /// The invite didn't go (they're gone, or too many invites), by DID.
    InviteFailed(String),
}

pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    postcard::to_allocvec(msg).expect("protocol messages always encode")
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Option<T> {
    postcard::from_bytes(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let state = PlayerState {
            t: 12.5,
            pos: [1.0, 2.0, 3.0],
            rot: [0.0, 0.0, 0.0, 1.0],
            vel: [200.0, 0.0, -10.0],
            throttle: 0.8,
            g: 1.2,
            flags: PlayerState::GUN,
            health: 75.0,
            status: Status::Flying,
        };
        let msg = ClientMsg::State(state);
        let bytes = encode(&msg);
        assert!(bytes.len() < 80, "a state is {} bytes", bytes.len());
        assert_eq!(decode::<ClientMsg>(&bytes), Some(msg));
        let msg = ServerMsg::Joined {
            code: "ABCD".into(),
            map: RoomMap { seed: 7, biome: 1, time: 2, weather: 3 },
            you: 2,
            players: vec![PlayerInfo { id: 1, did: "did:dev:a".into(), handle: "a".into() }],
        };
        assert_eq!(decode::<ServerMsg>(&encode(&msg)), Some(msg));
        let friend = Friend {
            did: "did:plc:b".into(),
            handle: "b.test".into(),
            display_name: Some("Bee".into()),
            avatar: None,
            status: FriendStatus::InRoom,
        };
        for msg in [
            ServerMsg::Friends(vec![friend.clone()]),
            ServerMsg::FriendUpdate(friend.clone()),
            ServerMsg::FriendGone("did:plc:b".into()),
            ServerMsg::Invited { from: friend, code: "ABCD".into() },
            ServerMsg::InviteFailed("did:plc:b".into()),
        ] {
            assert_eq!(decode::<ServerMsg>(&encode(&msg)), Some(msg));
        }
        let msg = ClientMsg::Invite { to: "did:plc:b".into(), map: RoomMap { seed: 1, biome: 0, time: 0, weather: 0 } };
        assert_eq!(decode::<ClientMsg>(&encode(&msg)), Some(msg));
        assert_eq!(decode::<ServerMsg>(&[0xff, 0xff, 0xff]), None);
    }

    #[test]
    fn codes() {
        assert!(valid_code("ABCD"));
        assert!(!valid_code("ABCI"));
        assert!(!valid_code("abcd"));
        assert!(!valid_code("ABC"));
        assert_eq!(normalize_code(" abcd "), "ABCD");
    }
}
