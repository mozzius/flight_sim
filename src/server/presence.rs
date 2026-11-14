//! Presence: who's online, and which online players are mutuals on Bluesky
//! (each follows the other).
//!
//! Every signed-in connection counts as online, in a room or not. When a
//! player comes online, the server asks the public AppView
//! (`app.bsky.graph.getRelationships`, up to 30 others a call) which of the
//! other online players are mutuals, and pushes each side the other as a
//! [`Friend`]. Only mutuals ever learn that someone is online: a client is
//! told about nobody else, and a failed invite looks the same whether the
//! target is offline or just not a mutual.
//!
//! Pure state like `rooms`: calls return messages ([`Out`]) and lookups to
//! run ([`Job`]); the server runs the jobs on a [`RelationshipSource`]
//! (rate-limited, see `main.rs`) and feeds the results back with
//! [`Presence::apply`]. Relationships are cached for [`PAIR_TTL`] and
//! looked up again when someone comes online after that (or by the
//! periodic sweep while both stay online).
//!
//! Across replicas: `users` is everyone online in the cluster. A user with
//! connections here is *local*; the others are *remote*, added from the
//! other replicas' views ([`Presence::online_remote`]) with the handle,
//! profile and status their home publishes. Each replica only tells its
//! own connections, so it only looks up pairs with a local user in them
//! (the lookup worker shares results through the cluster's cache, so a
//! pair is looked up once, not once per replica).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::protocol::{Friend, FriendStatus, ServerMsg};
use crate::rooms::{ConnId, Out};

pub type Did = String;

/// How long a looked-up relationship is trusted.
pub const PAIR_TTL: Duration = Duration::from_secs(5 * 60);
/// A failed lookup is tried again after this long.
const RETRY_AFTER: Duration = Duration::from_secs(60);
const PROFILE_TTL: Duration = Duration::from_secs(30 * 60);
/// Stale pairs among online players are looked up again this often.
const SWEEP_EVERY: Duration = Duration::from_secs(60);
/// AppView limits: `others` per getRelationships, `actors` per getProfiles.
pub const RELATIONSHIP_BATCH: usize = 30;
pub const PROFILE_BATCH: usize = 25;
/// One invite per target per this long.
pub const INVITE_GAP: Duration = Duration::from_secs(10);
/// And at most this many a minute from one player.
pub const INVITES_PER_MINUTE: usize = 10;
/// Join requests: one per host per this long, and a few a minute.
pub const REQUEST_GAP: Duration = Duration::from_secs(10);
pub const REQUESTS_PER_MINUTE: usize = 6;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Profile {
    pub did: Did,
    pub display_name: Option<String>,
    pub avatar: Option<String>,
}

/// A lookup for the [`RelationshipSource`].
#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    /// Which of `others` are `actor`'s mutuals.
    Mutuals { actor: Did, others: Vec<Did> },
    Profiles(Vec<Did>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum JobResult {
    Mutuals { actor: Did, others: Vec<Did>, result: Result<Vec<Did>, String> },
    Profiles { dids: Vec<Did>, result: Result<Vec<Profile>, String> },
}

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Where relationships come from: the public AppView in production
/// ([`AppView`], wrapped in [`DevAware`]), a mock in tests.
pub trait RelationshipSource: Send + Sync {
    /// Which of `others` (at most [`RELATIONSHIP_BATCH`]) follow `actor` and are followed back.
    fn mutuals(&self, actor: &str, others: &[Did]) -> BoxFuture<Result<Vec<Did>, String>>;
    /// Display names and avatars (at most [`PROFILE_BATCH`]).
    fn profiles(&self, dids: &[Did]) -> BoxFuture<Result<Vec<Profile>, String>>;
}

impl Job {
    pub async fn run(self, source: &dyn RelationshipSource) -> JobResult {
        match self {
            Job::Mutuals { actor, others } => {
                let result = source.mutuals(&actor, &others).await;
                JobResult::Mutuals { actor, others, result }
            }
            Job::Profiles(dids) => {
                let result = source.profiles(&dids).await;
                JobResult::Profiles { dids, result }
            }
        }
    }
}

struct User {
    /// Empty: a remote user (online on another replica).
    conns: Vec<ConnId>,
    /// The verified handle.
    handle: String,
    status: FriendStatus,
    /// The friends this user's clients have been told about.
    known: HashSet<Did>,
}

struct Pair {
    mutual: bool,
    at: Instant,
}

#[derive(Default)]
pub struct Presence {
    users: HashMap<Did, User>,
    /// Looked-up relationships, by (smaller DID, larger DID).
    pairs: HashMap<(Did, Did), Pair>,
    pending: HashSet<(Did, Did)>,
    profiles: HashMap<Did, (Profile, Instant)>,
    profiles_pending: HashSet<Did>,
    profile_queue: Vec<Did>,
    last_sweep: Option<Instant>,
}

fn key(a: &str, b: &str) -> (Did, Did) {
    if a < b { (a.to_string(), b.to_string()) } else { (b.to_string(), a.to_string()) }
}

impl Presence {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn online_count(&self) -> usize {
        self.users.len()
    }

    pub fn conns(&self, did: &str) -> Vec<ConnId> {
        self.users.get(did).map(|u| u.conns.clone()).unwrap_or_default()
    }

    /// The users with connections here.
    pub fn local_dids(&self) -> Vec<Did> {
        self.users.iter().filter(|(_, u)| !u.conns.is_empty()).map(|(d, _)| d.clone()).collect()
    }

    pub fn is_local(&self, did: &str) -> bool {
        self.users.get(did).is_some_and(|u| !u.conns.is_empty())
    }

    #[cfg(test)]
    pub fn is_online(&self, did: &str) -> bool {
        self.users.contains_key(did)
    }

    /// A connection signed in as `did`.
    pub fn online(&mut self, conn: ConnId, did: &str, handle: &str, now: Instant) -> (Vec<Out>, Vec<Job>) {
        let mut out = vec![];
        // (online on another replica until now: ours now, looked at afresh)
        if self.users.get(did).is_some_and(|u| u.conns.is_empty()) {
            self.users.remove(did);
        }
        if let Some(user) = self.users.get_mut(did) {
            // another tab: it gets the list the others already have
            user.conns.push(conn);
            let known: Vec<Did> = user.known.iter().cloned().collect();
            let list = known.iter().filter_map(|d| self.friend(d)).collect();
            out.push(Out { to: vec![conn], msg: ServerMsg::Friends(list) });
            return (out, vec![]);
        }
        self.users.insert(
            did.to_string(),
            User { conns: vec![conn], handle: handle.to_string(), status: FriendStatus::Online, known: HashSet::new() },
        );
        // cached mutuals show straight away (stale ones are checked again below)
        let mutuals: Vec<Did> = self
            .users
            .keys()
            .filter(|o| o.as_str() != did && self.pairs.get(&key(did, o)).is_some_and(|p| p.mutual))
            .cloned()
            .collect();
        let list = mutuals.iter().filter_map(|d| self.friend(d)).collect();
        out.push(Out { to: vec![conn], msg: ServerMsg::Friends(list) });
        if let Some(user) = self.users.get_mut(did) {
            user.known = mutuals.iter().cloned().collect();
        }
        if let Some(me) = self.friend(did) {
            for other in &mutuals {
                if let Some(u) = self.users.get_mut(other) {
                    u.known.insert(did.to_string());
                    out.push(Out { to: u.conns.clone(), msg: ServerMsg::FriendUpdate(me.clone()) });
                }
            }
        }
        let jobs = self.lookups_for(did, now);
        if !self.profile_fresh(did, now) && self.profiles_pending.insert(did.to_string()) {
            self.profile_queue.push(did.to_string());
        }
        (out, jobs)
    }

    /// A connection went away. `elsewhere`: the user is still online on
    /// another replica (as that replica tells it), so they stay, as a remote user.
    pub fn offline(&mut self, conn: ConnId, did: &str, elsewhere: Option<&Friend>) -> Vec<Out> {
        let mut out = vec![];
        let Some(user) = self.users.get_mut(did) else { return out };
        user.conns.retain(|c| *c != conn);
        if !user.conns.is_empty() {
            return out;
        }
        if let Some(f) = elsewhere {
            user.known.clear();
            return self.update_remote(f, Instant::now());
        }
        self.drop_user(did, &mut out);
        out
    }

    fn drop_user(&mut self, did: &str, out: &mut Vec<Out>) {
        self.users.remove(did);
        for u in self.users.values_mut() {
            if u.known.remove(did) {
                out.push(Out { to: u.conns.clone(), msg: ServerMsg::FriendGone(did.to_string()) });
            }
        }
    }

    /// Someone online on another replica (from its view): our users who are
    /// their mutuals are told, and unknown pairs with our users looked up.
    pub fn online_remote(&mut self, f: &Friend, now: Instant) -> (Vec<Out>, Vec<Job>) {
        let mut out = vec![];
        if self.users.contains_key(&f.did) {
            return (self.update_remote(f, now), vec![]);
        }
        self.users.insert(
            f.did.clone(),
            User { conns: vec![], handle: f.handle.clone(), status: f.status, known: HashSet::new() },
        );
        let profile = Profile { did: f.did.clone(), display_name: f.display_name.clone(), avatar: f.avatar.clone() };
        self.profiles.insert(f.did.clone(), (profile, now));
        let locals = self.local_dids();
        for l in &locals {
            if self.pairs.get(&key(l, &f.did)).is_some_and(|p| p.mutual) {
                self.link(l, &f.did, true, &mut out);
            }
        }
        let jobs = self.lookups_among(&f.did, &locals, now);
        (out, jobs)
    }

    /// A remote user's handle, profile or status changed (their friends here are told).
    pub fn update_remote(&mut self, f: &Friend, now: Instant) -> Vec<Out> {
        let mut out = vec![];
        let Some(user) = self.users.get_mut(&f.did).filter(|u| u.conns.is_empty()) else { return out };
        let profile = Profile { did: f.did.clone(), display_name: f.display_name.clone(), avatar: f.avatar.clone() };
        let changed = user.handle != f.handle
            || user.status != f.status
            || self.profiles.get(&f.did).is_none_or(|(p, _)| *p != profile);
        user.handle = f.handle.clone();
        user.status = f.status;
        self.profiles.insert(f.did.clone(), (profile, now));
        if changed {
            self.tell_friends(&f.did, &mut out);
        }
        out
    }

    /// A remote user went offline (or their replica did).
    pub fn offline_remote(&mut self, did: &str) -> Vec<Out> {
        let mut out = vec![];
        if self.users.get(did).is_some_and(|u| u.conns.is_empty()) {
            self.drop_user(did, &mut out);
        }
        out
    }

    /// The remote users.
    pub fn remote_dids(&self) -> Vec<Did> {
        self.users.iter().filter(|(_, u)| u.conns.is_empty()).map(|(d, _)| d.clone()).collect()
    }

    /// In a room, out of one, or the room filled up.
    pub fn set_status(&mut self, did: &str, status: FriendStatus) -> Vec<Out> {
        let mut out = vec![];
        let Some(user) = self.users.get_mut(did) else { return out };
        if user.status == status {
            return out;
        }
        user.status = status;
        self.tell_friends(did, &mut out);
        out
    }

    /// Both online, and `from` has been told `to` is their mutual.
    pub fn online_mutuals(&self, from: &str, to: &str) -> bool {
        from != to
            && self.users.contains_key(to)
            && self.users.get(from).is_some_and(|u| u.known.contains(to))
            && self.pairs.get(&key(from, to)).is_some_and(|p| p.mutual)
    }

    /// `did` as its friends see it.
    pub fn friend(&self, did: &str) -> Option<Friend> {
        let user = self.users.get(did)?;
        let profile = self.profiles.get(did).map(|(p, _)| p);
        Some(Friend {
            did: did.to_string(),
            handle: user.handle.clone(),
            display_name: profile.and_then(|p| p.display_name.clone()),
            avatar: profile.and_then(|p| p.avatar.clone()),
            status: user.status,
        })
    }

    /// A lookup came back.
    pub fn apply(&mut self, result: JobResult, now: Instant) -> Vec<Out> {
        let mut out = vec![];
        match result {
            JobResult::Mutuals { actor, others, result } => {
                let mutuals: Option<HashSet<Did>> = match result {
                    Ok(m) => Some(m.into_iter().collect()),
                    Err(e) => {
                        bevy::log::warn!("relationships for {actor}: {e}");
                        None
                    }
                };
                for other in others {
                    let k = key(&actor, &other);
                    self.pending.remove(&k);
                    let pair = match &mutuals {
                        Some(m) => Pair { mutual: m.contains(&other), at: now },
                        // keep what we knew; try again in a minute
                        None => Pair {
                            mutual: self.pairs.get(&k).is_some_and(|p| p.mutual),
                            at: now.checked_sub(PAIR_TTL - RETRY_AFTER).unwrap_or(now),
                        },
                    };
                    let mutual = pair.mutual;
                    self.pairs.insert(k, pair);
                    self.link(&actor, &other, mutual, &mut out);
                    self.link(&other, &actor, mutual, &mut out);
                }
            }
            JobResult::Profiles { dids, result } => {
                for d in &dids {
                    self.profiles_pending.remove(d);
                }
                match result {
                    Ok(profiles) => {
                        let mut seen = HashSet::new();
                        for p in profiles {
                            if !dids.contains(&p.did) {
                                continue;
                            }
                            seen.insert(p.did.clone());
                            let changed = self.profiles.get(&p.did).is_none_or(|(old, _)| *old != p);
                            let did = p.did.clone();
                            self.profiles.insert(did.clone(), (p, now));
                            if changed {
                                self.tell_friends(&did, &mut out);
                            }
                        }
                        // (no profile: remember that too, so it isn't asked again at once)
                        for d in dids.into_iter().filter(|d| !seen.contains(d)) {
                            self.profiles.insert(d.clone(), (Profile { did: d, ..Default::default() }, now));
                        }
                    }
                    Err(e) => bevy::log::warn!("profiles: {e}"),
                }
            }
        }
        out
    }

    /// Queued profile lookups, the periodic re-check of stale pairs, and
    /// forgetting old cache entries.
    pub fn tick(&mut self, now: Instant) -> Vec<Job> {
        let mut jobs = vec![];
        let queue = std::mem::take(&mut self.profile_queue);
        for chunk in queue.chunks(PROFILE_BATCH) {
            jobs.push(Job::Profiles(chunk.to_vec()));
        }
        if self.last_sweep.is_none_or(|t| now.duration_since(t) >= SWEEP_EVERY) {
            self.last_sweep = Some(now);
            let users = &self.users;
            self.pairs.retain(|(a, b), p| {
                now.duration_since(p.at) < PAIR_TTL * 2 || (users.contains_key(a) && users.contains_key(b))
            });
            self.profiles.retain(|d, (_, at)| now.duration_since(*at) < PROFILE_TTL || users.contains_key(d));
            let mut dids: Vec<Did> = self.users.keys().cloned().collect();
            dids.sort();
            for did in dids {
                jobs.extend(self.lookups_for(&did, now));
            }
        }
        jobs
    }

    /// Lookups for every online player whose relationship with `did` is
    /// unknown or stale (and not already being looked up). A remote user's
    /// only matter here with our own users.
    fn lookups_for(&mut self, did: &str, now: Instant) -> Vec<Job> {
        let online: Vec<Did> = if self.is_local(did) { self.users.keys().cloned().collect() } else { self.local_dids() };
        self.lookups_among(did, &online, now)
    }

    /// Lookups for whichever of `candidates` (online or not: the friends
    /// leaderboard asks about everyone on the board) have an unknown or
    /// stale relationship with `did`. The results land in the same cache.
    pub fn lookups_among(&mut self, did: &str, candidates: &[Did], now: Instant) -> Vec<Job> {
        let mut others: Vec<Did> = candidates
            .iter()
            .filter(|o| o.as_str() != did)
            .filter(|o| {
                let k = key(did, o);
                !self.pending.contains(&k) && self.pairs.get(&k).is_none_or(|p| now.duration_since(p.at) >= PAIR_TTL)
            })
            .cloned()
            .collect();
        others.sort();
        for o in &others {
            self.pending.insert(key(did, o));
        }
        others
            .chunks(RELATIONSHIP_BATCH)
            .map(|c| Job::Mutuals { actor: did.to_string(), others: c.to_vec() })
            .collect()
    }

    /// Whether `a` and `b` are mutuals, as last looked up (None: never).
    pub fn mutual(&self, a: &str, b: &str) -> Option<bool> {
        self.pairs.get(&key(a, b)).map(|p| p.mutual)
    }

    /// Whether a lookup of `did` with any of `others` is still out.
    pub fn looking_up(&self, did: &str, others: &[Did]) -> bool {
        others.iter().any(|o| self.pending.contains(&key(did, o)))
    }

    fn profile_fresh(&self, did: &str, now: Instant) -> bool {
        if !self.is_local(did) && self.users.contains_key(did) {
            return true;
        }
        self.profiles.get(did).is_some_and(|(_, at)| now.duration_since(*at) < PROFILE_TTL)
    }

    /// Tells `viewer` about `other` (or that it's gone) if that changed.
    fn link(&mut self, viewer: &str, other: &str, mutual: bool, out: &mut Vec<Out>) {
        let online = self.users.contains_key(other);
        let friend = self.friend(other);
        let Some(v) = self.users.get_mut(viewer) else { return };
        let knows = v.known.contains(other);
        match (mutual && online, knows) {
            (true, false) => {
                v.known.insert(other.to_string());
                if let Some(f) = friend {
                    out.push(Out { to: v.conns.clone(), msg: ServerMsg::FriendUpdate(f) });
                }
            }
            (false, true) => {
                v.known.remove(other);
                out.push(Out { to: v.conns.clone(), msg: ServerMsg::FriendGone(other.to_string()) });
            }
            _ => {}
        }
    }

    /// Everyone who knows `did` gets its new state.
    fn tell_friends(&self, did: &str, out: &mut Vec<Out>) {
        let Some(f) = self.friend(did) else { return };
        for u in self.users.values() {
            if u.known.contains(did) {
                out.push(Out { to: u.conns.clone(), msg: ServerMsg::FriendUpdate(f.clone()) });
            }
        }
    }
}

// ---- sources ----

/// Dev identities (`did:dev:<handle>`) don't exist on the AppView: they're
/// never anyone's mutual, unless `dev_mutuals` (`FLIGHT_SIM_DEV_MUTUALS=1`)
/// makes every dev user everyone else's. Real DIDs go to `inner`.
pub struct DevAware<S> {
    pub inner: S,
    pub dev_mutuals: bool,
}

fn is_dev(did: &str) -> bool {
    did.starts_with("did:dev:")
}

impl<S: RelationshipSource> RelationshipSource for DevAware<S> {
    fn mutuals(&self, actor: &str, others: &[Did]) -> BoxFuture<Result<Vec<Did>, String>> {
        if is_dev(actor) {
            let m = if self.dev_mutuals { others.iter().filter(|o| is_dev(o)).cloned().collect() } else { vec![] };
            return Box::pin(async move { Ok(m) });
        }
        let real: Vec<Did> = others.iter().filter(|o| !is_dev(o)).cloned().collect();
        if real.is_empty() {
            return Box::pin(async { Ok(vec![]) });
        }
        self.inner.mutuals(actor, &real)
    }

    fn profiles(&self, dids: &[Did]) -> BoxFuture<Result<Vec<Profile>, String>> {
        let real: Vec<Did> = dids.iter().filter(|d| !is_dev(d)).cloned().collect();
        if real.is_empty() {
            return Box::pin(async { Ok(vec![]) });
        }
        self.inner.profiles(&real)
    }
}

/// The public Bluesky AppView (no auth needed for these two).
pub struct AppView {
    client: reqwest::Client,
    base: String,
}

pub const PUBLIC_APPVIEW: &str = "https://public.api.bsky.app";

impl AppView {
    pub fn new(base: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("flight-sim-server")
            .build()
            .expect("http client");
        Self { client, base: base.trim_end_matches('/').to_string() }
    }

    /// `method` names the call in the metrics.
    fn get(&self, method: &'static str, url: String) -> BoxFuture<Result<Value, String>> {
        let client = self.client.clone();
        Box::pin(async move {
            let m = &crate::metrics::M;
            m.appview_calls.inc(&[method]);
            let t = std::time::Instant::now();
            let result = async {
                let r = client.get(&url).send().await.map_err(|e| e.to_string())?;
                if !r.status().is_success() {
                    return Err(format!("{}: {}", url.split('?').next().unwrap_or(""), r.status()));
                }
                serde_json::from_str(&r.text().await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())
            }
            .await;
            m.appview_latency.observe(t.elapsed());
            if result.is_err() {
                m.appview_errors.inc(&[method]);
            }
            result
        })
    }
}

/// DIDs are `[a-zA-Z0-9._:%-]`; anything else never reaches a URL.
fn url_safe_did(did: &str) -> Option<String> {
    let ok = did.starts_with("did:") && did.chars().all(|c| c.is_ascii_alphanumeric() || "._:%-".contains(c));
    ok.then(|| did.replace('%', "%25"))
}

impl RelationshipSource for AppView {
    fn mutuals(&self, actor: &str, others: &[Did]) -> BoxFuture<Result<Vec<Did>, String>> {
        let Some(actor) = url_safe_did(actor) else { return Box::pin(async { Err("bad did".to_string()) }) };
        let mut url = format!("{}/xrpc/app.bsky.graph.getRelationships?actor={actor}", self.base);
        for o in others.iter().take(RELATIONSHIP_BATCH).filter_map(|o| url_safe_did(o)) {
            url.push_str("&others=");
            url.push_str(&o);
        }
        let get = self.get("getRelationships", url);
        Box::pin(async move { Ok(parse_mutuals(&get.await?)) })
    }

    fn profiles(&self, dids: &[Did]) -> BoxFuture<Result<Vec<Profile>, String>> {
        let mut url = format!("{}/xrpc/app.bsky.actor.getProfiles?", self.base);
        for (i, d) in dids.iter().take(PROFILE_BATCH).filter_map(|d| url_safe_did(d)).enumerate() {
            if i > 0 {
                url.push('&');
            }
            url.push_str("actors=");
            url.push_str(&d);
        }
        let get = self.get("getProfiles", url);
        Box::pin(async move { Ok(parse_profiles(&get.await?)) })
    }
}

/// From a getRelationships response: the `others` who follow the actor and
/// whom the actor follows, with no block either way.
pub fn parse_mutuals(v: &Value) -> Vec<Did> {
    let Some(rels) = v.get("relationships").and_then(Value::as_array) else { return vec![] };
    rels.iter()
        .filter(|r| {
            let has = |k: &str| r.get(k).and_then(Value::as_str).is_some_and(|s| !s.is_empty());
            has("following")
                && has("followedBy")
                && !["blocking", "blockedBy", "blockingByList", "blockedByList"].iter().any(|k| has(k))
        })
        .filter_map(|r| r.get("did").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// From a getProfiles response: display names and avatars (the small
/// thumbnail variant of the avatar URL).
pub fn parse_profiles(v: &Value) -> Vec<Profile> {
    let Some(profiles) = v.get("profiles").and_then(Value::as_array) else { return vec![] };
    let s = |p: &Value, k: &str| p.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    profiles
        .iter()
        .filter_map(|p| {
            Some(Profile {
                did: s(p, "did")?,
                display_name: s(p, "displayName"),
                avatar: s(p, "avatar").map(|a| a.replace("/img/avatar/", "/img/avatar_thumbnail/")),
            })
        })
        .collect()
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A fake AppView: a follow graph, and a count of the calls made.
    #[derive(Default)]
    pub struct MockAppView {
        pub follows: HashSet<(Did, Did)>,
        pub calls: Mutex<Vec<Job>>,
        pub fail: bool,
    }

    impl MockAppView {
        pub fn follow(&mut self, a: &str, b: &str) {
            self.follows.insert((a.into(), b.into()));
        }
        pub fn mutual(&mut self, a: &str, b: &str) {
            self.follow(a, b);
            self.follow(b, a);
        }
    }

    impl RelationshipSource for MockAppView {
        fn mutuals(&self, actor: &str, others: &[Did]) -> BoxFuture<Result<Vec<Did>, String>> {
            self.calls.lock().unwrap().push(Job::Mutuals { actor: actor.into(), others: others.to_vec() });
            let r = if self.fail {
                Err("down".to_string())
            } else {
                let f = |a: &str, b: &str| self.follows.contains(&(a.to_string(), b.to_string()));
                Ok(others.iter().filter(|o| f(actor, o) && f(o, actor)).cloned().collect())
            };
            Box::pin(async move { r })
        }
        fn profiles(&self, dids: &[Did]) -> BoxFuture<Result<Vec<Profile>, String>> {
            self.calls.lock().unwrap().push(Job::Profiles(dids.to_vec()));
            let r = dids.iter().map(|d| Profile { did: d.clone(), display_name: Some(d.to_uppercase()), avatar: None }).collect();
            Box::pin(async move { Ok(r) })
        }
    }

    /// Runs jobs (and the lookups their results lead to) until none are left.
    pub fn settle(p: &mut Presence, source: &dyn RelationshipSource, mut jobs: Vec<Job>, now: Instant) -> Vec<Out> {
        let mut out = vec![];
        loop {
            jobs.extend(p.tick(now));
            if jobs.is_empty() {
                return out;
            }
            for job in std::mem::take(&mut jobs) {
                let result = bevy::tasks::block_on(job.run(source));
                out.extend(p.apply(result, now));
            }
        }
    }

    fn friends_of(out: &[Out], conn: ConnId) -> Vec<String> {
        let mut known: Vec<String> = vec![];
        for o in out.iter().filter(|o| o.to.contains(&conn)) {
            match &o.msg {
                ServerMsg::Friends(list) => known = list.iter().map(|f| f.handle.clone()).collect(),
                ServerMsg::FriendUpdate(f) => {
                    if !known.contains(&f.handle) {
                        known.push(f.handle.clone());
                    }
                }
                ServerMsg::FriendGone(did) => known.retain(|h| !did.ends_with(h.as_str())),
                _ => {}
            }
        }
        known.sort();
        known
    }

    fn did(h: &str) -> String {
        format!("did:plc:{h}")
    }

    #[test]
    fn only_online_mutuals_are_told() {
        let now = Instant::now();
        let mut graph = MockAppView::default();
        graph.mutual(&did("alice"), &did("bob"));
        // carol follows alice, alice doesn't follow back
        graph.follow(&did("carol"), &did("alice"));
        let mut p = Presence::new();
        let mut out = vec![];
        for (conn, h) in [(1, "alice"), (2, "bob"), (3, "carol")] {
            let (o, jobs) = p.online(conn, &did(h), h, now);
            out.extend(o);
            out.extend(settle(&mut p, &graph, jobs, now));
        }
        assert_eq!(friends_of(&out, 1), vec!["bob"]);
        assert_eq!(friends_of(&out, 2), vec!["alice"]);
        assert!(friends_of(&out, 3).is_empty());
        // carol never hears about anyone
        assert!(out.iter().filter(|o| o.to.contains(&3)).all(|o| matches!(&o.msg, ServerMsg::Friends(l) if l.is_empty())));
        // profiles arrived: display names on the updates
        assert!(p.friend(&did("bob")).unwrap().display_name.is_some());

        // bob goes: alice is told, carol isn't
        let out = p.offline(2, &did("bob"), None);
        assert_eq!(out, vec![Out { to: vec![1], msg: ServerMsg::FriendGone(did("bob")) }]);
    }

    #[test]
    fn lookups_are_batched_cached_and_retried() {
        let now = Instant::now();
        let mut graph = MockAppView::default();
        let mut p = Presence::new();
        for i in 0..40 {
            let d = did(&format!("u{i:02}"));
            graph.mutual(&did("me"), &d);
            let (_, jobs) = p.online(100 + i, &d, &format!("u{i:02}"), now);
            settle(&mut p, &graph, jobs, now);
        }
        graph.calls.lock().unwrap().clear();
        let (_, jobs) = p.online(1, &did("me"), "me", now);
        let out = settle(&mut p, &graph, jobs, now);
        let calls = graph.calls.lock().unwrap().clone();
        let lookups: Vec<usize> = calls
            .iter()
            .filter_map(|c| match c {
                Job::Mutuals { others, .. } => Some(others.len()),
                _ => None,
            })
            .collect();
        // 40 others: two calls (30 + 10)
        assert_eq!(lookups, vec![30, 10]);
        assert_eq!(friends_of(&out, 1).len(), 40);

        // back within the TTL: no new lookups, the cached list at once
        p.offline(1, &did("me"), None);
        graph.calls.lock().unwrap().clear();
        let (out, jobs) = p.online(2, &did("me"), "me", now + Duration::from_secs(60));
        assert!(jobs.iter().all(|j| matches!(j, Job::Profiles(_))), "{jobs:?}");
        assert!(matches!(&out[0].msg, ServerMsg::Friends(l) if l.len() == 40));

        // after it: looked up again
        p.offline(2, &did("me"), None);
        let (_, jobs) = p.online(3, &did("me"), "me", now + PAIR_TTL + Duration::from_secs(1));
        assert_eq!(jobs.iter().filter(|j| matches!(j, Job::Mutuals { .. })).count(), 2);

        // a failed lookup is retried by the sweep, not straight away
        let mut p = Presence::new();
        let mut down = MockAppView { fail: true, ..Default::default() };
        down.mutual(&did("a"), &did("b"));
        p.online(1, &did("a"), "a", now);
        let (_, jobs) = p.online(2, &did("b"), "b", now);
        let out = settle(&mut p, &down, jobs, now);
        assert!(friends_of(&out, 1).is_empty());
        let later = now + RETRY_AFTER + SWEEP_EVERY;
        let jobs = p.tick(later);
        assert_eq!(jobs.iter().filter(|j| matches!(j, Job::Mutuals { .. })).count(), 1);
        down.fail = false;
        let mut out = vec![];
        for job in jobs {
            out.extend(p.apply(bevy::tasks::block_on(job.run(&down)), later));
        }
        assert_eq!(friends_of(&out, 1), vec!["b"]);
    }

    #[test]
    fn invites_need_an_online_mutual() {
        let now = Instant::now();
        let mut graph = MockAppView::default();
        graph.mutual(&did("a"), &did("b"));
        let mut p = Presence::new();
        for (conn, h) in [(1, "a"), (2, "b"), (3, "c")] {
            let (_, jobs) = p.online(conn, &did(h), h, now);
            settle(&mut p, &graph, jobs, now);
        }
        assert!(!p.online_mutuals(&did("a"), &did("c")), "not a mutual");
        assert!(!p.online_mutuals(&did("a"), &did("zed")), "not online");
        assert!(p.online_mutuals(&did("a"), &did("b")));
        p.offline(2, &did("b"), None);
        assert!(!p.online_mutuals(&did("a"), &did("b")));
    }

    #[test]
    fn remote_users_reach_local_friends_only() {
        let now = Instant::now();
        let mut graph = MockAppView::default();
        graph.mutual(&did("a"), &did("r"));
        let mut p = Presence::new();
        let (_, jobs) = p.online(1, &did("a"), "a", now);
        settle(&mut p, &graph, jobs, now);
        let r = Friend { did: did("r"), handle: "r".into(), display_name: Some("Arr".into()), avatar: None, status: FriendStatus::InRoom };
        let (mut out, jobs) = p.online_remote(&r, now);
        // one lookup, r against our only local user (never a profile lookup: it came with the view)
        assert_eq!(jobs, vec![Job::Mutuals { actor: did("r"), others: vec![did("a")] }]);
        out.extend(settle(&mut p, &graph, jobs, now));
        assert!(out.iter().any(|o| o.to == vec![1] && matches!(&o.msg, ServerMsg::FriendUpdate(f) if f.handle == "r" && f.status == FriendStatus::InRoom)));
        assert_eq!(p.online_count(), 2);
        assert_eq!(p.friend(&did("r")).unwrap().display_name.as_deref(), Some("Arr"));
        // a status change from the view
        let out = p.update_remote(&Friend { status: FriendStatus::Online, ..r.clone() }, now);
        assert!(matches!(&out[..], [Out { msg: ServerMsg::FriendUpdate(f), .. }] if f.status == FriendStatus::Online));
        // they connect here too, then leave here while still online there: no FriendGone
        p.online(2, &did("r"), "r", now);
        assert!(p.is_local(&did("r")));
        let out = p.offline(2, &did("r"), Some(&r));
        assert!(!out.iter().any(|o| matches!(o.msg, ServerMsg::FriendGone(_))), "{out:?}");
        assert!(!p.is_local(&did("r")) && p.is_online(&did("r")));
        // their replica drops them: gone
        let out = p.offline_remote(&did("r"));
        assert_eq!(out, vec![Out { to: vec![1], msg: ServerMsg::FriendGone(did("r")) }]);
        assert_eq!(p.online_count(), 1);
    }

    #[test]
    fn statuses_reach_friends_only() {
        let now = Instant::now();
        let mut graph = MockAppView::default();
        graph.mutual(&did("a"), &did("b"));
        let mut p = Presence::new();
        for (conn, h) in [(1, "a"), (2, "b"), (3, "c")] {
            let (_, jobs) = p.online(conn, &did(h), h, now);
            settle(&mut p, &graph, jobs, now);
        }
        let out = p.set_status(&did("a"), FriendStatus::InRoom);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to, vec![2]);
        assert!(matches!(&out[0].msg, ServerMsg::FriendUpdate(f) if f.status == FriendStatus::InRoom));
        assert!(p.set_status(&did("a"), FriendStatus::InRoom).is_empty());
    }

    #[test]
    fn dev_users_are_mutuals_only_when_asked() {
        let graph = MockAppView::default();
        let on = DevAware { inner: graph, dev_mutuals: true };
        let others = vec!["did:dev:bob".to_string(), "did:plc:real".to_string()];
        let m = bevy::tasks::block_on(on.mutuals("did:dev:alice", &others)).unwrap();
        assert_eq!(m, vec!["did:dev:bob".to_string()]);
        let off = DevAware { inner: MockAppView::default(), dev_mutuals: false };
        assert!(bevy::tasks::block_on(off.mutuals("did:dev:alice", &others)).unwrap().is_empty());
        // real users never get dev users from the AppView
        let m = bevy::tasks::block_on(on.mutuals("did:plc:real", &["did:dev:bob".to_string()])).unwrap();
        assert!(m.is_empty());
        assert!(on.inner.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn parses_appview_responses() {
        let v: Value = serde_json::from_str(
            r#"{"actor":"did:plc:a","relationships":[
                {"$type":"app.bsky.graph.defs#relationship","did":"did:plc:b","following":"at://x","followedBy":"at://y"},
                {"$type":"app.bsky.graph.defs#relationship","did":"did:plc:c","followedBy":"at://y"},
                {"$type":"app.bsky.graph.defs#relationship","did":"did:plc:d","following":"at://x","followedBy":"at://y","blocking":"at://z"},
                {"$type":"app.bsky.graph.defs#notFoundActor","actor":"did:plc:e","notFound":true}
            ]}"#,
        )
        .unwrap();
        assert_eq!(parse_mutuals(&v), vec!["did:plc:b".to_string()]);
        let v: Value = serde_json::from_str(
            r#"{"profiles":[{"did":"did:plc:b","handle":"b.test","displayName":"Bee",
                "avatar":"https://cdn.bsky.app/img/avatar/plain/did:plc:b/bafk"},{"did":"did:plc:c","handle":"c.test","displayName":""}]}"#,
        )
        .unwrap();
        let p = parse_profiles(&v);
        assert_eq!(p[0].display_name.as_deref(), Some("Bee"));
        assert_eq!(p[0].avatar.as_deref(), Some("https://cdn.bsky.app/img/avatar_thumbnail/plain/did:plc:b/bafk"));
        assert_eq!(p[1].display_name, None);
        assert_eq!(url_safe_did("did:web:x%3A8080").as_deref(), Some("did:web:x%253A8080"));
        assert_eq!(url_safe_did("did:plc:a&others=b"), None);
    }

    /// The real AppView: bsky.app and atproto.com follow each other (checked
    /// 2026-11-13; jay.bsky.team follows bsky.app, not back).
    /// `cargo test --no-default-features --features server --bin flight_sim_server real_appview -- --ignored`
    #[test]
    #[ignore]
    fn real_appview_mutuals() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let app = AppView::new(PUBLIC_APPVIEW);
        let bsky = "did:plc:z72i7hdynmk6r22z27h6tvur";
        let atproto = "did:plc:ewvi7nxzyoun6zhxrhs64oiz";
        let jay = "did:plc:oky5czdrnfjpqslsw2a5iclo";
        let m = rt.block_on(app.mutuals(bsky, &[atproto.into(), jay.into(), "did:plc:aaaaaaaaaaaaaaaaaaaaaaaa".into()])).unwrap();
        assert_eq!(m, vec![atproto.to_string()]);
        let p = rt.block_on(app.profiles(&[bsky.into()])).unwrap();
        assert_eq!(p[0].display_name.as_deref(), Some("Bluesky"));
    }
}
