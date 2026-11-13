//! Service-auth verification (STUB: replaced by the real atproto JWT check).
//!
//! The stub accepts `dev:<handle>` tokens as `did:dev:<handle>`, and only
//! when `FLIGHT_SIM_DEV_AUTH=1`; anything else is refused.

pub struct VerifiedUser {
    pub did: String,
    pub handle: Option<String>,
}

pub async fn verify_service_auth(token: &str, aud: &str, lxm: &str) -> Result<VerifiedUser, String> {
    let _ = (aud, lxm);
    if std::env::var("FLIGHT_SIM_DEV_AUTH").as_deref() != Ok("1") {
        return Err("service auth is not implemented yet (dev tokens need FLIGHT_SIM_DEV_AUTH=1)".into());
    }
    let handle = token.strip_prefix("dev:").ok_or("not a dev token")?;
    let ok = !handle.is_empty() && handle.len() <= 64 && handle.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    if !ok {
        return Err("bad dev handle".into());
    }
    Ok(VerifiedUser { did: format!("did:dev:{handle}"), handle: Some(handle.to_string()) })
}
