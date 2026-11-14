//! Rooms: who's in which, relaying, and the rules (damage, kills, score).
//!
//! Two kinds of room ([`RoomKind`]). A hosted room starts as a lobby: its
//! first player is the host, and nothing is flown until they `Start` it.
//! When the host leaves, the longest-standing player takes over. A public
//! (quick-play) room has no host: it's flying from the start, anyone the
//! matchmaker sends drops in, and it goes soon after its last player
//! leaves. [`choose_public`] is the matchmaker's pick.
//!
//! Pure logic with no I/O, so it's testable: every call returns the
//! messages to send ([`Out`]) and the caller delivers them.
//!
//! Phase 2 (co-op against server-flown AI) would give each [`Room`] its own
//! AI aircraft, with ids from `AI_ID_START` up, simulated by the Bevy app.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::protocol::*;

pub type ConnId = u64;

/// A message for some connections.
#[derive(Debug, Clone, PartialEq)]
pub struct Out {
    pub to: Vec<ConnId>,
    pub msg: ServerMsg,
}

/// Empty rooms stay open this long (so a dropped creator can come back).
const EMPTY_ROOM_TTL: Duration = Duration::from_secs(120);
/// Empty public rooms, this long (nobody's coming back for them).
const EMPTY_PUBLIC_TTL: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoomKind {
    /// Quick play: no host, no lobby, no visible code.
    Public,
    /// Made by a host (or an invite): a lobby with a code, START and join requests.
    Hosted,
}

/// The matchmaker: of the public rooms (code, players, our online mutuals
/// in it), one with players and a seat, preferring our mutuals, then the
/// fullest. (An empty one never: nobody's there to play with, so we queue.)
pub fn choose_public<'a>(rooms: impl IntoIterator<Item = (&'a str, usize, usize)>) -> Option<&'a str> {
    rooms
        .into_iter()
        .filter(|(_, players, _)| (1..MAX_PLAYERS).contains(players))
        // (ties by code: the same pick every time)
        .max_by(|a, b| (a.2, a.1).cmp(&(b.2, b.1)).then(b.0.cmp(a.0)))
        .map(|(code, _, _)| code)
}
const MAX_ROOMS: usize = 2_000;
/// Hits need fresh positions for both aircraft.
const STATE_STALE: Duration = Duration::from_secs(3);
/// Missiles are remembered this long after their last update (a hit can
/// arrive just after the missile's last report).
const MISSILE_MEMORY: Duration = Duration::from_secs(3);

pub struct Server {
    rooms: HashMap<String, Room>,
    conns: HashMap<ConnId, Conn>,
    /// Honour requested room codes (dev servers, for automated tests).
    dev_codes: bool,
    /// Codes in use on other replicas (never handed out here).
    pub reserved: HashSet<String>,
}

struct Conn {
    did: String,
    handle: String,
    room: Option<(String, PlayerId)>,
}

pub struct Room {
    pub code: String,
    pub map: RoomMap,
    pub kind: RoomKind,
    players: BTreeMap<PlayerId, Player>,
    next_id: PlayerId,
    empty_since: Option<Instant>,
    host: PlayerId,
    /// Out of the lobby, flying.
    pub started: bool,
    /// Kills per (killer DID, victim DID), for the room's lifetime.
    records: HashMap<(String, String), u16>,
    /// Join order (the host passes to the lowest).
    next_seq: u32,
}

struct Player {
    conn: ConnId,
    seq: u32,
    info: PlayerInfo,
    health: f32,
    alive: bool,
    kills: u16,
    deaths: u16,
    state: Option<(PlayerState, Instant)>,
    /// Who last hurt us, and when (crash credit).
    last_hit: Option<(PlayerId, Instant)>,
    /// Gun-hit allowance (token bucket at the gun's rate).
    gun_tokens: f32,
    tokens_at: Instant,
    missiles: HashMap<u16, TrackedMissile>,
}

struct TrackedMissile {
    last: MissileState,
    seen: Instant,
    /// Aircraft it already damaged.
    hit: Vec<PlayerId>,
}

impl Server {
    pub fn new(dev_codes: bool) -> Self {
        Self { rooms: HashMap::new(), conns: HashMap::new(), dev_codes, reserved: HashSet::new() }
    }

    pub fn room_count(&self) -> usize {
        self.rooms.len()
    }

    pub fn player_count(&self) -> usize {
        self.rooms.values().map(|r| r.players.len()).sum()
    }

    /// The rooms' part of the metrics snapshot.
    pub fn fill_gauges(&self, g: &mut crate::metrics::Gauges) {
        for r in self.rooms.values() {
            let kind = (r.kind == RoomKind::Hosted) as usize;
            g.rooms[kind][r.started as usize] += 1;
            g.in_rooms[kind] += r.players.len() as u64;
            g.sizes[r.players.len().min(MAX_PLAYERS)] += 1;
        }
    }

    /// A connection signed in.
    pub fn connect(&mut self, conn: ConnId, did: String, handle: String) -> Vec<Out> {
        let msg = ServerMsg::Welcome { handle: handle.clone() };
        self.add_conn(conn, did, handle);
        vec![Out { to: vec![conn], msg }]
    }

    /// A connection that can be put in rooms (another replica's player, a guest here), without a welcome.
    pub fn add_conn(&mut self, conn: ConnId, did: String, handle: String) {
        self.conns.insert(conn, Conn { did, handle, room: None });
    }

    pub fn handle_of(&self, conn: ConnId) -> Option<&str> {
        self.conns.get(&conn).map(|c| c.handle.as_str())
    }

    pub fn exists(&self, code: &str) -> bool {
        self.rooms.contains_key(code)
    }

    /// Every room, for the replica's view (by code).
    pub fn entries(&self) -> Vec<crate::cluster::RoomEntry> {
        let mut codes: Vec<&String> = self.rooms.keys().collect();
        codes.sort();
        codes.into_iter().filter_map(|c| self.entry(c)).collect()
    }

    pub fn entry(&self, code: &str) -> Option<crate::cluster::RoomEntry> {
        let r = self.rooms.get(code)?;
        Some(crate::cluster::RoomEntry {
            code: r.code.clone(),
            public: r.kind == RoomKind::Public,
            started: r.started,
            map: r.map,
            players: r.players.values().map(|p| p.info.did.clone()).collect(),
            host: (r.kind == RoomKind::Hosted).then(|| r.players.get(&r.host).map(|p| p.info.did.clone())).flatten(),
        })
    }

    /// Every connection that's in a room.
    pub fn conns_in_rooms(&self) -> Vec<ConnId> {
        self.rooms.values().flat_map(|r| r.players.values().map(|p| p.conn)).collect()
    }

    /// Closes every room at once (shutting down: the players were told).
    pub fn end_all(&mut self) {
        self.rooms.clear();
        for c in self.conns.values_mut() {
            c.room = None;
        }
    }

    pub fn disconnect(&mut self, conn: ConnId, now: Instant) -> Vec<Out> {
        let mut out = vec![];
        self.leave(conn, now, &mut out);
        self.conns.remove(&conn);
        out
    }

    /// Drops empty rooms and forgotten missiles.
    pub fn tick(&mut self, now: Instant) {
        self.rooms.retain(|_, r| {
            let ttl = if r.kind == RoomKind::Public { EMPTY_PUBLIC_TTL } else { EMPTY_ROOM_TTL };
            r.empty_since.is_none_or(|t| now.duration_since(t) < ttl)
        });
        for room in self.rooms.values_mut() {
            for p in room.players.values_mut() {
                p.missiles.retain(|_, m| now.duration_since(m.seen) < MISSILE_MEMORY);
            }
        }
    }

    pub fn handle(&mut self, conn: ConnId, msg: ClientMsg, now: Instant) -> Vec<Out> {
        let mut out = vec![];
        if !self.conns.contains_key(&conn) {
            return out;
        }
        match msg {
            // (auth is done by the connection before it gets here)
            ClientMsg::Hello { .. } | ClientMsg::Ping => {}
            // (the hub handles join requests and matchmaking: they need presence)
            ClientMsg::RequestJoin { .. } | ClientMsg::AnswerJoin { .. } | ClientMsg::QuickPlay { .. } | ClientMsg::JoinFriend { .. } | ClientMsg::CancelQueue => {}
            ClientMsg::Create { map, code } => {
                self.create_room(conn, map, code, RoomKind::Hosted, now, &mut out);
            }
            ClientMsg::Leave => self.leave(conn, now, &mut out),
            // (the hub handles invites: they need presence)
            ClientMsg::Invite { .. } => {}
            ClientMsg::Join { code } => self.join_code(conn, &code, now, &mut out),
            other => {
                let Some((code, id)) = self.conns.get(&conn).and_then(|c| c.room.clone()) else { return out };
                if let Some(room) = self.rooms.get_mut(&code) {
                    room.handle(id, other, now, &mut out);
                }
            }
        }
        out
    }

    /// A new room on `map` with `conn` in it (leaving any room it was in).
    /// Returns the code, or None if the server is full.
    pub fn create_room(
        &mut self,
        conn: ConnId,
        map: RoomMap,
        code: Option<String>,
        kind: RoomKind,
        now: Instant,
        out: &mut Vec<Out>,
    ) -> Option<String> {
        self.leave(conn, now, out);
        let Some(code) = self.new_room(map, code, kind, now) else {
            out.push(Out { to: vec![conn], msg: ServerMsg::Error(ErrorCode::Full) });
            return None;
        };
        self.join(conn, &code, now, out);
        Some(code)
    }

    /// A new, empty room (None if the server is full).
    pub fn new_room(&mut self, map: RoomMap, code: Option<String>, kind: RoomKind, now: Instant) -> Option<String> {
        if self.rooms.len() >= MAX_ROOMS {
            return None;
        }
        let requested = code.map(|c| normalize_code(&c)).filter(|c| self.dev_codes && valid_code(c));
        let code = match requested {
            Some(c) if !self.rooms.contains_key(&c) && !self.reserved.contains(&c) => c,
            _ => self.fresh_code(),
        };
        self.rooms.insert(
            code.clone(),
            Room {
                code: code.clone(),
                map,
                kind,
                players: BTreeMap::new(),
                next_id: 1,
                empty_since: Some(now),
                host: 0,
                // (public rooms have no lobby)
                started: kind == RoomKind::Public,
                records: HashMap::new(),
                next_seq: 0,
            },
        );
        crate::metrics::M.rooms_created.inc(&[if kind == RoomKind::Public { "public" } else { "hosted" }]);
        Some(code)
    }

    /// Into room `code` (out of any other), or NO ROOM.
    pub fn join_code(&mut self, conn: ConnId, code: &str, now: Instant, out: &mut Vec<Out>) {
        let code = normalize_code(code);
        if !self.rooms.contains_key(&code) {
            out.push(Out { to: vec![conn], msg: ServerMsg::Error(ErrorCode::NoRoom) });
            return;
        }
        // (rejoining the same room is allowed: it replaces the old seat)
        self.leave(conn, now, out);
        self.join(conn, &code, now, out);
    }

    /// The connection of room `code`'s host (public rooms have none).
    pub fn host_conn(&self, code: &str) -> Option<ConnId> {
        let room = self.rooms.get(code).filter(|r| r.kind == RoomKind::Hosted)?;
        room.players.get(&room.host).map(|p| p.conn)
    }

    #[cfg(test)]
    pub fn is_public(&self, code: &str) -> bool {
        self.rooms.get(code).is_some_and(|r| r.kind == RoomKind::Public)
    }

    /// Every public room: (code, its players' DIDs).
    pub fn public_rooms(&self) -> Vec<(&str, Vec<&str>)> {
        self.rooms
            .values()
            .filter(|r| r.kind == RoomKind::Public)
            .map(|r| (r.code.as_str(), r.players.values().map(|p| p.info.did.as_str()).collect()))
            .collect()
    }

    /// Out of whatever room `conn` is in.
    pub fn leave_room(&mut self, conn: ConnId, now: Instant, out: &mut Vec<Out>) {
        self.leave(conn, now, out);
    }

    /// Whether `did` has a seat in room `code`.
    #[allow(dead_code)]
    pub fn has_player(&self, code: &str, did: &str) -> bool {
        self.rooms.get(code).is_some_and(|r| r.players.values().any(|p| p.info.did == did))
    }

    /// The room a connection is in: (code, whether it's full).
    pub fn room_of(&self, conn: ConnId) -> Option<(&str, bool)> {
        let (code, _) = self.conns.get(&conn)?.room.as_ref()?;
        let room = self.rooms.get(code)?;
        Some((code.as_str(), room.players.len() >= MAX_PLAYERS))
    }

    fn fresh_code(&self) -> String {
        loop {
            let code: String = (0..CODE_LEN)
                .map(|_| CODE_ALPHABET[rand::random_range(0..CODE_ALPHABET.len())] as char)
                .collect();
            if !self.rooms.contains_key(&code) && !self.reserved.contains(&code) {
                return code;
            }
        }
    }

    fn join(&mut self, conn: ConnId, code: &str, now: Instant, out: &mut Vec<Out>) {
        let Some(c) = self.conns.get(&conn) else { return };
        let (did, handle) = (c.did.clone(), c.handle.clone());
        let Some(room) = self.rooms.get_mut(code) else { return };
        // the same account in two places: the newer one takes the seat
        if let Some((&old_id, old)) = room.players.iter().find(|(_, p)| p.info.did == did) {
            let old_conn = old.conn;
            room.remove(old_id, now, out);
            if let Some(c) = self.conns.get_mut(&old_conn) {
                c.room = None;
            }
        }
        if room.players.len() >= MAX_PLAYERS {
            out.push(Out { to: vec![conn], msg: ServerMsg::Error(ErrorCode::Full) });
            return;
        }
        let id = room.free_id();
        let info = PlayerInfo { id, did, handle };
        if room.players.is_empty() && room.kind == RoomKind::Hosted {
            room.host = id;
        }
        let seq = room.next_seq;
        room.next_seq += 1;
        room.players.insert(
            id,
            Player {
                conn,
                seq,
                info: info.clone(),
                health: MAX_HEALTH,
                alive: true,
                kills: 0,
                deaths: 0,
                state: None,
                last_hit: None,
                gun_tokens: GUN_RATE,
                tokens_at: now,
                missiles: HashMap::new(),
            },
        );
        room.empty_since = None;
        out.push(Out {
            to: vec![conn],
            msg: ServerMsg::Joined {
                code: room.code.clone(),
                map: room.map,
                you: id,
                players: room.players.values().map(|p| p.info.clone()).collect(),
                host: room.host,
                started: room.started,
                public: room.kind == RoomKind::Public,
            },
        });
        out.push(Out { to: room.others(id), msg: ServerMsg::PlayerJoined(info) });
        out.push(room.scores());
        if let Some(c) = self.conns.get_mut(&conn) {
            c.room = Some((code.to_string(), id));
        }
    }

    fn leave(&mut self, conn: ConnId, now: Instant, out: &mut Vec<Out>) {
        let Some((code, id)) = self.conns.get_mut(&conn).and_then(|c| c.room.take()) else { return };
        if let Some(room) = self.rooms.get_mut(&code) {
            room.remove(id, now, out);
        }
    }
}

impl Room {
    fn all(&self) -> Vec<ConnId> {
        self.players.values().map(|p| p.conn).collect()
    }

    fn others(&self, id: PlayerId) -> Vec<ConnId> {
        self.players.iter().filter(|(i, _)| **i != id).map(|(_, p)| p.conn).collect()
    }

    fn free_id(&mut self) -> PlayerId {
        // cycle through 1..AI_ID_START so a leaver's id isn't reused straight away
        loop {
            let id = self.next_id;
            self.next_id = if self.next_id + 1 >= AI_ID_START { 1 } else { self.next_id + 1 };
            if !self.players.contains_key(&id) {
                return id;
            }
        }
    }

    fn scores(&self) -> Out {
        Out { to: self.all(), msg: ServerMsg::Scores(self.players.values().map(|p| (p.info.id, p.kills, p.deaths)).collect()) }
    }

    fn remove(&mut self, id: PlayerId, now: Instant, out: &mut Vec<Out>) {
        if self.players.remove(&id).is_none() {
            return;
        }
        if self.players.is_empty() {
            self.empty_since = Some(now);
        } else {
            out.push(Out { to: self.all(), msg: ServerMsg::PlayerLeft(id) });
            if id == self.host
                && self.kind == RoomKind::Hosted
                && let Some(next) = self.players.values().min_by_key(|p| p.seq).map(|p| p.info.id)
            {
                self.host = next;
                out.push(Out { to: self.all(), msg: ServerMsg::Host(next) });
            }
            out.push(self.scores());
        }
    }

    fn handle(&mut self, id: PlayerId, msg: ClientMsg, now: Instant, out: &mut Vec<Out>) {
        let others = self.others(id);
        let Some(p) = self.players.get_mut(&id) else { return };
        match msg {
            ClientMsg::State(mut state) => {
                let crashed = p.alive && state.status != Status::Flying;
                state.health = if p.alive { p.health } else { 0.0 };
                if !p.alive && state.status == Status::Flying {
                    // (the Killed hasn't reached them yet)
                    state.status = Status::Down;
                }
                p.state = Some((state, now));
                out.push(Out { to: others, msg: ServerMsg::State { id, state } });
                if crashed {
                    // flew into the ground: credit whoever was chasing them
                    let credit = p
                        .last_hit
                        .filter(|(_, t)| now.duration_since(*t).as_secs_f32() < CRASH_CREDIT_SECS)
                        .map(|(k, _)| k);
                    self.kill(id, credit, out);
                }
            }
            ClientMsg::Fire { t, mut shots, mut flares } => {
                if !p.alive {
                    return;
                }
                shots.truncate(64);
                flares.truncate(8);
                out.push(Out { to: others, msg: ServerMsg::Fire { id, t, shots, flares } });
            }
            ClientMsg::Missiles { t, mut missiles } => {
                missiles.truncate(16);
                for m in &missiles {
                    let tracked = p.missiles.entry(m.id).or_insert(TrackedMissile { last: *m, seen: now, hit: vec![] });
                    tracked.last = *m;
                    tracked.seen = now;
                }
                out.push(Out { to: others, msg: ServerMsg::Missiles { id, t, missiles } });
            }
            ClientMsg::Hit { target, weapon, damage } => self.hit(id, target, weapon, damage, now, out),
            ClientMsg::Start => {
                if self.kind == RoomKind::Hosted && id == self.host && !self.started {
                    self.started = true;
                    out.push(Out { to: self.all(), msg: ServerMsg::Started });
                }
            }
            ClientMsg::Respawn => {
                if !p.alive {
                    p.alive = true;
                    p.health = MAX_HEALTH;
                    p.last_hit = None;
                    out.push(Out { to: self.all(), msg: ServerMsg::Respawned(id) });
                }
            }
            ClientMsg::Hello { .. }
            | ClientMsg::Create { .. }
            | ClientMsg::Join { .. }
            | ClientMsg::Ping
            | ClientMsg::Leave
            | ClientMsg::Invite { .. }
            | ClientMsg::RequestJoin { .. }
            | ClientMsg::AnswerJoin { .. }
            | ClientMsg::ChallengeStart { .. }
            | ClientMsg::SubmitScore(_)
            | ClientMsg::Leaderboard(_)
            | ClientMsg::QuickPlay { .. }
            | ClientMsg::JoinFriend { .. }
            | ClientMsg::CancelQueue => {}
        }
    }

    /// A shooter says they hit someone: check it's believable, then apply it.
    fn hit(&mut self, shooter: PlayerId, target: PlayerId, weapon: Weapon, damage: f32, now: Instant, out: &mut Vec<Out>) {
        if shooter == target {
            return;
        }
        let fresh = |p: &Player| p.state.filter(|(_, t)| now.duration_since(*t) < STATE_STALE).map(|(s, _)| s.pos);
        let (Some(s), Some(v)) = (self.players.get(&shooter), self.players.get(&target)) else { return };
        if !s.alive || !v.alive {
            return;
        }
        let (Some(shooter_pos), Some(target_pos)) = (fresh(s), fresh(v)) else { return };
        let damage = match weapon {
            Weapon::Gun => {
                if dist(shooter_pos, target_pos) > GUN_MAX_RANGE {
                    return;
                }
                let s = self.players.get_mut(&shooter).unwrap();
                // no more hits than the gun can fire
                let dt = now.duration_since(s.tokens_at).as_secs_f32();
                s.tokens_at = now;
                s.gun_tokens = (s.gun_tokens + dt * GUN_RATE * 1.2).min(GUN_RATE);
                if s.gun_tokens < 1.0 {
                    return;
                }
                s.gun_tokens -= 1.0;
                GUN_DAMAGE
            }
            Weapon::Missile(mid) => {
                let s = self.players.get_mut(&shooter).unwrap();
                let Some(m) = s.missiles.get_mut(&mid) else { return };
                if m.hit.contains(&target) || dist(m.last.pos, target_pos) > MISSILE_HIT_SLACK {
                    return;
                }
                m.hit.push(target);
                damage.clamp(0.0, MISSILE_DAMAGE)
            }
        };
        let v = self.players.get_mut(&target).unwrap();
        v.health = (v.health - damage).max(0.0);
        v.last_hit = Some((shooter, now));
        let health = v.health;
        out.push(Out { to: self.all(), msg: ServerMsg::Damage { target, attacker: shooter, health } });
        if health <= 0.0 {
            self.kill(target, Some(shooter), out);
        }
    }

    fn kill(&mut self, victim: PlayerId, killer: Option<PlayerId>, out: &mut Vec<Out>) {
        let Some(v) = self.players.get_mut(&victim) else { return };
        if !v.alive {
            return;
        }
        v.alive = false;
        v.health = 0.0;
        v.deaths += 1;
        let victim_did = v.info.did.clone();
        let killer = killer.filter(|k| *k != victim);
        let mut record = (0, 0);
        let credited = killer.is_some_and(|k| self.players.contains_key(&k));
        crate::metrics::M.deaths.inc(&[if credited { "shot" } else { "crash" }]);
        if let Some(k) = killer.and_then(|k| self.players.get_mut(&k)) {
            crate::metrics::M.kills.inc();
            k.kills += 1;
            let killer_did = k.info.did.clone();
            let theirs = self.records.entry((killer_did.clone(), victim_did.clone())).or_default();
            *theirs += 1;
            record = (*theirs, self.records.get(&(victim_did, killer_did)).copied().unwrap_or(0));
        }
        out.push(Out { to: self.all(), msg: ServerMsg::Killed { victim, killer, record } });
        out.push(self.scores());
    }
}

fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAP: RoomMap = RoomMap { seed: 1, biome: 0, time: 2, weather: 0 };

    fn state(pos: [f32; 3], status: Status) -> ClientMsg {
        ClientMsg::State(PlayerState {
            t: 0.0,
            pos,
            rot: [0.0, 0.0, 0.0, 1.0],
            vel: [0.0; 3],
            throttle: 0.7,
            g: 1.0,
            flags: 0,
            health: 100.0,
            status,
        })
    }

    /// Two players in room TEST at `a` and `b`.
    fn duel(a: [f32; 3], b: [f32; 3]) -> (Server, Instant) {
        let now = Instant::now();
        let mut s = Server::new(true);
        s.connect(1, "did:dev:a".into(), "a".into());
        s.connect(2, "did:dev:b".into(), "b".into());
        let out = s.handle(1, ClientMsg::Create { map: MAP, code: Some("TEST".into()) }, now);
        assert!(matches!(&out[0].msg, ServerMsg::Joined { code, you: 1, .. } if code == "TEST"));
        let out = s.handle(2, ClientMsg::Join { code: "test".into() }, now);
        assert!(matches!(&out[0].msg, ServerMsg::Joined { you: 2, players, .. } if players.len() == 2));
        s.handle(1, state(a, Status::Flying), now);
        s.handle(2, state(b, Status::Flying), now);
        (s, now)
    }

    fn kills(out: &[Out]) -> Vec<(PlayerId, Option<PlayerId>)> {
        out.iter()
            .filter_map(|o| match o.msg {
                ServerMsg::Killed { victim, killer, .. } => Some((victim, killer)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn states_are_relayed_to_the_others_with_server_health() {
        let (mut s, now) = duel([0.0; 3], [100.0, 0.0, 0.0]);
        let out = s.handle(1, state([5.0, 0.0, 0.0], Status::Flying), now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to, vec![2]);
        assert!(matches!(out[0].msg, ServerMsg::State { id: 1, state } if state.health == MAX_HEALTH));
    }

    #[test]
    fn gun_hits_kill_and_score() {
        let (mut s, now) = duel([0.0; 3], [500.0, 0.0, 0.0]);
        let mut all = vec![];
        for i in 0..20 {
            let t = now + Duration::from_millis(40 * i);
            all.extend(s.handle(1, ClientMsg::Hit { target: 2, weapon: Weapon::Gun, damage: 999.0 }, t));
        }
        assert_eq!(kills(&all), vec![(2, Some(1))]);
        // 5 damage per hit, whatever the shooter claims: dead on the 20th
        assert!(all.iter().any(|o| o.msg == ServerMsg::Scores(vec![(1, 1, 0), (2, 0, 1)])));
        // the dead don't take more hits, and come back on request
        let out = s.handle(1, ClientMsg::Hit { target: 2, weapon: Weapon::Gun, damage: 5.0 }, now + Duration::from_secs(2));
        assert!(out.is_empty());
        let out = s.handle(2, ClientMsg::Respawn, now + Duration::from_secs(5));
        assert_eq!(out[0].msg, ServerMsg::Respawned(2));
    }

    #[test]
    fn unbelievable_hits_are_ignored() {
        // out of gun range
        let (mut s, now) = duel([0.0; 3], [5_000.0, 0.0, 0.0]);
        assert!(s.handle(1, ClientMsg::Hit { target: 2, weapon: Weapon::Gun, damage: 5.0 }, now).is_empty());
        // faster than the gun fires
        let (mut s, now) = duel([0.0; 3], [100.0, 0.0, 0.0]);
        let landed = (0..200)
            .filter(|_| !s.handle(1, ClientMsg::Hit { target: 2, weapon: Weapon::Gun, damage: 5.0 }, now).is_empty())
            .count();
        assert!(landed <= GUN_RATE as usize, "{landed} hits landed in an instant");
        // an unknown missile, then a known one that's far away, then a close one (once)
        let (mut s, now) = duel([0.0; 3], [3_000.0, 0.0, 0.0]);
        let missile = |pos| ClientMsg::Missiles { t: 0.0, missiles: vec![MissileState { id: 7, pos, vel: [0.0; 3], target: Some(2) }] };
        let hit = ClientMsg::Hit { target: 2, weapon: Weapon::Missile(7), damage: 95.0 };
        assert!(s.handle(1, hit.clone(), now).is_empty());
        s.handle(1, missile([1_000.0, 0.0, 0.0]), now);
        assert!(s.handle(1, hit.clone(), now).is_empty());
        s.handle(1, missile([2_990.0, 0.0, 0.0]), now);
        let out = s.handle(1, hit.clone(), now);
        assert!(matches!(out[0].msg, ServerMsg::Damage { target: 2, attacker: 1, health } if health == 5.0));
        assert!(s.handle(1, hit, now).is_empty());
    }

    #[test]
    fn crashing_after_a_hit_credits_the_attacker() {
        let (mut s, now) = duel([0.0; 3], [100.0, 0.0, 0.0]);
        s.handle(1, ClientMsg::Hit { target: 2, weapon: Weapon::Gun, damage: 5.0 }, now);
        let out = s.handle(2, state([100.0, 0.0, 0.0], Status::Dead), now + Duration::from_secs(3));
        assert_eq!(kills(&out), vec![(2, Some(1))]);
        // a plain crash: nobody's kill
        let out = s.handle(2, ClientMsg::Respawn, now + Duration::from_secs(4));
        assert_eq!(out[0].msg, ServerMsg::Respawned(2));
        let out = s.handle(2, state([100.0, 0.0, 0.0], Status::Dead), now + Duration::from_secs(30));
        assert_eq!(kills(&out), vec![(2, None)]);
    }

    #[test]
    fn rooms_fill_up_and_empty_rooms_expire() {
        let now = Instant::now();
        let mut s = Server::new(false);
        s.connect(100, "did:dev:host".into(), "host".into());
        let out = s.handle(100, ClientMsg::Create { map: MAP, code: Some("TEST".into()) }, now);
        let ServerMsg::Joined { code, .. } = &out[0].msg else { panic!() };
        let code = code.clone();
        // (not a dev server: the requested code is ignored)
        assert!(valid_code(&code));
        for i in 1..MAX_PLAYERS as u64 {
            s.connect(i, format!("did:dev:{i}"), i.to_string());
            let out = s.handle(i, ClientMsg::Join { code: code.clone() }, now);
            assert!(matches!(out[0].msg, ServerMsg::Joined { .. }));
        }
        s.connect(99, "did:dev:late".into(), "late".into());
        let out = s.handle(99, ClientMsg::Join { code: code.clone() }, now);
        assert_eq!(out[0].msg, ServerMsg::Error(ErrorCode::Full));
        assert_eq!(s.handle(99, ClientMsg::Join { code: "ZZZZ".into() }, now)[0].msg, ServerMsg::Error(ErrorCode::NoRoom));
        for i in 1..MAX_PLAYERS as u64 {
            s.disconnect(i, now);
        }
        s.disconnect(100, now);
        assert_eq!(s.player_count(), 0);
        s.tick(now + Duration::from_secs(10));
        assert_eq!(s.room_count(), 1);
        s.tick(now + EMPTY_ROOM_TTL + Duration::from_secs(1));
        assert_eq!(s.room_count(), 0);
    }

    #[test]
    fn the_same_account_twice_takes_over_the_seat() {
        let (mut s, now) = duel([0.0; 3], [100.0, 0.0, 0.0]);
        s.connect(3, "did:dev:b".into(), "b".into());
        let out = s.handle(3, ClientMsg::Join { code: "TEST".into() }, now);
        assert!(out.iter().any(|o| o.msg == ServerMsg::PlayerLeft(2)));
        assert_eq!(s.player_count(), 2);
    }

    #[test]
    fn rooms_wait_in_the_lobby_until_the_host_starts() {
        let now = Instant::now();
        let mut s = Server::new(true);
        for (c, h) in [(1, "a"), (2, "b"), (3, "c")] {
            s.connect(c, format!("did:dev:{h}"), h.into());
        }
        let out = s.handle(1, ClientMsg::Create { map: MAP, code: Some("TEST".into()) }, now);
        assert!(matches!(&out[0].msg, ServerMsg::Joined { host: 1, started: false, .. }));
        let out = s.handle(2, ClientMsg::Join { code: "TEST".into() }, now);
        assert!(matches!(&out[0].msg, ServerMsg::Joined { you: 2, host: 1, started: false, .. }));
        // only the host starts it
        assert!(s.handle(2, ClientMsg::Start, now).is_empty());
        let out = s.handle(1, ClientMsg::Start, now);
        assert_eq!(out, vec![Out { to: vec![1, 2], msg: ServerMsg::Started }]);
        assert!(s.handle(1, ClientMsg::Start, now).is_empty(), "once");
        // late joiners land in the game
        let out = s.handle(3, ClientMsg::Join { code: "TEST".into() }, now);
        assert!(matches!(&out[0].msg, ServerMsg::Joined { you: 3, started: true, .. }));
    }

    #[test]
    fn the_host_passes_to_the_next_player() {
        let now = Instant::now();
        let mut s = Server::new(true);
        for (c, h) in [(1, "a"), (2, "b"), (3, "c")] {
            s.connect(c, format!("did:dev:{h}"), h.into());
        }
        s.handle(1, ClientMsg::Create { map: MAP, code: Some("TEST".into()) }, now);
        s.handle(3, ClientMsg::Join { code: "TEST".into() }, now);
        s.handle(2, ClientMsg::Join { code: "TEST".into() }, now);
        assert_eq!(s.host_conn("TEST"), Some(1));
        let out = s.handle(1, ClientMsg::Leave, now);
        // c joined before b
        assert!(out.iter().any(|o| o.msg == ServerMsg::Host(2) && o.to.len() == 2), "{out:?}");
        assert_eq!(s.host_conn("TEST"), Some(3));
        let out = s.handle(3, ClientMsg::Start, now);
        assert_eq!(out[0].msg, ServerMsg::Started);
        // everyone gone, then back: the newcomer hosts the empty room
        s.disconnect(2, now);
        s.disconnect(3, now);
        let out = s.handle(1, ClientMsg::Join { code: "TEST".into() }, now);
        assert!(matches!(&out[0].msg, ServerMsg::Joined { you, host, started: true, .. } if you == host));
    }

    #[test]
    fn the_matchmaker_prefers_mutuals_then_full_rooms() {
        // (code, players, our mutuals in it)
        assert_eq!(choose_public([]), None);
        assert_eq!(choose_public([("AAAA", 2, 0), ("BBBB", 5, 0), ("CCCC", 3, 0)]), Some("BBBB"), "the fullest");
        assert_eq!(choose_public([("AAAA", 2, 1), ("BBBB", 5, 0)]), Some("AAAA"), "a mutual beats a fuller room");
        assert_eq!(choose_public([("AAAA", 2, 1), ("BBBB", 6, 2), ("CCCC", 7, 1)]), Some("BBBB"), "more mutuals first");
        assert_eq!(choose_public([("AAAA", 4, 1), ("BBBB", 6, 1)]), Some("BBBB"), "then the fuller");
        assert_eq!(choose_public([("AAAA", MAX_PLAYERS, 3), ("BBBB", 1, 0)]), Some("BBBB"), "never a full one");
        assert_eq!(choose_public([("AAAA", MAX_PLAYERS, 0)]), None);
        assert_eq!(choose_public([("BBBB", 3, 0), ("AAAA", 3, 0)]), Some("AAAA"), "ties by code");
        // an empty room (in its grace period) is nobody to play with
        assert_eq!(choose_public([("AAAA", 0, 0)]), None);
        assert_eq!(choose_public([("AAAA", 0, 0), ("BBBB", 1, 0)]), Some("BBBB"));
    }

    #[test]
    fn public_rooms_fly_at_once_with_no_host() {
        let now = Instant::now();
        let mut s = Server::new(true);
        for (c, h) in [(1, "a"), (2, "b"), (3, "c")] {
            s.connect(c, format!("did:dev:{h}"), h.into());
        }
        let mut out = vec![];
        let code = s.create_room(1, MAP, None, RoomKind::Public, now, &mut out).unwrap();
        assert!(matches!(&out[0].msg, ServerMsg::Joined { host: 0, started: true, public: true, .. }));
        assert!(s.is_public(&code));
        assert_eq!(s.host_conn(&code), None);
        // Start does nothing; nobody hosts after a leave
        assert!(s.handle(1, ClientMsg::Start, now).is_empty());
        s.handle(2, ClientMsg::Join { code: code.clone() }, now);
        let out = s.handle(1, ClientMsg::Leave, now);
        assert!(!out.iter().any(|o| matches!(o.msg, ServerMsg::Host(_))), "{out:?}");
        let out = s.handle(3, ClientMsg::Join { code: code.clone() }, now);
        assert!(matches!(&out[0].msg, ServerMsg::Joined { host: 0, started: true, public: true, .. }));
        assert_eq!(s.public_rooms(), vec![(code.as_str(), vec!["did:dev:b", "did:dev:c"])]);
        // empty: gone after a short grace (a hosted room would stay for minutes)
        s.disconnect(2, now);
        s.disconnect(3, now);
        s.tick(now + Duration::from_secs(5));
        assert_eq!(s.room_count(), 1);
        s.tick(now + EMPTY_PUBLIC_TTL + Duration::from_secs(1));
        assert_eq!(s.room_count(), 0);
    }

    #[test]
    fn head_to_head_records_count_both_ways() {
        let (mut s, now) = duel([0.0; 3], [500.0, 0.0, 0.0]);
        let record = |out: &[Out]| {
            out.iter().find_map(|o| match o.msg {
                ServerMsg::Killed { record, .. } => Some(record),
                _ => None,
            })
        };
        let mut t = now;
        let mut duel_to_the_death = |s: &mut Server, shooter: ConnId, target: PlayerId| {
            let mut out = vec![];
            for _ in 0..20 {
                t += Duration::from_millis(40);
                s.handle(1, state([0.0; 3], Status::Flying), t);
                s.handle(2, state([500.0, 0.0, 0.0], Status::Flying), t);
                out.extend(s.handle(shooter, ClientMsg::Hit { target, weapon: Weapon::Gun, damage: 5.0 }, t));
            }
            let victim = if shooter == 1 { 2 } else { 1 };
            s.handle(victim, ClientMsg::Respawn, t);
            out
        };
        assert_eq!(record(&duel_to_the_death(&mut s, 1, 2)), Some((1, 0)));
        assert_eq!(record(&duel_to_the_death(&mut s, 1, 2)), Some((2, 0)));
        // b gets one back: b's kills on a, a's on b
        assert_eq!(record(&duel_to_the_death(&mut s, 2, 1)), Some((1, 2)));
        // a crash with nobody chasing: no record
        t += Duration::from_secs(30);
        let out = s.handle(2, state([500.0, 0.0, 0.0], Status::Dead), t);
        assert!(out.iter().any(|o| o.msg == ServerMsg::Killed { victim: 2, killer: None, record: (0, 0) }));
    }
}
