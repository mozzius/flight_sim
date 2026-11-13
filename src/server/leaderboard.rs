//! The challenge leaderboard: each signed-in player's best challenge run,
//! kept in SQLite ([`Store`], at `FLIGHT_SIM_DB`, default `./flight_sim.db`)
//! so it survives restarts and redeploys, and the boards built from it.
//!
//! The client is authoritative, so a run is only believed if it's sensible
//! ([`Board::submit`]): a `ChallengeStart` on the same seed came first (the
//! server's clock times the run), it adds up under the scoring rules and
//! took long enough (`protocol::check_run`), and it's within the rate
//! limits. A rejected run is dropped quietly: the reply is the same
//! `Standing` an accepted run that wasn't a best gets.
//!
//! Boards: the global top [`BOARD_ROWS`], and the friends board (us and our
//! mutuals). Mutuals come from presence's relationship cache: the top
//! [`FRIEND_CANDIDATES`] players on the board are checked against us
//! (looked up on the AppView where unknown or stale, the same 5-minute
//! cache and rate limit as presence), and the request waits for those
//! lookups, at most [`FRIENDS_WAIT`]. Handles come from the verified
//! identity and are refreshed whenever a player connects.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::presence::{Did, Job, Presence};
use crate::protocol::*;
use crate::rooms::{ConnId, Out};

/// Where the database lives unless `FLIGHT_SIM_DB` says otherwise.
pub const DEFAULT_DB: &str = "./flight_sim.db";
/// The friends board considers this many of the best players.
pub const FRIEND_CANDIDATES: usize = 300;
/// A friends board waits at most this long for relationship lookups.
pub const FRIENDS_WAIT: Duration = Duration::from_secs(8);
/// `ChallengeStart`s a minute from one player (more are ignored).
const STARTS_PER_MINUTE: usize = 10;
/// One `SubmitScore` per player per this long.
const SUBMIT_GAP: Duration = Duration::from_secs(5);
/// Board requests a minute from one player.
const BOARD_REQUESTS_PER_MINUTE: usize = 30;
/// A run's start is forgotten after this long.
const START_TTL: Duration = Duration::from_secs(MAX_RUN_SECS as u64 + 600);

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The SQLite database: `players` (DID → current handle), `best` (each
/// player's best run) and `runs` (every accepted run, for a look later).
pub struct Store {
    db: Mutex<Connection>,
}

impl Store {
    /// Opens (or creates) the database at `path`, making its directory.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(dir);
        }
        let db = Connection::open(path)?;
        // (WAL: readers don't wait on the writer; NORMAL is still crash-safe under WAL)
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(db)
    }

    /// A throwaway database (tests, and the fallback if the file won't open).
    pub fn memory() -> Self {
        Self::init(Connection::open_in_memory().expect("in-memory sqlite")).expect("sqlite schema")
    }

    fn init(db: Connection) -> rusqlite::Result<Self> {
        db.busy_timeout(Duration::from_secs(2))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS players (
                did TEXT PRIMARY KEY,
                handle TEXT NOT NULL,
                seen INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS best (
                did TEXT PRIMARY KEY,
                score INTEGER NOT NULL,
                wave INTEGER NOT NULL,
                kills INTEGER NOT NULL,
                secs REAL NOT NULL,
                seed INTEGER NOT NULL,
                at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS best_by_score ON best (score DESC, at ASC);
            CREATE TABLE IF NOT EXISTS runs (
                id INTEGER PRIMARY KEY,
                did TEXT NOT NULL,
                score INTEGER NOT NULL,
                wave INTEGER NOT NULL,
                kills INTEGER NOT NULL,
                secs REAL NOT NULL,
                seed INTEGER NOT NULL,
                at INTEGER NOT NULL
            );",
        )?;
        Ok(Self { db: Mutex::new(db) })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The player's verified handle, as of now (renames show on the boards).
    pub fn set_handle(&self, did: &str, handle: &str) -> rusqlite::Result<()> {
        self.db().execute(
            "INSERT INTO players (did, handle, seen) VALUES (?1, ?2, ?3)
             ON CONFLICT (did) DO UPDATE SET handle = excluded.handle, seen = excluded.seen",
            params![did, handle, unix_now()],
        )?;
        Ok(())
    }

    /// Records an accepted run; true if it's the player's new best.
    pub fn record(&self, did: &str, run: &Run, at: i64) -> rusqlite::Result<bool> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO runs (did, score, wave, kills, secs, seed, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![did, run.score, run.wave, run.kills, run.secs as f64, run.seed, at],
        )?;
        let old: Option<u32> = tx.query_row("SELECT score FROM best WHERE did = ?1", [did], |r| r.get(0)).optional()?;
        let best = old.is_none_or(|old| run.score > old);
        if best {
            tx.execute(
                "INSERT INTO best (did, score, wave, kills, secs, seed, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (did) DO UPDATE SET score = excluded.score, wave = excluded.wave, kills = excluded.kills,
                    secs = excluded.secs, seed = excluded.seed, at = excluded.at",
                params![did, run.score, run.wave, run.kills, run.secs as f64, run.seed, at],
            )?;
        }
        tx.commit()?;
        Ok(best)
    }

    /// The player's best and global rank (ties: whoever got there first).
    pub fn standing(&self, did: &str) -> rusqlite::Result<Option<Standing>> {
        self.db()
            .query_row(
                "SELECT b.score, b.wave,
                    (SELECT COUNT(*) FROM best o WHERE o.score > b.score OR (o.score = b.score AND o.at < b.at)) + 1
                 FROM best b WHERE b.did = ?1",
                [did],
                |r| Ok(Standing { score: r.get(0)?, wave: r.get(1)?, rank: r.get(2)? }),
            )
            .optional()
    }

    /// The global top `n`.
    pub fn top(&self, n: usize) -> rusqlite::Result<Vec<BoardRow>> {
        let db = self.db();
        let mut q = db.prepare(
            "SELECT b.did, COALESCE(p.handle, b.did), b.score, b.wave FROM best b LEFT JOIN players p ON p.did = b.did
             ORDER BY b.score DESC, b.at ASC LIMIT ?1",
        )?;
        let rows = q.query_map([n as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        let mut out = vec![];
        for (i, row) in rows.enumerate() {
            let (did, handle, score, wave) = row?;
            out.push(BoardRow { rank: i as u32 + 1, did, handle, score, wave });
        }
        Ok(out)
    }

    /// The DIDs of the best `n` players.
    pub fn top_dids(&self, n: usize) -> rusqlite::Result<Vec<Did>> {
        let db = self.db();
        let mut q = db.prepare("SELECT did FROM best ORDER BY score DESC, at ASC LIMIT ?1")?;
        q.query_map([n as i64], |r| r.get(0))?.collect()
    }

    /// A board of just these players: the top `n` rows (ranked among them),
    /// and where `me` stands among them.
    pub fn among(&self, dids: &[Did], me: &str, n: usize) -> rusqlite::Result<(Vec<BoardRow>, Option<Standing>)> {
        if dids.is_empty() {
            return Ok((vec![], None));
        }
        let db = self.db();
        let marks = vec!["?"; dids.len()].join(",");
        let mut q = db.prepare(&format!(
            "SELECT b.did, COALESCE(p.handle, b.did), b.score, b.wave FROM best b LEFT JOIN players p ON p.did = b.did
             WHERE b.did IN ({marks}) ORDER BY b.score DESC, b.at ASC"
        ))?;
        let rows = q.query_map(params_from_iter(dids.iter()), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        let mut all = vec![];
        for (i, row) in rows.enumerate() {
            let (did, handle, score, wave) = row?;
            all.push(BoardRow { rank: i as u32 + 1, did, handle, score, wave });
        }
        let you = all.iter().find(|r| r.did == me).map(|r| Standing { rank: r.rank, score: r.score, wave: r.wave });
        all.truncate(n);
        Ok((all, you))
    }
}

/// A friends board waiting on relationship lookups.
struct Parked {
    conn: ConnId,
    did: Did,
    candidates: Vec<Did>,
    at: Instant,
}

/// Timestamps of one player's recent asks, for a per-minute cap.
#[derive(Default)]
struct PerMinute(HashMap<Did, VecDeque<Instant>>);

impl PerMinute {
    fn allow(&mut self, did: &str, cap: usize, now: Instant) -> bool {
        let q = self.0.entry(did.to_string()).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60)) {
            q.pop_front();
        }
        if q.len() >= cap {
            return false;
        }
        q.push_back(now);
        true
    }

    fn forget_old(&mut self, now: Instant) {
        self.0.retain(|_, q| q.back().is_some_and(|t| now.duration_since(*t) < Duration::from_secs(60)));
    }
}

/// The leaderboard's part of the hub: runs in progress, the limits, and
/// friends boards waiting for lookups.
pub struct Board {
    pub store: Store,
    /// Each player's run in progress: (seed, when the server heard it start).
    starts: HashMap<Did, (u32, Instant)>,
    start_asks: PerMinute,
    last_submit: HashMap<Did, Instant>,
    board_asks: PerMinute,
    parked: Vec<Parked>,
}

fn log_err<T: Default>(what: &str, r: rusqlite::Result<T>) -> T {
    r.unwrap_or_else(|e| {
        bevy::log::error!("leaderboard {what}: {e}");
        T::default()
    })
}

impl Board {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            starts: HashMap::new(),
            start_asks: PerMinute::default(),
            last_submit: HashMap::new(),
            board_asks: PerMinute::default(),
            parked: vec![],
        }
    }

    pub fn connect(&mut self, did: &str, handle: &str) {
        log_err("handle", self.store.set_handle(did, handle));
    }

    pub fn disconnect(&mut self, conn: ConnId) {
        self.parked.retain(|p| p.conn != conn);
    }

    /// A challenge round began (the newest start wins).
    pub fn start(&mut self, did: &str, seed: u32, now: Instant) {
        if self.start_asks.allow(did, STARTS_PER_MINUTE, now) {
            self.starts.insert(did.to_string(), (seed, now));
        }
    }

    /// A run ended. Believed runs go on the board; either way the reply is
    /// the player's standing (so a rejected run looks like any non-best one).
    pub fn submit(&mut self, conn: ConnId, did: &str, run: Run, now: Instant) -> Vec<Out> {
        let verdict = self.verdict(did, &run, now);
        match verdict {
            Ok(()) => {
                let best = log_err("record", self.store.record(did, &run, unix_now()));
                bevy::log::info!("run by {did}: {} (wave {}, {} kills, {:.0} s){}", run.score, run.wave, run.kills, run.secs, if best { ", a best" } else { "" });
            }
            Err(why) => bevy::log::info!("run by {did} ignored ({why}): {run:?}"),
        }
        vec![Out { to: vec![conn], msg: ServerMsg::Standing(log_err("standing", self.store.standing(did))) }]
    }

    fn verdict(&mut self, did: &str, run: &Run, now: Instant) -> Result<(), &'static str> {
        if self.last_submit.get(did).is_some_and(|t| now.duration_since(*t) < SUBMIT_GAP) {
            return Err("too soon");
        }
        self.last_submit.insert(did.to_string(), now);
        // (one submission per start)
        let Some((seed, at)) = self.starts.remove(did) else { return Err("no start") };
        if seed != run.seed {
            return Err("another seed");
        }
        check_run(run, now.duration_since(at).as_secs_f32())
    }

    /// A board, now or (friends, while lookups are out) once they're back.
    pub fn request(&mut self, conn: ConnId, did: &str, kind: BoardKind, presence: &mut Presence, now: Instant) -> (Vec<Out>, Vec<Job>) {
        if !self.board_asks.allow(did, BOARD_REQUESTS_PER_MINUTE, now) {
            return (vec![], vec![]);
        }
        match kind {
            BoardKind::Global => {
                let rows = log_err("top", self.store.top(BOARD_ROWS));
                let you = log_err("standing", self.store.standing(did));
                (vec![Out { to: vec![conn], msg: ServerMsg::LeaderboardData { kind, rows, you } }], vec![])
            }
            BoardKind::Friends => {
                let mut candidates = log_err("top dids", self.store.top_dids(FRIEND_CANDIDATES));
                candidates.retain(|d| d != did);
                let jobs = presence.lookups_among(did, &candidates, now);
                self.parked.retain(|p| !(p.conn == conn && p.did == did));
                self.parked.push(Parked { conn, did: did.to_string(), candidates, at: now });
                (self.resolve(presence, now), jobs)
            }
        }
    }

    /// Answers the friends boards whose lookups are all back (or that have
    /// waited long enough: they get what's known).
    pub fn resolve(&mut self, presence: &Presence, now: Instant) -> Vec<Out> {
        let mut out = vec![];
        let store = &self.store;
        self.parked.retain(|p| {
            if presence.looking_up(&p.did, &p.candidates) && now.duration_since(p.at) < FRIENDS_WAIT {
                return true;
            }
            let mut set: Vec<Did> = p.candidates.iter().filter(|c| presence.mutual(&p.did, c) == Some(true)).cloned().collect();
            set.push(p.did.clone());
            let (rows, you) = log_err("friends", store.among(&set, &p.did, BOARD_ROWS));
            out.push(Out { to: vec![p.conn], msg: ServerMsg::LeaderboardData { kind: BoardKind::Friends, rows, you } });
            false
        });
        out
    }

    pub fn tick(&mut self, presence: &Presence, now: Instant) -> Vec<Out> {
        self.starts.retain(|_, (_, at)| now.duration_since(*at) < START_TTL);
        self.last_submit.retain(|_, t| now.duration_since(*t) < SUBMIT_GAP);
        self.start_asks.forget_old(now);
        self.board_asks.forget_old(now);
        self.resolve(presence, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presence::tests::{MockAppView, settle};
    use crate::presence::DevAware;

    fn temp_db(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("flight_sim_test_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("nested").join("board.db")
    }

    fn run(seed: u32, score: u32, wave: u32, kills: u32, secs: f32) -> Run {
        Run { seed, score, wave, kills, secs }
    }

    #[test]
    fn bests_and_ranks_survive_reopening() {
        let path = temp_db("reopen");
        {
            let store = Store::open(&path).unwrap();
            store.set_handle("did:plc:a", "a.test").unwrap();
            store.set_handle("did:plc:b", "b.test").unwrap();
            store.set_handle("did:plc:c", "c.test").unwrap();
            assert!(store.record("did:plc:a", &run(1, 550, 2, 3, 40.0), 100).unwrap());
            assert!(store.record("did:plc:b", &run(2, 1150, 3, 4, 60.0), 101).unwrap());
            // a worse run isn't a best and doesn't replace it
            assert!(!store.record("did:plc:b", &run(3, 0, 1, 0, 9.0), 102).unwrap());
            // a tie ranks behind whoever got there first
            assert!(store.record("did:plc:c", &run(4, 550, 2, 3, 41.0), 103).unwrap());
        }
        let store = Store::open(&path).unwrap();
        let top = store.top(BOARD_ROWS).unwrap();
        let summary: Vec<(u32, &str, u32, u32)> = top.iter().map(|r| (r.rank, r.handle.as_str(), r.score, r.wave)).collect();
        assert_eq!(summary, vec![(1, "b.test", 1150, 3), (2, "a.test", 550, 2), (3, "c.test", 550, 2)]);
        assert_eq!(store.standing("did:plc:c").unwrap(), Some(Standing { rank: 3, score: 550, wave: 2 }));
        assert_eq!(store.standing("did:plc:nobody").unwrap(), None);
        // a rename shows on the board
        store.set_handle("did:plc:a", "renamed.test").unwrap();
        assert_eq!(store.top(1).unwrap()[0].handle, "b.test");
        assert_eq!(store.top(2).unwrap()[1].handle, "renamed.test");
        // a board of a few players ranks among them
        let (rows, you) = store.among(&["did:plc:a".into(), "did:plc:c".into()], "did:plc:c", BOARD_ROWS).unwrap();
        assert_eq!(rows.iter().map(|r| (r.rank, r.score)).collect::<Vec<_>>(), vec![(1, 550), (2, 550)]);
        assert_eq!(you, Some(Standing { rank: 2, score: 550, wave: 2 }));
        assert_eq!(store.top_dids(2).unwrap(), vec!["did:plc:b".to_string(), "did:plc:a".to_string()]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn top_twenty_only() {
        let store = Store::memory();
        for i in 0..25u32 {
            let did = format!("did:plc:p{i}");
            store.set_handle(&did, &format!("p{i}")).unwrap();
            // all wave 1 runs: score = kills * 100 (0..=2 kills), plus waves for variety
            let r = run(i, 100 * (i % 3), 1, i % 3, 30.0);
            store.record(&did, &r, i as i64).unwrap();
        }
        assert_eq!(store.top(BOARD_ROWS).unwrap().len(), BOARD_ROWS);
        assert_eq!(store.standing("did:plc:p24").unwrap().map(|s| s.rank), Some(25));
    }

    fn standing_of(out: &[Out]) -> Option<Standing> {
        match &out[..] {
            [Out { msg: ServerMsg::Standing(s), .. }] => *s,
            other => panic!("expected a standing, got {other:?}"),
        }
    }

    #[test]
    fn runs_need_a_start_on_the_same_seed_and_time() {
        let t0 = Instant::now();
        let mut board = Board::new(Store::memory());
        board.connect("did:dev:a", "a");
        // no start: ignored
        let out = board.submit(1, "did:dev:a", run(5, 550, 2, 3, 40.0), t0);
        assert_eq!(standing_of(&out), None);
        // started, but the server saw 20 s and the client claims 40
        board.start("did:dev:a", 5, t0);
        let t = t0 + Duration::from_secs(20);
        assert_eq!(standing_of(&board.submit(1, "did:dev:a", run(5, 550, 2, 3, 40.0), t)), None);
        // the start was used up by that submission
        let t = t + SUBMIT_GAP + Duration::from_secs(60);
        assert_eq!(standing_of(&board.submit(1, "did:dev:a", run(5, 550, 2, 3, 40.0), t)), None);
        // another seed
        board.start("did:dev:a", 6, t);
        let t = t + Duration::from_secs(60);
        assert_eq!(standing_of(&board.submit(1, "did:dev:a", run(5, 550, 2, 3, 40.0), t)), None);
        // all good
        board.start("did:dev:a", 7, t);
        let t = t + Duration::from_secs(45);
        assert_eq!(standing_of(&board.submit(1, "did:dev:a", run(7, 550, 2, 3, 40.0), t)), Some(Standing { rank: 1, score: 550, wave: 2 }));
        // a doctored run (too quick for its waves) is ignored; the reply's the same best
        board.start("did:dev:a", 8, t);
        let t = t + Duration::from_secs(30);
        assert_eq!(standing_of(&board.submit(1, "did:dev:a", run(8, 5150, 6, 12, 29.0), t)), Some(Standing { rank: 1, score: 550, wave: 2 }));
        // submitting again within SUBMIT_GAP is ignored (and doesn't use up the start)
        board.start("did:dev:a", 9, t);
        let out = board.submit(1, "did:dev:a", run(9, 0, 1, 0, 0.5), t + Duration::from_secs(1));
        assert_eq!(standing_of(&out).map(|s| s.score), Some(550));
        assert_eq!(board.starts.get("did:dev:a").map(|s| s.0), Some(9));
    }

    #[test]
    fn starts_are_rate_limited() {
        let t0 = Instant::now();
        let mut board = Board::new(Store::memory());
        for i in 0..STARTS_PER_MINUTE as u32 {
            board.start("did:dev:a", i, t0);
        }
        // the 11th start this minute is ignored: the 10th stands
        board.start("did:dev:a", 99, t0);
        assert_eq!(board.starts.get("did:dev:a").map(|s| s.0), Some(STARTS_PER_MINUTE as u32 - 1));
    }

    #[test]
    fn friends_board_is_us_and_our_mutuals_offline_or_not() {
        let now = Instant::now();
        let mut graph = MockAppView::default();
        graph.mutual("did:plc:me", "did:plc:pal");
        graph.follow("did:plc:me", "did:plc:idol");
        let graph = DevAware { inner: graph, dev_mutuals: false };
        let mut presence = Presence::new();
        // only "me" is online; the others are just on the board
        let (_, jobs) = presence.online(1, "did:plc:me", "me.test", now);
        settle(&mut presence, &graph, jobs, now);
        let mut board = Board::new(Store::memory());
        for (did, h, score) in [("did:plc:me", "me.test", 550), ("did:plc:pal", "pal.test", 1150), ("did:plc:idol", "idol.test", 9100)] {
            board.connect(did, h);
            board.store.record(did, &run(1, score, 3, 50, 900.0), 1).unwrap();
        }
        let (out, jobs) = board.request(1, "did:plc:me", BoardKind::Friends, &mut presence, now);
        assert!(out.is_empty(), "waits for the lookups");
        assert_eq!(jobs.len(), 1);
        for job in jobs {
            presence.apply(bevy::tasks::block_on(job.run(&graph)), now);
        }
        let out = board.resolve(&presence, now);
        let [Out { to, msg: ServerMsg::LeaderboardData { kind: BoardKind::Friends, rows, you } }] = &out[..] else {
            panic!("expected the friends board, got {out:?}")
        };
        assert_eq!(to, &vec![1]);
        assert_eq!(rows.iter().map(|r| r.handle.as_str()).collect::<Vec<_>>(), vec!["pal.test", "me.test"]);
        assert_eq!(*you, Some(Standing { rank: 2, score: 550, wave: 3 }));
        // asked again: the cache answers at once, no lookups
        let (out, jobs) = board.request(1, "did:plc:me", BoardKind::Friends, &mut presence, now);
        assert!(jobs.is_empty());
        assert_eq!(out.len(), 1);
        // the global board has everyone
        let (out, _) = board.request(1, "did:plc:me", BoardKind::Global, &mut presence, now);
        let [Out { msg: ServerMsg::LeaderboardData { rows, you, .. }, .. }] = &out[..] else { panic!() };
        assert_eq!(rows.len(), 3);
        assert_eq!(you.map(|s| s.rank), Some(3));
    }

    #[test]
    fn friends_board_answers_after_a_while_without_lookups() {
        let now = Instant::now();
        let mut presence = Presence::new();
        let _ = presence.online(1, "did:plc:me", "me.test", now);
        let mut board = Board::new(Store::memory());
        board.connect("did:plc:me", "me.test");
        board.connect("did:plc:x", "x.test");
        board.store.record("did:plc:x", &run(1, 100, 1, 1, 30.0), 1).unwrap();
        let (out, jobs) = board.request(1, "did:plc:me", BoardKind::Friends, &mut presence, now);
        assert!(out.is_empty() && !jobs.is_empty());
        // the AppView never answers
        assert!(board.tick(&presence, now + Duration::from_secs(2)).is_empty());
        let out = board.tick(&presence, now + FRIENDS_WAIT);
        assert!(matches!(&out[..], [Out { msg: ServerMsg::LeaderboardData { rows, you: None, .. }, .. }] if rows.is_empty()));
    }
}
