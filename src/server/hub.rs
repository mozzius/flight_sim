//! The rooms plus presence: what a connection's messages do. Invites live
//! here because they need both (an online mutual, and a room to ask them to,
//! made on the fly if the inviter isn't in one), and so do join requests
//! (asking the host of the room a mutual is in to let us in: the requester
//! only learns the code by being admitted).
//!
//! Matchmaking too: `QuickPlay` picks a public room by who's in it (our
//! online mutuals first: see `rooms::choose_public`), and `JoinFriend` hops
//! into a mutual's public room, both needing presence. With no public room
//! that has players, quick play waits in the queue ([`Hub::match_queue`]):
//! two or more waiting get a new public room together (on the first one's
//! map), and a waiting player goes into any public room that has players
//! as soon as there is one. Players in rooms are never moved, so the last
//! one left in a public room keeps flying (and the next quick play joins
//! them). Joining any room, `Leave`, `CancelQueue` or going offline takes
//! a player out of the queue.
//!
//! And the online count: `Online(n)` to each new connection, then to
//! everyone when it changes, at most every [`ONLINE_GAP`].
//!
//! The challenge leaderboard (`leaderboard`) hangs off the hub too: it
//! needs the player's DID and presence's relationship cache.
//!
//! Across replicas (see `cluster`): the hub is one replica's. Presence has
//! every online player in the cluster (from the other replicas' [`View`]s),
//! the matchmaker and join-by-code see every replica's rooms, and the queue
//! and rate limits are the cluster's ([`Backend`]). A player put in another
//! replica's room is *away* here: their room messages go to the owner
//! (`Body::Msg`), whose messages for them come back already encoded
//! (`Body::Deliver`, passed straight to the socket as [`Step::raw`]). A
//! room here with another replica's players in it has them as *guests*:
//! connections of their own in `rooms` (ids from [`GUEST_IDS`] up), whose
//! messages [`Hub::route`] sends home. With one replica none of that is
//! used: everything is local, as it always was.
//!
//! Pure like the parts: every call returns a [`Step`], the messages to send,
//! the AppView lookups and the database work to run; envelopes for other
//! replicas wait for [`Hub::flush`] (once per game-loop tick, after our view,
//! so a room is known before an invite names it).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cluster::{self, Backend, Body, CLUSTER_VERSION, Envelope, QueueEntry, RoomEntry, Tag, UserEntry, View};
use crate::leaderboard::{Board, DbJob, DbResult};
use crate::metrics::{self, M};
use crate::presence::{Did, INVITE_GAP, INVITES_PER_MINUTE, Job, JobResult, Presence, REQUEST_GAP, REQUESTS_PER_MINUTE};
use crate::protocol::*;
use crate::rooms::{self, ConnId, Out, RoomKind};

#[derive(Default, Debug)]
pub struct Step {
    pub out: Vec<Out>,
    /// Messages another replica's room encoded for our connections.
    pub raw: Vec<(Vec<ConnId>, Vec<u8>)>,
    pub jobs: Vec<Job>,
    pub db: Vec<DbJob>,
}

impl Step {
    pub fn extend(&mut self, s: Step) {
        self.out.extend(s.out);
        self.raw.extend(s.raw);
        self.jobs.extend(s.jobs);
        self.db.extend(s.db);
    }
}

/// How a player got into a room (`joins_total`'s label).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Method {
    Code,
    Invite,
    Friend,
    Request,
    Quickplay,
    Queue,
}

impl Method {
    const ALL: [Method; 6] = [Method::Code, Method::Invite, Method::Friend, Method::Request, Method::Quickplay, Method::Queue];

    fn label(self) -> &'static str {
        match self {
            Method::Code => "code",
            Method::Invite => "invite",
            Method::Friend => "friend",
            Method::Request => "request",
            Method::Quickplay => "quickplay",
            Method::Queue => "queue",
        }
    }
}

/// Guests' connection ids start here (ours count up from 1).
pub const GUEST_IDS: ConnId = 1 << 40;
/// A join on another replica that hasn't answered in this long: NO ROOM.
const AWAY_JOIN_WAIT: Duration = Duration::from_secs(10);
/// A guest whose home's said nothing for this long (clients ping every
/// few seconds) is dropped.
const GUEST_IDLE: Duration = Duration::from_secs(30);
/// The queue's matcher runs at most this often (and on every QUICK PLAY).
const MATCH_GAP: Duration = Duration::from_millis(250);
/// A waiting player whose entry left the queue (taken by another replica's
/// matcher) with no room to show for it is put back after this long.
const LOST_IN_QUEUE: Duration = Duration::from_secs(5);

/// A player of ours in another replica's room (or on their way in).
#[derive(Debug)]
struct Away {
    owner: String,
    code: String,
    /// The owner said `Joined` (until then, a join on its way).
    joined: bool,
    public: bool,
    at: Instant,
}

/// Another replica's player in one of our rooms.
#[derive(Debug)]
struct Guest {
    home: String,
    conn: ConnId,
    seen: Instant,
}

pub struct Hub {
    pub rooms: rooms::Server,
    pub presence: Presence,
    dids: HashMap<ConnId, Did>,
    asks: Vec<Ask>,
    pub board: Board,
    /// Quick play's waiting players (ours), first come first.
    queue: Vec<Waiting>,
    /// The online count everyone was last told, and when.
    online_sent: Option<(u32, Instant)>,
    /// Invites out (invitee conn, room code, when): a join with that code
    /// counts as an invite join in the metrics.
    invited: Vec<(ConnId, String, Instant)>,
    cluster: Arc<dyn Backend>,
    /// The other live replicas.
    views: HashMap<String, View>,
    /// When each last broadcast its view (a re-read that raced a new
    /// replica's first write doesn't drop it).
    heard: HashMap<String, Instant>,
    away: HashMap<ConnId, Away>,
    guests: HashMap<ConnId, Guest>,
    guest_ids: HashMap<(String, ConnId), ConnId>,
    next_guest: ConnId,
    /// Envelope bodies for other replicas, sent by `flush`.
    outbox: Vec<(String, Body)>,
    /// What our view last said (without the gauges), and when it was written.
    published: Option<(View, Instant)>,
    last_match: Option<Instant>,
    /// Shutting down: no new rooms or guests.
    pub draining: bool,
    /// Said goodbye: nothing more goes out.
    gone: bool,
    info: cluster::ReplicaInfo,
    /// (tests: the database the jobs run against, inline)
    #[cfg(test)]
    pub test_db: crate::leaderboard::Db,
}

/// Invites count as the way in for this long.
const INVITE_MEMORY: Duration = Duration::from_secs(300);

/// The room `conn` was just put in, if `out` says so.
fn joined_room(out: &[Out], conn: ConnId) -> Option<&str> {
    out.iter().find_map(|o| match &o.msg {
        ServerMsg::Joined { code, .. } if o.to == [conn] => Some(code.as_str()),
        _ => None,
    })
}

/// The online count goes out at most this often.
pub const ONLINE_GAP: Duration = Duration::from_secs(5);

/// A player in the quick-play queue.
#[derive(Debug)]
struct Waiting {
    conn: ConnId,
    /// The map they sent (a room made for them is on the first one's).
    map: RoomMap,
    since: Instant,
    since_ms: u64,
    /// Their entry left the queue without us placing them (another
    /// replica's matcher took it): since when.
    lost: Option<Instant>,
    /// When we last made sure their entry's still in the queue.
    checked: Instant,
}

/// A join request waiting for the host.
#[derive(Debug)]
struct Ask {
    conn: ConnId,
    did: Did,
    /// The friend whose room they asked for (what `JoinDeclined` names).
    asked: Did,
    code: String,
    /// The host's DID.
    host: Did,
    at: Instant,
}

/// A room, wherever it is.
#[derive(Debug, Clone, PartialEq)]
struct RoomRef {
    code: String,
    full: bool,
    public: bool,
    /// The replica it's on (None: here).
    owner: Option<String>,
    host: Option<Did>,
    players: Vec<Did>,
}

impl Hub {
    /// A hub on its own (tests: a throwaway database).
    #[cfg(test)]
    pub fn new(dev_codes: bool) -> Self {
        Self::with(dev_codes, cluster::Memory::single())
    }

    pub fn with(dev_codes: bool, cluster: Arc<dyn Backend>) -> Self {
        let info = metrics::this_replica(cluster.id());
        Self {
            rooms: rooms::Server::new(dev_codes),
            presence: Presence::new(),
            dids: HashMap::new(),
            asks: vec![],
            board: Board::new(),
            queue: vec![],
            online_sent: None,
            invited: vec![],
            views: HashMap::new(),
            heard: HashMap::new(),
            away: HashMap::new(),
            guests: HashMap::new(),
            guest_ids: HashMap::new(),
            next_guest: GUEST_IDS,
            outbox: vec![],
            published: None,
            last_match: None,
            draining: false,
            gone: false,
            info: cluster::ReplicaInfo { id: info.id, git: info.git, version: info.version, started: info.started },
            cluster,
            #[cfg(test)]
            test_db: crate::leaderboard::Db::memory(),
        }
    }

    #[cfg(test)]
    pub fn id(&self) -> &str {
        self.cluster.id()
    }

    /// Takes `conn` out of the queue, if it's in, counting why.
    fn unqueue(&mut self, conn: ConnId, outcome: &'static str) {
        let before = self.queue.len();
        self.queue.retain(|w| w.conn != conn);
        if self.queue.len() != before {
            self.cluster.queue_remove(self.cluster.id(), conn);
            M.queue_exits.inc(&[outcome]);
        }
    }

    /// The metrics snapshot (this replica's part).
    pub fn gauges(&self) -> metrics::Gauges {
        let mut g = metrics::Gauges {
            players_online: self.presence.online_count() as u64,
            signed_in: self.dids.len() as u64,
            queue: self.queue.len() as u64,
            pending_requests: self.asks.len() as u64,
            leaderboard: self.board.size(),
            websockets: M.ws_open.load(std::sync::atomic::Ordering::Relaxed),
            ..Default::default()
        };
        self.rooms.fill_gauges(&mut g);
        g
    }

    /// The cluster's gauges and replicas (what a scrape shows).
    pub fn snapshot(&self) -> metrics::Snapshot {
        let mut gauges = self.gauges();
        let mut replicas = vec![self.replica()];
        for v in self.views.values() {
            gauges.add(&v.gauges);
            replicas.push(metrics::Replica { id: v.id.clone(), git: v.info.git.clone(), version: v.info.version.clone(), started: v.info.started });
        }
        replicas.sort_by(|a, b| a.id.cmp(&b.id));
        metrics::Snapshot { gauges, replicas }
    }

    fn replica(&self) -> metrics::Replica {
        metrics::Replica { id: self.info.id.clone(), git: self.info.git.clone(), version: self.info.version.clone(), started: self.info.started }
    }

    /// How many are waiting for a quick-play match (here).
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Live replicas besides us.
    pub fn others(&self) -> usize {
        self.views.len()
    }

    pub fn connect(&mut self, conn: ConnId, did: String, handle: String, now: Instant) -> Step {
        let mut step = Step { out: self.rooms.connect(conn, did.clone(), handle.clone()), ..Default::default() };
        let (out, jobs) = self.presence.online(conn, &did, &handle, now);
        step.out.extend(out);
        step.jobs.extend(jobs);
        step.db.push(self.board.connect(&did, &handle));
        self.dids.insert(conn, did);
        step.out.push(Out { to: vec![conn], msg: ServerMsg::Online(self.presence.online_count() as u32) });
        self.finish(step, now)
    }

    pub fn disconnect(&mut self, conn: ConnId, now: Instant) -> Step {
        let mut step = Step { out: self.rooms.disconnect(conn, now), ..Default::default() };
        self.leave_away(conn);
        if let Some(did) = self.dids.remove(&conn) {
            let elsewhere = self.remote_user(&did).map(|u| u.friend.clone());
            step.out.extend(self.presence.offline(conn, &did, elsewhere.as_ref()));
        }
        self.asks.retain(|a| a.conn != conn);
        self.unqueue(conn, "offline");
        self.invited.retain(|i| i.0 != conn);
        self.board.disconnect(conn);
        step.out.extend(self.sync_statuses());
        self.finish(step, now)
    }

    pub fn handle(&mut self, conn: ConnId, msg: ClientMsg, now: Instant) -> Step {
        let mut step = Step::default();
        if !self.dids.contains_key(&conn) {
            return step;
        }
        let roomy = matches!(
            msg,
            ClientMsg::Create { .. } | ClientMsg::Join { .. } | ClientMsg::Leave | ClientMsg::QuickPlay { .. } | ClientMsg::JoinFriend { .. }
        );
        if matches!(msg, ClientMsg::Leave) {
            self.unqueue(conn, "cancelled");
        }
        // in another replica's room: its room messages go there
        if let Some(a) = self.away.get(&conn) {
            let room_msg = matches!(
                msg,
                ClientMsg::State(_) | ClientMsg::Fire { .. } | ClientMsg::Missiles { .. } | ClientMsg::Hit { .. } | ClientMsg::Respawn | ClientMsg::Start | ClientMsg::Ping
            );
            if room_msg {
                if a.joined {
                    let owner = a.owner.clone();
                    self.send(&owner, Body::Msg { conn, msg });
                }
                return self.finish(step, now);
            }
        }
        match msg {
            ClientMsg::QuickPlay { map } => self.quick_play(conn, map, now, &mut step.out),
            ClientMsg::CancelQueue => self.unqueue(conn, "cancelled"),
            ClientMsg::Join { code } => {
                let code = normalize_code(&code);
                let invited = self.invited.iter().any(|i| i.0 == conn && i.1 == code);
                self.invited.retain(|i| !(i.0 == conn && i.1 == code));
                self.enter(conn, &code, if invited { Method::Invite } else { Method::Code }, now, &mut step.out);
            }
            ClientMsg::JoinFriend { did } => self.join_friend(conn, &did, now, &mut step.out),
            ClientMsg::Invite { to, map } => self.invite(conn, &to, map, now, &mut step.out),
            ClientMsg::RequestJoin { to } => self.request_join(conn, &to, now, &mut step.out),
            ClientMsg::AnswerJoin { did, accept } => {
                self.answer_join(conn, &did, accept, now, &mut step.out);
                step.out.extend(self.sync_statuses());
            }
            ClientMsg::ChallengeStart { seed } => {
                if let Some(did) = self.dids.get(&conn) {
                    self.board.start(did, seed, &*self.cluster, now);
                }
            }
            ClientMsg::SubmitScore(run) => {
                if let Some(did) = self.dids.get(&conn).cloned() {
                    step.db.push(self.board.submit(conn, &did, run, &*self.cluster, now));
                }
            }
            ClientMsg::Leaderboard(kind) => {
                if let Some(did) = self.dids.get(&conn).cloned() {
                    step.db.extend(self.board.request(conn, &did, kind, now));
                }
            }
            ClientMsg::Leave if self.away.contains_key(&conn) => self.leave_away(conn),
            ClientMsg::Create { map, code } => {
                if self.draining {
                    step.out.push(Out { to: vec![conn], msg: ServerMsg::Error(ErrorCode::Full) });
                } else {
                    self.leave_away(conn);
                    step.out.extend(self.rooms.handle(conn, ClientMsg::Create { map, code }, now));
                }
            }
            other => step.out.extend(self.rooms.handle(conn, other, now)),
        }
        // in a room now (any way: a code, an invite, a friend, a request): out of the queue
        let (rooms, away) = (&self.rooms, &self.away);
        let (left, kept): (Vec<Waiting>, Vec<Waiting>) =
            std::mem::take(&mut self.queue).into_iter().partition(|w| rooms.room_of(w.conn).is_some() || away.contains_key(&w.conn));
        self.queue = kept;
        for w in left {
            self.cluster.queue_remove(self.cluster.id(), w.conn);
            M.queue_exits.inc(&["joined_elsewhere"]);
        }
        if roomy {
            step.out.extend(self.sync_statuses());
        }
        self.finish(step, now)
    }

    pub fn apply(&mut self, result: JobResult, now: Instant) -> Step {
        let out = self.presence.apply(result, now);
        let db = self.board.resolve(&self.presence, now);
        self.finish(Step { out, db, ..Default::default() }, now)
    }

    /// A database answer came back.
    pub fn apply_db(&mut self, result: DbResult, now: Instant) -> Step {
        let (out, jobs, db) = self.board.apply(result, &mut self.presence, now);
        self.finish(Step { out, jobs, db, ..Default::default() }, now)
    }

    pub fn tick(&mut self, now: Instant) -> Step {
        self.rooms.tick(now);
        let wait = Duration::from_secs_f32(JOIN_REQUEST_SECS);
        let mut out = vec![];
        self.asks.retain(|a| {
            let waiting = now.duration_since(a.at) < wait;
            if !waiting {
                M.join_requests.inc(&["expired"]);
                out.push(Out { to: vec![a.conn], msg: ServerMsg::JoinDeclined(a.asked.clone()) });
            }
            waiting
        });
        self.invited.retain(|i| now.duration_since(i.2) < INVITE_MEMORY);
        // joins on other replicas that never answered
        let stuck: Vec<ConnId> =
            self.away.iter().filter(|(_, a)| !a.joined && now.duration_since(a.at) >= AWAY_JOIN_WAIT).map(|(c, _)| *c).collect();
        for conn in stuck {
            self.leave_away(conn);
            out.push(Out { to: vec![conn], msg: ServerMsg::Error(ErrorCode::NoRoom) });
        }
        // guests whose home has gone quiet
        let quiet: Vec<(String, ConnId)> =
            self.guests.values().filter(|g| now.duration_since(g.seen) >= GUEST_IDLE).map(|g| (g.home.clone(), g.conn)).collect();
        for (home, conn) in quiet {
            self.drop_guest(&home, conn, now, &mut out);
        }
        // waiting players taken from the queue and never placed (or whose
        // entry never made it in): back in line
        let me = self.cluster.id().to_string();
        for i in 0..self.queue.len() {
            if self.queue[i].lost.is_none() && now.duration_since(self.queue[i].checked) >= LOST_IN_QUEUE {
                self.queue[i].checked = now;
                if !self.cluster.queue_has(&me, self.queue[i].conn) {
                    self.queue[i].lost = Some(now);
                }
            }
            if self.queue[i].lost.is_some_and(|t| now.duration_since(t) >= LOST_IN_QUEUE) {
                self.queue[i].lost = None;
                self.queue[i].checked = now;
                let entry = self.entry(&self.queue[i], &me);
                if let Some(e) = entry {
                    self.cluster.queue_add(&e);
                }
            }
        }
        let db = self.board.tick(&self.presence, now);
        if self.match_queue(now, false, &mut out) {
            out.extend(self.sync_statuses());
        }
        // the online count, when it's changed (not too often)
        let n = self.presence.online_count() as u32;
        let due = self.online_sent.is_none_or(|(sent, at)| sent != n && now.duration_since(at) >= ONLINE_GAP);
        if due && !self.dids.is_empty() {
            self.online_sent = Some((n, now));
            out.push(Out { to: self.dids.keys().copied().collect(), msg: ServerMsg::Online(n) });
        }
        let jobs = self.presence.tick(now);
        self.finish(Step { out, jobs, db, ..Default::default() }, now)
    }

    /// Our queue entry for `w`.
    fn entry(&self, w: &Waiting, me: &str) -> Option<QueueEntry> {
        Some(QueueEntry {
            replica: me.to_string(),
            conn: w.conn,
            did: self.dids.get(&w.conn)?.clone(),
            handle: self.rooms.handle_of(w.conn)?.to_string(),
            map: w.map,
            since_ms: w.since_ms,
        })
    }

    // ---- rooms anywhere ----

    /// The room `conn` (ours) is in, here or away.
    fn room_of(&self, conn: ConnId) -> Option<RoomRef> {
        if let Some((code, _)) = self.rooms.room_of(conn) {
            return self.room_ref(code);
        }
        let a = self.away.get(&conn).filter(|a| a.joined)?;
        Some(self.room_ref(&a.code).unwrap_or(RoomRef {
            code: a.code.clone(),
            full: false,
            public: a.public,
            owner: Some(a.owner.clone()),
            host: None,
            players: vec![],
        }))
    }

    /// Room `code`, here or on a live replica.
    fn room_ref(&self, code: &str) -> Option<RoomRef> {
        let entry = |e: &RoomEntry, owner: Option<String>| RoomRef {
            code: e.code.clone(),
            full: e.players.len() >= MAX_PLAYERS,
            public: e.public,
            owner,
            host: e.host.clone(),
            players: e.players.clone(),
        };
        if let Some(e) = self.rooms.entry(code) {
            return Some(entry(&e, None));
        }
        self.views.values().find_map(|v| v.rooms.iter().find(|r| r.code == code).map(|e| entry(e, Some(v.id.clone()))))
    }

    /// The room a (local or remote) player is in.
    fn friend_room(&self, did: &str) -> Option<RoomRef> {
        if let Some(r) = self.presence.conns(did).iter().find_map(|c| self.room_of(*c)) {
            return Some(r);
        }
        let code = self.remote_user(did)?.room.clone()?;
        self.room_ref(&code)
    }

    /// A player online elsewhere, as their home tells it.
    fn remote_user(&self, did: &str) -> Option<&UserEntry> {
        let mut ids: Vec<&String> = self.views.keys().collect();
        ids.sort();
        ids.iter().find_map(|id| self.views[*id].users.iter().find(|u| u.friend.did == did))
    }

    /// A live replica's room `code` (one that takes players).
    fn owner_of(&self, code: &str) -> Option<String> {
        self.views.values().find(|v| !v.draining && v.rooms.iter().any(|r| r.code == code)).map(|v| v.id.clone())
    }

    /// Puts `conn` (ours) in room `code`, wherever it is (out of any other).
    fn enter(&mut self, conn: ConnId, code: &str, method: Method, now: Instant, out: &mut Vec<Out>) {
        let code = normalize_code(code);
        if self.rooms.exists(&code) {
            self.leave_away(conn);
            let from = out.len();
            self.rooms.join_code(conn, &code, now, out);
            if joined_room(&out[from..], conn).is_some() {
                M.joins.inc(&[method.label()]);
            }
            return;
        }
        let (Some(owner), Some(did), Some(handle)) =
            (self.owner_of(&code), self.dids.get(&conn).cloned(), self.rooms.handle_of(conn).map(str::to_string))
        else {
            out.push(Out { to: vec![conn], msg: ServerMsg::Error(ErrorCode::NoRoom) });
            return;
        };
        self.rooms.leave_room(conn, now, out);
        self.leave_away(conn);
        self.send(&owner, Body::Join { conn, did, handle, code: code.clone(), method: method as u8 });
        self.away.insert(conn, Away { owner, code, joined: false, public: false, at: now });
    }

    /// Out of the room `conn` (ours) is in on another replica.
    fn leave_away(&mut self, conn: ConnId) {
        if let Some(a) = self.away.remove(&conn) {
            self.send(&a.owner, Body::Gone { conn });
        }
    }

    /// QUICK PLAY: into the public room with the most of our online mutuals,
    /// then the fullest (with players and a seat); none, the queue.
    fn quick_play(&mut self, conn: ConnId, map: RoomMap, now: Instant, out: &mut Vec<Out>) {
        if !self.dids.contains_key(&conn) {
            return;
        }
        self.rooms.leave_room(conn, now, out);
        self.leave_away(conn);
        // (pressed again while waiting: to the back of the queue, the wait still counting)
        let old = self.queue.iter().position(|w| w.conn == conn).map(|i| self.queue.remove(i));
        if old.is_some() && !self.cluster.queue_remove(self.cluster.id(), conn) {
            // (another replica just placed us: that room's on its way)
            if let Some(mut w) = old {
                w.lost = Some(now);
                self.queue.push(w);
            }
            return;
        }
        let (since, since_ms) = old.map_or((now, cluster::unix_ms()), |w| (w.since, w.since_ms));
        match self.pick_public(conn) {
            Some(code) => {
                M.quickplay.inc(&["joined"]);
                self.enter(conn, &code, Method::Quickplay, now, out);
            }
            None if self.draining => out.push(Out { to: vec![conn], msg: ServerMsg::Error(ErrorCode::Full) }),
            None => {
                M.quickplay.inc(&["queued"]);
                let w = Waiting { conn, map, since, since_ms, lost: None, checked: now };
                if let Some(e) = self.entry(&w, self.cluster.id()) {
                    self.cluster.queue_add(&e);
                }
                self.queue.push(w);
                out.push(Out { to: vec![conn], msg: ServerMsg::Queued });
                self.match_queue(now, true, out);
            }
        }
    }

    /// The public room (with players and a seat, on any live replica) quick
    /// play would put `conn` in.
    fn pick_public(&self, conn: ConnId) -> Option<String> {
        let from = self.dids.get(&conn)?;
        let mut rooms: Vec<(String, Vec<Did>)> =
            self.rooms.public_rooms().into_iter().map(|(c, d)| (c.to_string(), d.into_iter().map(str::to_string).collect())).collect();
        for v in self.views.values().filter(|v| !v.draining) {
            rooms.extend(v.rooms.iter().filter(|r| r.public).map(|r| (r.code.clone(), r.players.clone())));
        }
        let candidates: Vec<(&str, usize, usize)> = rooms
            .iter()
            .map(|(code, dids)| (code.as_str(), dids.len(), dids.iter().filter(|d| self.presence.online_mutuals(from, d)).count()))
            .collect();
        rooms::choose_public(candidates).map(str::to_string)
    }

    /// Moves waiting players into rooms: each into a public room with
    /// players if there is one (on any replica), then two or more left
    /// (anywhere in the cluster, with ours first in line) into a new public
    /// room here (on the first one's map). True if anyone moved.
    fn match_queue(&mut self, now: Instant, force: bool, out: &mut Vec<Out>) -> bool {
        let before = self.queue.len();
        let mut i = 0;
        while i < self.queue.len() {
            let conn = self.queue[i].conn;
            if self.queue[i].lost.is_some() {
                i += 1;
                continue;
            }
            match self.pick_public(conn) {
                // (ours to place only if it's still in the queue)
                Some(code) if self.cluster.queue_remove(self.cluster.id(), conn) => {
                    let w = self.queue.remove(i);
                    Self::matched(now.duration_since(w.since));
                    self.enter(conn, &code, Method::Queue, now, out);
                }
                Some(_) => {
                    self.queue[i].lost = Some(now);
                    i += 1;
                }
                None => i += 1,
            }
        }
        let due = force || self.last_match.is_none_or(|t| now.duration_since(t) >= MATCH_GAP);
        if due && !self.draining && self.queue.iter().any(|w| w.lost.is_none()) {
            self.last_match = Some(now);
            let group = self.cluster.queue_pop(MAX_PLAYERS);
            if !group.is_empty() {
                self.start_match(group, now, out);
            }
        }
        self.queue.len() != before
    }

    /// A group from the queue (the first one ours): a new public room here.
    fn start_match(&mut self, group: Vec<QueueEntry>, now: Instant, out: &mut Vec<Out>) {
        let me = self.cluster.id().to_string();
        // (ours that are still waiting, and everyone else's)
        let mut members: Vec<(QueueEntry, Option<Waiting>)> = vec![];
        for e in group {
            if e.replica == me {
                match self.queue.iter().position(|w| w.conn == e.conn) {
                    Some(i) => {
                        let w = self.queue.remove(i);
                        members.push((e, Some(w)));
                    }
                    None => {}
                }
            } else {
                members.push((e, None));
            }
        }
        if members.len() < 2 {
            for (e, w) in members {
                self.cluster.queue_add(&e);
                if let Some(w) = w {
                    self.queue.push(w);
                }
            }
            return;
        }
        let Some(code) = self.rooms.new_room(members[0].0.map, None, RoomKind::Public, now) else {
            // (the server's full)
            for (e, _) in members {
                self.tell(&e.replica, e.conn, ServerMsg::Error(ErrorCode::Full), out);
            }
            return;
        };
        M.queue_matches.inc();
        for (e, w) in members {
            match w {
                Some(w) => {
                    Self::matched(now.duration_since(w.since));
                    let from = out.len();
                    self.rooms.join_code(e.conn, &code, now, out);
                    if joined_room(&out[from..], e.conn).is_some() {
                        M.joins.inc(&[Method::Queue.label()]);
                    }
                }
                None => {
                    Self::matched(Duration::from_millis(cluster::unix_ms().saturating_sub(e.since_ms)));
                    self.guest_join(&e.replica, e.conn, e.did, e.handle, &code, Method::Queue, now, out);
                }
            }
        }
    }

    /// A message for one of `replica`'s connections (ours, or theirs: not a guest's).
    fn tell(&mut self, replica: &str, conn: ConnId, msg: ServerMsg, out: &mut Vec<Out>) {
        if replica == self.cluster.id() {
            out.push(Out { to: vec![conn], msg });
        } else {
            let tag = if matches!(msg, ServerMsg::Error(_)) { Tag::Ended } else { Tag::None };
            self.send(replica, Body::Deliver { conns: vec![conn], tag, bytes: encode(&msg) });
        }
    }

    /// A waiting player went into a room.
    fn matched(waited: Duration) {
        M.queue_exits.inc(&["matched"]);
        M.queue_wait.observe(waited);
    }

    /// JOIN on a mutual in a public room: straight in if it has a seat.
    /// Anything else is a plain decline (like a join request's).
    fn join_friend(&mut self, conn: ConnId, to: &str, now: Instant, out: &mut Vec<Out>) {
        let Some(from) = self.dids.get(&conn).cloned() else { return };
        let mine = self.room_of(conn).map(|r| r.code);
        match self.friend_room(to) {
            Some(r) if !r.full && self.presence.online_mutuals(&from, to) && r.public && mine.as_deref() != Some(r.code.as_str()) => {
                self.enter(conn, &r.code, Method::Friend, now, out);
            }
            _ => out.push(Out { to: vec![conn], msg: ServerMsg::JoinDeclined(to.to_string()) }),
        }
    }

    /// `conn` asks to join the room their mutual `to` is in: the request goes
    /// to that room's host (who must be a mutual too). Anything that doesn't
    /// go is a plain decline, so it never says where anyone is.
    fn request_join(&mut self, conn: ConnId, to: &str, now: Instant, out: &mut Vec<Out>) {
        let sent = self.ask_host(conn, to, now, out);
        M.join_requests.inc(&[if sent { "sent" } else { "refused" }]);
    }

    fn ask_host(&mut self, conn: ConnId, to: &str, now: Instant, out: &mut Vec<Out>) -> bool {
        let Some(from) = self.dids.get(&conn).cloned() else { return false };
        let declined = Out { to: vec![conn], msg: ServerMsg::JoinDeclined(to.to_string()) };
        if !self.presence.online_mutuals(&from, to) {
            out.push(declined);
            return false;
        }
        let Some(room) = self.friend_room(to).filter(|r| !r.full) else {
            out.push(declined);
            return false;
        };
        let Some(host) = room.host.clone() else {
            out.push(declined);
            return false;
        };
        if room.players.contains(&from)
            || !self.presence.online_mutuals(&from, &host)
            || !self.cluster.allow("request", &from, &host, REQUEST_GAP, REQUESTS_PER_MINUTE, now)
        {
            out.push(declined);
            return false;
        }
        let Some(me) = self.presence.friend(&from) else { return false };
        self.asks.retain(|a| !(a.conn == conn && a.code == room.code));
        self.asks.push(Ask { conn, did: from, asked: to.to_string(), code: room.code.clone(), host, at: now });
        let msg = ServerMsg::JoinRequest(me);
        match &room.owner {
            None => {
                if let Some(h) = self.rooms.host_conn(&room.code) {
                    out.push(Out { to: vec![h], msg });
                }
            }
            Some(owner) => self.send(owner, Body::ToHost { code: room.code.clone(), msg }),
        }
        true
    }

    /// The host says yes (the requester joins as if they had the code) or
    /// no: to wherever the requester is.
    fn answer_join(&mut self, conn: ConnId, did: &str, accept: bool, now: Instant, out: &mut Vec<Out>) {
        let (Some(host), Some(room)) = (self.dids.get(&conn).cloned(), self.room_of(conn)) else { return };
        let is_host = match &room.owner {
            None => self.rooms.host_conn(&room.code) == Some(conn),
            Some(_) => room.host.as_deref() == Some(host.as_str()),
        };
        if !is_host {
            return;
        }
        self.answer(&room.code, &host, did, accept, now, out);
        let homes: Vec<String> = self.views.values().filter(|v| v.users.iter().any(|u| u.friend.did == did)).map(|v| v.id.clone()).collect();
        for home in homes {
            self.send(&home, Body::Answer { code: room.code.clone(), host: host.clone(), requester: did.to_string(), accept });
        }
    }

    /// A host's answer to a request of ours.
    fn answer(&mut self, code: &str, host: &str, requester: &str, accept: bool, now: Instant, out: &mut Vec<Out>) {
        let Some(i) = self.asks.iter().position(|a| a.did == requester && a.code == code && a.host == host) else { return };
        let ask = self.asks.remove(i);
        if accept && self.dids.contains_key(&ask.conn) {
            M.join_requests.inc(&["accepted"]);
            self.enter(ask.conn, &ask.code, Method::Request, now, out);
        } else {
            M.join_requests.inc(&["declined"]);
            out.push(Out { to: vec![ask.conn], msg: ServerMsg::JoinDeclined(ask.asked) });
        }
    }

    fn invite(&mut self, conn: ConnId, to: &str, map: RoomMap, now: Instant, out: &mut Vec<Out>) {
        let Some(from) = self.dids.get(&conn).cloned() else { return };
        let failed = || Out { to: vec![conn], msg: ServerMsg::InviteFailed(to.to_string()) };
        let room = self.room_of(conn);
        // (offline and not a mutual fail alike: an invite never tells you who's online)
        if room.as_ref().is_some_and(|r| r.full)
            || !self.presence.online_mutuals(&from, to)
            || !self.cluster.allow("invite", &from, to, INVITE_GAP, INVITES_PER_MINUTE, now)
        {
            M.invites.inc(&["failed"]);
            out.push(failed());
            return;
        }
        let code = match room {
            Some(r) => r.code,
            None if self.draining => {
                M.invites.inc(&["failed"]);
                out.push(failed());
                return;
            }
            None => match self.rooms.create_room(conn, map, None, RoomKind::Hosted, now, out) {
                Some(code) => code,
                None => return,
            },
        };
        out.extend(self.sync_statuses());
        let Some(me) = self.presence.friend(&from) else { return };
        M.invites.inc(&["sent"]);
        self.send_did(to, ServerMsg::Invited { from: me, code }, now, out);
    }

    /// A message for every connection of `did` (here, and on the replicas it's on).
    fn send_did(&mut self, did: &str, msg: ServerMsg, now: Instant, out: &mut Vec<Out>) {
        let homes: Vec<String> = self.views.values().filter(|v| v.users.iter().any(|u| u.friend.did == did)).map(|v| v.id.clone()).collect();
        for home in homes {
            self.send(&home, Body::ToDid { did: did.to_string(), msg: msg.clone() });
        }
        self.deliver_did(did, msg, now, out);
    }

    fn deliver_did(&mut self, did: &str, msg: ServerMsg, now: Instant, out: &mut Vec<Out>) {
        let to = self.presence.conns(did);
        if to.is_empty() {
            return;
        }
        if let ServerMsg::Invited { code, .. } = &msg {
            self.invited.extend(to.iter().map(|c| (*c, code.clone(), now)));
        }
        out.push(Out { to, msg });
    }

    /// Friends see who's in a room (and whether it's full).
    fn sync_statuses(&mut self) -> Vec<Out> {
        let mut out = vec![];
        for did in self.presence.local_dids() {
            let room = self.presence.conns(&did).iter().find_map(|c| self.room_of(*c));
            let status = match room {
                None => FriendStatus::Online,
                Some(r) if r.full => FriendStatus::RoomFull,
                Some(r) if r.public => FriendStatus::InPublic,
                Some(_) => FriendStatus::InRoom,
            };
            out.extend(self.presence.set_status(&did, status));
        }
        out
    }

    // ---- guests (other replicas' players in our rooms) ----

    /// Puts `home`'s `conn` in our room `code`.
    #[allow(clippy::too_many_arguments)]
    fn guest_join(&mut self, home: &str, conn: ConnId, did: Did, handle: String, code: &str, method: Method, now: Instant, out: &mut Vec<Out>) {
        self.drop_guest(home, conn, now, out);
        if self.draining {
            self.tell(home, conn, ServerMsg::Error(ErrorCode::NoRoom), out);
            return;
        }
        let g = self.next_guest;
        self.next_guest += 1;
        self.rooms.add_conn(g, did, handle);
        self.guests.insert(g, Guest { home: home.to_string(), conn, seen: now });
        self.guest_ids.insert((home.to_string(), conn), g);
        let from = out.len();
        self.rooms.join_code(g, code, now, out);
        if joined_room(&out[from..], g).is_some() {
            M.joins.inc(&[method.label()]);
        } else {
            // (no such room, or it's full: they're told, and aren't our guest)
            let tail: Vec<Out> = out.drain(from..).collect();
            for o in tail {
                if o.to == [g] {
                    self.tell(home, conn, o.msg, out);
                } else {
                    out.push(o);
                }
            }
            self.drop_guest(home, conn, now, out);
        }
    }

    fn drop_guest(&mut self, home: &str, conn: ConnId, now: Instant, out: &mut Vec<Out>) {
        if let Some(g) = self.guest_ids.remove(&(home.to_string(), conn)) {
            self.guests.remove(&g);
            out.extend(self.rooms.disconnect(g, now));
        }
    }

    /// Sends guests' messages home, and keeps ours.
    fn route(&mut self, out: Vec<Out>) -> Vec<Out> {
        if self.guests.is_empty() {
            return out;
        }
        let mut local = Vec::with_capacity(out.len());
        for o in out {
            if !o.to.iter().any(|c| self.guests.contains_key(c)) {
                local.push(o);
                continue;
            }
            let mut here = vec![];
            let mut homes: BTreeMap<String, Vec<ConnId>> = BTreeMap::new();
            for c in o.to {
                match self.guests.get(&c) {
                    Some(g) => homes.entry(g.home.clone()).or_default().push(g.conn),
                    None => here.push(c),
                }
            }
            let tag = match &o.msg {
                ServerMsg::Joined { code, public, .. } => Tag::Joined { code: code.clone(), public: *public },
                ServerMsg::Error(_) => Tag::Ended,
                _ => Tag::None,
            };
            let bytes = encode(&o.msg);
            for (home, conns) in homes {
                self.send(&home, Body::Deliver { conns, tag: tag.clone(), bytes: bytes.clone() });
            }
            if !here.is_empty() {
                local.push(Out { to: here, msg: o.msg });
            }
        }
        local
    }

    /// Every call's last step: guests' messages go home (and in tests, the
    /// database work runs at once).
    fn finish(&mut self, mut step: Step, now: Instant) -> Step {
        step.out = self.route(std::mem::take(&mut step.out));
        #[cfg(test)]
        while !step.db.is_empty() {
            for job in std::mem::take(&mut step.db) {
                let result = bevy::tasks::block_on(self.test_db.run(job));
                let (out, jobs, db) = self.board.apply(result, &mut self.presence, now);
                step.out.extend(self.route(out));
                step.jobs.extend(jobs);
                step.db.extend(db);
            }
        }
        let _ = now;
        step
    }

    fn send(&mut self, to: &str, body: Body) {
        self.outbox.push((to.to_string(), body));
    }

    // ---- the cluster ----

    /// What we tell the other replicas about ourselves.
    pub fn view(&self) -> View {
        let mut users: Vec<UserEntry> = self
            .presence
            .local_dids()
            .into_iter()
            .filter_map(|did| {
                let friend = self.presence.friend(&did)?;
                let room = self.presence.conns(&did).iter().find_map(|c| self.room_of(*c)).map(|r| r.code);
                Some(UserEntry { friend, room })
            })
            .collect();
        users.sort_by(|a, b| a.friend.did.cmp(&b.friend.did));
        View {
            id: self.cluster.id().to_string(),
            draining: self.draining,
            users,
            rooms: self.rooms.entries(),
            gauges: self.gauges(),
            info: self.info.clone(),
        }
    }

    /// Sends what's waiting for other replicas: our view first (when it's
    /// changed, or every `HEARTBEAT`), then one envelope per replica. Once
    /// per game-loop tick.
    pub fn flush(&mut self, now: Instant) {
        if !self.cluster.clustered() || self.gone {
            self.outbox.clear();
            return;
        }
        let view = self.view();
        let mut bare = view.clone();
        bare.gauges = Default::default();
        let changed = self.published.as_ref().is_none_or(|(v, _)| *v != bare);
        let due = changed || self.published.as_ref().is_some_and(|(_, at)| now.duration_since(*at) >= cluster::HEARTBEAT);
        if due {
            self.cluster.put_view(encode(&view));
            if changed {
                self.publish(cluster::ALL, vec![Body::View(view)]);
            }
            self.published = Some((bare, now));
        }
        let mut by: BTreeMap<String, Vec<Body>> = BTreeMap::new();
        for (to, body) in std::mem::take(&mut self.outbox) {
            by.entry(to).or_default().push(body);
        }
        for (to, bodies) in by {
            self.publish(&to, bodies);
        }
    }

    fn publish(&self, to: &str, bodies: Vec<Body>) {
        M.cluster_messages.inc(&["sent"]);
        let env = Envelope { v: CLUSTER_VERSION, from: self.cluster.id().to_string(), bodies };
        self.cluster.publish(to, encode(&env));
    }

    /// An envelope from another replica.
    pub fn on_cluster(&mut self, bytes: &[u8], now: Instant) -> Step {
        let mut step = Step::default();
        let Some(env) = decode::<Envelope>(bytes) else { return step };
        if env.v != CLUSTER_VERSION || env.from == self.cluster.id() {
            return step;
        }
        M.cluster_messages.inc(&["received"]);
        for body in env.bodies {
            self.on_body(&env.from, body, now, &mut step);
        }
        self.finish(step, now)
    }

    fn on_body(&mut self, from: &str, body: Body, now: Instant, step: &mut Step) {
        match body {
            Body::View(v) => {
                if v.id == from {
                    self.heard.insert(v.id.clone(), now);
                    self.views.insert(v.id.clone(), v);
                    self.sync_view(now, step);
                }
            }
            Body::Bye => {
                self.heard.remove(from);
                if self.views.remove(from).is_some() {
                    self.sync_view(now, step);
                }
            }
            Body::Join { conn, did, handle, code, method } => {
                let method = Method::ALL.get(method as usize).copied().unwrap_or(Method::Code);
                self.guest_join(from, conn, did, handle, &code, method, now, &mut step.out);
                step.out.extend(self.sync_statuses());
            }
            Body::Msg { conn, msg } => {
                if let Some(&g) = self.guest_ids.get(&(from.to_string(), conn)) {
                    if let Some(guest) = self.guests.get_mut(&g) {
                        guest.seen = now;
                    }
                    if !matches!(msg, ClientMsg::Ping) {
                        step.out.extend(self.rooms.handle(g, msg, now));
                    }
                }
            }
            Body::Gone { conn } => {
                self.drop_guest(from, conn, now, &mut step.out);
                step.out.extend(self.sync_statuses());
            }
            Body::Deliver { conns, tag, bytes } => self.delivered(from, conns, tag, bytes, now, step),
            Body::ToHost { code, msg } => {
                if let Some(h) = self.rooms.host_conn(&code) {
                    step.out.push(Out { to: vec![h], msg });
                }
            }
            Body::ToDid { did, msg } => self.deliver_did(&did, msg, now, &mut step.out),
            Body::Answer { code, host, requester, accept } => {
                self.answer(&code, &host, &requester, accept, now, &mut step.out);
                step.out.extend(self.sync_statuses());
            }
        }
    }

    /// A room on `from` sent our players something.
    fn delivered(&mut self, from: &str, conns: Vec<ConnId>, tag: Tag, bytes: Vec<u8>, now: Instant, step: &mut Step) {
        let mut to = vec![];
        let mut moved = false;
        for c in conns {
            if !self.dids.contains_key(&c) {
                // (gone meanwhile)
                self.send(from, Body::Gone { conn: c });
                continue;
            }
            let away = self.away.get(&c).filter(|a| a.owner == from);
            match &tag {
                Tag::Joined { code, public } => {
                    let ours = match self.away.get(&c) {
                        Some(a) => a.owner == from && a.code == *code,
                        // (a match from the queue)
                        None => self.rooms.room_of(c).is_none() && self.queue.iter().any(|w| w.conn == c),
                    };
                    if !ours {
                        self.send(from, Body::Gone { conn: c });
                        continue;
                    }
                    self.away.insert(c, Away { owner: from.to_string(), code: code.clone(), joined: true, public: *public, at: now });
                    self.queue.retain(|w| w.conn != c);
                    moved = true;
                    to.push(c);
                }
                Tag::Ended => {
                    if away.is_some() {
                        self.away.remove(&c);
                        moved = true;
                        to.push(c);
                    } else if self.rooms.room_of(c).is_none() && !self.away.contains_key(&c) {
                        self.queue.retain(|w| w.conn != c);
                        to.push(c);
                    }
                }
                Tag::None => {
                    if away.is_some_and(|a| a.joined) {
                        to.push(c);
                    }
                }
            }
        }
        if !to.is_empty() {
            step.raw.push((to, bytes));
        }
        if moved {
            step.out.extend(self.sync_statuses());
        }
    }

    /// Every live replica's view (the periodic re-read: a replica missing
    /// from it has gone).
    pub fn on_views(&mut self, blobs: Vec<Vec<u8>>, now: Instant) -> Step {
        let me = self.cluster.id().to_string();
        let mut views = HashMap::new();
        for b in blobs {
            if let Some(v) = decode::<View>(&b).filter(|v| v.id != me) {
                views.insert(v.id.clone(), v);
            }
        }
        // (one we heard from just now stays, even if the read missed it)
        self.heard.retain(|_, at| now.duration_since(*at) < cluster::REFRESH);
        for (id, v) in std::mem::take(&mut self.views) {
            if self.heard.contains_key(&id) {
                views.entry(id).or_insert(v);
            }
        }
        self.views = views;
        let mut step = Step::default();
        self.sync_view(now, &mut step);
        self.finish(step, now)
    }

    /// The other replicas changed: presence, codes in use, and anyone tied
    /// to a replica that's gone.
    fn sync_view(&mut self, now: Instant, step: &mut Step) {
        let mut remote: BTreeMap<Did, &UserEntry> = BTreeMap::new();
        let mut ids: Vec<&String> = self.views.keys().collect();
        ids.sort();
        for id in ids {
            for u in &self.views[id].users {
                remote.entry(u.friend.did.clone()).or_insert(u);
            }
        }
        let remote: Vec<(Did, cluster::UserEntry)> = remote.into_iter().map(|(d, u)| (d, u.clone())).collect();
        let mut seen = HashSet::new();
        for (did, u) in &remote {
            seen.insert(did.clone());
            if self.presence.is_local(did) {
                continue;
            }
            let (out, jobs) = self.presence.online_remote(&u.friend, now);
            step.out.extend(out);
            step.jobs.extend(jobs);
        }
        for did in self.presence.remote_dids() {
            if !seen.contains(&did) {
                step.out.extend(self.presence.offline_remote(&did));
            }
        }
        self.rooms.reserved = self.views.values().flat_map(|v| v.rooms.iter().map(|r| r.code.clone())).collect();
        // our players in rooms on replicas that have gone: the room's gone
        let orphans: Vec<ConnId> = self.away.iter().filter(|(_, a)| !self.views.contains_key(&a.owner)).map(|(c, _)| *c).collect();
        for c in orphans {
            self.away.remove(&c);
            step.out.push(Out { to: vec![c], msg: ServerMsg::Error(ErrorCode::NoRoom) });
        }
        let lost: Vec<(String, ConnId)> =
            self.guests.values().filter(|g| !self.views.contains_key(&g.home)).map(|g| (g.home.clone(), g.conn)).collect();
        for (home, conn) in lost {
            self.drop_guest(&home, conn, now, &mut step.out);
        }
        step.out.extend(self.sync_statuses());
    }

    /// Shutting down (SIGTERM: a scale-down or a redeploy): every room here
    /// ends (its players, ours and other replicas', are told NO ROOM and
    /// go back to the menu), our players leave rooms elsewhere, and we
    /// leave the queue. Then `flush`, `farewell`, and close the sockets.
    pub fn shutdown(&mut self, now: Instant) -> Step {
        self.draining = true;
        let mut step = Step::default();
        // (ours in rooms elsewhere too: their sockets close with ours)
        let mut players = self.rooms.conns_in_rooms();
        players.extend(self.away.keys());
        if !players.is_empty() {
            step.out.push(Out { to: players, msg: ServerMsg::Error(ErrorCode::NoRoom) });
        }
        let step = self.finish(step, now);
        self.rooms.end_all();
        let guests: Vec<(String, ConnId)> = self.guests.values().map(|g| (g.home.clone(), g.conn)).collect();
        for (home, conn) in guests {
            self.drop_guest(&home, conn, now, &mut vec![]);
        }
        for conn in self.away.keys().copied().collect::<Vec<_>>() {
            self.leave_away(conn);
        }
        for w in std::mem::take(&mut self.queue) {
            self.cluster.queue_remove(self.cluster.id(), w.conn);
        }
        step
    }

    /// The last word: we're gone (others drop our players and rooms now
    /// rather than when our view expires).
    pub fn farewell(&mut self) {
        if self.cluster.clustered() && !self.gone {
            self.gone = true;
            self.publish(cluster::ALL, vec![Body::Bye]);
            self.cluster.drop_view();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presence::tests::{MockAppView, settle};
    use crate::presence::{DevAware, INVITE_GAP};

    const MAP: RoomMap = RoomMap { seed: 9, biome: 1, time: 0, weather: 2 };

    fn online(hub: &mut Hub, graph: &dyn crate::presence::RelationshipSource, conn: ConnId, h: &str, now: Instant) -> Vec<Out> {
        let step = hub.connect(conn, format!("did:dev:{h}"), h.into(), now);
        let mut out = step.out;
        out.extend(settle(&mut hub.presence, graph, step.jobs, now));
        out
    }

    fn sent_to(out: &[Out], conn: ConnId) -> Vec<&ServerMsg> {
        out.iter().filter(|o| o.to.contains(&conn)).map(|o| &o.msg).collect()
    }

    #[test]
    fn inviting_from_the_menu_makes_a_room_and_brings_the_friend_in() {
        let now = Instant::now();
        let graph = DevAware { inner: MockAppView::default(), dev_mutuals: true };
        let mut hub = Hub::new(true);
        online(&mut hub, &graph, 1, "alice", now);
        let out = online(&mut hub, &graph, 2, "bob", now);
        assert!(
            sent_to(&out, 1).iter().any(|m| matches!(m, ServerMsg::FriendUpdate(f) if f.handle == "bob")),
            "alice sees bob come online"
        );

        let step = hub.handle(1, ClientMsg::Invite { to: "did:dev:bob".into(), map: MAP }, now);
        let code = sent_to(&step.out, 1)
            .iter()
            .find_map(|m| match m {
                ServerMsg::Joined { code, map, .. } => {
                    assert_eq!(*map, MAP);
                    Some(code.clone())
                }
                _ => None,
            })
            .expect("alice is put in a new room");
        let invited = sent_to(&step.out, 2)
            .iter()
            .find_map(|m| match m {
                ServerMsg::Invited { from, code } => Some((from.handle.clone(), code.clone())),
                _ => None,
            })
            .expect("bob is invited");
        assert_eq!(invited, ("alice".to_string(), code.clone()));
        // bob sees alice in a room
        assert!(sent_to(&step.out, 2).iter().any(|m| matches!(m, ServerMsg::FriendUpdate(f) if f.status == FriendStatus::InRoom)));

        let step = hub.handle(2, ClientMsg::Join { code: code.clone() }, now);
        assert!(sent_to(&step.out, 2).iter().any(|m| matches!(m, ServerMsg::Joined { players, .. } if players.len() == 2)));
        assert_eq!(hub.rooms.room_of(1).map(|(c, _)| c.to_string()), Some(code.clone()));
        assert_eq!(hub.rooms.room_of(2).map(|(c, _)| c.to_string()), Some(code.clone()));

        // invited again from inside the room: no new room
        let step = hub.handle(2, ClientMsg::Leave, now);
        assert!(hub.rooms.room_of(2).is_none());
        assert!(sent_to(&step.out, 1).iter().any(|m| matches!(m, ServerMsg::FriendUpdate(f) if f.status == FriendStatus::Online)));
        let step = hub.handle(1, ClientMsg::Invite { to: "did:dev:bob".into(), map: MAP }, now + INVITE_GAP);
        assert!(sent_to(&step.out, 2).iter().any(|m| matches!(m, ServerMsg::Invited { code: c, .. } if *c == code)));
        assert!(!sent_to(&step.out, 1).iter().any(|m| matches!(m, ServerMsg::Joined { .. })));
        assert_eq!(hub.rooms.room_count(), 1);
    }

    #[test]
    fn invites_to_strangers_fail_without_saying_why() {
        let now = Instant::now();
        // dev users aren't mutuals unless asked
        let graph = DevAware { inner: MockAppView::default(), dev_mutuals: false };
        let mut hub = Hub::new(true);
        let out = online(&mut hub, &graph, 1, "alice", now);
        assert!(matches!(sent_to(&out, 1)[..], [ServerMsg::Welcome { .. }, ServerMsg::Friends(l), ServerMsg::Online(1)] if l.is_empty()));
        let out = online(&mut hub, &graph, 2, "bob", now);
        assert!(sent_to(&out, 1).is_empty(), "alice hears nothing about bob");
        for to in ["did:dev:bob", "did:dev:nobody"] {
            let step = hub.handle(1, ClientMsg::Invite { to: to.into(), map: MAP }, now);
            assert_eq!(step.out, vec![Out { to: vec![1], msg: ServerMsg::InviteFailed(to.into()) }]);
        }
        assert_eq!(hub.rooms.room_count(), 0, "no room made for a failed invite");
    }

    #[test]
    fn going_offline_leaves_the_room_and_tells_friends() {
        let now = Instant::now();
        let graph = DevAware { inner: MockAppView::default(), dev_mutuals: true };
        let mut hub = Hub::new(true);
        online(&mut hub, &graph, 1, "alice", now);
        online(&mut hub, &graph, 2, "bob", now);
        hub.handle(1, ClientMsg::Create { map: MAP, code: Some("TEST".into()) }, now);
        let step = hub.disconnect(1, now);
        assert!(sent_to(&step.out, 2).contains(&&ServerMsg::FriendGone("did:dev:alice".into())));
        assert_eq!(hub.rooms.player_count(), 0);
        assert_eq!(hub.presence.online_count(), 1);
    }

    /// alice hosts room TEST with bob in it; carol is everyone's mutual; dave is nobody's.
    fn party(now: Instant) -> (Hub, DevAware<MockAppView>) {
        let graph = DevAware { inner: MockAppView::default(), dev_mutuals: true };
        let mut hub = Hub::new(true);
        for (c, h) in [(1, "alice"), (2, "bob"), (3, "carol")] {
            online(&mut hub, &graph, c, h, now);
        }
        hub.handle(1, ClientMsg::Create { map: MAP, code: Some("TEST".into()) }, now);
        hub.handle(2, ClientMsg::Join { code: "TEST".into() }, now);
        (hub, graph)
    }

    fn mentions_code(out: &[Out], conn: ConnId) -> bool {
        sent_to(out, conn).iter().any(|m| format!("{m:?}").contains("TEST"))
    }

    #[test]
    fn challenge_runs_reach_both_boards() {
        let now = Instant::now();
        let graph = DevAware { inner: MockAppView::default(), dev_mutuals: true };
        let mut hub = Hub::new(true);
        online(&mut hub, &graph, 1, "alice", now);
        online(&mut hub, &graph, 2, "bob", now);
        online(&mut hub, &graph, 3, "carol", now);
        hub.handle(1, ClientMsg::ChallengeStart { seed: 11 }, now);
        hub.handle(2, ClientMsg::ChallengeStart { seed: 22 }, now);
        let later = now + Duration::from_secs(70);
        let step = hub.handle(1, ClientMsg::SubmitScore(Run { seed: 11, score: 550, wave: 2, kills: 3, secs: 60.0 }), later);
        assert_eq!(sent_to(&step.out, 1), vec![&ServerMsg::Standing(Some(Standing { rank: 1, score: 550, wave: 2 }))]);
        let step = hub.handle(2, ClientMsg::SubmitScore(Run { seed: 22, score: 1150, wave: 3, kills: 4, secs: 62.0 }), later);
        assert_eq!(sent_to(&step.out, 2), vec![&ServerMsg::Standing(Some(Standing { rank: 1, score: 1150, wave: 3 }))]);
        // carol never started a run: hers is ignored
        let step = hub.handle(3, ClientMsg::SubmitScore(Run { seed: 33, score: 100, wave: 1, kills: 1, secs: 30.0 }), later);
        assert_eq!(sent_to(&step.out, 3), vec![&ServerMsg::Standing(None)]);

        let step = hub.handle(1, ClientMsg::Leaderboard(BoardKind::Global), later);
        let [ServerMsg::LeaderboardData { rows, you, .. }] = &sent_to(&step.out, 1)[..] else { panic!("{:?}", step.out) };
        assert_eq!(rows.iter().map(|r| (r.rank, r.handle.as_str(), r.score)).collect::<Vec<_>>(), vec![(1, "bob", 1150), (2, "alice", 550)]);
        assert_eq!(you.map(|s| s.rank), Some(2));
        // friends: the pair's already cached from presence, so it answers at once
        let step = hub.handle(1, ClientMsg::Leaderboard(BoardKind::Friends), later);
        assert!(step.jobs.is_empty());
        let [ServerMsg::LeaderboardData { kind: BoardKind::Friends, rows, .. }] = &sent_to(&step.out, 1)[..] else { panic!("{:?}", step.out) };
        assert_eq!(rows.len(), 2);
    }

    fn joined(out: &[Out], conn: ConnId) -> Option<(String, bool, usize)> {
        sent_to(out, conn).iter().find_map(|m| match m {
            ServerMsg::Joined { code, public, players, .. } => Some((code.clone(), *public, players.len())),
            _ => None,
        })
    }

    fn queued(out: &[Out], conn: ConnId) -> bool {
        sent_to(out, conn).contains(&&ServerMsg::Queued)
    }

    /// Strangers (dev users who aren't mutuals) online on conns 1.., by handle.
    fn strangers(handles: &[&str], now: Instant) -> Hub {
        let graph = DevAware { inner: MockAppView::default(), dev_mutuals: false };
        let mut hub = Hub::new(true);
        for (i, h) in handles.iter().enumerate() {
            online(&mut hub, &graph, i as ConnId + 1, h, now);
        }
        hub
    }

    #[test]
    fn two_queued_players_are_matched_together() {
        let now = Instant::now();
        let mut hub = strangers(&["alice", "bob", "carol"], now);
        // nobody's playing: alice waits (no room made for her alone)
        let step = hub.handle(1, ClientMsg::QuickPlay { map: MAP }, now);
        assert!(queued(&step.out, 1));
        assert!(joined(&step.out, 1).is_none());
        assert_eq!((hub.rooms.room_count(), hub.queued()), (0, 1));
        // ...and keeps waiting
        assert!(joined(&hub.tick(now + Duration::from_secs(30)).out, 1).is_none());
        // bob quick-plays: both go into a new public room, on alice's map (the one she's been baking)
        let other = RoomMap { seed: 1, ..MAP };
        let step = hub.handle(2, ClientMsg::QuickPlay { map: other }, now);
        let (code, public, _) = joined(&step.out, 1).expect("alice is matched");
        assert!(public);
        assert_eq!(joined(&step.out, 2), Some((code.clone(), true, 2)));
        assert!(sent_to(&step.out, 2).iter().any(|m| matches!(m, ServerMsg::Joined { map, started: true, host: 0, .. } if *map == MAP)));
        assert_eq!((hub.rooms.room_count(), hub.queued()), (1, 0));
        // the next goes straight in
        let step = hub.handle(3, ClientMsg::QuickPlay { map: other }, now);
        assert!(!queued(&step.out, 3));
        assert_eq!(joined(&step.out, 3), Some((code, true, 3)));
    }

    #[test]
    fn a_queued_player_joins_a_populated_room() {
        let now = Instant::now();
        let mut hub = strangers(&["alice", "bob", "carol", "dave"], now);
        // a hosted room is never matched
        hub.handle(4, ClientMsg::Create { map: MAP, code: Some("TEST".into()) }, now);
        hub.handle(1, ClientMsg::QuickPlay { map: MAP }, now);
        let step = hub.handle(2, ClientMsg::QuickPlay { map: MAP }, now);
        let (code, ..) = joined(&step.out, 2).unwrap();
        // bob leaves: alice is alone in the room, and keeps flying there
        hub.handle(2, ClientMsg::Leave, now);
        let later = now + Duration::from_secs(60);
        let step = hub.tick(later);
        assert!(sent_to(&step.out, 1).iter().all(|m| !matches!(m, ServerMsg::Joined { .. } | ServerMsg::Queued)));
        assert_eq!(hub.rooms.room_of(1).map(|(c, _)| c.to_string()), Some(code.clone()), "alice isn't moved");
        assert_eq!(hub.queued(), 0);
        // carol quick-plays: into alice's room, no wait
        let step = hub.handle(3, ClientMsg::QuickPlay { map: RoomMap { seed: 2, ..MAP } }, later);
        assert!(!queued(&step.out, 3));
        assert_eq!(joined(&step.out, 3), Some((code.clone(), true, 2)));
        assert_eq!(hub.rooms.room_of(1).map(|(c, _)| c.to_string()), Some(code), "alice stays put");
    }

    #[test]
    fn a_waiting_player_goes_in_when_a_room_fills() {
        let now = Instant::now();
        let mut hub = strangers(&["alice", "bob", "carol"], now);
        // an empty public room in its grace period isn't a match...
        let mut out = vec![];
        let code = hub.rooms.create_room(2, MAP, Some("PUBB".into()), RoomKind::Public, now, &mut out).unwrap();
        hub.handle(2, ClientMsg::Leave, now);
        let step = hub.handle(1, ClientMsg::QuickPlay { map: MAP }, now);
        assert!(queued(&step.out, 1));
        // ...until someone's in it (by its code): then the next tick takes alice there
        hub.handle(3, ClientMsg::Join { code: code.clone() }, now);
        let step = hub.tick(now);
        assert_eq!(joined(&step.out, 1), Some((code, true, 2)));
        assert_eq!(hub.queued(), 0);
    }

    #[test]
    fn cancelling_or_going_offline_leaves_the_queue() {
        let now = Instant::now();
        let mut hub = strangers(&["alice", "bob", "carol", "dave"], now);
        hub.handle(1, ClientMsg::QuickPlay { map: MAP }, now);
        hub.handle(1, ClientMsg::CancelQueue, now);
        assert_eq!(hub.queued(), 0);
        // bob isn't matched with her
        let step = hub.handle(2, ClientMsg::QuickPlay { map: MAP }, now);
        assert!(queued(&step.out, 2) && joined(&step.out, 2).is_none());
        assert!(sent_to(&step.out, 1).is_empty());
        // bob drops: carol waits alone
        hub.disconnect(2, now);
        let step = hub.handle(3, ClientMsg::QuickPlay { map: MAP }, now);
        assert!(queued(&step.out, 3) && joined(&step.out, 3).is_none());
        // LEAVE does it too
        hub.handle(3, ClientMsg::Leave, now);
        let step = hub.handle(4, ClientMsg::QuickPlay { map: MAP }, now);
        assert!(queued(&step.out, 4));
        assert_eq!(hub.rooms.room_count(), 0);
    }

    #[test]
    fn joining_a_room_another_way_leaves_the_queue() {
        let now = Instant::now();
        let graph = DevAware { inner: MockAppView::default(), dev_mutuals: true };
        let mut hub = Hub::new(true);
        for (c, h) in [(1, "alice"), (2, "bob"), (3, "carol")] {
            online(&mut hub, &graph, c, h, now);
        }
        hub.handle(1, ClientMsg::QuickPlay { map: MAP }, now);
        // bob invites alice from the menu; she accepts (a code join)
        let step = hub.handle(2, ClientMsg::Invite { to: "did:dev:alice".into(), map: MAP }, now);
        let code = sent_to(&step.out, 1)
            .iter()
            .find_map(|m| match m {
                ServerMsg::Invited { code, .. } => Some(code.clone()),
                _ => None,
            })
            .expect("a queued player still gets invites");
        hub.handle(1, ClientMsg::Join { code }, now);
        assert_eq!(hub.queued(), 0);
        // carol isn't matched with anyone
        let step = hub.handle(3, ClientMsg::QuickPlay { map: MAP }, now);
        assert!(queued(&step.out, 3));
        // carol asks to join bob's room (a request, from the queue): the host lets her in
        hub.handle(3, ClientMsg::RequestJoin { to: "did:dev:bob".into() }, now);
        hub.handle(2, ClientMsg::AnswerJoin { did: "did:dev:carol".into(), accept: true }, now);
        assert!(hub.rooms.room_of(3).is_some());
        assert_eq!(hub.queued(), 0);
    }

    #[test]
    fn the_online_count_goes_out_on_change_now_and_then() {
        let now = Instant::now();
        let mut hub = strangers(&["alice"], now);
        let graph = DevAware { inner: MockAppView::default(), dev_mutuals: false };
        let count = |out: &[Out], c| {
            sent_to(out, c).iter().find_map(|m| match m {
                ServerMsg::Online(n) => Some(*n),
                _ => None,
            })
        };
        assert_eq!(count(&hub.tick(now).out, 1), Some(1));
        // bob hears the count at once; alice at the next broadcast, not before the gap
        let out = online(&mut hub, &graph, 2, "bob", now);
        assert_eq!(count(&out, 2), Some(2));
        assert_eq!(count(&hub.tick(now + Duration::from_secs(1)).out, 1), None);
        let step = hub.tick(now + ONLINE_GAP);
        assert_eq!((count(&step.out, 1), count(&step.out, 2)), (Some(2), Some(2)));
        // unchanged: nothing more
        assert_eq!(count(&hub.tick(now + ONLINE_GAP * 3).out, 1), None);
        hub.disconnect(2, now);
        assert_eq!(count(&hub.tick(now + ONLINE_GAP * 4).out, 1), Some(1));
    }

    #[test]
    fn quick_play_prefers_mutuals_and_skips_full_rooms() {
        let now = Instant::now();
        // real-looking DIDs, so the mock AppView decides: only me and friend are mutuals
        let mut graph = MockAppView::default();
        graph.mutual("did:plc:me", "did:plc:friend");
        let mut hub = Hub::new(true);
        let on = |hub: &mut Hub, conn: ConnId, h: &str| {
            let step = hub.connect(conn, format!("did:plc:{h}"), h.into(), now);
            settle(&mut hub.presence, &graph, step.jobs, now);
        };
        // a fuller room of strangers, and a room with our friend
        let mut out = vec![];
        on(&mut hub, 10, "s0");
        let big = hub.rooms.create_room(10, MAP, Some("BIGG".into()), RoomKind::Public, now, &mut out).unwrap();
        let mut c = 10;
        for i in 1..5 {
            c += 1;
            on(&mut hub, c, &format!("s{i}"));
            hub.handle(c, ClientMsg::Join { code: big.clone() }, now);
        }
        on(&mut hub, 2, "friend");
        let small = hub.rooms.create_room(2, MAP, Some("SMAL".into()), RoomKind::Public, now, &mut out).unwrap();
        on(&mut hub, 1, "me");
        assert!(hub.presence.online_mutuals("did:plc:me", "did:plc:friend"));
        let step = hub.handle(1, ClientMsg::QuickPlay { map: MAP }, now);
        assert_eq!(joined(&step.out, 1).map(|j| j.0), Some(small.clone()), "with the mutual");
        // a stranger goes to the fuller one
        on(&mut hub, 3, "stranger");
        let step = hub.handle(3, ClientMsg::QuickPlay { map: MAP }, now);
        assert_eq!(joined(&step.out, 3).map(|j| j.0), Some(big.clone()), "the fullest");
        // fill the big one up: the next stranger goes to the small one
        for i in 0..(MAX_PLAYERS - 6) {
            c += 1;
            on(&mut hub, c, &format!("t{i}"));
            hub.handle(c, ClientMsg::Join { code: big.clone() }, now);
        }
        on(&mut hub, 4, "late");
        let step = hub.handle(4, ClientMsg::QuickPlay { map: MAP }, now);
        assert_eq!(joined(&step.out, 4).map(|j| j.0), Some(small), "never a full room");
    }

    #[test]
    fn mutuals_hop_into_public_rooms_only() {
        let now = Instant::now();
        let (mut hub, _) = party(now);
        // alice hosts TEST (bob's in it); carol may not hop into a hosted room
        let step = hub.handle(3, ClientMsg::JoinFriend { did: "did:dev:bob".into() }, now);
        assert_eq!(step.out, vec![Out { to: vec![3], msg: ServerMsg::JoinDeclined("did:dev:bob".into()) }]);
        assert!(!mentions_code(&step.out, 3));
        // bob goes quick-playing (matched with erin): carol sees him in a public room and hops in
        let graph = DevAware { inner: MockAppView::default(), dev_mutuals: true };
        online(&mut hub, &graph, 5, "erin", now);
        hub.handle(5, ClientMsg::QuickPlay { map: MAP }, now);
        let step = hub.handle(2, ClientMsg::QuickPlay { map: MAP }, now);
        let (code, true, _) = joined(&step.out, 2).unwrap() else { panic!() };
        assert!(sent_to(&step.out, 3).iter().any(|m| matches!(m, ServerMsg::FriendUpdate(f) if f.handle == "bob" && f.status == FriendStatus::InPublic)));
        let step = hub.handle(3, ClientMsg::JoinFriend { did: "did:dev:bob".into() }, now);
        assert_eq!(joined(&step.out, 3), Some((code.clone(), true, 3)));
        // again from inside: nothing to do
        let step = hub.handle(3, ClientMsg::JoinFriend { did: "did:dev:bob".into() }, now);
        assert_eq!(step.out, vec![Out { to: vec![3], msg: ServerMsg::JoinDeclined("did:dev:bob".into()) }]);
        // a friend in no room
        hub.handle(2, ClientMsg::Leave, now);
        let step = hub.handle(1, ClientMsg::JoinFriend { did: "did:dev:bob".into() }, now);
        assert_eq!(step.out, vec![Out { to: vec![1], msg: ServerMsg::JoinDeclined("did:dev:bob".into()) }]);

        // a stranger can't hop in, even into a public room
        let strangers = DevAware { inner: MockAppView::default(), dev_mutuals: false };
        let mut hub2 = Hub::new(true);
        online(&mut hub2, &strangers, 1, "alice", now);
        online(&mut hub2, &strangers, 4, "dave", now);
        hub2.handle(1, ClientMsg::QuickPlay { map: MAP }, now);
        let step = hub2.handle(4, ClientMsg::JoinFriend { did: "did:dev:alice".into() }, now);
        assert_eq!(step.out, vec![Out { to: vec![4], msg: ServerMsg::JoinDeclined("did:dev:alice".into()) }]);
        assert!(hub2.rooms.room_of(4).is_none());
    }

    #[test]
    fn full_public_rooms_show_full_and_turn_mutuals_away() {
        let now = Instant::now();
        let graph = DevAware { inner: MockAppView::default(), dev_mutuals: true };
        let mut hub = Hub::new(true);
        online(&mut hub, &graph, 1, "alice", now);
        hub.handle(1, ClientMsg::QuickPlay { map: MAP }, now);
        for i in 1..MAX_PLAYERS as u64 {
            online(&mut hub, &graph, 10 + i, &format!("p{i}"), now);
            hub.handle(10 + i, ClientMsg::QuickPlay { map: MAP }, now);
        }
        assert_eq!(hub.rooms.room_count(), 1);
        let out = online(&mut hub, &graph, 2, "bob", now);
        let alice = sent_to(&out, 2)
            .iter()
            .find_map(|m| match m {
                ServerMsg::Friends(l) => l.iter().find(|f| f.handle == "alice").cloned(),
                ServerMsg::FriendUpdate(f) if f.handle == "alice" => Some(f.clone()),
                _ => None,
            })
            .expect("bob sees alice");
        assert_eq!(alice.status, FriendStatus::RoomFull);
        let step = hub.handle(2, ClientMsg::JoinFriend { did: "did:dev:alice".into() }, now);
        assert_eq!(step.out, vec![Out { to: vec![2], msg: ServerMsg::JoinDeclined("did:dev:alice".into()) }]);
        // and quick play has bob wait (no room of his own)
        let step = hub.handle(2, ClientMsg::QuickPlay { map: MAP }, now);
        assert!(queued(&step.out, 2) && joined(&step.out, 2).is_none());
        assert_eq!(hub.rooms.room_count(), 1);
    }

    #[test]
    fn invites_from_a_public_room_bring_the_friend_in() {
        let now = Instant::now();
        let graph = DevAware { inner: MockAppView::default(), dev_mutuals: true };
        let mut hub = Hub::new(true);
        online(&mut hub, &graph, 1, "alice", now);
        online(&mut hub, &graph, 2, "bob", now);
        online(&mut hub, &graph, 3, "carol", now);
        hub.handle(3, ClientMsg::QuickPlay { map: MAP }, now);
        let step = hub.handle(1, ClientMsg::QuickPlay { map: MAP }, now);
        let (code, ..) = joined(&step.out, 1).unwrap();
        let step = hub.handle(1, ClientMsg::Invite { to: "did:dev:bob".into(), map: MAP }, now);
        assert!(sent_to(&step.out, 2).iter().any(|m| matches!(m, ServerMsg::Invited { code: c, .. } if *c == code)));
        assert_eq!(hub.rooms.room_count(), 1, "no new room");
        let step = hub.handle(2, ClientMsg::Join { code: code.clone() }, now);
        assert_eq!(joined(&step.out, 2), Some((code, true, 3)));
    }

    #[test]
    fn join_requests_go_to_the_host_and_admit_on_accept() {
        let now = Instant::now();
        let (mut hub, _) = party(now);
        // carol asks via bob (not the host): alice is asked
        let step = hub.handle(3, ClientMsg::RequestJoin { to: "did:dev:bob".into() }, now);
        assert!(matches!(sent_to(&step.out, 1)[..], [ServerMsg::JoinRequest(f)] if f.handle == "carol"));
        assert!(sent_to(&step.out, 2).is_empty());
        assert!(!mentions_code(&step.out, 3), "no code before the host says yes");
        // bob isn't the host: his answer does nothing
        assert!(hub.handle(2, ClientMsg::AnswerJoin { did: "did:dev:carol".into(), accept: true }, now).out.is_empty());
        let step = hub.handle(1, ClientMsg::AnswerJoin { did: "did:dev:carol".into(), accept: true }, now);
        assert!(sent_to(&step.out, 3).iter().any(|m| matches!(m, ServerMsg::Joined { code, players, .. } if code == "TEST" && players.len() == 3)));
        assert_eq!(hub.rooms.room_of(3).map(|(c, _)| c.to_string()), Some("TEST".into()));
        // answered: a second answer does nothing
        assert!(hub.handle(1, ClientMsg::AnswerJoin { did: "did:dev:carol".into(), accept: true }, now).out.is_empty());
    }

    #[test]
    fn join_requests_decline_on_no_timeout_and_strangers() {
        let now = Instant::now();
        let (mut hub, graph) = party(now);
        let declined = |out: &[Out], c| sent_to(out, c).contains(&&ServerMsg::JoinDeclined("did:dev:alice".into()));
        hub.handle(3, ClientMsg::RequestJoin { to: "did:dev:alice".into() }, now);
        let step = hub.handle(1, ClientMsg::AnswerJoin { did: "did:dev:carol".into(), accept: false }, now);
        assert!(declined(&step.out, 3));
        assert!(hub.rooms.room_of(3).is_none());
        // again straight away: rate-limited, declined at once and the host isn't bothered
        let step = hub.handle(3, ClientMsg::RequestJoin { to: "did:dev:alice".into() }, now);
        assert!(declined(&step.out, 3) && sent_to(&step.out, 1).is_empty());
        // later: unanswered, it times out
        let later = now + crate::presence::REQUEST_GAP;
        let step = hub.handle(3, ClientMsg::RequestJoin { to: "did:dev:alice".into() }, later);
        assert!(!sent_to(&step.out, 1).is_empty());
        // (only the online count goes out)
        assert!(hub.tick(later + Duration::from_secs(5)).out.iter().all(|o| matches!(o.msg, ServerMsg::Online(_))));
        assert!(declined(&hub.tick(later + Duration::from_secs_f32(JOIN_REQUEST_SECS + 1.0)).out, 3));

        // a friend who's in no room, and a stranger
        let step = hub.handle(1, ClientMsg::RequestJoin { to: "did:dev:carol".into() }, now);
        assert_eq!(step.out, vec![Out { to: vec![1], msg: ServerMsg::JoinDeclined("did:dev:carol".into()) }]);
        let strangers = DevAware { inner: MockAppView::default(), dev_mutuals: false };
        let _ = graph;
        let mut hub2 = Hub::new(true);
        online(&mut hub2, &strangers, 1, "alice", now);
        online(&mut hub2, &strangers, 4, "dave", now);
        hub2.handle(1, ClientMsg::Create { map: MAP, code: Some("TEST".into()) }, now);
        let step = hub2.handle(4, ClientMsg::RequestJoin { to: "did:dev:alice".into() }, now);
        assert_eq!(step.out, vec![Out { to: vec![4], msg: ServerMsg::JoinDeclined("did:dev:alice".into()) }]);
    }

    #[test]
    fn the_metrics_snapshot_counts_rooms_players_and_the_queue() {
        let now = Instant::now();
        let (mut hub, graph) = party(now);
        online(&mut hub, &graph, 4, "dave", now);
        hub.handle(3, ClientMsg::QuickPlay { map: MAP }, now);
        let g = hub.gauges();
        assert_eq!((g.players_online, g.signed_in, g.queue), (4, 4, 1));
        assert_eq!(g.rooms, [[0, 0], [1, 0]], "one hosted lobby");
        assert_eq!(g.in_rooms, [0, 2]);
        assert_eq!(g.sizes[2], 1);
        hub.handle(4, ClientMsg::QuickPlay { map: MAP }, now);
        let g = hub.gauges();
        assert_eq!(g.queue, 0);
        assert_eq!(g.rooms, [[0, 1], [1, 0]], "carol and dave matched into a public room");
        assert_eq!(g.in_rooms, [2, 2]);
        assert_eq!(g.sizes[2], 2);
    }

    // ---- across replicas ----

    use crate::cluster::Bus;

    /// Several replicas on one in-process bus, a dev follow graph where
    /// everyone's everyone's mutual, and a clock.
    struct Cluster {
        bus: Bus,
        hubs: Vec<Hub>,
        graph: DevAware<MockAppView>,
        now: Instant,
    }

    /// What reached whom: (replica, conn, message).
    type Got = Vec<(usize, ConnId, ServerMsg)>;

    impl Cluster {
        fn new(ids: &[&str]) -> Self {
            let bus = Bus::new();
            let hubs = ids.iter().map(|id| Hub::with(true, bus.replica(id))).collect();
            let mut c = Cluster { bus, hubs, graph: DevAware { inner: MockAppView::default(), dev_mutuals: true }, now: Instant::now() };
            c.pump();
            c.refresh();
            c
        }

        /// Collects a step's messages, and runs its lookups.
        fn take(&mut self, h: usize, step: Step, got: &mut Got) {
            for o in step.out {
                for c in o.to {
                    got.push((h, c, o.msg.clone()));
                }
            }
            for (to, bytes) in step.raw {
                let msg: ServerMsg = decode(&bytes).expect("a server message");
                for c in to {
                    got.push((h, c, msg.clone()));
                }
            }
            for job in step.jobs {
                let result = bevy::tasks::block_on(job.run(&self.graph));
                let s = self.hubs[h].apply(result, self.now);
                self.take(h, s, got);
            }
        }

        /// Moves envelopes until nobody has anything to say.
        fn pump(&mut self) -> Got {
            let mut got = vec![];
            for _ in 0..20 {
                let mut any = false;
                for h in 0..self.hubs.len() {
                    self.hubs[h].flush(self.now);
                }
                for h in 0..self.hubs.len() {
                    let id = self.hubs[h].id().to_string();
                    for env in self.bus.take_inbox(&id) {
                        any = true;
                        let s = self.hubs[h].on_cluster(&env, self.now);
                        self.take(h, s, &mut got);
                    }
                }
                if !any {
                    return got;
                }
            }
            panic!("the replicas never settled");
        }

        /// The periodic re-read of every view.
        fn refresh(&mut self) -> Got {
            let mut got = vec![];
            for h in 0..self.hubs.len() {
                let s = self.hubs[h].on_views(self.bus.views(), self.now);
                self.take(h, s, &mut got);
            }
            got.extend(self.pump());
            got
        }

        fn online(&mut self, h: usize, conn: ConnId, handle: &str) -> Got {
            let mut got = vec![];
            let s = self.hubs[h].connect(conn, format!("did:dev:{handle}"), handle.into(), self.now);
            self.take(h, s, &mut got);
            got.extend(self.pump());
            got
        }

        fn send(&mut self, h: usize, conn: ConnId, msg: ClientMsg) -> Got {
            let mut got = vec![];
            let s = self.hubs[h].handle(conn, msg, self.now);
            self.take(h, s, &mut got);
            got.extend(self.pump());
            got
        }

        fn tick(&mut self, h: usize) -> Got {
            let mut got = vec![];
            let s = self.hubs[h].tick(self.now);
            self.take(h, s, &mut got);
            got.extend(self.pump());
            got
        }
    }

    fn to(got: &Got, h: usize, conn: ConnId) -> Vec<ServerMsg> {
        got.iter().filter(|(hh, c, _)| *hh == h && *c == conn).map(|(_, _, m)| m.clone()).collect()
    }

    fn joined_in(got: &Got, h: usize, conn: ConnId) -> Option<(String, bool, usize, PlayerId)> {
        to(got, h, conn).into_iter().find_map(|m| match m {
            ServerMsg::Joined { code, public, players, you, .. } => Some((code, public, players.len(), you)),
            _ => None,
        })
    }

    fn fly(pos: [f32; 3]) -> ClientMsg {
        ClientMsg::State(PlayerState {
            t: 0.0,
            pos,
            rot: [0.0, 0.0, 0.0, 1.0],
            vel: [0.0; 3],
            throttle: 0.7,
            g: 1.0,
            flags: 0,
            health: 100.0,
            status: Status::Flying,
        })
    }

    #[test]
    fn quick_play_matches_across_replicas_and_bridges_the_fight() {
        let mut c = Cluster::new(&["a", "b"]);
        c.online(0, 1, "alice");
        let got = c.online(1, 1, "bob");
        // everyone hears the cluster's count, and sees their friend on the other replica
        assert!(to(&got, 1, 1).contains(&ServerMsg::Online(2)), "{got:?}");
        assert!(to(&got, 0, 1).iter().any(|m| matches!(m, ServerMsg::FriendUpdate(f) if f.handle == "bob")), "{got:?}");
        assert_eq!(c.hubs[0].presence.online_count(), 2);

        // alice waits on a; bob quick-plays on b: the first in line's replica (a) makes the room
        let got = c.send(0, 1, ClientMsg::QuickPlay { map: MAP });
        assert!(to(&got, 0, 1).contains(&ServerMsg::Queued));
        let got = c.send(1, 1, ClientMsg::QuickPlay { map: RoomMap { seed: 1, ..MAP } });
        assert!(to(&got, 1, 1).contains(&ServerMsg::Queued), "{got:?}");
        c.now += MATCH_GAP;
        let got = c.tick(0);
        let (code, public, _, alice) = joined_in(&got, 0, 1).expect("alice is matched");
        let (bcode, _, n, bob) = joined_in(&got, 1, 1).expect("bob is matched, over the bridge");
        assert!(public);
        assert_eq!((bcode, n), (code.clone(), 2));
        assert!(to(&got, 1, 1).iter().any(|m| matches!(m, ServerMsg::Joined { map, .. } if *map == MAP)), "on alice's map");
        assert_eq!((c.hubs[0].rooms.room_count(), c.hubs[1].rooms.room_count()), (1, 0));
        assert_eq!(c.bus.queue_len(), 0);
        assert_eq!(c.hubs[1].queued(), 0);

        // states both ways
        let got = c.send(1, 1, fly([500.0, 0.0, 0.0]));
        assert!(to(&got, 0, 1).iter().any(|m| matches!(m, ServerMsg::State { id, .. } if *id == bob)), "{got:?}");
        let got = c.send(0, 1, fly([0.0; 3]));
        assert!(to(&got, 1, 1).iter().any(|m| matches!(m, ServerMsg::State { id, .. } if *id == alice)), "{got:?}");
        // alice shoots bob down: the owner decides, and bob hears it
        let mut all = vec![];
        for _ in 0..20 {
            c.now += Duration::from_millis(40);
            c.send(0, 1, fly([0.0; 3]));
            c.send(1, 1, fly([500.0, 0.0, 0.0]));
            all.extend(c.send(0, 1, ClientMsg::Hit { target: bob, weapon: Weapon::Gun, damage: 5.0 }));
        }
        assert!(to(&all, 1, 1).contains(&ServerMsg::Killed { victim: bob, killer: Some(alice), record: (1, 0) }), "{all:?}");
        assert!(to(&all, 1, 1).contains(&ServerMsg::Scores(vec![(alice, 1, 0), (bob, 0, 1)])));
        let got = c.send(1, 1, ClientMsg::Respawn);
        assert!(to(&got, 1, 1).contains(&ServerMsg::Respawned(bob)) && to(&got, 0, 1).contains(&ServerMsg::Respawned(bob)));

        // a third player on b quick-plays: straight into a's room
        c.online(1, 2, "carol");
        let got = c.send(1, 2, ClientMsg::QuickPlay { map: MAP });
        assert_eq!(joined_in(&got, 1, 2).map(|j| (j.0, j.2)), Some((code.clone(), 3)));
        // bob leaves: alice hears it
        let got = c.send(1, 1, ClientMsg::Leave);
        assert!(to(&got, 0, 1).contains(&ServerMsg::PlayerLeft(bob)), "{got:?}");
        let g = c.hubs[0].snapshot().gauges;
        assert_eq!((g.players_online, g.in_rooms[0]), (3, 2));
    }

    #[test]
    fn codes_invites_and_lobbies_work_across_replicas() {
        let mut c = Cluster::new(&["a", "b"]);
        c.online(0, 1, "alice");
        c.online(1, 1, "bob");
        c.online(1, 2, "carol");
        // a hosted room on a, joined by code from b
        let got = c.send(0, 1, ClientMsg::Create { map: MAP, code: Some("TEST".into()) });
        assert_eq!(joined_in(&got, 0, 1).map(|j| j.0), Some("TEST".into()));
        let got = c.send(1, 1, ClientMsg::Join { code: "test".into() });
        assert!(to(&got, 1, 1).iter().any(|m| matches!(m, ServerMsg::Joined { code, started: false, host: 1, players, .. } if code == "TEST" && players.len() == 2)), "{got:?}");
        // the code isn't handed out again on b
        let got = c.send(1, 2, ClientMsg::Create { map: MAP, code: Some("TEST".into()) });
        assert_ne!(joined_in(&got, 1, 2).map(|j| j.0), Some("TEST".into()));
        c.send(1, 2, ClientMsg::Leave);
        // the host starts it: bob hears
        let got = c.send(0, 1, ClientMsg::Start);
        assert!(to(&got, 1, 1).contains(&ServerMsg::Started));
        // alice sees bob in her room; carol sees alice InRoom
        assert_eq!(c.hubs[1].presence.friend("did:dev:alice").map(|f| f.status), Some(FriendStatus::InRoom));

        // an invite from a to carol on b, and in she comes
        let got = c.send(0, 1, ClientMsg::Invite { to: "did:dev:carol".into(), map: MAP });
        assert!(to(&got, 1, 2).iter().any(|m| matches!(m, ServerMsg::Invited { code, from } if code == "TEST" && from.handle == "alice")), "{got:?}");
        let before = M.joins.snapshot().into_iter().find(|(l, _)| l.contains("invite")).map(|(_, n)| n).unwrap_or(0);
        let got = c.send(1, 2, ClientMsg::Join { code: "TEST".into() });
        assert_eq!(joined_in(&got, 1, 2).map(|j| j.2), Some(3));
        let after = M.joins.snapshot().into_iter().find(|(l, _)| l.contains("invite")).map(|(_, n)| n).unwrap_or(0);
        assert!(after > before, "counted as an invite join");
        // invites are rate-limited across the cluster: alice again at once fails
        let got = c.send(0, 1, ClientMsg::Invite { to: "did:dev:carol".into(), map: MAP });
        assert!(to(&got, 0, 1).contains(&ServerMsg::InviteFailed("did:dev:carol".into())));

        // an unknown code is NO ROOM, wherever it's asked
        let got = c.send(1, 1, ClientMsg::Join { code: "ZZZZ".into() });
        assert!(to(&got, 1, 1).contains(&ServerMsg::Error(ErrorCode::NoRoom)));
    }

    #[test]
    fn requests_and_hop_ins_work_across_replicas() {
        let mut c = Cluster::new(&["a", "b"]);
        c.online(0, 1, "alice");
        c.online(0, 2, "erin");
        c.online(1, 1, "bob");
        c.online(1, 2, "carol");
        // alice hosts on a; carol (b) asks to join via alice: alice is asked, carol gets in on yes
        c.send(0, 1, ClientMsg::Create { map: MAP, code: Some("HOST".into()) });
        let code = c.hubs[0].rooms.room_of(1).unwrap().0.to_string();
        let got = c.send(1, 2, ClientMsg::RequestJoin { to: "did:dev:alice".into() });
        assert!(matches!(&to(&got, 0, 1)[..], [ServerMsg::JoinRequest(f)] if f.handle == "carol"), "{got:?}");
        assert!(!format!("{:?}", to(&got, 1, 2)).contains(&code), "no code before the host says yes");
        let got = c.send(0, 1, ClientMsg::AnswerJoin { did: "did:dev:carol".into(), accept: true });
        assert_eq!(joined_in(&got, 1, 2).map(|j| (j.0, j.2)), Some((code.clone(), 2)), "{got:?}");

        // alice leaves: carol (a guest on a) hosts now; bob (b) asks via carol, and carol answers from b
        let got = c.send(0, 1, ClientMsg::Leave);
        assert!(to(&got, 1, 2).iter().any(|m| matches!(m, ServerMsg::Host(_))), "{got:?}");
        let got = c.send(1, 1, ClientMsg::RequestJoin { to: "did:dev:carol".into() });
        assert!(matches!(&to(&got, 1, 2)[..], [ServerMsg::JoinRequest(f)] if f.handle == "bob"), "{got:?}");
        let got = c.send(1, 2, ClientMsg::AnswerJoin { did: "did:dev:bob".into(), accept: false });
        assert!(to(&got, 1, 1).contains(&ServerMsg::JoinDeclined("did:dev:carol".into())), "{got:?}");

        // hop-ins: erin (a) quick-plays with bob (b): a public room; alice (a) hops in through bob
        c.send(1, 1, ClientMsg::QuickPlay { map: MAP });
        c.send(0, 2, ClientMsg::QuickPlay { map: MAP });
        c.now += MATCH_GAP;
        c.tick(1);
        c.tick(0);
        let room = c.hubs[0].rooms.room_of(2).or(c.hubs[1].rooms.room_of(1)).map(|r| r.0.to_string()).expect("matched");
        assert_eq!(c.hubs[0].presence.friend("did:dev:bob").map(|f| f.status), Some(FriendStatus::InPublic));
        let got = c.send(0, 1, ClientMsg::JoinFriend { did: "did:dev:bob".into() });
        assert_eq!(joined_in(&got, 0, 1).map(|j| (j.0, j.2)), Some((room, 3)), "{got:?}");
        // a hosted room can't be hopped into
        let got = c.send(0, 2, ClientMsg::JoinFriend { did: "did:dev:carol".into() });
        assert!(to(&got, 0, 2).contains(&ServerMsg::JoinDeclined("did:dev:carol".into())), "{got:?}");
    }

    #[test]
    fn a_replica_that_goes_takes_its_rooms_and_players_with_it() {
        let mut c = Cluster::new(&["a", "b"]);
        c.online(0, 1, "alice");
        c.online(1, 1, "bob");
        c.send(0, 1, ClientMsg::Create { map: MAP, code: Some("TEST".into()) });
        c.send(1, 1, ClientMsg::Join { code: "TEST".into() });
        assert_eq!(c.hubs[1].presence.online_count(), 2);
        // a dies without a word: once its view's gone, bob's room is gone and alice is offline
        c.bus.kill("a");
        c.now += cluster::REFRESH;
        let got = c.refresh();
        assert!(to(&got, 1, 1).contains(&ServerMsg::Error(ErrorCode::NoRoom)), "{got:?}");
        assert!(to(&got, 1, 1).contains(&ServerMsg::FriendGone("did:dev:alice".into())));
        assert_eq!(c.hubs[1].presence.online_count(), 1);
        // and bob plays on: quick play waits on b (a's queue entries are gone too)
        let got = c.send(1, 1, ClientMsg::QuickPlay { map: MAP });
        assert!(to(&got, 1, 1).contains(&ServerMsg::Queued));
    }

    #[test]
    fn a_replica_shutting_down_ends_its_rooms_cleanly() {
        let mut c = Cluster::new(&["a", "b"]);
        c.online(0, 1, "alice");
        c.online(1, 1, "bob");
        c.online(1, 2, "carol");
        c.send(0, 1, ClientMsg::Create { map: MAP, code: Some("TEST".into()) });
        c.send(1, 1, ClientMsg::Join { code: "TEST".into() });
        // carol (b) hosts a room on b with alice (a) in it
        c.send(1, 2, ClientMsg::Create { map: MAP, code: Some("BBBB".into()) });
        let got = c.send(0, 1, ClientMsg::Join { code: "BBBB".into() });
        assert!(joined_in(&got, 0, 1).is_some());
        // a shuts down: its room's players are told, and its own player leaves b's room
        let mut got = vec![];
        let s = c.hubs[0].shutdown(c.now);
        c.take(0, s, &mut got);
        c.hubs[0].flush(c.now);
        c.hubs[0].farewell();
        got.extend(c.pump());
        assert!(to(&got, 0, 1).contains(&ServerMsg::Error(ErrorCode::NoRoom)), "{got:?}");
        assert!(to(&got, 1, 1).contains(&ServerMsg::Error(ErrorCode::NoRoom)), "bob's room (on a) ended: {got:?}");
        assert!(to(&got, 1, 2).iter().any(|m| matches!(m, ServerMsg::PlayerLeft(_))), "alice left carol's room");
        assert_eq!(c.hubs[1].others(), 0, "b let a go at once");
        assert_eq!(c.hubs[1].presence.online_count(), 2);
        // bob carries on on b
        let got = c.send(1, 1, ClientMsg::Join { code: "BBBB".into() });
        assert_eq!(joined_in(&got, 1, 1).map(|j| j.2), Some(2));
    }

    #[test]
    fn guests_whose_home_goes_quiet_are_dropped() {
        let mut c = Cluster::new(&["a", "b"]);
        c.online(0, 1, "alice");
        c.online(1, 1, "bob");
        c.send(0, 1, ClientMsg::Create { map: MAP, code: Some("TEST".into()) });
        c.send(1, 1, ClientMsg::Join { code: "TEST".into() });
        assert_eq!(c.hubs[0].rooms.player_count(), 2);
        // pings keep a lobby guest
        c.now += GUEST_IDLE / 2;
        c.send(1, 1, ClientMsg::Ping);
        c.now += GUEST_IDLE / 2;
        c.tick(0);
        assert_eq!(c.hubs[0].rooms.player_count(), 2);
        // silence doesn't
        c.now += GUEST_IDLE;
        let got = c.tick(0);
        assert_eq!(c.hubs[0].rooms.player_count(), 1);
        assert!(to(&got, 0, 1).iter().any(|m| matches!(m, ServerMsg::PlayerLeft(_))));
    }
}
