//! Server-side verification of atproto service-auth JWTs (the token a
//! player's PDS mints via `com.atproto.server.getServiceAuth`, see
//! `web/auth.js`). Native only: the game server runs this, the web client
//! never does.
//!
//! A token is a compact JWS signed with the account's `#atproto` repo
//! signing key (ES256K for secp256k1, ES256 for P-256). We check the header,
//! `aud`, `lxm`, `exp` / `iat`, resolve the `iss` DID (did:plc via
//! plc.directory, did:web via `/.well-known/did.json`) and verify the
//! signature against the DID document's multikey. A failed signature is
//! retried once with a freshly fetched document, in case the key rotated.
//!
//! Dev escape hatch: with `FLIGHT_SIM_DEV_AUTH=1`, `dev:<handle>` is accepted
//! as `did:dev:<handle>` (what native clients send, see `identity.rs`).

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::Engine as _;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedUser {
    pub did: String,
    pub handle: Option<String>,
}

/// Verifies a service-auth JWT for `aud` (e.g. `did:web:flight.mozzius.dev`)
/// and method `lxm` (e.g. `dev.mozzius.flight.join`).
pub async fn verify_service_auth(token: &str, aud: &str, lxm: &str) -> Result<VerifiedUser, String> {
    if let Some(handle) = token.strip_prefix("dev:") {
        return dev_user(handle, std::env::var("FLIGHT_SIM_DEV_AUTH").is_ok_and(|v| v == "1"));
    }
    static RESOLVER: std::sync::OnceLock<HttpResolver> = std::sync::OnceLock::new();
    static CACHE: std::sync::OnceLock<DocCache> = std::sync::OnceLock::new();
    let resolver = RESOLVER.get_or_init(HttpResolver::new);
    let cache = CACHE.get_or_init(DocCache::default);
    verify_with(token, aud, lxm, unix_now(), resolver, cache).await
}

fn dev_user(handle: &str, allowed: bool) -> Result<VerifiedUser, String> {
    if !allowed {
        return Err("dev tokens need FLIGHT_SIM_DEV_AUTH=1".into());
    }
    let ok = !handle.is_empty()
        && handle.len() <= 64
        && handle.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_');
    if !ok {
        return Err("bad dev handle".into());
    }
    Ok(VerifiedUser { did: format!("did:dev:{handle}"), handle: Some(handle.to_string()) })
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Where DID documents and handles come from. Injectable for tests.
pub trait Resolver {
    /// The DID document for `did` (did:plc or did:web).
    async fn did_doc(&self, did: &str) -> Result<Value, String>;
    /// The DID a handle points at, if it resolves.
    async fn handle_did(&self, handle: &str) -> Option<String>;
}

/// A short-lived cache of DID documents.
#[derive(Default)]
pub struct DocCache(Mutex<HashMap<String, (Instant, Value)>>);

const DOC_TTL: Duration = Duration::from_secs(300);

impl DocCache {
    fn get(&self, did: &str) -> Option<Value> {
        let map = self.0.lock().ok()?;
        map.get(did).filter(|(at, _)| at.elapsed() < DOC_TTL).map(|(_, doc)| doc.clone())
    }
    fn put(&self, did: &str, doc: Value) {
        if let Ok(mut map) = self.0.lock() {
            map.retain(|_, (at, _)| at.elapsed() < DOC_TTL);
            map.insert(did.to_string(), (Instant::now(), doc));
        }
    }
}

/// Clock skew allowed on `exp` / `iat`.
const SKEW: i64 = 60;
/// PDSes mint at most an hour.
const MAX_LIFETIME: i64 = 3600;

pub async fn verify_with<R: Resolver>(
    token: &str,
    aud: &str,
    lxm: &str,
    now: i64,
    resolver: &R,
    cache: &DocCache,
) -> Result<VerifiedUser, String> {
    let jwt = Jwt::parse(token)?;
    let claims = &jwt.claims;
    let str_claim = |k: &str| claims.get(k).and_then(Value::as_str);
    let int_claim = |k: &str| claims.get(k).and_then(Value::as_i64);

    if str_claim("aud") != Some(aud) {
        return Err("wrong aud".into());
    }
    if str_claim("lxm") != Some(lxm) {
        return Err("wrong lxm".into());
    }
    let exp = int_claim("exp").ok_or("no exp")?;
    if exp + SKEW < now {
        return Err("expired".into());
    }
    if exp > now + MAX_LIFETIME + SKEW {
        return Err("exp too far ahead".into());
    }
    if let Some(iat) = int_claim("iat") {
        if iat > now + SKEW || iat > exp {
            return Err("bad iat".into());
        }
    }
    let iss = str_claim("iss").ok_or("no iss")?;
    // a service DID ref (did:x#svc) still signs with the account's key
    let did = iss.split('#').next().unwrap_or(iss);
    if !(did.starts_with("did:plc:") || did.starts_with("did:web:")) {
        return Err("unsupported iss".into());
    }

    // cached doc first; on a bad signature (or none cached) fetch fresh, once
    let mut last_err = String::new();
    let cached = cache.get(did);
    let attempts: &[bool] = if cached.is_some() { &[true, false] } else { &[false] };
    for &use_cache in attempts {
        let doc = if use_cache {
            cached.clone().ok_or("no cache")?
        } else {
            let doc = match resolver.did_doc(did).await {
                Ok(d) => d,
                // a flaky fetch gets one retry too
                Err(_) => resolver.did_doc(did).await?,
            };
            cache.put(did, doc.clone());
            doc
        };
        if doc.get("id").and_then(Value::as_str) != Some(did) {
            last_err = "did doc id mismatch".into();
            continue;
        }
        match signing_key(&doc, did).and_then(|key| key.verify(&jwt)) {
            Ok(()) => {
                let handle = verified_handle(&doc, did, resolver).await;
                return Ok(VerifiedUser { did: did.to_string(), handle });
            }
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// The handle from `alsoKnownAs`, only if it resolves back to the DID.
async fn verified_handle<R: Resolver>(doc: &Value, did: &str, resolver: &R) -> Option<String> {
    let handle = doc
        .get("alsoKnownAs")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .find_map(|u| u.strip_prefix("at://"))?
        .to_ascii_lowercase();
    (resolver.handle_did(&handle).await.as_deref() == Some(did)).then_some(handle)
}

struct Jwt {
    alg: Alg,
    signing_input: Vec<u8>,
    signature: Vec<u8>,
    claims: serde_json::Map<String, Value>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Alg {
    Es256k,
    Es256,
}

fn b64(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).map_err(|_| "bad base64".to_string())
}

impl Jwt {
    fn parse(token: &str) -> Result<Jwt, String> {
        let mut parts = token.split('.');
        let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return Err("not a jwt".into());
        };
        let header: Value = serde_json::from_slice(&b64(h)?).map_err(|_| "bad header")?;
        let alg = match header.get("alg").and_then(Value::as_str) {
            Some("ES256K") => Alg::Es256k,
            Some("ES256") => Alg::Es256,
            _ => return Err("unsupported alg".into()),
        };
        let claims = match serde_json::from_slice(&b64(p)?).map_err(|_| "bad payload")? {
            Value::Object(m) => m,
            _ => return Err("bad payload".into()),
        };
        Ok(Jwt { alg, signing_input: format!("{h}.{p}").into_bytes(), signature: b64(s)?, claims })
    }
}

enum Key {
    K256(k256::ecdsa::VerifyingKey),
    P256(p256::ecdsa::VerifyingKey),
}

impl Key {
    fn verify(&self, jwt: &Jwt) -> Result<(), String> {
        use k256::ecdsa::signature::Verifier as _;
        let bad = || "bad signature".to_string();
        match (self, jwt.alg) {
            (Key::K256(key), Alg::Es256k) => {
                let sig = k256::ecdsa::Signature::from_slice(&jwt.signature).map_err(|_| bad())?;
                // atproto only accepts low-S signatures
                if sig.normalize_s().is_some() {
                    return Err("high-S signature".into());
                }
                key.verify(&jwt.signing_input, &sig).map_err(|_| bad())
            }
            (Key::P256(key), Alg::Es256) => {
                let sig = p256::ecdsa::Signature::from_slice(&jwt.signature).map_err(|_| bad())?;
                if sig.normalize_s().is_some() {
                    return Err("high-S signature".into());
                }
                key.verify(&jwt.signing_input, &sig).map_err(|_| bad())
            }
            _ => Err("alg doesn't match key".into()),
        }
    }
}

/// The `#atproto` verification method's key.
fn signing_key(doc: &Value, did: &str) -> Result<Key, String> {
    let full = format!("{did}#atproto");
    let method = doc
        .get("verificationMethod")
        .and_then(Value::as_array)
        .and_then(|ms| {
            ms.iter().find(|m| matches!(m.get("id").and_then(Value::as_str), Some(id) if id == "#atproto" || id == full))
        })
        .ok_or("no #atproto key")?;
    let multibase = method.get("publicKeyMultibase").and_then(Value::as_str).ok_or("no publicKeyMultibase")?;
    parse_multikey(multibase)
}

/// A multikey: `z` + base58btc(multicodec varint + compressed SEC1 point).
fn parse_multikey(s: &str) -> Result<Key, String> {
    let b58 = s.strip_prefix('z').ok_or("not base58btc multibase")?;
    let bytes = bs58::decode(b58).into_vec().map_err(|_| "bad base58")?;
    let bad = |_| "bad public key".to_string();
    match bytes.as_slice() {
        // secp256k1-pub (0xe7)
        [0xe7, 0x01, key @ ..] => k256::ecdsa::VerifyingKey::from_sec1_bytes(key).map(Key::K256).map_err(bad),
        // p256-pub (0x1200)
        [0x80, 0x24, key @ ..] => p256::ecdsa::VerifyingKey::from_sec1_bytes(key).map(Key::P256).map_err(bad),
        _ => Err("unsupported key type".into()),
    }
}

/// Fetches DID docs from plc.directory / did:web hosts, and resolves
/// handles via `/.well-known/atproto-did` (then the public AppView, which
/// also covers DNS TXT handles).
pub struct HttpResolver {
    client: reqwest::Client,
}

impl HttpResolver {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .user_agent("flight-sim-server")
            .build()
            .expect("http client");
        Self { client }
    }

    async fn get_text(&self, url: &str) -> Result<String, String> {
        let r = self.client.get(url).send().await.map_err(|e| e.to_string())?;
        if !r.status().is_success() {
            return Err(format!("{url}: {}", r.status()));
        }
        r.text().await.map_err(|e| e.to_string())
    }
}

/// `did:web:host` (with `%3A` for a port) → its did.json URL. atproto only
/// allows host-level did:web, no paths.
fn did_web_url(did: &str) -> Result<String, String> {
    let host = did.strip_prefix("did:web:").ok_or("not did:web")?.replace("%3A", ":");
    if host.is_empty() || host.contains('/') || host.matches(':').count() > 1 {
        return Err("bad did:web".into());
    }
    let scheme = if host.starts_with("localhost") { "http" } else { "https" };
    Ok(format!("{scheme}://{host}/.well-known/did.json"))
}

impl Resolver for HttpResolver {
    async fn did_doc(&self, did: &str) -> Result<Value, String> {
        let url = if did.starts_with("did:plc:") {
            if !did[8..].chars().all(|c| c.is_ascii_alphanumeric()) {
                return Err("bad did:plc".into());
            }
            format!("https://plc.directory/{did}")
        } else {
            did_web_url(did)?
        };
        serde_json::from_str(&self.get_text(&url).await?).map_err(|e| e.to_string())
    }

    async fn handle_did(&self, handle: &str) -> Option<String> {
        if !handle.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') || !handle.contains('.') {
            return None;
        }
        if let Ok(t) = self.get_text(&format!("https://{handle}/.well-known/atproto-did")).await {
            let t = t.trim();
            if t.starts_with("did:") {
                return Some(t.to_string());
            }
        }
        let url = format!("https://public.api.bsky.app/xrpc/com.atproto.identity.resolveHandle?handle={handle}");
        let v: Value = serde_json::from_str(&self.get_text(&url).await.ok()?).ok()?;
        v.get("did").and_then(Value::as_str).map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::{SigningKey, signature::Signer as _};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const AUD: &str = "did:web:flight.mozzius.dev";
    const LXM: &str = "dev.mozzius.flight.join";
    const DID: &str = "did:plc:testtesttesttesttesttest";
    const NOW: i64 = 1_800_000_000;

    fn k256_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32].into()).unwrap()
    }

    fn multikey_k256(key: &SigningKey) -> String {
        let point = key.verifying_key().to_encoded_point(true);
        let mut bytes = vec![0xe7, 0x01];
        bytes.extend_from_slice(point.as_bytes());
        format!("z{}", bs58::encode(bytes).into_string())
    }

    fn doc(key_multibase: &str) -> Value {
        json!({
            "@context": ["https://www.w3.org/ns/did/v1"],
            "id": DID,
            "alsoKnownAs": ["at://pilot.example.com"],
            "verificationMethod": [{
                "id": format!("{DID}#atproto"),
                "type": "Multikey",
                "controller": DID,
                "publicKeyMultibase": key_multibase,
            }],
        })
    }

    fn enc(v: &Value) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
    }

    fn jwt_k256(key: &SigningKey, alg: &str, claims: Value) -> String {
        let input = format!("{}.{}", enc(&json!({"typ": "JWT", "alg": alg})), enc(&claims));
        let sig: k256::ecdsa::Signature = key.sign(input.as_bytes());
        let sig = sig.normalize_s().unwrap_or(sig);
        format!("{input}.{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.to_bytes()))
    }

    fn claims() -> Value {
        json!({"iss": DID, "aud": AUD, "lxm": LXM, "iat": NOW - 5, "exp": NOW + 55, "jti": "abc"})
    }

    /// Serves a fixed doc (swappable to simulate a key rotation) and counts fetches.
    struct Mock {
        doc: Mutex<Value>,
        fetches: AtomicUsize,
        handle_did: Option<String>,
    }

    impl Mock {
        fn new(doc: Value) -> Self {
            Mock { doc: Mutex::new(doc), fetches: AtomicUsize::new(0), handle_did: Some(DID.into()) }
        }
    }

    impl Resolver for Mock {
        async fn did_doc(&self, did: &str) -> Result<Value, String> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            assert_eq!(did, DID);
            Ok(self.doc.lock().unwrap().clone())
        }
        async fn handle_did(&self, handle: &str) -> Option<String> {
            assert_eq!(handle, "pilot.example.com");
            self.handle_did.clone()
        }
    }

    async fn verify(token: &str, mock: &Mock, cache: &DocCache) -> Result<VerifiedUser, String> {
        verify_with(token, AUD, LXM, NOW, mock, cache).await
    }

    #[tokio::test]
    async fn accepts_a_valid_k256_token() {
        let key = k256_key(7);
        let mock = Mock::new(doc(&multikey_k256(&key)));
        let user = verify(&jwt_k256(&key, "ES256K", claims()), &mock, &DocCache::default()).await.unwrap();
        assert_eq!(user, VerifiedUser { did: DID.into(), handle: Some("pilot.example.com".into()) });
    }

    #[tokio::test]
    async fn accepts_a_valid_p256_token() {
        use p256::ecdsa::SigningKey as P256Key;
        let key = P256Key::from_bytes(&[9u8; 32].into()).unwrap();
        let mut bytes = vec![0x80, 0x24];
        bytes.extend_from_slice(key.verifying_key().to_encoded_point(true).as_bytes());
        let mock = Mock::new(doc(&format!("z{}", bs58::encode(bytes).into_string())));
        let input = format!("{}.{}", enc(&json!({"typ": "JWT", "alg": "ES256"})), enc(&claims()));
        let sig: p256::ecdsa::Signature = key.sign(input.as_bytes());
        let sig = sig.normalize_s().unwrap_or(sig);
        let token = format!("{input}.{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.to_bytes()));
        assert_eq!(verify(&token, &mock, &DocCache::default()).await.unwrap().did, DID);
    }

    #[tokio::test]
    async fn rejects_bad_claims() {
        let key = k256_key(7);
        let mock = Mock::new(doc(&multikey_k256(&key)));
        let cache = DocCache::default();
        let with = |k: &str, v: Value| {
            let mut c = claims();
            c[k] = v;
            jwt_k256(&key, "ES256K", c)
        };
        assert_eq!(verify(&with("aud", json!("did:web:evil.example")), &mock, &cache).await.unwrap_err(), "wrong aud");
        assert_eq!(verify(&with("lxm", json!("dev.mozzius.flight.other")), &mock, &cache).await.unwrap_err(), "wrong lxm");
        assert_eq!(verify(&with("exp", json!(NOW - 120)), &mock, &cache).await.unwrap_err(), "expired");
        assert_eq!(verify(&with("exp", json!(NOW + 7200)), &mock, &cache).await.unwrap_err(), "exp too far ahead");
        assert_eq!(verify(&with("iat", json!(NOW + 600)), &mock, &cache).await.unwrap_err(), "bad iat");
        assert_eq!(verify(&with("iss", json!("did:key:zabc")), &mock, &cache).await.unwrap_err(), "unsupported iss");
        assert_eq!(verify(&jwt_k256(&key, "HS256", claims()), &mock, &cache).await.unwrap_err(), "unsupported alg");
        assert_eq!(verify(&jwt_k256(&key, "ES256", claims()), &mock, &cache).await.unwrap_err(), "alg doesn't match key");
        assert_eq!(verify("a.b", &mock, &cache).await.unwrap_err(), "not a jwt");
    }

    #[tokio::test]
    async fn rejects_tampered_and_high_s_tokens() {
        let key = k256_key(7);
        let mock = Mock::new(doc(&multikey_k256(&key)));
        let cache = DocCache::default();
        let token = jwt_k256(&key, "ES256K", claims());
        // swap in a different payload under the same signature
        let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
        let mut c = claims();
        c["iss"] = json!(DID);
        c["jti"] = json!("other");
        parts[1] = enc(&c);
        assert_eq!(verify(&parts.join("."), &mock, &cache).await.unwrap_err(), "bad signature");

        // the high-S twin of a valid signature
        let sig_bytes = b64(token.rsplit('.').next().unwrap()).unwrap();
        let sig = k256::ecdsa::Signature::from_slice(&sig_bytes).unwrap();
        let (r, s) = sig.split_scalars();
        let high = k256::ecdsa::Signature::from_scalars(r, -*s).unwrap();
        let input = token.rsplit_once('.').unwrap().0;
        let forged = format!("{input}.{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(high.to_bytes()));
        assert_eq!(verify(&forged, &mock, &cache).await.unwrap_err(), "high-S signature");
    }

    #[tokio::test]
    async fn caches_docs_and_refetches_once_after_key_rotation() {
        let old = k256_key(7);
        let new = k256_key(8);
        let mock = Mock::new(doc(&multikey_k256(&old)));
        let cache = DocCache::default();
        verify(&jwt_k256(&old, "ES256K", claims()), &mock, &cache).await.unwrap();
        verify(&jwt_k256(&old, "ES256K", claims()), &mock, &cache).await.unwrap();
        assert_eq!(mock.fetches.load(Ordering::SeqCst), 1, "second verify uses the cache");

        // the account rotates its key: the cached doc fails, a fresh fetch passes
        *mock.doc.lock().unwrap() = doc(&multikey_k256(&new));
        verify(&jwt_k256(&new, "ES256K", claims()), &mock, &cache).await.unwrap();
        assert_eq!(mock.fetches.load(Ordering::SeqCst), 2);

        // a token from a key the doc doesn't have fails after one refetch
        let stranger = k256_key(9);
        assert_eq!(verify(&jwt_k256(&stranger, "ES256K", claims()), &mock, &cache).await.unwrap_err(), "bad signature");
        assert_eq!(mock.fetches.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn drops_handles_that_dont_resolve_back() {
        let key = k256_key(7);
        let mut mock = Mock::new(doc(&multikey_k256(&key)));
        mock.handle_did = Some("did:plc:someoneelse".into());
        let user = verify(&jwt_k256(&key, "ES256K", claims()), &mock, &DocCache::default()).await.unwrap();
        assert_eq!(user.handle, None);
    }

    #[test]
    fn dev_tokens_need_the_env_switch() {
        assert_eq!(
            dev_user("alice", true).unwrap(),
            VerifiedUser { did: "did:dev:alice".into(), handle: Some("alice".into()) }
        );
        assert!(dev_user("alice", false).is_err());
        assert!(dev_user("", true).is_err());
        assert!(dev_user("a/b", true).is_err());
    }

    /// Real plc.directory + handle lookups (`cargo test -- --ignored`).
    #[tokio::test]
    #[ignore]
    async fn resolves_a_real_did() {
        let did = "did:plc:z72i7hdynmk6r22z27h6tvur"; // @bsky.app
        let r = HttpResolver::new();
        let doc = r.did_doc(did).await.unwrap();
        assert!(signing_key(&doc, did).is_ok());
        assert_eq!(verified_handle(&doc, did, &r).await.as_deref(), Some("bsky.app"));
    }

    #[test]
    fn did_web_urls() {
        assert_eq!(did_web_url("did:web:flight.mozzius.dev").unwrap(), "https://flight.mozzius.dev/.well-known/did.json");
        assert_eq!(did_web_url("did:web:localhost%3A8080").unwrap(), "http://localhost:8080/.well-known/did.json");
        assert!(did_web_url("did:web:example.com:user:alice").is_err());
    }
}
