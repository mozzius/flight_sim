//! The rooms plus presence: what a connection's messages do. Invites live
//! here because they need both (an online mutual, and a room to ask them to,
//! made on the fly if the inviter isn't in one), and so do join requests
//! (asking the host of the room a mutual is in to let us in: the requester
//! only learns the code by being admitted).
//!
//! Pure like the parts: every call returns a [`Step`], the messages to send
//! and the AppView lookups to run.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::presence::{Did, Job, JobResult, Presence};
use crate::protocol::*;
use crate::rooms::{self, ConnId, Out};

#[derive(Default, Debug)]
pub struct Step {
    pub out: Vec<Out>,
    pub jobs: Vec<Job>,
}

pub struct Hub {
    pub rooms: rooms::Server,
    pub presence: Presence,
    dids: HashMap<ConnId, Did>,
    asks: Vec<Ask>,
}

/// A join request waiting for the host.
#[derive(Debug)]
struct Ask {
    conn: ConnId,
    did: Did,
    /// The friend whose room they asked for (what `JoinDeclined` names).
    asked: Did,
    code: String,
    host: ConnId,
    at: Instant,
}

impl Hub {
    pub fn new(dev_codes: bool) -> Self {
        Self { rooms: rooms::Server::new(dev_codes), presence: Presence::new(), dids: HashMap::new(), asks: vec![] }
    }

    pub fn connect(&mut self, conn: ConnId, did: String, handle: String, now: Instant) -> Step {
        let mut step = Step { out: self.rooms.connect(conn, did.clone(), handle.clone()), jobs: vec![] };
        let (out, jobs) = self.presence.online(conn, &did, &handle, now);
        step.out.extend(out);
        step.jobs.extend(jobs);
        self.dids.insert(conn, did);
        step
    }

    pub fn disconnect(&mut self, conn: ConnId, now: Instant) -> Step {
        let mut step = Step { out: self.rooms.disconnect(conn, now), jobs: vec![] };
        if let Some(did) = self.dids.remove(&conn) {
            step.out.extend(self.presence.offline(conn, &did));
        }
        self.asks.retain(|a| a.conn != conn);
        step.out.extend(self.sync_statuses());
        step
    }

    pub fn handle(&mut self, conn: ConnId, msg: ClientMsg, now: Instant) -> Step {
        let mut step = Step::default();
        let roomy = matches!(msg, ClientMsg::Create { .. } | ClientMsg::Join { .. } | ClientMsg::Leave);
        match msg {
            ClientMsg::Invite { to, map } => self.invite(conn, &to, map, now, &mut step.out),
            ClientMsg::RequestJoin { to } => self.request_join(conn, &to, now, &mut step.out),
            ClientMsg::AnswerJoin { did, accept } => {
                self.answer_join(conn, &did, accept, now, &mut step.out);
                step.out.extend(self.sync_statuses());
            }
            other => step.out.extend(self.rooms.handle(conn, other, now)),
        }
        if roomy {
            step.out.extend(self.sync_statuses());
        }
        step
    }

    pub fn apply(&mut self, result: JobResult, now: Instant) -> Step {
        Step { out: self.presence.apply(result, now), jobs: vec![] }
    }

    pub fn tick(&mut self, now: Instant) -> Step {
        self.rooms.tick(now);
        let wait = Duration::from_secs_f32(JOIN_REQUEST_SECS);
        let mut out = vec![];
        self.asks.retain(|a| {
            let waiting = now.duration_since(a.at) < wait;
            if !waiting {
                out.push(Out { to: vec![a.conn], msg: ServerMsg::JoinDeclined(a.asked.clone()) });
            }
            waiting
        });
        Step { out, jobs: self.presence.tick(now) }
    }

    /// `conn` asks to join the room their mutual `to` is in: the request goes
    /// to that room's host (who must be a mutual too). Anything that doesn't
    /// go is a plain decline, so it never says where anyone is.
    fn request_join(&mut self, conn: ConnId, to: &str, now: Instant, out: &mut Vec<Out>) {
        let Some(from) = self.dids.get(&conn).cloned() else { return };
        let declined = Out { to: vec![conn], msg: ServerMsg::JoinDeclined(to.to_string()) };
        if !self.presence.online_mutuals(&from, to) {
            out.push(declined);
            return;
        }
        let room = self.presence.conns(to).iter().find_map(|c| self.rooms.room_of(*c).map(|(code, full)| (code.to_string(), full)));
        let Some((code, false)) = room else {
            out.push(declined);
            return;
        };
        let host = self.rooms.host_conn(&code);
        let host_did = host.and_then(|h| self.dids.get(&h)).cloned();
        let (Some(host), Some(host_did)) = (host, host_did) else {
            out.push(declined);
            return;
        };
        if self.rooms.has_player(&code, &from) || !self.presence.may_request(&from, &host_did, now) {
            out.push(declined);
            return;
        }
        let Some(me) = self.presence.friend(&from) else { return };
        self.asks.retain(|a| !(a.conn == conn && a.code == code));
        self.asks.push(Ask { conn, did: from, asked: to.to_string(), code, host, at: now });
        out.push(Out { to: vec![host], msg: ServerMsg::JoinRequest(me) });
    }

    /// The host says yes (the requester joins as if they had the code) or no.
    fn answer_join(&mut self, conn: ConnId, did: &str, accept: bool, now: Instant, out: &mut Vec<Out>) {
        let Some(i) = self.asks.iter().position(|a| {
            a.did == did && a.host == conn && self.rooms.host_conn(&a.code) == Some(conn)
        }) else {
            return;
        };
        let ask = self.asks.remove(i);
        if accept && self.dids.contains_key(&ask.conn) {
            self.rooms.join_code(ask.conn, &ask.code, now, out);
        } else {
            out.push(Out { to: vec![ask.conn], msg: ServerMsg::JoinDeclined(ask.asked) });
        }
    }

    fn invite(&mut self, conn: ConnId, to: &str, map: RoomMap, now: Instant, out: &mut Vec<Out>) {
        let Some(from) = self.dids.get(&conn).cloned() else { return };
        let failed = || Out { to: vec![conn], msg: ServerMsg::InviteFailed(to.to_string()) };
        // (offline and not a mutual fail alike: an invite never tells you who's online)
        if self.rooms.room_of(conn).is_some_and(|(_, full)| full) || !self.presence.may_invite(&from, to, now) {
            out.push(failed());
            return;
        }
        let code = match self.rooms.room_of(conn) {
            Some((code, _)) => code.to_string(),
            None => match self.rooms.create_room(conn, map, None, now, out) {
                Some(code) => code,
                None => return,
            },
        };
        out.extend(self.sync_statuses());
        let Some(me) = self.presence.friend(&from) else { return };
        out.push(Out { to: self.presence.conns(to), msg: ServerMsg::Invited { from: me, code } });
    }

    /// Friends see who's in a room (and whether it's full).
    fn sync_statuses(&mut self) -> Vec<Out> {
        let mut out = vec![];
        for did in self.presence.online_dids() {
            let rooms: Vec<bool> = self.presence.conns(&did).iter().filter_map(|c| self.rooms.room_of(*c).map(|(_, f)| f)).collect();
            let status = match rooms.first() {
                None => FriendStatus::Online,
                Some(true) => FriendStatus::RoomFull,
                Some(false) => FriendStatus::InRoom,
            };
            out.extend(self.presence.set_status(&did, status));
        }
        out
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
        assert!(matches!(sent_to(&out, 1)[..], [ServerMsg::Welcome { .. }, ServerMsg::Friends(l)] if l.is_empty()));
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
        assert!(hub.tick(later + Duration::from_secs(5)).out.is_empty());
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
}
