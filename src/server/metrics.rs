//! Prometheus metrics: a tiny hand-rolled text exposition (format 0.0.4) at
//! `GET /metrics`, behind a bearer token (`FLIGHT_SIM_METRICS_TOKEN`; unset,
//! the endpoint is a 404).
//!
//! Counters and histograms are process-wide statics ([`M`]) bumped wherever
//! things happen: hot paths (frames, bytes, the tick) are plain atomics, rare
//! labelled events take a mutex. Gauges (who's online, the rooms, the queue)
//! live in the game loop, which copies a [`Gauges`] snapshot into [`GAUGES`]
//! once a second; a scrape just formats what's there.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::protocol::{MAX_PLAYERS, PROTOCOL_VERSION};

/// A monotonic count.
pub struct Counter(AtomicU64);

impl Counter {
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }
    pub fn inc(&self) {
        self.add(1);
    }
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Counters by label values. Callers only pass values from small fixed sets
/// (code constants), so the series can't grow without bound.
pub struct Family {
    labels: &'static [&'static str],
    values: Mutex<BTreeMap<Vec<String>, u64>>,
}

impl Family {
    pub const fn new(labels: &'static [&'static str]) -> Self {
        Self { labels, values: Mutex::new(BTreeMap::new()) }
    }
    pub fn inc(&self, values: &[&str]) {
        debug_assert_eq!(values.len(), self.labels.len());
        let key: Vec<String> = values.iter().map(|v| v.to_string()).collect();
        *self.values.lock().unwrap().entry(key).or_default() += 1;
    }
    fn snapshot(&self) -> Vec<(String, u64)> {
        let values = self.values.lock().unwrap();
        values.iter().map(|(v, n)| (label_set(self.labels, v), *n)).collect()
    }
}

/// Up to this many buckets per histogram.
const MAX_BUCKETS: usize = 16;

/// A histogram of durations (in seconds).
pub struct Histogram {
    bounds: &'static [f64],
    counts: [AtomicU64; MAX_BUCKETS],
    /// (beyond the last bound)
    over: AtomicU64,
    sum_nanos: AtomicU64,
}

impl Histogram {
    pub const fn new(bounds: &'static [f64]) -> Self {
        assert!(bounds.len() <= MAX_BUCKETS);
        Self {
            bounds,
            counts: [const { AtomicU64::new(0) }; MAX_BUCKETS],
            over: AtomicU64::new(0),
            sum_nanos: AtomicU64::new(0),
        }
    }
    pub fn observe(&self, d: Duration) {
        let secs = d.as_secs_f64();
        match self.bounds.iter().position(|b| secs <= *b) {
            Some(i) => self.counts[i].fetch_add(1, Ordering::Relaxed),
            None => self.over.fetch_add(1, Ordering::Relaxed),
        };
        self.sum_nanos.fetch_add(d.as_nanos().min(u64::MAX as u128) as u64, Ordering::Relaxed);
    }
}

const TICK_BUCKETS: &[f64] = &[0.00001, 0.000025, 0.00005, 0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1];
const HTTP_BUCKETS: &[f64] = &[0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];
const WAIT_BUCKETS: &[f64] = &[1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0, 600.0];

/// Every counter and histogram.
pub struct Metrics {
    pub auth: Family,
    pub ws_messages_in: Counter,
    pub ws_messages_out: Counter,
    pub ws_bytes_in: Counter,
    pub ws_bytes_out: Counter,
    pub ws_dropped: Counter,
    pub rooms_created: Family,
    pub joins: Family,
    pub quickplay: Family,
    pub queue_matches: Counter,
    pub queue_exits: Family,
    pub queue_wait: Histogram,
    pub invites: Family,
    pub join_requests: Family,
    pub kills: Counter,
    pub deaths: Family,
    pub challenge_starts: Counter,
    pub challenge_runs: Family,
    pub appview_calls: Family,
    pub appview_errors: Family,
    pub appview_latency: Histogram,
    pub tick: Histogram,
    /// Open WebSockets, signed in or not (it's the sockets that keep it, so not a snapshot).
    pub ws_open: AtomicU64,
}

pub static M: Metrics = Metrics {
    auth: Family::new(&["result"]),
    ws_messages_in: Counter::new(),
    ws_messages_out: Counter::new(),
    ws_bytes_in: Counter::new(),
    ws_bytes_out: Counter::new(),
    ws_dropped: Counter::new(),
    rooms_created: Family::new(&["kind"]),
    joins: Family::new(&["method"]),
    quickplay: Family::new(&["result"]),
    queue_matches: Counter::new(),
    queue_exits: Family::new(&["outcome"]),
    queue_wait: Histogram::new(WAIT_BUCKETS),
    invites: Family::new(&["result"]),
    join_requests: Family::new(&["outcome"]),
    kills: Counter::new(),
    deaths: Family::new(&["cause"]),
    challenge_starts: Counter::new(),
    challenge_runs: Family::new(&["result", "reason"]),
    appview_calls: Family::new(&["method"]),
    appview_errors: Family::new(&["method"]),
    appview_latency: Histogram::new(HTTP_BUCKETS),
    tick: Histogram::new(TICK_BUCKETS),
    ws_open: AtomicU64::new(0),
};

/// Counts an open socket for as long as it's alive.
pub struct OpenSocket;

impl OpenSocket {
    pub fn new() -> Self {
        M.ws_open.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for OpenSocket {
    fn drop(&mut self) {
        M.ws_open.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The game loop's state, copied out once a second.
#[derive(Clone, Debug, Default)]
pub struct Gauges {
    /// Distinct signed-in players.
    pub players_online: u64,
    /// Signed-in connections (a player can have more than one).
    pub signed_in: u64,
    /// Rooms by [kind][started]: kind 0 public, 1 hosted.
    pub rooms: [[u64; 2]; 2],
    /// Players in rooms, by kind.
    pub in_rooms: [u64; 2],
    /// Rooms by player count (0..=MAX_PLAYERS).
    pub sizes: [u64; MAX_PLAYERS + 1],
    pub queue: u64,
    pub pending_requests: u64,
    pub leaderboard: u64,
}

pub static GAUGES: LazyLock<Mutex<Gauges>> = LazyLock::new(Default::default);
static STARTED: LazyLock<(Instant, f64)> = LazyLock::new(|| {
    let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    (Instant::now(), unix)
});

/// Marks the process start (call early: uptime counts from the first call).
pub fn start() {
    LazyLock::force(&STARTED);
}

/// The build: the commit Railway deployed (`RAILWAY_GIT_COMMIT_SHA`, or
/// `FLIGHT_SIM_GIT_HASH` at compile time), the crate version and the protocol.
fn build_labels() -> String {
    let git = std::env::var("RAILWAY_GIT_COMMIT_SHA")
        .ok()
        .or(option_env!("FLIGHT_SIM_GIT_HASH").map(str::to_string))
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(12).collect())
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "git=\"{}\",version=\"{}\",protocol=\"{PROTOCOL_VERSION}\"",
        escape(&git),
        env!("CARGO_PKG_VERSION")
    )
}

/// `a="x",b="y"`
fn label_set(names: &[&str], values: &[String]) -> String {
    names.iter().zip(values).map(|(n, v)| format!("{n}=\"{}\"", escape(v))).collect::<Vec<_>>().join(",")
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// A label value from free text ("score doesn't add up" → `score_doesnt_add_up`).
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if c == ' ' || c == '-' || c == '_' {
            if !out.ends_with('_') {
                out.push('_');
            }
        }
    }
    out
}

struct Writer(String);

impl Writer {
    fn head(&mut self, name: &str, kind: &str, help: &str) {
        let _ = writeln!(self.0, "# HELP flightsim_{name} {help}");
        let _ = writeln!(self.0, "# TYPE flightsim_{name} {kind}");
    }
    fn sample(&mut self, name: &str, labels: &str, v: impl std::fmt::Display) {
        if labels.is_empty() {
            let _ = writeln!(self.0, "flightsim_{name} {v}");
        } else {
            let _ = writeln!(self.0, "flightsim_{name}{{{labels}}} {v}");
        }
    }
    fn gauge(&mut self, name: &str, help: &str, v: impl std::fmt::Display) {
        self.head(name, "gauge", help);
        self.sample(name, "", v);
    }
    fn counter(&mut self, name: &str, help: &str, c: &Counter) {
        self.head(name, "counter", help);
        self.sample(name, "", c.get());
    }
    fn family(&mut self, name: &str, help: &str, f: &Family) {
        self.head(name, "counter", help);
        for (labels, n) in f.snapshot() {
            self.sample(name, &labels, n);
        }
    }
    fn histogram(&mut self, name: &str, help: &str, h: &Histogram) {
        self.head(name, "histogram", help);
        let mut total = 0;
        for (i, b) in h.bounds.iter().enumerate() {
            total += h.counts[i].load(Ordering::Relaxed);
            self.sample(&format!("{name}_bucket"), &format!("le=\"{b}\""), total);
        }
        total += h.over.load(Ordering::Relaxed);
        self.sample(&format!("{name}_bucket"), "le=\"+Inf\"", total);
        self.sample(&format!("{name}_sum"), "", h.sum_nanos.load(Ordering::Relaxed) as f64 / 1e9);
        self.sample(&format!("{name}_count"), "", total);
    }
}

/// The whole exposition.
pub fn render(g: &Gauges, m: &Metrics) -> String {
    let mut w = Writer(String::with_capacity(8 * 1024));
    let (t0, unix0) = *STARTED;

    w.head("build_info", "gauge", "The running build (labels), always 1.");
    w.sample("build_info", &build_labels(), 1);
    w.gauge("uptime_seconds", "Seconds since the server started.", format!("{:.0}", t0.elapsed().as_secs_f64()));
    w.gauge("start_time_seconds", "When the server started (unix seconds).", format!("{unix0:.0}"));

    w.gauge("players_online", "Distinct signed-in players.", g.players_online);
    w.gauge("connections_signed_in", "Signed-in WebSocket connections.", g.signed_in);
    w.gauge("websocket_connections", "Open WebSocket connections, signed in or not.", m.ws_open.load(Ordering::Relaxed));

    const KINDS: [&str; 2] = ["public", "hosted"];
    w.head("players_in_rooms", "gauge", "Players in rooms, by room kind.");
    for (k, n) in KINDS.iter().zip(g.in_rooms) {
        w.sample("players_in_rooms", &format!("kind=\"{k}\""), n);
    }
    w.head("rooms", "gauge", "Rooms by kind and state.");
    for (k, by_state) in KINDS.iter().zip(g.rooms) {
        for (s, n) in ["lobby", "started"].iter().zip(by_state) {
            w.sample("rooms", &format!("kind=\"{k}\",state=\"{s}\""), n);
        }
    }
    w.head("rooms_by_size", "gauge", "Rooms by how many players are in them.");
    for (players, n) in g.sizes.iter().enumerate() {
        w.sample("rooms_by_size", &format!("players=\"{players}\""), n);
    }
    let biggest = g.sizes.iter().rposition(|n| *n > 0).unwrap_or(0);
    w.gauge("room_size_max", "Players in the fullest room.", biggest);
    w.gauge("queue_length", "Players waiting for a quick-play match.", g.queue);
    w.gauge("join_requests_pending", "Join requests waiting for the host.", g.pending_requests);
    w.gauge("leaderboard_players", "Players on the challenge leaderboard.", g.leaderboard);

    w.family("auth_attempts_total", "WebSocket sign-ins by result.", &m.auth);
    w.counter("ws_messages_in_total", "WebSocket frames received.", &m.ws_messages_in);
    w.counter("ws_messages_out_total", "WebSocket frames sent.", &m.ws_messages_out);
    w.counter("ws_bytes_in_total", "WebSocket payload bytes received.", &m.ws_bytes_in);
    w.counter("ws_bytes_out_total", "WebSocket payload bytes sent.", &m.ws_bytes_out);
    w.counter("ws_messages_dropped_total", "Frames dropped by the per-connection rate limit.", &m.ws_dropped);

    w.family("rooms_created_total", "Rooms created, by kind.", &m.rooms_created);
    w.family("joins_total", "Players into a room, by how they got there.", &m.joins);
    w.family("quickplay_total", "QUICK PLAY presses: straight into a room, or queued.", &m.quickplay);
    w.counter("queue_matches_total", "New public rooms made from the queue.", &m.queue_matches);
    w.family("queue_exits_total", "Players out of the queue, by outcome.", &m.queue_exits);
    w.histogram("queue_wait_seconds", "Time in the queue before a match.", &m.queue_wait);
    w.family("invites_total", "Invites, by result.", &m.invites);
    w.family("join_requests_total", "Join requests, by outcome.", &m.join_requests);
    w.counter("kills_total", "Kills credited to a player.", &m.kills);
    w.family("deaths_total", "Deaths, by cause (shot down, or a crash nobody gets credit for).", &m.deaths);
    w.counter("challenge_starts_total", "Challenge rounds started.", &m.challenge_starts);
    w.family("challenge_runs_total", "Challenge runs submitted, by result and reason.", &m.challenge_runs);

    w.family("appview_calls_total", "AppView calls, by method.", &m.appview_calls);
    w.family("appview_errors_total", "Failed AppView calls, by method.", &m.appview_errors);
    w.histogram("appview_latency_seconds", "AppView call latency.", &m.appview_latency);
    w.histogram("tick_seconds", "Time the game loop spends per tick.", &m.tick);
    w.0
}

/// Whether an `Authorization` header carries the token (in constant time).
pub fn authorized(header: Option<&str>, token: &str) -> bool {
    let Some(given) = header.and_then(|h| h.strip_prefix("Bearer ")) else { return false };
    let (a, b) = (given.trim().as_bytes(), token.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposition_is_well_formed() {
        let m = Metrics {
            auth: Family::new(&["result"]),
            ws_messages_in: Counter::new(),
            ws_messages_out: Counter::new(),
            ws_bytes_in: Counter::new(),
            ws_bytes_out: Counter::new(),
            ws_dropped: Counter::new(),
            rooms_created: Family::new(&["kind"]),
            joins: Family::new(&["method"]),
            quickplay: Family::new(&["result"]),
            queue_matches: Counter::new(),
            queue_exits: Family::new(&["outcome"]),
            queue_wait: Histogram::new(WAIT_BUCKETS),
            invites: Family::new(&["result"]),
            join_requests: Family::new(&["outcome"]),
            kills: Counter::new(),
            deaths: Family::new(&["cause"]),
            challenge_starts: Counter::new(),
            challenge_runs: Family::new(&["result", "reason"]),
            appview_calls: Family::new(&["method"]),
            appview_errors: Family::new(&["method"]),
            appview_latency: Histogram::new(HTTP_BUCKETS),
            tick: Histogram::new(TICK_BUCKETS),
            ws_open: AtomicU64::new(3),
        };
        m.auth.inc(&["ok"]);
        m.auth.inc(&["ok"]);
        m.auth.inc(&["rejected"]);
        m.challenge_runs.inc(&["rejected", "too_quick"]);
        m.ws_bytes_in.add(1234);
        m.tick.observe(Duration::from_micros(80));
        m.tick.observe(Duration::from_micros(80));
        m.tick.observe(Duration::from_secs(1));
        let mut g = Gauges { players_online: 5, ..Default::default() };
        g.rooms[1][0] = 2;
        g.sizes[3] = 1;
        let text = render(&g, &m);

        for line in ["flightsim_players_online 5", "flightsim_websocket_connections 3", "flightsim_ws_bytes_in_total 1234"] {
            assert!(text.lines().any(|l| l == line), "{line} missing in\n{text}");
        }
        assert!(text.contains("flightsim_auth_attempts_total{result=\"ok\"} 2\n"));
        assert!(text.contains("flightsim_auth_attempts_total{result=\"rejected\"} 1\n"));
        assert!(text.contains("flightsim_challenge_runs_total{result=\"rejected\",reason=\"too_quick\"} 1\n"));
        assert!(text.contains("flightsim_rooms{kind=\"hosted\",state=\"lobby\"} 2\n"));
        assert!(text.contains("flightsim_room_size_max 3\n"));
        let build = text.lines().find(|l| l.starts_with("flightsim_build_info{git=\"")).expect("build info");
        assert!(build.ends_with(&format!(",version=\"{}\",protocol=\"{PROTOCOL_VERSION}\"}} 1", env!("CARGO_PKG_VERSION"))), "{build}");
        // cumulative buckets, +Inf = count
        assert!(text.contains("flightsim_tick_seconds_bucket{le=\"0.00005\"} 0\n"));
        assert!(text.contains("flightsim_tick_seconds_bucket{le=\"0.0001\"} 2\n"));
        assert!(text.contains("flightsim_tick_seconds_bucket{le=\"0.1\"} 2\n"));
        assert!(text.contains("flightsim_tick_seconds_bucket{le=\"+Inf\"} 3\n"));
        assert!(text.contains("flightsim_tick_seconds_count 3\n"));
        assert!(text.contains("flightsim_tick_seconds_sum 1.00016\n"));

        // every sample line is `name{labels} value`, and every family has HELP and TYPE first
        let mut typed = std::collections::HashSet::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                typed.insert(rest.split(' ').next().unwrap().to_string());
                continue;
            }
            if line.starts_with("# HELP ") {
                continue;
            }
            let (series, value) = line.rsplit_once(' ').expect(line);
            assert!(value.parse::<f64>().is_ok(), "bad value in {line}");
            let name = series.split('{').next().unwrap();
            assert!(name.starts_with("flightsim_") && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'), "{line}");
            let base = ["_bucket", "_sum", "_count"].iter().find_map(|s| name.strip_suffix(s)).filter(|b| typed.contains(*b));
            assert!(typed.contains(name) || base.is_some(), "{name} has no TYPE");
        }
    }

    #[test]
    fn tokens_and_slugs() {
        assert!(authorized(Some("Bearer s3cret"), "s3cret"));
        assert!(!authorized(Some("Bearer s3cre"), "s3cret"));
        assert!(!authorized(Some("s3cret"), "s3cret"));
        assert!(!authorized(None, "s3cret"));
        assert_eq!(slug("score doesn't add up"), "score_doesnt_add_up");
        assert_eq!(slug("longer than the server saw"), "longer_than_the_server_saw");
    }
}
