//! The crypto bits of atproto OAuth: PKCE (S256), the ES256 DPoP key, DPoP
//! proofs (RFC 9449) and the key's JWK thumbprint (RFC 7638).

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use p256::ecdsa::{Signature, SigningKey, signature::Signer as _};
use rand::TryRng as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub fn b64(bytes: impl AsRef<[u8]>) -> String {
    B64.encode(bytes)
}

/// Bytes from the OS's CSPRNG.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    rand::rngs::SysRng.try_fill_bytes(&mut out).expect("OS random numbers");
    out
}

/// A random URL-safe token (`state`, `jti`, the PKCE verifier).
pub fn random_token(bytes: usize) -> String {
    let raw = random_bytes::<64>();
    b64(&raw[..bytes.min(64)])
}

/// PKCE: (verifier, S256 challenge).
pub fn pkce() -> (String, String) {
    // 32 random bytes → 43 characters, the RFC 7636 minimum length
    let verifier = random_token(32);
    let challenge = pkce_challenge(&verifier);
    (verifier, challenge)
}

pub fn pkce_challenge(verifier: &str) -> String {
    b64(Sha256::digest(verifier.as_bytes()))
}

/// The session's DPoP key (P-256).
#[derive(Clone)]
pub struct DpopKey(SigningKey);

impl DpopKey {
    pub fn generate() -> Self {
        loop {
            // (from_slice rejects 0 and values ≥ n; vanishingly rare)
            if let Ok(k) = SigningKey::from_slice(&random_bytes::<32>()) {
                return Self(k);
            }
        }
    }

    /// The private scalar, base64url (the JWK `d`), for storage.
    pub fn secret(&self) -> String {
        b64(self.0.to_bytes())
    }

    pub fn from_secret(d: &str) -> Result<Self, String> {
        let bytes = B64.decode(d).map_err(|e| e.to_string())?;
        SigningKey::from_slice(&bytes).map(Self).map_err(|e| e.to_string())
    }

    /// The public JWK (`kty`, `crv`, `x`, `y`).
    pub fn public_jwk(&self) -> Value {
        let point = self.0.verifying_key().to_encoded_point(false);
        json!({
            "kty": "EC",
            "crv": "P-256",
            "x": b64(point.x().expect("uncompressed point")),
            "y": b64(point.y().expect("uncompressed point")),
        })
    }

    /// RFC 7638 thumbprint: SHA-256 of the required members, in
    /// lexicographic order, no whitespace.
    pub fn thumbprint(&self) -> String {
        let jwk = self.public_jwk();
        let canonical = format!(
            r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
            jwk["x"].as_str().unwrap_or_default(),
            jwk["y"].as_str().unwrap_or_default()
        );
        b64(Sha256::digest(canonical.as_bytes()))
    }

    /// A DPoP proof for `method url`. `url` is reduced to the RFC 9449
    /// `htu` (no query or fragment); `access_token` adds `ath`.
    pub fn proof(&self, method: &str, url: &str, nonce: Option<&str>, access_token: Option<&str>, now: i64) -> String {
        let header = json!({ "typ": "dpop+jwt", "alg": "ES256", "jwk": self.public_jwk() });
        let mut claims = json!({
            "jti": random_token(16),
            "htm": method,
            "htu": htu(url),
            "iat": now,
        });
        if let Some(n) = nonce {
            claims["nonce"] = n.into();
        }
        if let Some(t) = access_token {
            claims["ath"] = b64(Sha256::digest(t.as_bytes())).into();
        }
        let input = format!("{}.{}", b64(header.to_string()), b64(claims.to_string()));
        // ES256: deterministic (RFC 6979) ECDSA over SHA-256, the raw r || s
        let sig: Signature = self.0.sign(input.as_bytes());
        format!("{input}.{}", b64(sig.to_bytes()))
    }
}

/// The URL without its query and fragment.
pub fn htu(url: &str) -> String {
    let end = url.find(['?', '#']).unwrap_or(url.len());
    url[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{VerifyingKey, signature::Verifier as _};

    fn decode(part: &str) -> Value {
        serde_json::from_slice(&B64.decode(part).unwrap()).unwrap()
    }

    #[test]
    fn pkce_matches_rfc7636() {
        // RFC 7636 appendix B
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let (verifier, challenge) = pkce();
        assert_eq!(verifier.len(), 43);
        assert!(verifier.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_eq!(challenge, pkce_challenge(&verifier));
        assert_ne!(pkce().0, verifier);
    }

    #[test]
    fn thumbprint_matches_rfc7638_style() {
        // the RFC 7638 §3.1 procedure, by hand, for a fixed key
        let key = DpopKey::from_secret(&b64([7u8; 32])).unwrap();
        let jwk = key.public_jwk();
        let manual = format!(
            "{{\"crv\":\"P-256\",\"kty\":\"EC\",\"x\":\"{}\",\"y\":\"{}\"}}",
            jwk["x"].as_str().unwrap(),
            jwk["y"].as_str().unwrap()
        );
        assert_eq!(key.thumbprint(), b64(Sha256::digest(manual.as_bytes())));
        assert_eq!(key.thumbprint().len(), 43);
        // and it survives a round trip through storage
        let back = DpopKey::from_secret(&key.secret()).unwrap();
        assert_eq!(back.thumbprint(), key.thumbprint());
        assert_ne!(DpopKey::generate().thumbprint(), key.thumbprint());
    }

    #[test]
    fn dpop_proof_has_the_right_claims_and_signature() {
        let key = DpopKey::generate();
        let proof = key.proof(
            "GET",
            "https://pds.example.com/xrpc/com.atproto.server.getServiceAuth?aud=did:web:x#frag",
            Some("nonce-1"),
            Some("the-access-token"),
            1_800_000_000,
        );
        let parts: Vec<&str> = proof.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header = decode(parts[0]);
        assert_eq!(header["typ"], "dpop+jwt");
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["jwk"], key.public_jwk());
        assert!(header["jwk"].get("d").is_none(), "no private key in the proof");
        let claims = decode(parts[1]);
        assert_eq!(claims["htm"], "GET");
        assert_eq!(claims["htu"], "https://pds.example.com/xrpc/com.atproto.server.getServiceAuth");
        assert_eq!(claims["iat"], 1_800_000_000);
        assert_eq!(claims["nonce"], "nonce-1");
        assert_eq!(claims["ath"], b64(Sha256::digest(b"the-access-token")));
        assert!(claims["jti"].as_str().unwrap().len() >= 16);
        // the signature verifies against the header's key
        let jwk = &header["jwk"];
        let mut sec1 = vec![4u8];
        sec1.extend(B64.decode(jwk["x"].as_str().unwrap()).unwrap());
        sec1.extend(B64.decode(jwk["y"].as_str().unwrap()).unwrap());
        let vk = VerifyingKey::from_sec1_bytes(&sec1).unwrap();
        let sig = Signature::from_slice(&B64.decode(parts[2]).unwrap()).unwrap();
        vk.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).unwrap();

        // no nonce or token: no claims for them, and a fresh jti each time
        let plain = key.proof("POST", "https://as.example.com/oauth/par", None, None, 1);
        let c = decode(plain.split('.').nth(1).unwrap());
        assert!(c.get("nonce").is_none() && c.get("ath").is_none());
        assert_ne!(c["jti"], claims["jti"]);
    }
}
