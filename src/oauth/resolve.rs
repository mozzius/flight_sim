//! Finding a sign-in's authorization server: handle → DID → DID document →
//! PDS → its protected-resource metadata → the authorization server's
//! metadata.

use reqwest::{Client, Url};
use serde_json::Value;

/// Resolves handles (DNS TXT handles too, which we can't look up ourselves).
pub const APPVIEW: &str = "https://public.api.bsky.app";
/// Where an empty handle starts: it asks for the account itself.
pub const ENTRYWAY: &str = "https://bsky.social";

/// What the player typed in the sign-in prompt.
#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    /// Nothing: the entryway.
    Entryway,
    Handle(String),
    Did(String),
    /// A server URL (a PDS or an entryway).
    Server(String),
}

impl Input {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let s = raw.trim().trim_start_matches('@').trim();
        if s.is_empty() {
            return Ok(Self::Entryway);
        }
        if s.starts_with("did:plc:") || s.starts_with("did:web:") {
            return Ok(Self::Did(s.to_string()));
        }
        if s.starts_with("https://") {
            let url = Url::parse(s).map_err(|_| "bad server URL")?;
            return Ok(Self::Server(url.origin().ascii_serialization()));
        }
        let h = s.to_ascii_lowercase();
        let ok = h.contains('.')
            && h.len() <= 253
            && h.split('.').all(|l| !l.is_empty() && !l.starts_with('-') && !l.ends_with('-'))
            && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
        if !ok {
            return Err("not a handle".into());
        }
        Ok(Self::Handle(h))
    }
}

/// An authorization server's endpoints.
#[derive(Clone, Debug)]
pub struct AuthServer {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub par_endpoint: String,
    pub revocation_endpoint: Option<String>,
    /// Whether it names itself in the redirect (RFC 9207 `iss`).
    pub iss_parameter: bool,
}

async fn get_json(client: &Client, url: &str) -> Result<Value, String> {
    let r = client.get(url).header("accept", "application/json").send().await.map_err(|e| format!("{url}: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("{url}: {}", r.status()));
    }
    let bytes = r.bytes().await.map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{url}: {e}"))
}

pub async fn resolve_handle(client: &Client, handle: &str) -> Result<String, String> {
    let url = format!("{APPVIEW}/xrpc/com.atproto.identity.resolveHandle?handle={handle}");
    if let Ok(v) = get_json(client, &url).await
        && let Some(did) = v["did"].as_str()
    {
        return Ok(did.to_string());
    }
    // (a handle the AppView doesn't know yet)
    let url = format!("https://{handle}/.well-known/atproto-did");
    if let Ok(r) = client.get(&url).send().await
        && r.status().is_success()
        && let Ok(t) = r.text().await
        && t.trim().starts_with("did:")
    {
        return Ok(t.trim().to_string());
    }
    Err("handle not found".into())
}

/// `did:web:host` → its did.json URL (host-level only, as atproto requires).
pub fn did_web_url(did: &str) -> Result<String, String> {
    let host = did.strip_prefix("did:web:").ok_or("not did:web")?.replace("%3A", ":");
    if host.is_empty() || host.contains('/') || host.matches(':').count() > 1 {
        return Err("bad did:web".into());
    }
    let scheme = if host.starts_with("localhost") { "http" } else { "https" };
    Ok(format!("{scheme}://{host}/.well-known/did.json"))
}

/// The account's PDS (its DID document's `#atproto_pds` service).
pub async fn pds_of(client: &Client, did: &str) -> Result<String, String> {
    let url = if let Some(id) = did.strip_prefix("did:plc:") {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err("bad did:plc".into());
        }
        format!("https://plc.directory/{did}")
    } else {
        did_web_url(did)?
    };
    let doc = get_json(client, &url).await?;
    if doc["id"].as_str() != Some(did) {
        return Err("DID document doesn't match".into());
    }
    pds_from_doc(&doc).ok_or_else(|| "no PDS in the DID document".into())
}

pub fn pds_from_doc(doc: &Value) -> Option<String> {
    let did = doc["id"].as_str().unwrap_or_default();
    doc["service"].as_array()?.iter().find_map(|s| {
        let id = s["id"].as_str()?;
        let is_pds = id == "#atproto_pds" || id.strip_prefix(did) == Some("#atproto_pds");
        let endpoint = s["serviceEndpoint"].as_str()?;
        (is_pds && s["type"].as_str() == Some("AtprotoPersonalDataServer") && endpoint.starts_with("https://"))
            .then(|| endpoint.trim_end_matches('/').to_string())
    })
}

/// The authorization server that protects `server` (a PDS or entryway).
pub async fn issuer_of(client: &Client, server: &str) -> Result<String, String> {
    let v = get_json(client, &format!("{server}/.well-known/oauth-protected-resource")).await?;
    v["authorization_servers"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(|s| s.trim_end_matches('/').to_string())
        .ok_or_else(|| "no authorization server".into())
}

/// The authorization server for what the player named: the one protecting
/// a PDS, or the server itself when it is one (the bsky.social entryway
/// has no protected-resource metadata: it's the authorization server).
pub async fn auth_server_for(client: &Client, server: &str) -> Result<AuthServer, String> {
    match issuer_of(client, server).await {
        Ok(issuer) => auth_server(client, &issuer).await,
        Err(e) => auth_server(client, server).await.map_err(|e2| format!("{e}; {e2}")),
    }
}

pub async fn auth_server(client: &Client, issuer: &str) -> Result<AuthServer, String> {
    let url = Url::parse(issuer).map_err(|_| "bad issuer")?;
    if url.scheme() != "https" || url.origin().ascii_serialization() != issuer {
        return Err("bad issuer".into());
    }
    let v = get_json(client, &format!("{issuer}/.well-known/oauth-authorization-server")).await?;
    parse_auth_server(&v, issuer)
}

pub fn parse_auth_server(v: &Value, issuer: &str) -> Result<AuthServer, String> {
    if v["issuer"].as_str() != Some(issuer) {
        return Err("issuer mismatch".into());
    }
    let s = |k: &str| v[k].as_str().filter(|u| u.starts_with("https://")).map(str::to_string);
    let has = |k: &str, want: &str| v[k].as_array().is_some_and(|a| a.iter().any(|x| x.as_str() == Some(want)));
    if !has("code_challenge_methods_supported", "S256") || !has("dpop_signing_alg_values_supported", "ES256") {
        return Err("authorization server lacks S256 / ES256".into());
    }
    Ok(AuthServer {
        issuer: issuer.to_string(),
        authorization_endpoint: s("authorization_endpoint").ok_or("no authorization_endpoint")?,
        token_endpoint: s("token_endpoint").ok_or("no token_endpoint")?,
        par_endpoint: s("pushed_authorization_request_endpoint").ok_or("no PAR endpoint")?,
        revocation_endpoint: s("revocation_endpoint"),
        iss_parameter: v["authorization_response_iss_parameter_supported"] == true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_what_was_typed() {
        assert_eq!(Input::parse("  "), Ok(Input::Entryway));
        assert_eq!(Input::parse("@Alice.BSKY.social "), Ok(Input::Handle("alice.bsky.social".into())));
        assert_eq!(Input::parse("did:plc:abc123"), Ok(Input::Did("did:plc:abc123".into())));
        assert_eq!(Input::parse("https://pds.example.com/x"), Ok(Input::Server("https://pds.example.com".into())));
        assert!(Input::parse("alice").is_err());
        assert!(Input::parse("al ice.com").is_err());
        assert!(Input::parse("-a.com").is_err());
    }

    #[test]
    fn finds_the_pds() {
        let doc = json!({
            "id": "did:plc:abc",
            "service": [
                { "id": "#other", "type": "X", "serviceEndpoint": "https://nope" },
                { "id": "#atproto_pds", "type": "AtprotoPersonalDataServer", "serviceEndpoint": "https://pds.example.com/" },
            ]
        });
        assert_eq!(pds_from_doc(&doc).as_deref(), Some("https://pds.example.com"));
        assert_eq!(did_web_url("did:web:example.com").unwrap(), "https://example.com/.well-known/did.json");
        assert!(did_web_url("did:web:example.com:a:b").is_err());
    }

    #[test]
    fn checks_the_auth_server_metadata() {
        let mut v = json!({
            "issuer": "https://bsky.social",
            "authorization_endpoint": "https://bsky.social/oauth/authorize",
            "token_endpoint": "https://bsky.social/oauth/token",
            "pushed_authorization_request_endpoint": "https://bsky.social/oauth/par",
            "revocation_endpoint": "https://bsky.social/oauth/revoke",
            "code_challenge_methods_supported": ["S256"],
            "dpop_signing_alg_values_supported": ["RS256", "ES256"],
        });
        let a = parse_auth_server(&v, "https://bsky.social").unwrap();
        assert_eq!(a.par_endpoint, "https://bsky.social/oauth/par");
        assert!(parse_auth_server(&v, "https://evil.example").is_err());
        v["dpop_signing_alg_values_supported"] = json!(["RS256"]);
        assert!(parse_auth_server(&v, "https://bsky.social").is_err());
    }
}
