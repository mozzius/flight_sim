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
//! A new room is a lobby until its host sends `Start` (players who join it
//! wait there, baking the map); after that joiners drop straight into the
//! game. `RequestJoin` asks a friend's room's host to let us in
//! (`JoinRequest` / `AnswerJoin`), without the room code ever leaving the
//! server until they say yes.
//!
//! Challenge leaderboard (v4): a signed-in client says `ChallengeStart` when
//! a challenge round begins and `SubmitScore` when it ends; the server checks
//! the run against the scoring rules and its own clock ([`check_run`]),
//! keeps each player's best, and answers with their [`Standing`].
//! `Leaderboard` asks for the global or the friends (mutuals) board.
//!
//! Authority: each client flies its own jet and simulates its own bullets and
//! missiles; the server relays states and events, and decides damage, kills
//! and score from the hits shooters report ([`ClientMsg::Hit`]).

// (shared with the server: each side uses part of it)
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// Bumped whenever the messages change shape.
pub const PROTOCOL_VERSION: u16 = 4;

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
/// Join requests wait this long for the host's answer (then count as declined).
pub const JOIN_REQUEST_SECS: f32 = 20.0;
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
    /// The host starts the room's game (lobby → flying).
    Start,
    /// Ask to join the room an online mutual (by DID) is in; the server asks
    /// that room's host.
    RequestJoin { to: String },
    /// The host's answer to a `JoinRequest` (by the requester's DID).
    AnswerJoin { did: String, accept: bool },
    /// A challenge round began on this map seed (the server times the run).
    ChallengeStart { seed: u32 },
    /// The challenge round that began with `ChallengeStart` ended.
    SubmitScore(Run),
    /// Asks for a board (`LeaderboardData`).
    Leaderboard(BoardKind),
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
    /// In a room. `started`: flying already (else the lobby).
    Joined { code: String, map: RoomMap, you: PlayerId, players: Vec<PlayerInfo>, host: PlayerId, started: bool },
    /// The room's host changed (the old one left).
    Host(PlayerId),
    /// The host started the game.
    Started,
    PlayerJoined(PlayerInfo),
    PlayerLeft(PlayerId),
    State { id: PlayerId, state: PlayerState },
    Fire { id: PlayerId, t: f64, shots: Vec<Shot>, flares: Vec<Shot> },
    Missiles { id: PlayerId, t: f64, missiles: Vec<MissileState> },
    Damage { target: PlayerId, attacker: PlayerId, health: f32 },
    /// `record`: the killer's kills on the victim and the victim's on the
    /// killer, this room (counting this one); (0, 0) with no killer.
    Killed { victim: PlayerId, killer: Option<PlayerId>, record: (u16, u16) },
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
    /// Someone asks to join our room (we're its host): `AnswerJoin`.
    JoinRequest(Friend),
    /// Our `RequestJoin` (to this DID) was turned down, timed out or
    /// couldn't go (all look the same).
    JoinDeclined(String),
    /// Our best challenge run and its global rank (after `SubmitScore`,
    /// whether or not the run counted). None: no run on the board yet.
    Standing(Option<Standing>),
    /// A board: the top rows, and where we stand on it.
    LeaderboardData { kind: BoardKind, rows: Vec<BoardRow>, you: Option<Standing> },
}

// ---- challenge leaderboard ----

/// Challenge scoring (the game's, `modes` re-exports these): points per kill
/// of a hostile, and per cleared wave times the wave number.
pub const KILL_POINTS: u32 = 100;
pub const WAVE_POINTS: u32 = 250;
/// Rows on a board.
pub const BOARD_ROWS: usize = 20;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BoardKind {
    Global,
    /// Us and our mutuals.
    Friends,
}

/// A finished challenge run.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Run {
    /// The map seed (what `ChallengeStart` said).
    pub seed: u32,
    pub score: u32,
    /// The wave reached.
    pub wave: u32,
    /// Everything we shot down (friendlies too).
    pub kills: u32,
    /// Game seconds from the start to going down.
    pub secs: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BoardRow {
    pub rank: u32,
    pub did: String,
    pub handle: String,
    pub score: u32,
    pub wave: u32,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Standing {
    pub rank: u32,
    pub score: u32,
    pub wave: u32,
}

/// Limits a believable run stays within.
pub const MAX_RUN_SCORE: u32 = 2_000_000;
pub const MAX_RUN_WAVE: u32 = 150;
pub const MAX_RUN_SECS: f32 = 4.0 * 3600.0;
/// Every wave after the first takes at least this long to reach (each
/// needs the last one cleared, then a 5 s breather).
pub const MIN_SECS_PER_WAVE: f32 = 10.0;
/// And each kill at least this long.
pub const MIN_SECS_PER_KILL: f32 = 2.0;
/// How far the client's clock may run ahead of the server's.
pub const RUN_CLOCK_SLACK: f32 = 15.0;
/// Friendly wingmen in a challenge (shooting them counts as a kill, not points).
const CHALLENGE_WINGMEN: u32 = 2;

/// Hostiles in challenge wave `wave` (as `game::spawn_enemy_wave` spawns them).
pub fn challenge_hostiles(wave: u32) -> u32 {
    (1 + wave * 2).min(25)
}

/// Whether a run adds up under the scoring rules, within the limits, and
/// took long enough: by its own clock (`run.secs`) and by the server's
/// (`server_secs` since its `ChallengeStart`). The reason if not.
pub fn check_run(run: &Run, server_secs: f32) -> Result<(), &'static str> {
    if run.wave == 0 || run.wave > MAX_RUN_WAVE || run.score > MAX_RUN_SCORE {
        return Err("out of range");
    }
    if !run.secs.is_finite() || run.secs < 0.0 || run.secs > MAX_RUN_SECS {
        return Err("bad duration");
    }
    if run.secs > server_secs + RUN_CLOCK_SLACK {
        return Err("longer than the server saw");
    }
    let hostiles: u32 = (1..=run.wave).map(challenge_hostiles).sum();
    if run.kills > hostiles + CHALLENGE_WINGMEN {
        return Err("too many kills");
    }
    // waves cleared: the one in progress isn't, unless we went down in the breather after it
    let hostile_kills = [run.wave - 1, run.wave].into_iter().find_map(|cleared| {
        let bonus = WAVE_POINTS * cleared * (cleared + 1) / 2;
        let rest = run.score.checked_sub(bonus)?;
        let kills = rest / KILL_POINTS;
        (rest % KILL_POINTS == 0 && kills <= run.kills && kills <= hostiles).then_some(kills)
    });
    let Some(hostile_kills) = hostile_kills else {
        return Err("score doesn't add up");
    };
    let min_secs = MIN_SECS_PER_WAVE * (run.wave - 1) as f32 + MIN_SECS_PER_KILL * hostile_kills as f32;
    if run.secs < min_secs || server_secs < min_secs {
        return Err("too quick");
    }
    Ok(())
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
            host: 1,
            started: false,
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
            ServerMsg::Invited { from: friend.clone(), code: "ABCD".into() },
            ServerMsg::InviteFailed("did:plc:b".into()),
            ServerMsg::JoinRequest(friend.clone()),
            ServerMsg::JoinDeclined("did:plc:b".into()),
            ServerMsg::Host(3),
            ServerMsg::Started,
            ServerMsg::Killed { victim: 2, killer: Some(1), record: (2, 3) },
        ] {
            assert_eq!(decode::<ServerMsg>(&encode(&msg)), Some(msg));
        }
        let msg = ClientMsg::Invite { to: "did:plc:b".into(), map: RoomMap { seed: 1, biome: 0, time: 0, weather: 0 } };
        assert_eq!(decode::<ClientMsg>(&encode(&msg)), Some(msg));
        for msg in [ClientMsg::Start, ClientMsg::RequestJoin { to: "did:plc:b".into() }, ClientMsg::AnswerJoin { did: "did:plc:b".into(), accept: true }] {
            assert_eq!(decode::<ClientMsg>(&encode(&msg)), Some(msg));
        }
        assert_eq!(decode::<ServerMsg>(&[0xff, 0xff, 0xff]), None);
        let run = Run { seed: 7, score: 1150, wave: 3, kills: 4, secs: 61.5 };
        for msg in [ClientMsg::ChallengeStart { seed: 7 }, ClientMsg::SubmitScore(run), ClientMsg::Leaderboard(BoardKind::Friends)] {
            assert_eq!(decode::<ClientMsg>(&encode(&msg)), Some(msg));
        }
        let standing = Standing { rank: 3, score: 1150, wave: 3 };
        let row = BoardRow { rank: 1, did: "did:plc:b".into(), handle: "b.test".into(), score: 9000, wave: 7 };
        for msg in [
            ServerMsg::Standing(Some(standing)),
            ServerMsg::Standing(None),
            ServerMsg::LeaderboardData { kind: BoardKind::Global, rows: vec![row], you: Some(standing) },
        ] {
            assert_eq!(decode::<ServerMsg>(&encode(&msg)), Some(msg));
        }
    }

    fn run(score: u32, wave: u32, kills: u32, secs: f32) -> Run {
        Run { seed: 1, score, wave, kills, secs }
    }

    #[test]
    fn plausible_runs_pass() {
        // wave 1, nothing done
        assert_eq!(check_run(&run(0, 1, 0, 8.0), 9.0), Ok(()));
        // 3 kills in wave 2 (wave 1 cleared): 300 + 250
        assert_eq!(check_run(&run(550, 2, 3, 40.0), 41.0), Ok(()));
        // a friendly kill counts as a kill but scores nothing
        assert_eq!(check_run(&run(550, 2, 4, 40.0), 41.0), Ok(()));
        // down in the breather after clearing wave 2: 250 + 500 + 8 kills
        assert_eq!(check_run(&run(1550, 2, 8, 60.0), 60.0), Ok(()));
        // the client's game clock is behind the server's (pauses, loading)
        assert_eq!(check_run(&run(1150, 3, 4, 40.0), 300.0), Ok(()));
        // a long, good run: wave 12 reached, every hostile of waves 1-11 down
        let kills: u32 = (1..=11).map(challenge_hostiles).sum::<u32>() + 3;
        let score = KILL_POINTS * kills + WAVE_POINTS * 11 * 12 / 2;
        assert_eq!(check_run(&run(score, 12, kills, 1500.0), 1510.0), Ok(()));
    }

    #[test]
    fn implausible_runs_fail() {
        // doesn't add up: not a multiple of the kill points after the wave bonus
        assert!(check_run(&run(560, 2, 3, 40.0), 41.0).is_err());
        // more points than kills
        assert!(check_run(&run(400, 1, 3, 60.0), 61.0).is_err());
        // more kills than hostiles (wave 1 has 3, plus 2 wingmen)
        assert!(check_run(&run(600, 1, 6, 60.0), 61.0).is_err());
        // too quick for the waves / kills
        assert!(check_run(&run(1150, 3, 4, 20.0), 21.0).is_err());
        // the client says longer than the server saw
        assert!(check_run(&run(550, 2, 3, 120.0), 40.0).is_err());
        // the server saw too little time
        assert!(check_run(&run(1150, 3, 4, 40.0), 10.0).is_err());
        // caps and junk
        assert!(check_run(&run(0, 0, 0, 10.0), 10.0).is_err());
        assert!(check_run(&run(0, MAX_RUN_WAVE + 1, 0, 10.0), 10.0).is_err());
        assert!(check_run(&run(MAX_RUN_SCORE + 100, 100, 1000, 9000.0), 9000.0).is_err());
        assert!(check_run(&run(0, 1, 0, f32::NAN), 10.0).is_err());
        assert!(check_run(&run(0, 1, 0, -1.0), 10.0).is_err());
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
