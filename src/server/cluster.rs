//! What the server's replicas share (see README, "Scaling").
//!
//! Every replica is the same program. A player's WebSocket lands on any of
//! them (Railway balances connections at random), and that replica is the
//! player's *home*: it holds their presence, friends list, invites, join
//! requests and queue entry. A room lives on the replica that made it (its
//! *owner*), which keeps the room's authoritative state. A player whose home
//! isn't the room's owner is a *guest* there: their home forwards their room
//! messages to the owner, and the owner's messages for them back (see
//! `hub`). Players whose home owns the room never touch the shared state.
//!
//! The shared state is behind [`Backend`]: Redis in production
//! (`redis_backend`), or [`Memory`] in-process (no `REDIS_URL`: local dev and
//! tests). Each replica publishes a [`View`] of itself (its signed-in users,
//! their statuses and its rooms) to a key with a short TTL, refreshed every
//! second and broadcast whenever it changes, so every replica knows who's
//! online and where every room is, and a replica that dies drops out when
//! its key expires. Messages to one replica (bridged room traffic, invites,
//! join requests and answers) go on that replica's channel as an
//! [`Envelope`] of [`Body`]s: binary (postcard), one per destination per
//! game-loop tick.
//!
//! The quick-play queue, the rate limits that matter for abuse, challenge
//! starts, the AppView relationship cache and the metrics totals are shared
//! too.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::metrics::Gauges;
use crate::presence::Did;
use crate::protocol::{ClientMsg, Friend, PROTOCOL_VERSION, RoomMap, ServerMsg};
use crate::rooms::ConnId;

/// The shape of everything here, bumped with the client protocol (whose
/// messages ride inside). Replicas only talk to replicas of the same
/// version, and the Redis keys and channels carry it too, so a rolling
/// deploy that changes it runs two separate clusters for a minute instead of
/// one confused one.
pub const CLUSTER_VERSION: u16 = PROTOCOL_VERSION * 100 + 1;
/// A replica's view expires this long after its last heartbeat.
pub const VIEW_TTL: Duration = Duration::from_secs(10);
/// Views are rewritten (and their TTL refreshed) at least this often.
pub const HEARTBEAT: Duration = Duration::from_secs(1);
/// Every replica re-reads all the views this often (that's how a replica
/// that died, and whose key expired, is noticed).
pub const REFRESH: Duration = Duration::from_secs(2);
/// The channel every replica hears (views and goodbyes).
pub const ALL: &str = "*";

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

pub fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A signed-in player, as their home replica sees them.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UserEntry {
    pub friend: Friend,
    /// The room they're in (on any replica).
    pub room: Option<String>,
}

/// A room, as its owner sees it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RoomEntry {
    pub code: String,
    pub public: bool,
    pub started: bool,
    pub map: RoomMap,
    /// Its players' DIDs.
    pub players: Vec<Did>,
    /// The host's DID (hosted rooms with players).
    pub host: Option<Did>,
}

/// The running build, for `flightsim_build_info` and uptime.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct ReplicaInfo {
    pub id: String,
    pub git: String,
    pub version: String,
    /// Unix seconds.
    pub started: f64,
}

/// One replica, as it tells the others.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct View {
    pub id: String,
    /// Shutting down: no new rooms or guests here.
    pub draining: bool,
    pub users: Vec<UserEntry>,
    pub rooms: Vec<RoomEntry>,
    /// Its part of the metrics' gauges.
    pub gauges: Gauges,
    pub info: ReplicaInfo,
}

/// A player waiting for a quick-play match.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct QueueEntry {
    /// Their home.
    pub replica: String,
    pub conn: ConnId,
    pub did: Did,
    pub handle: String,
    pub map: RoomMap,
    /// When they started waiting (for the wait metrics).
    pub since_ms: u64,
}

impl QueueEntry {
    /// Its name in the queue: `replica/conn`.
    pub fn member(&self) -> String {
        member(&self.replica, self.conn)
    }
}

pub fn member(replica: &str, conn: ConnId) -> String {
    format!("{replica}/{conn}")
}

/// What a bridged message means to the player's home.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Tag {
    /// A room message.
    None,
    /// `Joined`: they're the owner's guest in room `code` now.
    Joined { code: String, public: bool },
    /// `Error`: they're in no room of the owner's any more.
    Ended,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Body {
    /// The sender's state (on [`ALL`]).
    View(View),
    /// The sender is going (on [`ALL`]).
    Bye,
    /// Home → owner: put our `conn` in room `code` (as a guest). `method` is
    /// the `joins_total` label it counts under.
    Join { conn: ConnId, did: Did, handle: String, code: String, method: u8 },
    /// Home → owner: a room message from our `conn`.
    Msg { conn: ConnId, msg: ClientMsg },
    /// Home → owner: our `conn` left the room (or went offline).
    Gone { conn: ConnId },
    /// Owner → home: an encoded `ServerMsg` for these of the home's conns.
    Deliver { conns: Vec<ConnId>, tag: Tag, bytes: Vec<u8> },
    /// To a room's owner: for the room's host.
    ToHost { code: String, msg: ServerMsg },
    /// To a player's home: for all their connections.
    ToDid { did: Did, msg: ServerMsg },
    /// The host's answer to a join request, to the requester's home.
    Answer { code: String, host: Did, requester: Did, accept: bool },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Envelope {
    pub v: u16,
    pub from: String,
    pub bodies: Vec<Body>,
}

/// The shared state. Calls from the game loop are synchronous: the Redis
/// backend answers them from a short round trip (a millisecond on Railway's
/// private network) with a timeout, and only rare, player-driven things
/// (pressing QUICK PLAY, sending an invite) wait on one; the hot path (room
/// traffic) is fire-and-forget [`Backend::publish`]. The lookup worker's
/// calls are async.
pub trait Backend: Send + Sync {
    /// This replica's id (unique per process).
    fn id(&self) -> &str;
    /// Whether other replicas can exist (false: an in-process backend for
    /// one server, where nothing is ever published).
    fn clustered(&self) -> bool;

    /// Sends an envelope to replica `to` (or [`ALL`]), in order with every
    /// other publish and view write.
    fn publish(&self, to: &str, bytes: Vec<u8>);
    /// Writes our view (refreshing its TTL).
    fn put_view(&self, bytes: Vec<u8>);
    /// Deletes our view (shutting down).
    fn drop_view(&self);
    /// Every live replica's view (None: couldn't read them, keep what's known).
    fn read_views(&self) -> BoxFuture<Option<Vec<Vec<u8>>>>;

    /// Into the quick-play queue (at the back; replaces an older entry of the same conn).
    fn queue_add(&self, entry: &QueueEntry);
    /// Out of the queue: true if it was there (and so is ours to place).
    fn queue_remove(&self, replica: &str, conn: ConnId) -> bool;
    /// Whether the entry is (still) in the queue.
    fn queue_has(&self, replica: &str, conn: ConnId) -> bool;
    /// Atomically takes up to `max` waiting players, if there are two or
    /// more and the first in line is ours (so only one replica ever places a
    /// group, and the room is made where its first player is). Entries of
    /// replicas that have gone are dropped on the way.
    fn queue_pop(&self, max: usize) -> Vec<QueueEntry>;

    /// A rate limit: one ask per (`from`, `to`) per `gap` (zero: no pair
    /// limit), and at most `per_minute` from `from`, across the cluster.
    /// Records the ask if allowed. (Fails open if the store is down.)
    fn allow(&self, scope: &str, from: &str, to: &str, gap: Duration, per_minute: usize, now: Instant) -> bool;
    /// A challenge run began (the newest start wins).
    fn start_run(&self, did: &str, seed: u32, ttl: Duration, now: Instant);
    /// Takes the player's run start: (seed, seconds since it).
    fn take_run(&self, did: &str, now: Instant) -> Option<(u32, f32)>;

    /// The relationship cache, for `others` against `actor`: Some(mutual)
    /// where known.
    fn cached_pairs(&self, actor: Did, others: Vec<Did>) -> BoxFuture<Vec<Option<bool>>>;
    /// Claims the lookup of each pair (true: ours to look up; false: another
    /// replica is on it).
    fn lock_pairs(&self, actor: Did, others: Vec<Did>) -> BoxFuture<Vec<bool>>;
    /// Caches looked-up pairs for `ttl` (and releases their locks).
    fn store_pairs(&self, actor: Did, pairs: Vec<(Did, bool)>, ttl: Duration) -> BoxFuture<()>;
    /// Waits for an AppView call slot (`per_sec` across the cluster).
    fn lookup_slot(&self, per_sec: u32) -> BoxFuture<()>;

    /// Adds counter deltas to the cluster's totals (true if they're in).
    fn push_metrics(&self, deltas: Vec<(String, u64)>) -> BoxFuture<bool>;
    /// The cluster's counter totals. Ok(None): there's no cluster, use the
    /// process's own counters.
    fn metric_totals(&self) -> BoxFuture<Result<Option<BTreeMap<String, u64>>, String>>;
}

/// Asks from one player to another: one per pair per `gap`, and at most
/// `per_minute` from anyone (a sliding minute).
#[derive(Default)]
pub struct Limiter {
    /// (from, to) → when.
    last: HashMap<(String, String), Instant>,
    by: HashMap<String, VecDeque<Instant>>,
}

impl Limiter {
    pub fn allow(&mut self, from: &str, to: &str, gap: Duration, per_minute: usize, now: Instant) -> bool {
        let pair = (from.to_string(), to.to_string());
        if !gap.is_zero() && self.last.get(&pair).is_some_and(|t| now.duration_since(*t) < gap) {
            return false;
        }
        let recent = self.by.entry(from.to_string()).or_default();
        while recent.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60)) {
            recent.pop_front();
        }
        if recent.len() >= per_minute {
            return false;
        }
        recent.push_back(now);
        if !gap.is_zero() {
            self.last.insert(pair, now);
        }
        // (forget what can't matter any more, now and then)
        if self.last.len() + self.by.len() > 10_000 {
            self.last.retain(|_, t| now.duration_since(*t) < Duration::from_secs(600));
            self.by.retain(|_, q| q.back().is_some_and(|t| now.duration_since(*t) < Duration::from_secs(60)));
        }
        true
    }
}

// ---- in-process ----

#[derive(Default)]
struct BusState {
    views: BTreeMap<String, Vec<u8>>,
    /// Envelopes waiting for each replica (tests drain them).
    inboxes: BTreeMap<String, Vec<Vec<u8>>>,
    /// The queue, in line order: seq → entry.
    queue: BTreeMap<u64, QueueEntry>,
    seq: u64,
    limits: HashMap<String, Limiter>,
    runs: HashMap<Did, (u32, Instant, Duration)>,
    dead: HashSet<String>,
}

/// Several in-process replicas sharing state, for tests: `Bus::replica`
/// makes each one's backend, and the test moves the envelopes
/// (`take_inbox`).
#[derive(Clone, Default)]
pub struct Bus(Arc<Mutex<BusState>>);

#[cfg(test)]
impl Bus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn replica(&self, id: &str) -> Arc<Memory> {
        self.0.lock().unwrap().inboxes.entry(id.to_string()).or_default();
        Arc::new(Memory { id: id.to_string(), bus: self.clone(), clustered: true, slot: Mutex::new(None) })
    }

    /// What's been published to `id` since last time.
    pub fn take_inbox(&self, id: &str) -> Vec<Vec<u8>> {
        std::mem::take(self.0.lock().unwrap().inboxes.entry(id.to_string()).or_default())
    }

    pub fn views(&self) -> Vec<Vec<u8>> {
        self.0.lock().unwrap().views.values().cloned().collect()
    }

    /// The replica dies: its view is gone and nothing reaches it, or comes from it.
    pub fn kill(&self, id: &str) {
        let mut s = self.0.lock().unwrap();
        s.views.remove(id);
        s.inboxes.remove(id);
        s.dead.insert(id.to_string());
    }

    pub fn queue_len(&self) -> usize {
        self.0.lock().unwrap().queue.len()
    }
}

/// The in-process backend: one server on its own ([`Memory::single`]), or
/// a replica on a test [`Bus`].
pub struct Memory {
    id: String,
    bus: Bus,
    clustered: bool,
    slot: Mutex<Option<Instant>>,
}

impl Memory {
    pub fn single() -> Arc<Self> {
        Arc::new(Self { id: "local".into(), bus: Bus::default(), clustered: false, slot: Mutex::new(None) })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, BusState> {
        self.bus.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl BusState {
    fn alive(&self, me: &str, replica: &str) -> bool {
        replica == me || (self.views.contains_key(replica) && !self.dead.contains(replica))
    }
}

impl Backend for Memory {
    fn id(&self) -> &str {
        &self.id
    }

    fn clustered(&self) -> bool {
        self.clustered
    }

    fn publish(&self, to: &str, bytes: Vec<u8>) {
        let mut s = self.state();
        if s.dead.contains(&self.id) {
            return;
        }
        let targets: Vec<String> = if to == ALL {
            s.inboxes.keys().filter(|k| **k != self.id).cloned().collect()
        } else {
            vec![to.to_string()]
        };
        for t in targets {
            if !s.dead.contains(&t)
                && let Some(inbox) = s.inboxes.get_mut(&t)
            {
                inbox.push(bytes.clone());
            }
        }
    }

    fn put_view(&self, bytes: Vec<u8>) {
        let mut s = self.state();
        if !s.dead.contains(&self.id) {
            s.views.insert(self.id.clone(), bytes);
        }
    }

    fn drop_view(&self) {
        self.state().views.remove(&self.id);
    }

    fn read_views(&self) -> BoxFuture<Option<Vec<Vec<u8>>>> {
        let views = self.state().views.values().cloned().collect();
        Box::pin(async move { Some(views) })
    }

    fn queue_add(&self, entry: &QueueEntry) {
        let mut s = self.state();
        s.queue.retain(|_, e| e.member() != entry.member());
        s.seq += 1;
        let seq = s.seq;
        s.queue.insert(seq, entry.clone());
    }

    fn queue_remove(&self, replica: &str, conn: ConnId) -> bool {
        let mut s = self.state();
        let before = s.queue.len();
        s.queue.retain(|_, e| !(e.replica == replica && e.conn == conn));
        s.queue.len() != before
    }

    fn queue_has(&self, replica: &str, conn: ConnId) -> bool {
        self.state().queue.values().any(|e| e.replica == replica && e.conn == conn)
    }

    fn queue_pop(&self, max: usize) -> Vec<QueueEntry> {
        let mut s = self.state();
        let me = self.id.clone();
        // (the same rules as the Redis script)
        let dead: Vec<u64> = s.queue.iter().filter(|(_, e)| !s.alive(&me, &e.replica)).map(|(k, _)| *k).collect();
        for k in dead {
            s.queue.remove(&k);
        }
        let Some(head) = s.queue.values().next() else { return vec![] };
        if head.replica != me || s.queue.len() < 2 {
            return vec![];
        }
        let keys: Vec<u64> = s.queue.keys().take(max).copied().collect();
        keys.iter().filter_map(|k| s.queue.remove(k)).collect()
    }

    fn allow(&self, scope: &str, from: &str, to: &str, gap: Duration, per_minute: usize, now: Instant) -> bool {
        self.state().limits.entry(scope.to_string()).or_default().allow(from, to, gap, per_minute, now)
    }

    fn start_run(&self, did: &str, seed: u32, ttl: Duration, now: Instant) {
        let mut s = self.state();
        s.runs.retain(|_, (_, at, ttl)| now.duration_since(*at) < *ttl);
        s.runs.insert(did.to_string(), (seed, now, ttl));
    }

    fn take_run(&self, did: &str, now: Instant) -> Option<(u32, f32)> {
        let (seed, at, ttl) = self.state().runs.remove(did)?;
        let secs = now.duration_since(at);
        (secs < ttl).then_some((seed, secs.as_secs_f32()))
    }

    // (one server: presence's own cache is the only one)
    fn cached_pairs(&self, _actor: Did, others: Vec<Did>) -> BoxFuture<Vec<Option<bool>>> {
        Box::pin(async move { vec![None; others.len()] })
    }

    fn lock_pairs(&self, _actor: Did, others: Vec<Did>) -> BoxFuture<Vec<bool>> {
        Box::pin(async move { vec![true; others.len()] })
    }

    fn store_pairs(&self, _actor: Did, _pairs: Vec<(Did, bool)>, _ttl: Duration) -> BoxFuture<()> {
        Box::pin(async {})
    }

    fn lookup_slot(&self, per_sec: u32) -> BoxFuture<()> {
        let gap = Duration::from_secs_f64(1.0 / per_sec.max(1) as f64);
        let now = Instant::now();
        let wait = {
            let mut slot = self.slot.lock().unwrap();
            let at = slot.map_or(now, |t| t.max(now));
            *slot = Some(at + gap);
            at.duration_since(now)
        };
        Box::pin(async move {
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        })
    }

    fn push_metrics(&self, _deltas: Vec<(String, u64)>) -> BoxFuture<bool> {
        Box::pin(async { true })
    }

    fn metric_totals(&self) -> BoxFuture<Result<Option<BTreeMap<String, u64>>, String>> {
        Box::pin(async { Ok(None) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAP: RoomMap = RoomMap { seed: 1, biome: 0, time: 0, weather: 0 };

    fn entry(replica: &str, conn: ConnId) -> QueueEntry {
        QueueEntry { replica: replica.into(), conn, did: format!("did:dev:{conn}"), handle: conn.to_string(), map: MAP, since_ms: 0 }
    }

    #[test]
    fn the_queue_is_popped_by_the_first_in_lines_replica_only() {
        let bus = Bus::new();
        let (a, b) = (bus.replica("a"), bus.replica("b"));
        a.put_view(vec![1]);
        b.put_view(vec![2]);
        a.queue_add(&entry("a", 1));
        assert!(a.queue_pop(8).is_empty(), "one isn't a match");
        b.queue_add(&entry("b", 7));
        assert!(b.queue_pop(8).is_empty(), "b's player isn't first in line");
        assert_eq!(a.queue_pop(8).iter().map(QueueEntry::member).collect::<Vec<_>>(), vec!["a/1", "b/7"]);
        assert_eq!(bus.queue_len(), 0);
        // a replica that's gone: its entries go
        b.queue_add(&entry("b", 8));
        a.queue_add(&entry("a", 2));
        bus.kill("b");
        assert!(a.queue_pop(8).is_empty());
        assert_eq!(bus.queue_len(), 1);
        // removing claims an entry once
        assert!(a.queue_has("a", 2));
        assert!(a.queue_remove("a", 2));
        assert!(!a.queue_remove("a", 2));
    }

    #[test]
    fn limits_and_runs_are_shared() {
        let bus = Bus::new();
        let (a, b) = (bus.replica("a"), bus.replica("b"));
        let now = Instant::now();
        let gap = Duration::from_secs(10);
        assert!(a.allow("invite", "x", "y", gap, 3, now));
        assert!(!b.allow("invite", "x", "y", gap, 3, now), "the other replica knows");
        assert!(b.allow("invite", "x", "z", gap, 3, now));
        assert!(a.allow("invite", "x", "w", gap, 3, now));
        assert!(!a.allow("invite", "x", "v", gap, 3, now), "three a minute");
        assert!(a.allow("invite", "x", "v", gap, 3, now + Duration::from_secs(60)));
        a.start_run("did:x", 5, Duration::from_secs(100), now);
        assert_eq!(b.take_run("did:x", now + Duration::from_secs(2)), Some((5, 2.0)));
        assert_eq!(a.take_run("did:x", now), None, "used up");
    }

    #[test]
    fn envelopes_round_trip() {
        let env = Envelope {
            v: CLUSTER_VERSION,
            from: "a".into(),
            bodies: vec![
                Body::Msg { conn: 3, msg: ClientMsg::Respawn },
                Body::Deliver { conns: vec![1, 2], tag: Tag::Joined { code: "ABCD".into(), public: true }, bytes: vec![9, 9] },
            ],
        };
        let bytes = crate::protocol::encode(&env);
        assert_eq!(crate::protocol::decode::<Envelope>(&bytes), Some(env));
    }
}
