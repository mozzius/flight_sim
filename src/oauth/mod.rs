//! Native sign-in: an atproto OAuth client (the web build's is `web/auth.js`).
//!
//! A public "native" client whose metadata is
//! `https://flight.mozzius.dev/oauth-native-client-metadata.json`
//! (`web/oauth-native-client-metadata.json`). The flow:
//!
//! 1. Resolve what was typed: a handle (via the AppView's `resolveHandle`),
//!    a DID, a server URL, or nothing (the bsky.social entryway, which asks
//!    for the account itself) → the PDS → its authorization server
//!    (`/.well-known/oauth-protected-resource`, then
//!    `/.well-known/oauth-authorization-server`).
//! 2. Listen on `127.0.0.1:<any free port>` and push the authorization
//!    request (PAR) with PKCE (S256), a fresh ES256 DPoP key and
//!    `redirect_uri=http://127.0.0.1:<port>/callback`. The metadata registers
//!    `http://127.0.0.1/callback` without a port: RFC 8252 §7.3 has the
//!    server accept any port for a loopback redirect, and the atproto PDS
//!    does (`compareRedirectUri` in `@atproto/oauth-provider`).
//! 3. Open the system browser on the authorization page
//!    (`FLIGHT_SIM_OAUTH_PRINT_URL=1` logs the URL instead).
//! 4. The browser comes back to the listener with `code`, `state` and `iss`;
//!    check them, swap the code for DPoP-bound tokens, and check that the
//!    token's `sub` really lives on a PDS this authorization server
//!    protects. The page answers in one line.
//!
//! DPoP nonces (RFC 9449 §8 / §9) are kept per server and retried once on
//! `use_dpop_nonce`; access tokens are refreshed when they're about to
//! expire or the PDS turns them down. The session lives in `store`.
//!
//! Everything runs on one background tokio thread; the game polls
//! [`status`] and [`user`].

mod dpop;
mod resolve;
mod store;

use std::{
    future::Future,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bevy::log::{info, warn};
use reqwest::{Client, StatusCode, Url};
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

use dpop::DpopKey;
use resolve::{AuthServer, Input};
pub use store::Session;

pub const CLIENT_ID: &str = "https://flight.mozzius.dev/oauth-native-client-metadata.json";
/// Registered without a port (any port is accepted for loopback redirects).
pub const REDIRECT_URI: &str = "http://127.0.0.1/callback";
const CALLBACK_PATH: &str = "/callback";
/// The same as the web's: just enough to mint the game server's token.
pub const SCOPE: &str = "atproto rpc:dev.mozzius.flight.join?aud=*";
const SERVER_DID: &str = "did:web:flight.mozzius.dev";
const JOIN: &str = "dev.mozzius.flight.join";
/// How long the browser has to come back.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);
/// How long an error shows in the prompt.
const ERROR_SECS: f32 = 5.0;

/// Where a sign-in is at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Idle,
    /// The browser is open on the authorization page.
    Waiting,
    /// The last attempt failed (one short line).
    Failed(String),
}

enum Flow {
    Idle,
    Waiting { id: u64, cancel: Option<oneshot::Sender<()>> },
    Failed(String, Instant),
}

struct State {
    session: Option<Session>,
    flow: Flow,
    next_id: u64,
}

fn state() -> std::sync::MutexGuard<'static, State> {
    static STATE: Mutex<State> = Mutex::new(State { session: None, flow: Flow::Idle, next_id: 0 });
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("sign-in")
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

fn http() -> &'static Client {
    static HTTP: OnceLock<Client> = OnceLock::new();
    HTTP.get_or_init(|| {
        Client::builder()
            .user_agent(concat!("flight-sim/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(20))
            // (metadata and token endpoints must answer directly)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("http client")
    })
}

/// Runs `f` on the sign-in thread.
pub fn spawn<F: Future<Output = ()> + Send + 'static>(f: F) {
    runtime().spawn(f);
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Restores the stored session (and refreshes its profile in the background).
pub fn init() {
    let Some(session) = store::load() else { return };
    info!("sign-in: signed in as @{}", session.handle);
    let did = session.did.clone();
    state().session = Some(session);
    spawn(async move {
        if let Some(p) = profile(&did).await {
            update(&did, |s| {
                (s.handle, s.display_name, s.avatar) = (p.handle.clone(), p.display_name.clone(), p.avatar.clone())
            });
        }
    });
}

/// The signed-in session, if any.
pub fn user() -> Option<Session> {
    state().session.clone()
}

pub fn status() -> Status {
    let mut s = state();
    match &s.flow {
        Flow::Idle => Status::Idle,
        Flow::Waiting { .. } => Status::Waiting,
        Flow::Failed(_, at) if at.elapsed().as_secs_f32() > ERROR_SECS => {
            s.flow = Flow::Idle;
            Status::Idle
        }
        Flow::Failed(msg, _) => Status::Failed(msg.clone()),
    }
}

/// Starts signing in with what was typed (empty: the bsky.social entryway).
pub fn begin(typed: &str) {
    let input = match Input::parse(typed) {
        Ok(i) => i,
        Err(e) => {
            state().flow = Flow::Failed(e, Instant::now());
            return;
        }
    };
    let (tx, rx) = oneshot::channel();
    let id = {
        let mut s = state();
        if matches!(s.flow, Flow::Waiting { .. }) {
            return;
        }
        s.next_id += 1;
        s.flow = Flow::Waiting { id: s.next_id, cancel: Some(tx) };
        s.next_id
    };
    spawn(async move {
        let result = tokio::select! {
            r = sign_in(input) => r,
            _ = rx => Err(Fail::Cancelled),
        };
        let mut s = state();
        // (a cancelled or superseded attempt doesn't touch the prompt)
        if !matches!(s.flow, Flow::Waiting { id: current, .. } if current == id) {
            return;
        }
        s.flow = match result {
            Ok(session) => {
                info!("sign-in: signed in as @{} ({})", session.handle, session.did);
                store::save(&session);
                s.session = Some(session);
                Flow::Idle
            }
            Err(Fail::Cancelled) => Flow::Idle,
            Err(Fail::Show(msg, detail)) => {
                warn!("sign-in failed: {detail}");
                Flow::Failed(msg.to_string(), Instant::now())
            }
        };
    });
}

/// Gives up waiting for the browser.
pub fn cancel() {
    let mut s = state();
    if let Flow::Waiting { cancel, .. } = &mut s.flow
        && let Some(tx) = cancel.take()
    {
        let _ = tx.send(());
    }
    s.flow = Flow::Idle;
}

/// Forgets the session and revokes its tokens.
pub fn sign_out() {
    let Some(session) = state().session.take() else { return };
    store::delete();
    info!("sign-in: signed out");
    spawn(async move {
        let Some(endpoint) = session.revocation_endpoint.clone() else { return };
        let Ok(key) = DpopKey::from_secret(&session.dpop_key) else { return };
        let mut nonce = session.as_nonce.clone();
        for token in [session.refresh_token.as_deref(), Some(session.access_token.as_str())].into_iter().flatten() {
            let form = [("token", token), ("client_id", CLIENT_ID)];
            if let Err(e) = as_post(http(), &key, &endpoint, &form, &mut nonce).await {
                warn!("sign-in: couldn't revoke the token: {e}");
            }
        }
    });
}

/// A fresh service-auth JWT for the game server, minted by the PDS
/// (`com.atproto.server.getServiceAuth`, like `serviceToken()` in auth.js).
pub async fn service_token() -> Result<String, String> {
    runtime().spawn(service_token_inner()).await.map_err(|e| e.to_string())?
}

async fn service_token_inner() -> Result<String, String> {
    // one token operation at a time (refreshes rotate the refresh token)
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = LOCK.lock().await;
    let mut session = user().ok_or("signed out")?;
    let key = DpopKey::from_secret(&session.dpop_key)?;
    let mut refreshed = false;
    if session.expires_at - 60 < now() {
        refresh_or_sign_out(&mut session, &key).await?;
        refreshed = true;
    }
    let exp = (now() + 60).to_string();
    let mut url = Url::parse(&format!("{}/xrpc/com.atproto.server.getServiceAuth", session.pds)).map_err(|e| e.to_string())?;
    url.query_pairs_mut().append_pair("aud", SERVER_DID).append_pair("lxm", JOIN).append_pair("exp", &exp);
    loop {
        let (status, body) = pds_get(&mut session, &key, url.as_str()).await?;
        if status == StatusCode::UNAUTHORIZED && !refreshed {
            refresh_or_sign_out(&mut session, &key).await?;
            refreshed = true;
            continue;
        }
        let did = session.did.clone();
        let (nonce, access, refresh, expires) =
            (session.pds_nonce.clone(), session.access_token.clone(), session.refresh_token.clone(), session.expires_at);
        update(&did, |s| (s.pds_nonce, s.access_token, s.refresh_token, s.expires_at) = (nonce, access, refresh, expires));
        if !status.is_success() {
            return Err(format!("getServiceAuth: {status} {body}"));
        }
        return body["token"].as_str().map(str::to_string).ok_or_else(|| "no token".into());
    }
}

/// Applies `f` to the stored session if it's still `did`'s (a sign-out in
/// the meantime wins), and saves it.
fn update(did: &str, f: impl FnOnce(&mut Session)) {
    let mut s = state();
    if let Some(session) = s.session.as_mut().filter(|s| s.did == did) {
        f(session);
        store::save(session);
    }
}

async fn refresh_or_sign_out(session: &mut Session, key: &DpopKey) -> Result<(), String> {
    match refresh(session, key).await {
        Ok(()) => {
            let snapshot = session.clone();
            update(&session.did, |s| *s = snapshot);
            Ok(())
        }
        Err(Refresh::Revoked(e)) => {
            // the grant's gone (revoked, expired): sign out
            warn!("sign-in: session ended: {e}");
            let mut s = state();
            if s.session.as_ref().is_some_and(|s| s.did == session.did) {
                s.session = None;
                store::delete();
            }
            Err("signed out".into())
        }
        Err(Refresh::Failed(e)) => Err(e),
    }
}

enum Refresh {
    /// The authorization server refused the refresh token.
    Revoked(String),
    /// Anything else (offline...): try again later.
    Failed(String),
}

async fn refresh(session: &mut Session, key: &DpopKey) -> Result<(), Refresh> {
    let token = session.refresh_token.clone().ok_or_else(|| Refresh::Revoked("no refresh token".into()))?;
    let form = [("grant_type", "refresh_token"), ("refresh_token", token.as_str()), ("client_id", CLIENT_ID)];
    let endpoint = session.token_endpoint.clone();
    let (status, body) = as_post(http(), key, &endpoint, &form, &mut session.as_nonce).await.map_err(Refresh::Failed)?;
    if status == StatusCode::BAD_REQUEST || status == StatusCode::UNAUTHORIZED {
        return Err(Refresh::Revoked(format!("{status} {body}")));
    }
    let t = parse_tokens(status, &body).map_err(Refresh::Failed)?;
    if t.sub != session.did {
        return Err(Refresh::Revoked("refresh came back for another account".into()));
    }
    session.access_token = t.access_token;
    session.refresh_token = t.refresh_token.or(session.refresh_token.take());
    session.expires_at = t.expires_at;
    session.scope = t.scope;
    Ok(())
}

// ---- the authorization flow ----

enum Fail {
    Cancelled,
    /// (shown, logged)
    Show(&'static str, String),
}

fn show(msg: &'static str) -> impl FnOnce(String) -> Fail {
    move |detail| Fail::Show(msg, detail)
}

async fn sign_in(input: Input) -> Result<Session, Fail> {
    let client = http();
    // what was typed → the server to ask, a login hint and the DID we expect back
    let (server, hint, expect) = match input {
        Input::Entryway => (resolve::ENTRYWAY.to_string(), None, None),
        Input::Server(s) => (s, None, None),
        Input::Handle(h) => {
            let did = resolve::resolve_handle(client, &h).await.map_err(show("handle not found"))?;
            (resolve::pds_of(client, &did).await.map_err(show("account not found"))?, Some(h), Some(did))
        }
        Input::Did(d) => (resolve::pds_of(client, &d).await.map_err(show("account not found"))?, Some(d.clone()), Some(d)),
    };
    let auth = resolve::auth_server_for(client, &server).await.map_err(show("can't reach the server"))?;

    let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| Fail::Show("sign-in failed", e.to_string()))?;
    let port = listener.local_addr().map_err(|e| Fail::Show("sign-in failed", e.to_string()))?.port();
    let redirect_uri = redirect_uri(port);
    let key = DpopKey::generate();
    let (verifier, challenge) = dpop::pkce();
    let state_token = dpop::random_token(16);
    let mut as_nonce = None;

    let jkt = key.thumbprint();
    let mut form = vec![
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("response_mode", "query"),
        ("redirect_uri", redirect_uri.as_str()),
        ("scope", SCOPE),
        ("state", state_token.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("dpop_jkt", jkt.as_str()),
    ];
    if let Some(h) = &hint {
        form.push(("login_hint", h.as_str()));
    }
    let (status, body) =
        as_post(client, &key, &auth.par_endpoint, &form, &mut as_nonce).await.map_err(show("can't reach the server"))?;
    let request_uri = body["request_uri"]
        .as_str()
        .filter(|_| status.is_success())
        .ok_or_else(|| Fail::Show("sign-in refused", format!("PAR: {status} {body}")))?;
    let url = authorize_url(&auth, request_uri);

    if crate::environment::setting("FLIGHT_SIM_OAUTH_PRINT_URL").is_some_and(|v| v == "1") {
        info!("sign-in: authorization URL: {url}");
        println!("FLIGHT_SIM_OAUTH_URL={url}");
    } else if let Err(e) = open::that_detached(&url) {
        return Err(Fail::Show("couldn't open the browser", e.to_string()));
    }

    let (mut stream, params) = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_callback(&listener, &state_token))
        .await
        .map_err(|_| Fail::Show("timed out", "no callback".into()))?;
    let result = finish(client, &auth, &key, &params, &redirect_uri, &verifier, &mut as_nonce, expect.as_deref()).await;
    let page = if result.is_ok() { "Signed in. Return to the game." } else { "Sign-in failed. Return to the game." };
    respond(&mut stream, "200 OK", page).await;
    result
}

/// The registered redirect URI with the listener's port.
pub fn redirect_uri(port: u16) -> String {
    let mut url = Url::parse(REDIRECT_URI).expect("redirect URI");
    let _ = url.set_port(Some(port));
    url.to_string()
}

/// The authorization page for a pushed request.
fn authorize_url(auth: &AuthServer, request_uri: &str) -> String {
    let mut url = Url::parse(&auth.authorization_endpoint).expect("checked https URL");
    url.query_pairs_mut().append_pair("client_id", CLIENT_ID).append_pair("request_uri", request_uri);
    url.to_string()
}

#[allow(clippy::too_many_arguments)]
async fn finish(
    client: &Client,
    auth: &AuthServer,
    key: &DpopKey,
    params: &[(String, String)],
    redirect_uri: &str,
    verifier: &str,
    as_nonce: &mut Option<String>,
    expect: Option<&str>,
) -> Result<Session, Fail> {
    let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    // RFC 9207: the response names its issuer (bsky.social always does)
    if get("iss").is_some_and(|iss| iss != auth.issuer) || (get("iss").is_none() && auth.iss_parameter) {
        return Err(Fail::Show("sign-in failed", format!("wrong issuer {:?}", get("iss"))));
    }
    if let Some(e) = get("error") {
        let detail = format!("{e}: {}", get("error_description").unwrap_or_default());
        return Err(if e == "access_denied" { Fail::Show("sign-in cancelled", detail) } else { Fail::Show("sign-in refused", detail) });
    }
    let code = get("code").ok_or_else(|| Fail::Show("sign-in failed", "no code".into()))?;
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
        ("client_id", CLIENT_ID),
    ];
    let (status, body) = as_post(client, key, &auth.token_endpoint, &form, as_nonce).await.map_err(show("can't reach the server"))?;
    let t = parse_tokens(status, &body).map_err(show("sign-in refused"))?;
    if expect.is_some_and(|d| d != t.sub) {
        return Err(Fail::Show("signed in as someone else", format!("expected {expect:?}, got {}", t.sub)));
    }
    // the account must really be on a PDS this server speaks for
    let pds = resolve::pds_of(client, &t.sub).await.map_err(show("account not found"))?;
    let issuer = resolve::issuer_of(client, &pds).await.map_err(show("can't reach the server"))?;
    if issuer != auth.issuer {
        return Err(Fail::Show("sign-in failed", format!("{pds} is protected by {issuer}, not {}", auth.issuer)));
    }
    let p = profile(&t.sub).await;
    Ok(Session {
        handle: p.as_ref().map_or_else(|| t.sub.clone(), |p| p.handle.clone()),
        display_name: p.as_ref().and_then(|p| p.display_name.clone()),
        avatar: p.and_then(|p| p.avatar),
        did: t.sub,
        pds,
        issuer: auth.issuer.clone(),
        token_endpoint: auth.token_endpoint.clone(),
        revocation_endpoint: auth.revocation_endpoint.clone(),
        dpop_key: key.secret(),
        access_token: t.access_token,
        refresh_token: t.refresh_token,
        expires_at: t.expires_at,
        scope: t.scope,
        as_nonce: as_nonce.clone(),
        pds_nonce: None,
    })
}

struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: i64,
    scope: String,
    sub: String,
}

fn parse_tokens(status: StatusCode, body: &Value) -> Result<Tokens, String> {
    if !status.is_success() {
        return Err(format!("token: {status} {body}"));
    }
    let s = |k: &str| body[k].as_str().map(str::to_string);
    if !s("token_type").is_some_and(|t| t.eq_ignore_ascii_case("DPoP")) {
        return Err("not a DPoP token".into());
    }
    let scope = s("scope").unwrap_or_default();
    if !scope.split(' ').any(|x| x == "atproto") {
        return Err(format!("scope {scope:?} lacks atproto"));
    }
    let sub = s("sub").filter(|d| d.starts_with("did:")).ok_or("no sub")?;
    Ok(Tokens {
        access_token: s("access_token").ok_or("no access_token")?,
        refresh_token: s("refresh_token"),
        expires_at: now() + body["expires_in"].as_i64().unwrap_or(300),
        scope,
        sub,
    })
}

/// Waits for the browser's `GET /callback?...` with our `state`, answering
/// anything else (a favicon, a stale tab) with an error page.
async fn wait_for_callback(listener: &TcpListener, state_token: &str) -> (TcpStream, Vec<(String, String)>) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else { continue };
        let Some(target) = read_request_target(&mut stream).await else { continue };
        match callback_params(&target) {
            Some(params) if params.iter().any(|(k, v)| k == "state" && v == state_token) => return (stream, params),
            Some(_) => respond(&mut stream, "400 Bad Request", "This sign-in has expired. Return to the game.").await,
            None => respond(&mut stream, "404 Not Found", "Not found.").await,
        }
    }
}

/// The query of a `/callback` request target.
fn callback_params(target: &str) -> Option<Vec<(String, String)>> {
    let url = Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    (url.path() == CALLBACK_PATH).then(|| url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
}

async fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let read = async {
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 16 * 1024 {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        Some(())
    };
    tokio::time::timeout(Duration::from_secs(10), read).await.ok()??;
    let text = String::from_utf8_lossy(&buf);
    let mut parts = text.lines().next()?.split(' ');
    (parts.next()? == "GET").then_some(())?;
    parts.next().filter(|t| t.starts_with('/')).map(str::to_string)
}

async fn respond(stream: &mut TcpStream, status: &str, line: &str) {
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Flight Sim</title></head>\
         <body style=\"margin:0;height:100vh;display:grid;place-items:center;background:#0b0f14;color:#66ff8c;font:16px ui-monospace,monospace\">\
         <p>{line}</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

// ---- DPoP requests ----

fn dpop_nonce(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers.get("dpop-nonce").and_then(|v| v.to_str().ok()).map(str::to_string)
}

/// A form POST to the authorization server, with a DPoP proof (retried
/// once with the server's nonce).
async fn as_post(
    client: &Client,
    key: &DpopKey,
    url: &str,
    form: &[(&str, &str)],
    nonce: &mut Option<String>,
) -> Result<(StatusCode, Value), String> {
    for attempt in 0..2 {
        let proof = key.proof("POST", url, nonce.as_deref(), None, now());
        let r = client.post(url).header("DPoP", proof).form(form).send().await.map_err(|e| format!("{url}: {e}"))?;
        let status = r.status();
        if let Some(n) = dpop_nonce(r.headers()) {
            *nonce = Some(n);
        }
        let body: Value = r.bytes().await.ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
        if attempt == 0 && status == StatusCode::BAD_REQUEST && body["error"] == "use_dpop_nonce" {
            continue;
        }
        return Ok((status, body));
    }
    unreachable!()
}

/// A GET on the PDS with the access token (retried once with the PDS's nonce).
async fn pds_get(session: &mut Session, key: &DpopKey, url: &str) -> Result<(StatusCode, Value), String> {
    for attempt in 0..2 {
        let proof = key.proof("GET", url, session.pds_nonce.as_deref(), Some(&session.access_token), now());
        let r = http()
            .get(url)
            .header("Authorization", format!("DPoP {}", session.access_token))
            .header("DPoP", proof)
            .send()
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        let status = r.status();
        if let Some(n) = dpop_nonce(r.headers()) {
            session.pds_nonce = Some(n);
        }
        let wants_nonce = r
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("use_dpop_nonce"));
        let body: Value = r.bytes().await.ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
        if attempt == 0 && status == StatusCode::UNAUTHORIZED && (wants_nonce || body["error"] == "use_dpop_nonce") {
            continue;
        }
        return Ok((status, body));
    }
    unreachable!()
}

// ---- the profile ----

struct Profile {
    handle: String,
    display_name: Option<String>,
    avatar: Option<String>,
}

/// From the public AppView (unauthenticated, like the web's).
async fn profile(did: &str) -> Option<Profile> {
    let mut url = Url::parse(&format!("{}/xrpc/app.bsky.actor.getProfile", resolve::APPVIEW)).ok()?;
    url.query_pairs_mut().append_pair("actor", did);
    let r = http().get(url).send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&r.bytes().await.ok()?).ok()?;
    let s = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    Some(Profile { handle: s("handle").filter(|h| h != "handle.invalid")?, display_name: s("displayName"), avatar: s("avatar") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_file_matches_the_client() {
        let v: Value = serde_json::from_str(include_str!("../../web/oauth-native-client-metadata.json")).unwrap();
        assert_eq!(v["client_id"], CLIENT_ID);
        assert_eq!(v["application_type"], "native");
        assert_eq!(v["token_endpoint_auth_method"], "none");
        assert_eq!(v["dpop_bound_access_tokens"], true);
        assert_eq!(v["scope"], SCOPE);
        assert_eq!(v["redirect_uris"], serde_json::json!([REDIRECT_URI]));
        assert_eq!(v["grant_types"], serde_json::json!(["authorization_code", "refresh_token"]));
        assert_eq!(v["response_types"], serde_json::json!(["code"]));
        // client_uri must share the client_id's origin
        let id = Url::parse(CLIENT_ID).unwrap();
        assert_eq!(Url::parse(v["client_uri"].as_str().unwrap()).unwrap().origin(), id.origin());
        // the web client's scope is the same
        let web: Value = serde_json::from_str(include_str!("../../web/oauth-client-metadata.json")).unwrap();
        assert_eq!(web["scope"], SCOPE);
    }

    #[test]
    fn builds_redirect_and_authorize_urls() {
        assert_eq!(redirect_uri(53_123), "http://127.0.0.1:53123/callback");
        let auth = AuthServer {
            issuer: "https://bsky.social".into(),
            authorization_endpoint: "https://bsky.social/oauth/authorize".into(),
            token_endpoint: "https://bsky.social/oauth/token".into(),
            par_endpoint: "https://bsky.social/oauth/par".into(),
            revocation_endpoint: None,
            iss_parameter: true,
        };
        let url = Url::parse(&authorize_url(&auth, "urn:ietf:params:oauth:request_uri:req-abc")).unwrap();
        assert_eq!(url.path(), "/oauth/authorize");
        let q: Vec<(String, String)> = url.query_pairs().map(|(k, v)| (k.into(), v.into())).collect();
        assert_eq!(
            q,
            vec![
                ("client_id".into(), CLIENT_ID.into()),
                ("request_uri".into(), "urn:ietf:params:oauth:request_uri:req-abc".into())
            ]
        );
    }

    #[test]
    fn reads_the_callback() {
        let p = callback_params("/callback?code=c%2B1&state=s1&iss=https%3A%2F%2Fbsky.social").unwrap();
        assert_eq!(p[0], ("code".into(), "c+1".into()));
        assert_eq!(p[2], ("iss".into(), "https://bsky.social".into()));
        assert!(callback_params("/favicon.ico").is_none());
    }

    #[test]
    fn loopback_listener_waits_for_our_state() {
        runtime().block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let get = |path: &'static str| async move {
                let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
                s.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes()).await.unwrap();
                let mut out = String::new();
                s.read_to_string(&mut out).await.unwrap();
                out
            };
            let waiter = tokio::spawn(async move {
                let (mut stream, params) = wait_for_callback(&listener, "good").await;
                respond(&mut stream, "200 OK", "Signed in. Return to the game.").await;
                params
            });
            assert!(get("/favicon.ico").await.starts_with("HTTP/1.1 404"));
            assert!(get("/callback?state=bad&code=x").await.starts_with("HTTP/1.1 400"));
            let ok = get("/callback?state=good&code=c1&iss=https%3A%2F%2Fbsky.social").await;
            assert!(ok.starts_with("HTTP/1.1 200") && ok.contains("Return to the game."));
            let params = waiter.await.unwrap();
            assert!(params.contains(&("code".into(), "c1".into())));
        });
    }

    #[test]
    fn checks_token_responses() {
        let ok = serde_json::json!({
            "access_token": "a", "refresh_token": "r", "token_type": "DPoP", "expires_in": 3600,
            "scope": SCOPE, "sub": "did:plc:abc",
        });
        let t = parse_tokens(StatusCode::OK, &ok).unwrap();
        assert_eq!((t.sub.as_str(), t.refresh_token.as_deref()), ("did:plc:abc", Some("r")));
        assert!(t.expires_at > now() + 3000);
        let mut bearer = ok.clone();
        bearer["token_type"] = "Bearer".into();
        assert!(parse_tokens(StatusCode::OK, &bearer).is_err());
        let mut no_atproto = ok.clone();
        no_atproto["scope"] = "transition:generic".into();
        assert!(parse_tokens(StatusCode::OK, &no_atproto).is_err());
        assert!(parse_tokens(StatusCode::BAD_REQUEST, &ok).is_err());
    }

    /// The real thing up to the authorization page: PAR against bsky.social
    /// with the deployed metadata, then the page loads.
    /// `cargo test par_against_bsky_social -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn par_against_bsky_social() {
        runtime().block_on(async {
            let client = http();
            let auth = resolve::auth_server_for(client, resolve::ENTRYWAY).await.unwrap();
            let key = DpopKey::generate();
            let (_, challenge) = dpop::pkce();
            let state = dpop::random_token(16);
            let redirect = redirect_uri(54_321);
            let jkt = key.thumbprint();
            let form = [
                ("client_id", CLIENT_ID),
                ("response_type", "code"),
                ("response_mode", "query"),
                ("redirect_uri", redirect.as_str()),
                ("scope", SCOPE),
                ("state", state.as_str()),
                ("code_challenge", challenge.as_str()),
                ("code_challenge_method", "S256"),
                ("dpop_jkt", jkt.as_str()),
            ];
            let mut nonce = None;
            let (status, body) = as_post(client, &key, &auth.par_endpoint, &form, &mut nonce).await.unwrap();
            assert!(status.is_success(), "PAR: {status} {body}");
            let url = authorize_url(&auth, body["request_uri"].as_str().unwrap());
            println!("{url}");
            // (the page wants a browser's fetch metadata, like a top-level navigation)
            let page = client
                .get(&url)
                .header("accept", "text/html")
                .header("sec-fetch-site", "none")
                .header("sec-fetch-mode", "navigate")
                .header("sec-fetch-dest", "document")
                .header("sec-fetch-user", "?1")
                .send()
                .await
                .unwrap();
            let status = page.status();
            let text = page.text().await.unwrap();
            assert_eq!(status, StatusCode::OK, "{text}");
            assert!(!text.contains("invalid_client") && !text.contains("__errorData"), "{text}");
            assert!(text.contains("__authorizeData"), "{text}");
        });
    }
}
