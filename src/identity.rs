//! Who's playing: an atproto (Bluesky) account on the web, signed in through
//! OAuth in `web/auth.js`. The page keeps `window.FLIGHT_SIM_USER` current
//! and exposes `window.flightSimAuth`; this reads them live, so the menu
//! follows a sign-in or sign-out without a restart.
//!
//! Natively it's the same atproto OAuth from Rust (`crate::oauth`), started
//! from the in-game prompt (`crate::signin`). `FLIGHT_SIM_DEV_USER=<handle>`
//! plays as a dev user instead, whose token is `dev:<handle>` (which the
//! server accepts only with `FLIGHT_SIM_DEV_AUTH=1`, see `atproto`).

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct User {
    pub did: String,
    pub handle: String,
    pub display_name: Option<String>,
    pub avatar: Option<String>,
}

#[cfg(target_arch = "wasm32")]
mod web {
    use super::User;
    use wasm_bindgen::{JsCast, JsValue};

    fn get(obj: &JsValue, key: &str) -> Option<JsValue> {
        js_sys::Reflect::get(obj, &key.into()).ok().filter(|v| !v.is_undefined() && !v.is_null())
    }

    fn string(obj: &JsValue, key: &str) -> Option<String> {
        get(obj, key)?.as_string().filter(|s| !s.is_empty())
    }

    pub fn current_user() -> Option<User> {
        let u = get(&js_sys::global(), "FLIGHT_SIM_USER")?;
        Some(User {
            did: string(&u, "did")?,
            handle: string(&u, "handle")?,
            display_name: string(&u, "displayName"),
            avatar: string(&u, "avatar"),
        })
    }

    pub fn available() -> bool {
        get(&js_sys::global(), "flightSimAuth").and_then(|a| get(&a, "available")).is_some_and(|v| v.is_truthy())
    }

    /// Calls `window.flightSimAuth[name]()`.
    pub fn call(name: &str) -> Result<JsValue, String> {
        let auth = get(&js_sys::global(), "flightSimAuth").ok_or("no flightSimAuth")?;
        let f: js_sys::Function = get(&auth, name).ok_or("missing")?.dyn_into().map_err(|_| "not a function")?;
        f.call0(&auth).map_err(|e| format!("{e:?}"))
    }

    pub async fn service_token() -> Result<String, String> {
        let promise: js_sys::Promise = call("serviceToken")?.dyn_into().map_err(|_| "not a promise")?;
        let token = wasm_bindgen_futures::JsFuture::from(promise)
            .await
            .map_err(|e| e.as_string().or_else(|| js_sys::Error::from(e).message().as_string()).unwrap_or_default())?;
        token.as_string().ok_or_else(|| "token isn't a string".into())
    }
}

/// The signed-in account, if any.
pub fn current_user() -> Option<User> {
    #[cfg(target_arch = "wasm32")]
    {
        web::current_user()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        if let Some(h) = dev_handle() {
            return Some(User { did: format!("did:dev:{h}"), handle: h, display_name: None, avatar: None });
        }
        crate::oauth::user().map(|s| User { did: s.did, handle: s.handle, display_name: s.display_name, avatar: s.avatar })
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn dev_handle() -> Option<String> {
    std::env::var("FLIGHT_SIM_DEV_USER").ok().filter(|h| !h.is_empty())
}

/// Whether this build can sign in at all (the web page on its own domain or
/// localhost; natively always, unless playing as a dev user).
pub fn can_sign_in() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        web::available()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        dev_handle().is_none()
    }
}

/// The web page's handle prompt (it then redirects to the account's
/// authorization server and comes back signed in). Natively the prompt is
/// the game's own (`crate::signin`), which calls `crate::oauth::begin`.
#[cfg(target_arch = "wasm32")]
pub fn sign_in() {
    if let Err(e) = web::call("signIn") {
        bevy::log::warn!("sign in: {e}");
    }
}

pub fn sign_out() {
    #[cfg(target_arch = "wasm32")]
    if let Err(e) = web::call("signOut") {
        bevy::log::warn!("sign out: {e}");
    }
    #[cfg(not(target_arch = "wasm32"))]
    crate::oauth::sign_out();
}

/// A fresh service-auth JWT for the game server (`aud`
/// `did:web:flight.mozzius.dev`, `lxm` `dev.mozzius.flight.join`), minted by
/// the player's PDS. Natively: `dev:<handle>` for a dev user.
pub async fn service_token() -> Result<String, String> {
    #[cfg(target_arch = "wasm32")]
    {
        web::service_token().await
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        match dev_handle() {
            Some(h) => Ok(format!("dev:{h}")),
            None => crate::oauth::service_token().await,
        }
    }
}
