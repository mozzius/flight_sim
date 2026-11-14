//! The cluster's shared state in Redis (`REDIS_URL`): see `cluster`.
//!
//! Keys, under `FLIGHT_SIM_REDIS_PREFIX` (default `fs:`) plus `v<N>:` for
//! the ones whose shape follows [`CLUSTER_VERSION`]:
//!
//! - `v<N>:view:<replica>`: a replica's [`cluster::View`] (postcard), with a
//!   [`cluster::VIEW_TTL`] expiry it refreshes every second; `v<N>:views`
//!   is the set of replica ids (pruned of expired ones on every read).
//! - channels `v<N>:all` (views, goodbyes) and `v<N>:r:<replica>` (each
//!   replica's own: bridged room traffic, invites, requests).
//! - `v<N>:queue`: the quick-play queue, a sorted set of `replica/conn` by
//!   when they joined the line, with each entry in the hash `v<N>:queue:info`.
//!   Matching is one Lua script ([`POP`]), so two replicas never take the
//!   same player.
//! - `rl:<scope>:…`: rate limits (a Lua script, [`ALLOW`]); `run:<did>`:
//!   challenge starts; `pair:<a>|<b>` and `pairlock:…`: the AppView
//!   relationship cache; `appview:<second>`: the cluster's AppView call
//!   budget; `metrics`: the counter totals (a hash; never versioned, so
//!   counters carry on across deploys).
//!
//! Game-loop calls block on one round trip, at most [`SYNC_TIMEOUT`]; the
//! publisher pipelines everything that's waiting into one round trip, in
//! order.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use redis::{AsyncCommands, Script};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::cluster::{self, Backend, BoxFuture, CLUSTER_VERSION, QueueEntry, unix_ms};
use crate::presence::Did;
use crate::protocol::{decode, encode};
use crate::rooms::ConnId;

/// The game loop waits at most this long for Redis.
pub const SYNC_TIMEOUT: Duration = Duration::from_millis(250);

/// The quick-play matcher. KEYS: queue, info. ARGV: our id, the view key
/// prefix, the most to take. Returns the entries taken (none unless there
/// are two or more and the first in line is ours); entries of replicas whose
/// view has expired are dropped on the way.
pub const POP: &str = r#"
local function rid(m) return string.match(m, '^(.*)/') end
local function alive(r) return r == ARGV[1] or redis.call('EXISTS', ARGV[2] .. r) == 1 end
local function drop(m) redis.call('ZREM', KEYS[1], m); redis.call('HDEL', KEYS[2], m) end
local members = redis.call('ZRANGE', KEYS[1], 0, -1)
local live = {}
for _, m in ipairs(members) do
  if alive(rid(m)) then table.insert(live, m) else drop(m) end
end
if #live < 2 or rid(live[1]) ~= ARGV[1] then return {} end
local out = {}
for i, m in ipairs(live) do
  if i > tonumber(ARGV[3]) then break end
  table.insert(out, redis.call('HGET', KEYS[2], m))
  drop(m)
end
return out
"#;

/// Out of the queue. KEYS: queue, info. ARGV: member. 1 if it was there.
const REMOVE: &str = r#"
local n = redis.call('ZREM', KEYS[1], ARGV[1])
redis.call('HDEL', KEYS[2], ARGV[1])
return n
"#;

/// A rate limit. KEYS: the pair's key, the sender's window. ARGV: the pair
/// gap (ms, 0: none), the cap a minute, now (ms), a unique member. 1 if allowed (and recorded).
const ALLOW: &str = r#"
local gap = tonumber(ARGV[1])
if gap > 0 and redis.call('EXISTS', KEYS[1]) == 1 then return 0 end
local now = tonumber(ARGV[3])
redis.call('ZREMRANGEBYSCORE', KEYS[2], '-inf', now - 60000)
if redis.call('ZCARD', KEYS[2]) >= tonumber(ARGV[2]) then return 0 end
redis.call('ZADD', KEYS[2], now, ARGV[4])
redis.call('PEXPIRE', KEYS[2], 61000)
if gap > 0 then redis.call('SET', KEYS[1], '1', 'PX', gap) end
return 1
"#;

enum Cmd {
    Publish(String, Vec<u8>),
    PutView(Vec<u8>),
    DropView,
}

pub struct Redis {
    id: String,
    /// `<prefix>`: unversioned keys.
    base: String,
    /// `<prefix>v<N>:`: keys and channels shaped by this version.
    vers: String,
    conn: ConnectionManager,
    client: redis::Client,
    rt: tokio::runtime::Handle,
    tx: UnboundedSender<Cmd>,
    pop: Script,
    remove: Script,
    allow: Script,
    seq: AtomicU64,
}

fn pair_key(a: &str, b: &str) -> String {
    if a < b { format!("{a}|{b}") } else { format!("{b}|{a}") }
}

impl Redis {
    /// Connects (retrying for a while: Railway's private network can take a
    /// moment) and starts the publisher.
    pub async fn connect(url: &str, prefix: &str, id: &str) -> Result<Arc<Self>, String> {
        let client = redis::Client::open(url).map_err(|e| e.to_string())?;
        let config = ConnectionManagerConfig::new()
            .set_connection_timeout(Some(Duration::from_secs(5)))
            .set_response_timeout(Some(Duration::from_secs(5)));
        let mut tries = 0;
        let conn = loop {
            match ConnectionManager::new_with_config(client.clone(), config.clone()).await {
                Ok(c) => break c,
                // (within Railway's 60 s health check)
                Err(e) if tries < 5 => {
                    tries += 1;
                    bevy::log::warn!("redis: {e}; retrying");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(e) => return Err(e.to_string()),
            }
        };
        let (tx, rx) = unbounded_channel();
        let me = Arc::new(Self {
            id: id.to_string(),
            base: prefix.to_string(),
            vers: format!("{prefix}v{CLUSTER_VERSION}:"),
            conn,
            client,
            rt: tokio::runtime::Handle::current(),
            tx,
            pop: Script::new(POP),
            remove: Script::new(REMOVE),
            allow: Script::new(ALLOW),
            seq: AtomicU64::new(0),
        });
        tokio::spawn(me.clone().publisher(rx));
        Ok(me)
    }

    fn channel(&self, to: &str) -> String {
        if to == cluster::ALL { format!("{}all", self.vers) } else { format!("{}r:{to}", self.vers) }
    }

    fn view_key(&self, id: &str) -> String {
        format!("{}view:{id}", self.vers)
    }

    fn queue_keys(&self) -> (String, String) {
        (format!("{}queue", self.vers), format!("{}queue:info", self.vers))
    }

    /// One round trip from the game loop (None: an error or too slow).
    fn sync<T, F>(&self, what: &str, f: F) -> Option<T>
    where
        F: Future<Output = redis::RedisResult<T>>,
    {
        match self.rt.block_on(async { tokio::time::timeout(SYNC_TIMEOUT, f).await }) {
            Ok(Ok(v)) => Some(v),
            Ok(Err(e)) => {
                bevy::log::warn!("redis {what}: {e}");
                None
            }
            Err(_) => {
                bevy::log::warn!("redis {what}: timed out");
                None
            }
        }
    }

    /// Sends everything waiting, pipelined, in order.
    async fn publisher(self: Arc<Self>, mut rx: UnboundedReceiver<Cmd>) {
        let mut conn = self.conn.clone();
        let (key, views) = (self.view_key(&self.id), format!("{}views", self.vers));
        let ttl = cluster::VIEW_TTL.as_millis() as u64;
        let mut failing = false;
        while let Some(first) = rx.recv().await {
            let mut batch = vec![first];
            while batch.len() < 1024 {
                match rx.try_recv() {
                    Ok(c) => batch.push(c),
                    Err(_) => break,
                }
            }
            let mut pipe = redis::pipe();
            for c in batch {
                match c {
                    Cmd::Publish(ch, b) => {
                        pipe.cmd("PUBLISH").arg(ch).arg(b).ignore();
                    }
                    Cmd::PutView(b) => {
                        pipe.cmd("SET").arg(&key).arg(b).arg("PX").arg(ttl).ignore();
                        pipe.cmd("SADD").arg(&views).arg(&self.id).ignore();
                    }
                    Cmd::DropView => {
                        pipe.cmd("DEL").arg(&key).ignore();
                        pipe.cmd("SREM").arg(&views).arg(&self.id).ignore();
                    }
                }
            }
            match pipe.query_async::<()>(&mut conn).await {
                Ok(()) if failing => {
                    failing = false;
                    bevy::log::info!("redis publish: back");
                }
                Ok(()) => {}
                Err(e) if !failing => {
                    failing = true;
                    bevy::log::warn!("redis publish: {e}");
                }
                Err(_) => {}
            }
        }
    }

    /// Hears our channel and the cluster's, forever (reconnecting): every
    /// message to `on_message`, and `on_connect` each time we're (back) on.
    pub async fn subscribe(self: Arc<Self>, on_message: impl Fn(Vec<u8>) + Send + 'static, on_connect: impl Fn() + Send + 'static) {
        let channels = [self.channel(cluster::ALL), self.channel(&self.id)];
        loop {
            match self.client.get_async_pubsub().await {
                Ok(mut pubsub) => {
                    let mut ok = true;
                    for ch in &channels {
                        if let Err(e) = pubsub.subscribe(ch).await {
                            bevy::log::warn!("redis subscribe: {e}");
                            ok = false;
                        }
                    }
                    if ok {
                        on_connect();
                        let mut stream = pubsub.into_on_message();
                        while let Some(msg) = stream.next().await {
                            on_message(msg.get_payload_bytes().to_vec());
                        }
                        bevy::log::warn!("redis subscription lost; reconnecting");
                    }
                }
                Err(e) => bevy::log::warn!("redis subscribe: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Deletes every key under our prefix (tests).
    #[cfg(test)]
    pub async fn clear(&self) {
        let mut conn = self.conn.clone();
        let keys: Vec<String> = redis::cmd("KEYS").arg(format!("{}*", self.base)).query_async(&mut conn).await.unwrap_or_default();
        if !keys.is_empty() {
            let _: () = conn.del(keys).await.unwrap();
        }
    }
}

use std::future::Future;

impl Backend for Redis {
    fn id(&self) -> &str {
        &self.id
    }

    fn clustered(&self) -> bool {
        true
    }

    fn publish(&self, to: &str, bytes: Vec<u8>) {
        let _ = self.tx.send(Cmd::Publish(self.channel(to), bytes));
    }

    fn put_view(&self, bytes: Vec<u8>) {
        let _ = self.tx.send(Cmd::PutView(bytes));
    }

    fn drop_view(&self) {
        let _ = self.tx.send(Cmd::DropView);
    }

    fn read_views(&self) -> BoxFuture<Option<Vec<Vec<u8>>>> {
        let mut conn = self.conn.clone();
        let set = format!("{}views", self.vers);
        let vers = self.vers.clone();
        Box::pin(async move {
            let ids: Vec<String> = match conn.smembers(&set).await {
                Ok(ids) => ids,
                Err(e) => {
                    bevy::log::warn!("redis views: {e}");
                    return None;
                }
            };
            if ids.is_empty() {
                return Some(vec![]);
            }
            let keys: Vec<String> = ids.iter().map(|id| format!("{vers}view:{id}")).collect();
            let blobs: Vec<Option<Vec<u8>>> = match redis::cmd("MGET").arg(&keys).query_async(&mut conn).await {
                Ok(b) => b,
                Err(e) => {
                    bevy::log::warn!("redis views: {e}");
                    return None;
                }
            };
            let gone: Vec<&String> = ids.iter().zip(&blobs).filter(|(_, b)| b.is_none()).map(|(id, _)| id).collect();
            if !gone.is_empty() {
                let _: redis::RedisResult<()> = conn.srem(&set, gone).await;
            }
            Some(blobs.into_iter().flatten().collect())
        })
    }

    fn queue_add(&self, entry: &QueueEntry) {
        let (q, info) = self.queue_keys();
        let mut conn = self.conn.clone();
        let member = entry.member();
        let blob = encode(entry);
        self.sync("queue add", async move {
            redis::pipe().atomic().zadd(&q, &member, unix_ms()).hset(&info, &member, blob).query_async::<()>(&mut conn).await
        });
    }

    fn queue_remove(&self, replica: &str, conn: ConnId) -> bool {
        let (q, info) = self.queue_keys();
        let mut c = self.conn.clone();
        let member = cluster::member(replica, conn);
        self.sync("queue remove", async move { self.remove.key(&q).key(&info).arg(&member).invoke_async::<i64>(&mut c).await })
            .is_some_and(|n| n > 0)
    }

    fn queue_has(&self, replica: &str, conn: ConnId) -> bool {
        let (q, _) = self.queue_keys();
        let mut c = self.conn.clone();
        let member = cluster::member(replica, conn);
        self.sync("queue has", async move { c.zscore::<_, _, Option<f64>>(&q, &member).await }).flatten().is_some()
    }

    fn queue_pop(&self, max: usize) -> Vec<QueueEntry> {
        let (q, info) = self.queue_keys();
        let mut c = self.conn.clone();
        let views = format!("{}view:", self.vers);
        let blobs: Vec<Vec<u8>> = self
            .sync("queue pop", async move {
                self.pop.key(&q).key(&info).arg(&self.id).arg(&views).arg(max).invoke_async(&mut c).await
            })
            .unwrap_or_default();
        blobs.iter().filter_map(|b| decode(b)).collect()
    }

    fn allow(&self, scope: &str, from: &str, to: &str, gap: Duration, per_minute: usize, _now: Instant) -> bool {
        let pair = format!("{}rl:{scope}:{from}|{to}", self.base);
        let window = format!("{}rl:{scope}:{from}", self.base);
        let now = unix_ms();
        let unique = format!("{now}-{}-{}", self.id, self.seq.fetch_add(1, Ordering::Relaxed));
        let cap = per_minute.min(i64::MAX as usize) as i64;
        let mut c = self.conn.clone();
        // (fails open: a Redis hiccup doesn't stop invites)
        self.sync("allow", async move {
            self.allow.key(&pair).key(&window).arg(gap.as_millis() as u64).arg(cap).arg(now).arg(&unique).invoke_async::<i64>(&mut c).await
        })
        .is_none_or(|n| n > 0)
    }

    fn start_run(&self, did: &str, seed: u32, ttl: Duration, _now: Instant) {
        let key = format!("{}run:{did}", self.base);
        let mut c = self.conn.clone();
        let value = format!("{seed}:{}", unix_ms());
        self.sync("start run", async move { c.pset_ex::<_, _, ()>(&key, value, ttl.as_millis() as u64).await });
    }

    fn take_run(&self, did: &str, _now: Instant) -> Option<(u32, f32)> {
        let key = format!("{}run:{did}", self.base);
        let mut c = self.conn.clone();
        let value: String = self.sync("take run", async move { redis::cmd("GETDEL").arg(&key).query_async::<Option<String>>(&mut c).await }).flatten()?;
        let (seed, at) = value.split_once(':')?;
        let secs = unix_ms().saturating_sub(at.parse().ok()?) as f32 / 1000.0;
        Some((seed.parse().ok()?, secs))
    }

    fn cached_pairs(&self, actor: Did, others: Vec<Did>) -> BoxFuture<Vec<Option<bool>>> {
        let mut c = self.conn.clone();
        let keys: Vec<String> = others.iter().map(|o| format!("{}pair:{}", self.base, pair_key(&actor, o))).collect();
        Box::pin(async move {
            if keys.is_empty() {
                return vec![];
            }
            let found: Vec<Option<String>> = redis::cmd("MGET").arg(&keys).query_async(&mut c).await.unwrap_or_else(|_| vec![None; keys.len()]);
            found.into_iter().map(|v| v.map(|v| v == "1")).collect()
        })
    }

    fn lock_pairs(&self, actor: Did, others: Vec<Did>) -> BoxFuture<Vec<bool>> {
        let mut c = self.conn.clone();
        let keys: Vec<String> = others.iter().map(|o| format!("{}pairlock:{}", self.base, pair_key(&actor, o))).collect();
        Box::pin(async move {
            if keys.is_empty() {
                return vec![];
            }
            let mut pipe = redis::pipe();
            for k in &keys {
                pipe.cmd("SET").arg(k).arg(1).arg("NX").arg("EX").arg(15);
            }
            // (Redis down: look them up ourselves)
            let got: Vec<Option<String>> = pipe.query_async(&mut c).await.unwrap_or_else(|_| vec![Some("OK".into()); keys.len()]);
            got.into_iter().map(|g| g.is_some()).collect()
        })
    }

    fn store_pairs(&self, actor: Did, pairs: Vec<(Did, bool)>, ttl: Duration) -> BoxFuture<()> {
        let mut c = self.conn.clone();
        let base = self.base.clone();
        Box::pin(async move {
            if pairs.is_empty() {
                return;
            }
            let mut pipe = redis::pipe();
            for (other, mutual) in &pairs {
                let k = pair_key(&actor, other);
                pipe.cmd("SET").arg(format!("{base}pair:{k}")).arg(if *mutual { "1" } else { "0" }).arg("EX").arg(ttl.as_secs()).ignore();
                pipe.cmd("DEL").arg(format!("{base}pairlock:{k}")).ignore();
            }
            let _: redis::RedisResult<()> = pipe.query_async(&mut c).await;
        })
    }

    fn lookup_slot(&self, per_sec: u32) -> BoxFuture<()> {
        let mut c = self.conn.clone();
        let base = self.base.clone();
        Box::pin(async move {
            loop {
                let ms = unix_ms();
                let key = format!("{base}appview:{}", ms / 1000);
                let n: redis::RedisResult<(i64, ())> = redis::pipe().atomic().incr(&key, 1).expire(&key, 5).query_async(&mut c).await;
                match n {
                    Ok((n, ())) if n <= per_sec as i64 => return,
                    Ok(_) => tokio::time::sleep(Duration::from_millis(1000 - ms % 1000 + 5)).await,
                    Err(_) => {
                        tokio::time::sleep(Duration::from_secs_f64(1.0 / per_sec.max(1) as f64)).await;
                        return;
                    }
                }
            }
        })
    }

    fn push_metrics(&self, deltas: Vec<(String, u64)>) -> BoxFuture<bool> {
        let mut c = self.conn.clone();
        let key = format!("{}metrics", self.base);
        Box::pin(async move {
            if deltas.is_empty() {
                return true;
            }
            let mut pipe = redis::pipe();
            pipe.atomic();
            for (field, d) in &deltas {
                pipe.hincr(&key, field, *d as i64).ignore();
            }
            pipe.query_async::<()>(&mut c).await.is_ok()
        })
    }

    fn metric_totals(&self) -> BoxFuture<Result<Option<BTreeMap<String, u64>>, String>> {
        let mut c = self.conn.clone();
        let key = format!("{}metrics", self.base);
        Box::pin(async move {
            let all: BTreeMap<String, i64> = c.hgetall(&key).await.map_err(|e| e.to_string())?;
            Ok(Some(all.into_iter().map(|(k, v)| (k, v.max(0) as u64)).collect()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::RoomMap;

    /// Against a real Redis, under a throwaway prefix (deleted after):
    /// `FLIGHT_SIM_TEST_REDIS_URL=redis://… cargo test --no-default-features --features server --bin flight_sim_server redis`
    fn redis_url() -> Option<String> {
        std::env::var("FLIGHT_SIM_TEST_REDIS_URL").ok().filter(|u| !u.is_empty())
    }

    const MAP: RoomMap = RoomMap { seed: 1, biome: 0, time: 0, weather: 0 };

    fn entry(replica: &str, conn: ConnId) -> QueueEntry {
        QueueEntry { replica: replica.into(), conn, did: format!("did:dev:{conn}"), handle: conn.to_string(), map: MAP, since_ms: 0 }
    }

    #[test]
    fn redis_backend() {
        let Some(url) = redis_url() else {
            eprintln!("(skipped: no FLIGHT_SIM_TEST_REDIS_URL)");
            return;
        };
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let prefix = format!("test_{:08x}:", rand::random::<u32>());
        let (a, b) = rt.block_on(async {
            (Redis::connect(&url, &prefix, "a").await.unwrap(), Redis::connect(&url, &prefix, "b").await.unwrap())
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&rt, &a, &b)));
        rt.block_on(a.clear());
        result.unwrap();
    }

    fn check(rt: &tokio::runtime::Runtime, a: &Arc<Redis>, b: &Arc<Redis>) {
        let now = Instant::now();
        // views (written through the publisher: give it a moment)
        a.put_view(vec![1, 2, 3]);
        b.put_view(vec![4]);
        std::thread::sleep(Duration::from_millis(300));
        let mut views = rt.block_on(a.read_views()).unwrap();
        views.sort();
        assert_eq!(views, vec![vec![1, 2, 3], vec![4]]);

        // messages: each replica hears its own channel and the cluster's
        let (tx, mut rx) = unbounded_channel::<Vec<u8>>();
        let (ctx, mut crx) = unbounded_channel::<()>();
        rt.spawn(b.clone().subscribe(move |m| {
            let _ = tx.send(m);
        }, move || {
            let _ = ctx.send(());
        }));
        rt.block_on(async { tokio::time::timeout(Duration::from_secs(5), crx.recv()).await.unwrap() });
        a.publish("b", vec![7]);
        a.publish("c", vec![8]);
        a.publish(cluster::ALL, vec![9]);
        let got: Vec<Vec<u8>> = rt.block_on(async {
            let mut got = vec![];
            while let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(1000), rx.recv()).await {
                got.push(m);
            }
            got
        });
        assert_eq!(got, vec![vec![7], vec![9]], "in order, and not c's");

        // the queue: only the first in line's replica pops, two at least, each entry once
        a.queue_add(&entry("a", 1));
        assert!(a.queue_pop(8).is_empty());
        std::thread::sleep(Duration::from_millis(5));
        b.queue_add(&entry("b", 2));
        assert!(b.queue_pop(8).is_empty(), "not b's to place");
        assert!(a.queue_has("a", 1) && b.queue_has("b", 2));
        let got = a.queue_pop(8);
        assert_eq!(got.iter().map(QueueEntry::member).collect::<Vec<_>>(), vec!["a/1", "b/2"]);
        assert!(!b.queue_remove("b", 2), "taken already");
        // a dead replica's entries go
        std::thread::sleep(Duration::from_millis(5));
        a.queue_add(&entry("zz", 9));
        std::thread::sleep(Duration::from_millis(5));
        a.queue_add(&entry("a", 3));
        assert!(a.queue_pop(8).is_empty());
        assert!(!a.queue_has("zz", 9));
        assert!(a.queue_remove("a", 3));

        // rate limits across replicas
        let gap = Duration::from_secs(10);
        assert!(a.allow("invite", "x", "y", gap, 2, now));
        assert!(!b.allow("invite", "x", "y", gap, 2, now), "one per pair per gap");
        assert!(b.allow("invite", "x", "z", gap, 2, now));
        assert!(!a.allow("invite", "x", "w", gap, 2, now), "two a minute");
        assert!(a.allow("submit", "x", "", Duration::from_millis(200), usize::MAX, now));
        assert!(!a.allow("submit", "x", "", Duration::from_millis(200), usize::MAX, now));
        std::thread::sleep(Duration::from_millis(250));
        assert!(b.allow("submit", "x", "", Duration::from_millis(200), usize::MAX, now));

        // challenge starts
        a.start_run("did:x", 42, Duration::from_secs(60), now);
        std::thread::sleep(Duration::from_millis(100));
        let (seed, secs) = b.take_run("did:x", now).unwrap();
        assert_eq!(seed, 42);
        assert!((0.05..5.0).contains(&secs), "{secs}");
        assert_eq!(a.take_run("did:x", now), None);

        // the pair cache
        rt.block_on(async {
            let others = vec!["did:o1".to_string(), "did:o2".to_string()];
            assert_eq!(a.cached_pairs("did:me".into(), others.clone()).await, vec![None, None]);
            assert_eq!(a.lock_pairs("did:me".into(), others.clone()).await, vec![true, true]);
            assert_eq!(b.lock_pairs("did:o1".into(), vec!["did:me".into()]).await, vec![false], "a has it");
            a.store_pairs("did:me".into(), vec![("did:o1".into(), true), ("did:o2".into(), false)], Duration::from_secs(60)).await;
            assert_eq!(b.cached_pairs("did:o2".into(), vec!["did:me".into()]).await, vec![Some(false)]);
            assert_eq!(b.cached_pairs("did:me".into(), others).await, vec![Some(true), Some(false)]);
            // the budget: 3 a second
            let t = std::time::Instant::now();
            for _ in 0..7 {
                a.lookup_slot(3).await;
            }
            assert!(t.elapsed() >= Duration::from_millis(900), "{:?}", t.elapsed());
            // metrics add up across replicas
            assert!(a.push_metrics(vec![("kills_total".into(), 2), ("joins_total{method=\"code\"}".into(), 0)]).await);
            assert!(b.push_metrics(vec![("kills_total".into(), 3)]).await);
            let totals = a.metric_totals().await.unwrap().unwrap();
            assert_eq!(totals.get("kills_total"), Some(&5));
            assert_eq!(totals.get("joins_total{method=\"code\"}"), Some(&0));
        });

        // a view drops out when deleted
        a.drop_view();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(rt.block_on(b.read_views()).unwrap(), vec![vec![4]]);
    }
}
