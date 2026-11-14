//! The challenge leaderboard: each signed-in player's best challenge run,
//! kept in a database so it survives restarts and redeploys, and the boards
//! built from it.
//!
//! The database ([`Db`]) is Postgres (`DATABASE_URL`), shared by every
//! replica, or a SQLite file (`FLIGHT_SIM_DB`, default `./flight_sim.db`)
//! when there's no `DATABASE_URL` (local dev and tests). On a Postgres with
//! no rows yet, a SQLite file at `FLIGHT_SIM_DB` is imported once
//! ([`Db::import_sqlite`]): that's how the scores from before Postgres
//! moved over.
//!
//! Queries never run in the game loop: [`Board`] turns what players do into
//! [`DbJob`]s, which a worker runs ([`Db::run`]), and the [`DbResult`]s come
//! back to the loop ([`Board::apply`]).
//!
//! The client is authoritative, so a run is only believed if it's sensible
//! ([`Board::submit`]): a `ChallengeStart` on the same seed came first (the
//! server's clock times the run; starts are kept in the cluster's shared
//! state, so a reconnect to another replica keeps them), it adds up under
//! the scoring rules and took long enough (`protocol::check_run`), and it's
//! within the rate limits (shared too). A rejected run is dropped quietly:
//! the reply is the same `Standing` an accepted run that wasn't a best gets.
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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use sqlx::Row;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

use crate::cluster::Backend;
use crate::presence::{Did, Job, Presence};
use crate::protocol::*;
use crate::rooms::{ConnId, Out};

/// Where the SQLite database lives unless `FLIGHT_SIM_DB` says otherwise.
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
/// The board's size (a gauge) is counted again this often.
const SIZE_EVERY: Duration = Duration::from_secs(30);

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Work for the database worker.
#[derive(Clone, Debug, PartialEq)]
pub enum DbJob {
    /// The player's verified handle, as of now (renames show on the boards).
    Handle { did: Did, handle: String },
    /// An accepted run: record it, then the player's standing.
    Record { conn: ConnId, did: Did, run: Run },
    /// The player's standing (after a rejected run).
    Standing { conn: ConnId, did: Did },
    /// The global board.
    Global { conn: ConnId, did: Did },
    /// The best players' DIDs, for a friends board.
    TopDids { conn: ConnId, did: Did },
    /// A friends board of these players.
    Among { conn: ConnId, did: Did, set: Vec<Did> },
    /// How many players are on the board.
    Size,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DbResult {
    Nothing,
    Standing { conn: ConnId, standing: Option<Standing> },
    Board { conn: ConnId, kind: BoardKind, rows: Vec<BoardRow>, you: Option<Standing> },
    TopDids { conn: ConnId, did: Did, dids: Vec<Did> },
    Size(u64),
}

/// The tables: `players` (DID → current handle), `best` (each player's best
/// run) and `runs` (every accepted run, for a look later). The same in both.
#[derive(Clone)]
pub enum Db {
    Sqlite(Arc<Mutex<Connection>>),
    Postgres(PgPool),
}

/// Rows moved by an import (players, best, runs).
pub type Counts = (u64, u64, u64);

const SQLITE_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS players (
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
    );";

const PG_SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS players (
        did TEXT PRIMARY KEY,
        handle TEXT NOT NULL,
        seen BIGINT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS best (
        did TEXT PRIMARY KEY,
        score BIGINT NOT NULL,
        wave BIGINT NOT NULL,
        kills BIGINT NOT NULL,
        secs DOUBLE PRECISION NOT NULL,
        seed BIGINT NOT NULL,
        at BIGINT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS best_by_score ON best (score DESC, at ASC)",
    "CREATE TABLE IF NOT EXISTS runs (
        id BIGSERIAL PRIMARY KEY,
        did TEXT NOT NULL,
        score BIGINT NOT NULL,
        wave BIGINT NOT NULL,
        kills BIGINT NOT NULL,
        secs DOUBLE PRECISION NOT NULL,
        seed BIGINT NOT NULL,
        at BIGINT NOT NULL
    )",
];

/// Every replica takes this advisory lock to set up the tables (and import).
const SETUP_LOCK: i64 = 0x666c_6967_6874;

type DbErr = String;

fn e<E: std::fmt::Display>(err: E) -> DbErr {
    err.to_string()
}

impl Db {
    /// Opens (or creates) the SQLite database at `path`, making its directory.
    pub fn sqlite(path: &Path) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(dir);
        }
        let db = Connection::open(path)?;
        // (WAL: readers don't wait on the writer; NORMAL is still crash-safe under WAL)
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        Self::sqlite_init(db)
    }

    /// A throwaway SQLite database (tests, and the fallback if the file won't open).
    pub fn memory() -> Self {
        Self::sqlite_init(Connection::open_in_memory().expect("in-memory sqlite")).expect("sqlite schema")
    }

    fn sqlite_init(db: Connection) -> rusqlite::Result<Self> {
        db.busy_timeout(Duration::from_secs(2))?;
        db.execute_batch(SQLITE_SCHEMA)?;
        Ok(Self::Sqlite(Arc::new(Mutex::new(db))))
    }

    /// Connects to Postgres and makes the tables. `schema`: use (and make)
    /// that schema instead of the default (tests).
    pub async fn postgres(url: &str, schema: Option<&str>) -> Result<Self, DbErr> {
        let mut opts: PgConnectOptions = url.parse().map_err(e)?;
        if let Some(s) = schema {
            opts = opts.options([("search_path", s)]);
        }
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(opts)
            .await
            .map_err(e)?;
        let mut tx = pool.begin().await.map_err(e)?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(SETUP_LOCK).execute(&mut *tx).await.map_err(e)?;
        if let Some(s) = schema {
            sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS \"{}\"", s.replace('"', ""))).execute(&mut *tx).await.map_err(e)?;
        }
        for stmt in PG_SCHEMA {
            sqlx::query(stmt).execute(&mut *tx).await.map_err(e)?;
        }
        tx.commit().await.map_err(e)?;
        Ok(Self::Postgres(pool))
    }

    /// Row counts (players, best, runs).
    pub async fn counts(&self) -> Result<Counts, DbErr> {
        match self {
            Db::Sqlite(db) => {
                let db = db.lock().unwrap_or_else(|e| e.into_inner());
                let n = |t: &str| db.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get::<_, i64>(0)).map(|n| n as u64);
                Ok((n("players").map_err(e)?, n("best").map_err(e)?, n("runs").map_err(e)?))
            }
            Db::Postgres(pool) => {
                let n = |t: &'static str| async move {
                    sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {t}")).fetch_one(pool).await.map(|n| n as u64).map_err(e)
                };
                Ok((n("players").await?, n("best").await?, n("runs").await?))
            }
        }
    }

    /// Copies a SQLite leaderboard into this (Postgres) one, if this one is
    /// empty and the file exists: Ok(Some((sqlite counts, postgres counts
    /// after))) if it did. Every replica may try; the advisory lock and the
    /// emptiness check make it happen once.
    pub async fn import_sqlite(&self, path: &Path) -> Result<Option<(Counts, Counts)>, DbErr> {
        let Db::Postgres(pool) = self else { return Ok(None) };
        if !path.exists() {
            return Ok(None);
        }
        let mut tx = pool.begin().await.map_err(e)?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(SETUP_LOCK).execute(&mut *tx).await.map_err(e)?;
        let rows: i64 = sqlx::query_scalar("SELECT (SELECT COUNT(*) FROM players) + (SELECT COUNT(*) FROM best) + (SELECT COUNT(*) FROM runs)")
            .fetch_one(&mut *tx)
            .await
            .map_err(e)?;
        if rows > 0 {
            return Ok(None);
        }
        type PlayerRow = (String, String, i64);
        type RunRow = (String, i64, i64, i64, f64, i64, i64);
        let (players, best, runs) = {
            let src = Connection::open(path).map_err(e)?;
            let players: Vec<PlayerRow> = src
                .prepare("SELECT did, handle, seen FROM players")
                .map_err(e)?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(e)?
                .collect::<Result<_, _>>()
                .map_err(e)?;
            let run_rows = |sql: &str| -> Result<Vec<RunRow>, DbErr> {
                src.prepare(sql)
                    .map_err(e)?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))
                    .map_err(e)?
                    .collect::<Result<_, _>>()
                    .map_err(e)
            };
            let best = run_rows("SELECT did, score, wave, kills, secs, seed, at FROM best")?;
            let runs = run_rows("SELECT did, score, wave, kills, secs, seed, at FROM runs ORDER BY id")?;
            (players, best, runs)
        };
        let from = (players.len() as u64, best.len() as u64, runs.len() as u64);
        for chunk in players.chunks(1000) {
            sqlx::query("INSERT INTO players (did, handle, seen) SELECT * FROM UNNEST($1::text[], $2::text[], $3::bigint[])")
                .bind(chunk.iter().map(|r| r.0.clone()).collect::<Vec<_>>())
                .bind(chunk.iter().map(|r| r.1.clone()).collect::<Vec<_>>())
                .bind(chunk.iter().map(|r| r.2).collect::<Vec<_>>())
                .execute(&mut *tx)
                .await
                .map_err(e)?;
        }
        for (table, rows) in [("best", &best), ("runs", &runs)] {
            for chunk in rows.chunks(1000) {
                sqlx::query(&format!(
                    "INSERT INTO {table} (did, score, wave, kills, secs, seed, at)
                     SELECT * FROM UNNEST($1::text[], $2::bigint[], $3::bigint[], $4::bigint[], $5::float8[], $6::bigint[], $7::bigint[])"
                ))
                .bind(chunk.iter().map(|r| r.0.clone()).collect::<Vec<_>>())
                .bind(chunk.iter().map(|r| r.1).collect::<Vec<_>>())
                .bind(chunk.iter().map(|r| r.2).collect::<Vec<_>>())
                .bind(chunk.iter().map(|r| r.3).collect::<Vec<_>>())
                .bind(chunk.iter().map(|r| r.4).collect::<Vec<_>>())
                .bind(chunk.iter().map(|r| r.5).collect::<Vec<_>>())
                .bind(chunk.iter().map(|r| r.6).collect::<Vec<_>>())
                .execute(&mut *tx)
                .await
                .map_err(e)?;
            }
        }
        tx.commit().await.map_err(e)?;
        Ok(Some((from, self.counts().await?)))
    }

    /// Runs a job; errors are logged and answered with nothing.
    pub async fn run(&self, job: DbJob) -> DbResult {
        let what = format!("{job:?}");
        match self.try_run(job).await {
            Ok(r) => r,
            Err(err) => {
                bevy::log::error!("leaderboard {}: {err}", what.split([' ', '{', '(']).next().unwrap_or(""));
                DbResult::Nothing
            }
        }
    }

    async fn try_run(&self, job: DbJob) -> Result<DbResult, DbErr> {
        Ok(match job {
            DbJob::Handle { did, handle } => {
                self.set_handle(&did, &handle).await?;
                DbResult::Nothing
            }
            DbJob::Record { conn, did, run } => {
                let best = self.record(&did, &run, unix_now()).await?;
                bevy::log::info!(
                    "run by {did}: {} (wave {}, {} kills, {:.0} s){}",
                    run.score,
                    run.wave,
                    run.kills,
                    run.secs,
                    if best { ", a best" } else { "" }
                );
                DbResult::Standing { conn, standing: self.standing(&did).await? }
            }
            DbJob::Standing { conn, did } => DbResult::Standing { conn, standing: self.standing(&did).await? },
            DbJob::Global { conn, did } => {
                DbResult::Board { conn, kind: BoardKind::Global, rows: self.top(BOARD_ROWS).await?, you: self.standing(&did).await? }
            }
            DbJob::TopDids { conn, did } => DbResult::TopDids { conn, did, dids: self.top_dids(FRIEND_CANDIDATES).await? },
            DbJob::Among { conn, did, set } => {
                let (rows, you) = self.among(&set, &did, BOARD_ROWS).await?;
                DbResult::Board { conn, kind: BoardKind::Friends, rows, you }
            }
            DbJob::Size => DbResult::Size(self.size().await?),
        })
    }

    fn lite(db: &Arc<Mutex<Connection>>) -> std::sync::MutexGuard<'_, Connection> {
        db.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub async fn set_handle(&self, did: &str, handle: &str) -> Result<(), DbErr> {
        match self {
            Db::Sqlite(db) => {
                Self::lite(db)
                    .execute(
                        "INSERT INTO players (did, handle, seen) VALUES (?1, ?2, ?3)
                         ON CONFLICT (did) DO UPDATE SET handle = excluded.handle, seen = excluded.seen",
                        params![did, handle, unix_now()],
                    )
                    .map_err(e)?;
            }
            Db::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO players (did, handle, seen) VALUES ($1, $2, $3)
                     ON CONFLICT (did) DO UPDATE SET handle = excluded.handle, seen = excluded.seen",
                )
                .bind(did)
                .bind(handle)
                .bind(unix_now())
                .execute(pool)
                .await
                .map_err(e)?;
            }
        }
        Ok(())
    }

    /// Records an accepted run; true if it's the player's new best.
    pub async fn record(&self, did: &str, run: &Run, at: i64) -> Result<bool, DbErr> {
        match self {
            Db::Sqlite(db) => {
                let mut db = Self::lite(db);
                let tx = db.transaction().map_err(e)?;
                tx.execute(
                    "INSERT INTO runs (did, score, wave, kills, secs, seed, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![did, run.score, run.wave, run.kills, run.secs as f64, run.seed, at],
                )
                .map_err(e)?;
                let old: Option<u32> = tx.query_row("SELECT score FROM best WHERE did = ?1", [did], |r| r.get(0)).optional().map_err(e)?;
                let best = old.is_none_or(|old| run.score > old);
                if best {
                    tx.execute(
                        "INSERT INTO best (did, score, wave, kills, secs, seed, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                         ON CONFLICT (did) DO UPDATE SET score = excluded.score, wave = excluded.wave, kills = excluded.kills,
                            secs = excluded.secs, seed = excluded.seed, at = excluded.at",
                        params![did, run.score, run.wave, run.kills, run.secs as f64, run.seed, at],
                    )
                    .map_err(e)?;
                }
                tx.commit().map_err(e)?;
                Ok(best)
            }
            Db::Postgres(pool) => {
                let mut tx = pool.begin().await.map_err(e)?;
                let vals = (run.score as i64, run.wave as i64, run.kills as i64, run.secs as f64, run.seed as i64, at);
                sqlx::query("INSERT INTO runs (did, score, wave, kills, secs, seed, at) VALUES ($1, $2, $3, $4, $5, $6, $7)")
                    .bind(did)
                    .bind(vals.0)
                    .bind(vals.1)
                    .bind(vals.2)
                    .bind(vals.3)
                    .bind(vals.4)
                    .bind(vals.5)
                    .execute(&mut *tx)
                    .await
                    .map_err(e)?;
                // (a row back: inserted, or it beat the old best; atomic across replicas)
                let best = sqlx::query(
                    "INSERT INTO best (did, score, wave, kills, secs, seed, at) VALUES ($1, $2, $3, $4, $5, $6, $7)
                     ON CONFLICT (did) DO UPDATE SET score = excluded.score, wave = excluded.wave, kills = excluded.kills,
                        secs = excluded.secs, seed = excluded.seed, at = excluded.at
                     WHERE best.score < excluded.score
                     RETURNING did",
                )
                .bind(did)
                .bind(vals.0)
                .bind(vals.1)
                .bind(vals.2)
                .bind(vals.3)
                .bind(vals.4)
                .bind(vals.5)
                .fetch_optional(&mut *tx)
                .await
                .map_err(e)?
                .is_some();
                tx.commit().await.map_err(e)?;
                Ok(best)
            }
        }
    }

    /// How many players have a best run.
    pub async fn size(&self) -> Result<u64, DbErr> {
        match self {
            Db::Sqlite(db) => Self::lite(db).query_row("SELECT COUNT(*) FROM best", [], |r| r.get::<_, i64>(0)).map(|n| n as u64).map_err(e),
            Db::Postgres(pool) => {
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM best").fetch_one(pool).await.map(|n| n as u64).map_err(e)
            }
        }
    }

    /// The player's best and global rank (ties: whoever got there first).
    pub async fn standing(&self, did: &str) -> Result<Option<Standing>, DbErr> {
        const SQL: &str = "SELECT b.score, b.wave,
                (SELECT COUNT(*) FROM best o WHERE o.score > b.score OR (o.score = b.score AND o.at < b.at)) + 1
             FROM best b WHERE b.did = ";
        match self {
            Db::Sqlite(db) => Self::lite(db)
                .query_row(&format!("{SQL}?1"), [did], |r| Ok(Standing { score: r.get(0)?, wave: r.get(1)?, rank: r.get(2)? }))
                .optional()
                .map_err(e),
            Db::Postgres(pool) => {
                let row = sqlx::query(&format!("{SQL}$1")).bind(did).fetch_optional(pool).await.map_err(e)?;
                Ok(row.map(|r| Standing {
                    score: r.get::<i64, _>(0) as u32,
                    wave: r.get::<i64, _>(1) as u32,
                    rank: r.get::<i64, _>(2) as u32,
                }))
            }
        }
    }

    fn rows(found: Vec<(String, String, i64, i64)>) -> Vec<BoardRow> {
        found
            .into_iter()
            .enumerate()
            .map(|(i, (did, handle, score, wave))| BoardRow { rank: i as u32 + 1, did, handle, score: score as u32, wave: wave as u32 })
            .collect()
    }

    /// The global top `n`.
    pub async fn top(&self, n: usize) -> Result<Vec<BoardRow>, DbErr> {
        const SQL: &str = "SELECT b.did, COALESCE(p.handle, b.did), b.score, b.wave FROM best b LEFT JOIN players p ON p.did = b.did
             ORDER BY b.score DESC, b.at ASC LIMIT ";
        let found = match self {
            Db::Sqlite(db) => {
                let db = Self::lite(db);
                let mut q = db.prepare(&format!("{SQL}?1")).map_err(e)?;
                q.query_map([n as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map_err(e)?.collect::<Result<_, _>>().map_err(e)?
            }
            Db::Postgres(pool) => sqlx::query_as(&format!("{SQL}$1")).bind(n as i64).fetch_all(pool).await.map_err(e)?,
        };
        Ok(Self::rows(found))
    }

    /// The DIDs of the best `n` players.
    pub async fn top_dids(&self, n: usize) -> Result<Vec<Did>, DbErr> {
        const SQL: &str = "SELECT did FROM best ORDER BY score DESC, at ASC LIMIT ";
        match self {
            Db::Sqlite(db) => {
                let db = Self::lite(db);
                let mut q = db.prepare(&format!("{SQL}?1")).map_err(e)?;
                q.query_map([n as i64], |r| r.get(0)).map_err(e)?.collect::<Result<_, _>>().map_err(e)
            }
            Db::Postgres(pool) => sqlx::query_scalar(&format!("{SQL}$1")).bind(n as i64).fetch_all(pool).await.map_err(e),
        }
    }

    /// A board of just these players: the top `n` rows (ranked among them),
    /// and where `me` stands among them.
    pub async fn among(&self, dids: &[Did], me: &str, n: usize) -> Result<(Vec<BoardRow>, Option<Standing>), DbErr> {
        if dids.is_empty() {
            return Ok((vec![], None));
        }
        const SQL: &str = "SELECT b.did, COALESCE(p.handle, b.did), b.score, b.wave FROM best b LEFT JOIN players p ON p.did = b.did WHERE b.did";
        const ORDER: &str = "ORDER BY b.score DESC, b.at ASC";
        let found = match self {
            Db::Sqlite(db) => {
                let db = Self::lite(db);
                let marks = vec!["?"; dids.len()].join(",");
                let mut q = db.prepare(&format!("{SQL} IN ({marks}) {ORDER}")).map_err(e)?;
                q.query_map(params_from_iter(dids.iter()), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                    .map_err(e)?
                    .collect::<Result<_, _>>()
                    .map_err(e)?
            }
            Db::Postgres(pool) => {
                sqlx::query_as(&format!("{SQL} = ANY($1) {ORDER}")).bind(dids.to_vec()).fetch_all(pool).await.map_err(e)?
            }
        };
        let mut all = Self::rows(found);
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

/// The leaderboard's part of the hub: the limits, and friends boards
/// waiting for lookups. (Run starts and the submission limit are in the
/// cluster's shared state.)
pub struct Board {
    board_asks: PerMinute,
    parked: Vec<Parked>,
    /// The last count of players on the board.
    size: u64,
    last_size: Option<Instant>,
}

impl Default for Board {
    fn default() -> Self {
        Self::new()
    }
}

impl Board {
    pub fn new() -> Self {
        Self { board_asks: PerMinute::default(), parked: vec![], size: 0, last_size: None }
    }

    pub fn connect(&mut self, did: &str, handle: &str) -> DbJob {
        DbJob::Handle { did: did.to_string(), handle: handle.to_string() }
    }

    pub fn disconnect(&mut self, conn: ConnId) {
        self.parked.retain(|p| p.conn != conn);
    }

    /// How many players are on the board.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// A challenge round began (the newest start wins).
    pub fn start(&mut self, did: &str, seed: u32, cluster: &dyn Backend, now: Instant) {
        crate::metrics::M.challenge_starts.inc();
        if cluster.allow("start", did, "", Duration::ZERO, STARTS_PER_MINUTE, now) {
            cluster.start_run(did, seed, START_TTL, now);
        }
    }

    /// A run ended. Believed runs go on the board; either way the reply is
    /// the player's standing (so a rejected run looks like any non-best one).
    pub fn submit(&mut self, conn: ConnId, did: &str, run: Run, cluster: &dyn Backend, now: Instant) -> DbJob {
        let verdict = Self::verdict(did, &run, cluster, now);
        match verdict {
            Ok(()) => {
                crate::metrics::M.challenge_runs.inc(&["accepted", ""]);
                // (the board's grown, maybe)
                self.last_size = None;
                DbJob::Record { conn, did: did.to_string(), run }
            }
            Err(why) => {
                crate::metrics::M.challenge_runs.inc(&["rejected", &crate::metrics::slug(why)]);
                bevy::log::info!("run by {did} ignored ({why}): {run:?}");
                DbJob::Standing { conn, did: did.to_string() }
            }
        }
    }

    fn verdict(did: &str, run: &Run, cluster: &dyn Backend, now: Instant) -> Result<(), &'static str> {
        if !cluster.allow("submit", did, "", SUBMIT_GAP, usize::MAX, now) {
            return Err("too soon");
        }
        // (one submission per start)
        let Some((seed, secs)) = cluster.take_run(did, now) else { return Err("no start") };
        if seed != run.seed {
            return Err("another seed");
        }
        check_run(run, secs)
    }

    /// A board, now (global) or once the lookups it needs are back (friends).
    pub fn request(&mut self, conn: ConnId, did: &str, kind: BoardKind, now: Instant) -> Option<DbJob> {
        if !self.board_asks.allow(did, BOARD_REQUESTS_PER_MINUTE, now) {
            return None;
        }
        let did = did.to_string();
        Some(match kind {
            BoardKind::Global => DbJob::Global { conn, did },
            BoardKind::Friends => DbJob::TopDids { conn, did },
        })
    }

    /// A database answer: messages, lookups for a friends board, and more database work.
    pub fn apply(&mut self, result: DbResult, presence: &mut Presence, now: Instant) -> (Vec<Out>, Vec<Job>, Vec<DbJob>) {
        match result {
            DbResult::Nothing => (vec![], vec![], vec![]),
            DbResult::Standing { conn, standing } => (vec![Out { to: vec![conn], msg: ServerMsg::Standing(standing) }], vec![], vec![]),
            DbResult::Board { conn, kind, rows, you } => {
                (vec![Out { to: vec![conn], msg: ServerMsg::LeaderboardData { kind, rows, you } }], vec![], vec![])
            }
            DbResult::TopDids { conn, did, mut dids } => {
                dids.retain(|d| *d != did);
                let jobs = presence.lookups_among(&did, &dids, now);
                self.parked.retain(|p| !(p.conn == conn && p.did == did));
                self.parked.push(Parked { conn, did, candidates: dids, at: now });
                (vec![], jobs, self.resolve(presence, now))
            }
            DbResult::Size(n) => {
                self.size = n;
                (vec![], vec![], vec![])
            }
        }
    }

    /// Friends boards whose lookups are all back (or that have waited long
    /// enough: they get what's known), ready to be read.
    pub fn resolve(&mut self, presence: &Presence, now: Instant) -> Vec<DbJob> {
        let mut jobs = vec![];
        self.parked.retain(|p| {
            if presence.looking_up(&p.did, &p.candidates) && now.duration_since(p.at) < FRIENDS_WAIT {
                return true;
            }
            let mut set: Vec<Did> = p.candidates.iter().filter(|c| presence.mutual(&p.did, c) == Some(true)).cloned().collect();
            set.push(p.did.clone());
            jobs.push(DbJob::Among { conn: p.conn, did: p.did.clone(), set });
            false
        });
        jobs
    }

    pub fn tick(&mut self, presence: &Presence, now: Instant) -> Vec<DbJob> {
        self.board_asks.forget_old(now);
        let mut jobs = self.resolve(presence, now);
        if self.last_size.is_none_or(|t| now.duration_since(t) >= SIZE_EVERY) {
            self.last_size = Some(now);
            jobs.push(DbJob::Size);
        }
        jobs
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::cluster::Memory;
    use crate::presence::DevAware;
    use crate::presence::tests::{MockAppView, settle};
    use bevy::tasks::block_on;

    fn temp_db(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("flight_sim_test_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("nested").join("board.db")
    }

    fn run(seed: u32, score: u32, wave: u32, kills: u32, secs: f32) -> Run {
        Run { seed, score, wave, kills, secs }
    }

    /// The same checks on any database (SQLite here, Postgres below).
    pub async fn check_store(store: &Db) {
        store.set_handle("did:plc:a", "a.test").await.unwrap();
        store.set_handle("did:plc:b", "b.test").await.unwrap();
        store.set_handle("did:plc:c", "c.test").await.unwrap();
        assert!(store.record("did:plc:a", &run(1, 550, 2, 3, 40.0), 100).await.unwrap());
        assert!(store.record("did:plc:b", &run(2, 1150, 3, 4, 60.0), 101).await.unwrap());
        // a worse run isn't a best and doesn't replace it
        assert!(!store.record("did:plc:b", &run(3, 0, 1, 0, 9.0), 102).await.unwrap());
        // a tie ranks behind whoever got there first
        assert!(store.record("did:plc:c", &run(4, 550, 2, 3, 41.0), 103).await.unwrap());
        let top = store.top(BOARD_ROWS).await.unwrap();
        let summary: Vec<(u32, &str, u32, u32)> = top.iter().map(|r| (r.rank, r.handle.as_str(), r.score, r.wave)).collect();
        assert_eq!(summary, vec![(1, "b.test", 1150, 3), (2, "a.test", 550, 2), (3, "c.test", 550, 2)]);
        assert_eq!(store.standing("did:plc:c").await.unwrap(), Some(Standing { rank: 3, score: 550, wave: 2 }));
        assert_eq!(store.standing("did:plc:nobody").await.unwrap(), None);
        // a rename shows on the board
        store.set_handle("did:plc:a", "renamed.test").await.unwrap();
        assert_eq!(store.top(1).await.unwrap()[0].handle, "b.test");
        assert_eq!(store.top(2).await.unwrap()[1].handle, "renamed.test");
        // a board of a few players ranks among them
        let (rows, you) = store.among(&["did:plc:a".into(), "did:plc:c".into()], "did:plc:c", BOARD_ROWS).await.unwrap();
        assert_eq!(rows.iter().map(|r| (r.rank, r.score)).collect::<Vec<_>>(), vec![(1, 550), (2, 550)]);
        assert_eq!(you, Some(Standing { rank: 2, score: 550, wave: 2 }));
        assert_eq!(store.top_dids(2).await.unwrap(), vec!["did:plc:b".to_string(), "did:plc:a".to_string()]);
        assert_eq!(store.size().await.unwrap(), 3);
        assert_eq!(store.counts().await.unwrap(), (3, 3, 4));
    }

    #[test]
    fn bests_and_ranks_survive_reopening() {
        let path = temp_db("reopen");
        {
            let store = Db::sqlite(&path).unwrap();
            block_on(check_store(&store));
        }
        let store = Db::sqlite(&path).unwrap();
        assert_eq!(block_on(store.top(BOARD_ROWS)).unwrap().len(), 3);
        assert_eq!(block_on(store.top(1)).unwrap()[0].handle, "b.test");
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn top_twenty_only() {
        let store = Db::memory();
        for i in 0..25u32 {
            let did = format!("did:plc:p{i}");
            block_on(store.set_handle(&did, &format!("p{i}"))).unwrap();
            // all wave 1 runs: score = kills * 100 (0..=2 kills), plus waves for variety
            let r = run(i, 100 * (i % 3), 1, i % 3, 30.0);
            block_on(store.record(&did, &r, i as i64)).unwrap();
        }
        assert_eq!(block_on(store.top(BOARD_ROWS)).unwrap().len(), BOARD_ROWS);
        assert_eq!(block_on(store.standing("did:plc:p24")).unwrap().map(|s| s.rank), Some(25));
    }

    /// The submission, run against the database: the standing it answers.
    fn submit(board: &mut Board, db: &Db, cluster: &dyn Backend, run: Run, now: Instant) -> Option<Standing> {
        match block_on(db.run(board.submit(1, "did:dev:a", run, cluster, now))) {
            DbResult::Standing { standing, .. } => standing,
            other => panic!("expected a standing, got {other:?}"),
        }
    }

    #[test]
    fn runs_need_a_start_on_the_same_seed_and_time() {
        let t0 = Instant::now();
        let mut board = Board::new();
        let db = Db::memory();
        let cluster = Memory::single();
        let c: &dyn Backend = &*cluster;
        block_on(db.run(board.connect("did:dev:a", "a")));
        // no start: ignored
        assert_eq!(submit(&mut board, &db, c, run(5, 550, 2, 3, 40.0), t0), None);
        // started, but the server saw 20 s and the client claims 40
        board.start("did:dev:a", 5, c, t0);
        let t = t0 + Duration::from_secs(20);
        assert_eq!(submit(&mut board, &db, c, run(5, 550, 2, 3, 40.0), t), None);
        // the start was used up by that submission
        let t = t + SUBMIT_GAP + Duration::from_secs(60);
        assert_eq!(submit(&mut board, &db, c, run(5, 550, 2, 3, 40.0), t), None);
        // another seed
        board.start("did:dev:a", 6, c, t);
        let t = t + Duration::from_secs(60);
        assert_eq!(submit(&mut board, &db, c, run(5, 550, 2, 3, 40.0), t), None);
        // all good
        board.start("did:dev:a", 7, c, t);
        let t = t + Duration::from_secs(45);
        assert_eq!(submit(&mut board, &db, c, run(7, 550, 2, 3, 40.0), t), Some(Standing { rank: 1, score: 550, wave: 2 }));
        // a doctored run (too quick for its waves) is ignored; the reply's the same best
        board.start("did:dev:a", 8, c, t);
        let t = t + Duration::from_secs(30);
        assert_eq!(submit(&mut board, &db, c, run(8, 5150, 6, 12, 29.0), t), Some(Standing { rank: 1, score: 550, wave: 2 }));
        // submitting again within SUBMIT_GAP is ignored (and doesn't use up the start)
        board.start("did:dev:a", 9, c, t);
        assert_eq!(submit(&mut board, &db, c, run(9, 0, 1, 0, 0.5), t + Duration::from_secs(1)).map(|s| s.score), Some(550));
        assert_eq!(c.take_run("did:dev:a", t + Duration::from_secs(1)).map(|s| s.0), Some(9));
    }

    #[test]
    fn starts_are_rate_limited() {
        let t0 = Instant::now();
        let mut board = Board::new();
        let cluster = Memory::single();
        for i in 0..STARTS_PER_MINUTE as u32 {
            board.start("did:dev:a", i, &*cluster, t0);
        }
        // the 11th start this minute is ignored: the 10th stands
        board.start("did:dev:a", 99, &*cluster, t0);
        assert_eq!(cluster.take_run("did:dev:a", t0).map(|s| s.0), Some(STARTS_PER_MINUTE as u32 - 1));
    }

    /// Runs database jobs (and the ones their results lead to): the messages.
    fn settle_db(board: &mut Board, db: &Db, presence: &mut Presence, mut jobs: Vec<DbJob>, now: Instant) -> (Vec<Out>, Vec<Job>) {
        let (mut out, mut lookups) = (vec![], vec![]);
        while let Some(job) = jobs.pop() {
            let (o, l, more) = board.apply(block_on(db.run(job)), presence, now);
            out.extend(o);
            lookups.extend(l);
            jobs.extend(more);
        }
        (out, lookups)
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
        let mut board = Board::new();
        let db = Db::memory();
        for (did, h, score) in [("did:plc:me", "me.test", 550), ("did:plc:pal", "pal.test", 1150), ("did:plc:idol", "idol.test", 9100)] {
            block_on(db.run(board.connect(did, h)));
            block_on(db.record(did, &run(1, score, 3, 50, 900.0), 1)).unwrap();
        }
        let job = board.request(1, "did:plc:me", BoardKind::Friends, now).unwrap();
        let (out, jobs) = settle_db(&mut board, &db, &mut presence, vec![job], now);
        assert!(out.is_empty(), "waits for the lookups");
        assert_eq!(jobs.len(), 1);
        for job in jobs {
            presence.apply(block_on(job.run(&graph)), now);
        }
        let ready = board.resolve(&presence, now);
        let (out, _) = settle_db(&mut board, &db, &mut presence, ready, now);
        let [Out { to, msg: ServerMsg::LeaderboardData { kind: BoardKind::Friends, rows, you } }] = &out[..] else {
            panic!("expected the friends board, got {out:?}")
        };
        assert_eq!(to, &vec![1]);
        assert_eq!(rows.iter().map(|r| r.handle.as_str()).collect::<Vec<_>>(), vec!["pal.test", "me.test"]);
        assert_eq!(*you, Some(Standing { rank: 2, score: 550, wave: 3 }));
        // asked again: the cache answers at once, no lookups
        let job = board.request(1, "did:plc:me", BoardKind::Friends, now).unwrap();
        let (out, jobs) = settle_db(&mut board, &db, &mut presence, vec![job], now);
        assert!(jobs.is_empty());
        assert_eq!(out.len(), 1);
        // the global board has everyone
        let job = board.request(1, "did:plc:me", BoardKind::Global, now).unwrap();
        let (out, _) = settle_db(&mut board, &db, &mut presence, vec![job], now);
        let [Out { msg: ServerMsg::LeaderboardData { rows, you, .. }, .. }] = &out[..] else { panic!() };
        assert_eq!(rows.len(), 3);
        assert_eq!(you.map(|s| s.rank), Some(3));
    }

    #[test]
    fn friends_board_answers_after_a_while_without_lookups() {
        let now = Instant::now();
        let mut presence = Presence::new();
        let _ = presence.online(1, "did:plc:me", "me.test", now);
        let mut board = Board::new();
        let db = Db::memory();
        block_on(db.run(board.connect("did:plc:me", "me.test")));
        block_on(db.run(board.connect("did:plc:x", "x.test")));
        block_on(db.record("did:plc:x", &run(1, 100, 1, 1, 30.0), 1)).unwrap();
        let job = board.request(1, "did:plc:me", BoardKind::Friends, now).unwrap();
        let (out, jobs) = settle_db(&mut board, &db, &mut presence, vec![job], now);
        assert!(out.is_empty() && !jobs.is_empty());
        // the AppView never answers
        assert!(board.resolve(&presence, now + Duration::from_secs(2)).is_empty());
        let ready = board.tick(&presence, now + FRIENDS_WAIT);
        let (out, _) = settle_db(&mut board, &db, &mut presence, ready, now);
        assert!(matches!(&out[..], [Out { msg: ServerMsg::LeaderboardData { rows, you: None, .. }, .. }] if rows.is_empty()));
    }

    fn pg_url() -> Option<String> {
        std::env::var("FLIGHT_SIM_TEST_DATABASE_URL").ok().filter(|u| !u.is_empty())
    }

    fn test_schema() -> String {
        format!("test_{:08x}", rand::random::<u32>())
    }

    async fn drop_schema(url: &str, schema: &str) {
        let pool = PgPool::connect(url).await.unwrap();
        sqlx::query(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE")).execute(&pool).await.unwrap();
    }

    /// Against a real Postgres, in a throwaway schema:
    /// `FLIGHT_SIM_TEST_DATABASE_URL=postgres://… cargo test --no-default-features --features server --bin flight_sim_server postgres`
    #[test]
    fn postgres_store_and_import() {
        let Some(url) = pg_url() else {
            eprintln!("(skipped: no FLIGHT_SIM_TEST_DATABASE_URL)");
            return;
        };
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async {
            // the store itself
            let schema = test_schema();
            let db = Db::postgres(&url, Some(&schema)).await.unwrap();
            let result = std::panic::AssertUnwindSafe(check_store(&db));
            let ok = futures_util::FutureExt::catch_unwind(result).await;
            drop_schema(&url, &schema).await;
            ok.unwrap();

            // a SQLite board moves over once
            let path = temp_db("import");
            let src = Db::sqlite(&path).unwrap();
            check_store(&src).await;
            drop(src);
            let schema = test_schema();
            let db = Db::postgres(&url, Some(&schema)).await.unwrap();
            let imported = db.import_sqlite(&path).await;
            let again = db.import_sqlite(&path).await;
            let top = db.top(3).await;
            let standing = db.standing("did:plc:c").await;
            drop_schema(&url, &schema).await;
            let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
            assert_eq!(imported.unwrap(), Some(((3, 3, 4), (3, 3, 4))));
            assert_eq!(again.unwrap(), None, "only into an empty board");
            assert_eq!(top.unwrap().iter().map(|r| r.handle.as_str()).collect::<Vec<_>>(), vec!["b.test", "renamed.test", "c.test"]);
            assert_eq!(standing.unwrap(), Some(Standing { rank: 3, score: 550, wave: 2 }));
        });
    }

    /// Read-only: the row counts of the real (default-schema) leaderboard.
    /// `FLIGHT_SIM_TEST_DATABASE_URL=… cargo test … postgres_counts -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn postgres_counts() {
        let url = pg_url().expect("FLIGHT_SIM_TEST_DATABASE_URL");
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let pool = PgPool::connect(&url).await.unwrap();
            for t in ["players", "best", "runs"] {
                let n: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM public.{t}")).fetch_one(&pool).await.unwrap();
                println!("{t}: {n}");
            }
            let top: Vec<(String, i64)> =
                sqlx::query_as("SELECT did, score FROM public.best ORDER BY score DESC, at ASC LIMIT 3").fetch_all(&pool).await.unwrap();
            println!("top: {top:?}");
        });
    }
}
