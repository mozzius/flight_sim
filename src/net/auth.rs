//! Who's playing, and the token that proves it to the server.
//!
//! Web: the page's sign-in sets `window.FLIGHT_SIM_USER` (null or
//! `{did, handle, displayName, avatar}`) and provides
//! `window.flightSimAuth.serviceToken()` (a Promise of a service-auth JWT for
//! the server) and `window.flightSimAuth.signIn()`.
//!
//! Native: no sign-in yet. `FLIGHT_SIM_DEV_USER=<handle>` plays as
//! `<handle>` with a `dev:` token (accepted by servers run with
//! `FLIGHT_SIM_DEV_AUTH=1`); without it multiplayer is hidden.
//!
//! The server URL: `FLIGHT_SIM_SERVER` (`?server=` on the web), else
//! [`DEFAULT_SERVER`].

pub const DEFAULT_SERVER: &str = "wss://mp.flight.mozzius.dev";

pub struct User {
    pub handle: String,
}

/// The WebSocket URL (`<server>/ws`).
pub fn server_url() -> String {
    let base = crate::environment::setting("FLIGHT_SIM_SERVER").unwrap_or_else(|| DEFAULT_SERVER.to_string());
    let base = base.trim_end_matches('/');
    if base.ends_with("/ws") { base.to_string() } else { format!("{base}/ws") }
}

/// Whether the menu offers multiplayer at all.
pub fn available() -> bool {
    cfg!(target_arch = "wasm32") || dev_user().is_some()
}

fn dev_user() -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    return None;
    #[cfg(not(target_arch = "wasm32"))]
    std::env::var("FLIGHT_SIM_DEV_USER").ok().filter(|s| !s.is_empty())
}

/// The signed-in user, if any.
pub fn user() -> Option<User> {
    if let Some(handle) = dev_user() {
        return Some(User { handle });
    }
    #[cfg(target_arch = "wasm32")]
    {
        let user = js_sys::Reflect::get(&js_sys::global(), &"FLIGHT_SIM_USER".into()).ok().filter(|v| v.is_object())?;
        let field = |k: &str| js_sys::Reflect::get(&user, &k.into()).ok().and_then(|v| v.as_string());
        let handle = field("handle").or_else(|| field("did"))?;
        return Some(User { handle });
    }
    #[allow(unreachable_code)]
    None
}

/// Starts the page's sign-in.
pub fn sign_in() {
    #[cfg(target_arch = "wasm32")]
    {
        let call = || -> Option<()> {
            let (auth, f) = auth_fn("signIn")?;
            f.call0(&auth).ok().map(|_| ())
        };
        if call().is_none() {
            bevy::log::warn!("no window.flightSimAuth.signIn");
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn auth_fn(name: &str) -> Option<(wasm_bindgen::JsValue, js_sys::Function)> {
    use wasm_bindgen::JsCast;
    let auth = js_sys::Reflect::get(&js_sys::global(), &"flightSimAuth".into()).ok().filter(|v| v.is_object())?;
    let f = js_sys::Reflect::get(&auth, &name.into()).ok()?.dyn_into::<js_sys::Function>().ok()?;
    Some((auth, f))
}

/// A token on its way (the page fetches it asynchronously).
pub struct TokenRequest {
    #[cfg(target_arch = "wasm32")]
    slot: std::rc::Rc<std::cell::RefCell<Option<Result<String, String>>>>,
    #[cfg(not(target_arch = "wasm32"))]
    ready: Option<Result<String, String>>,
}

// (the web's slot never leaves the main thread)
#[cfg(target_arch = "wasm32")]
unsafe impl Send for TokenRequest {}
#[cfg(target_arch = "wasm32")]
unsafe impl Sync for TokenRequest {}

pub fn request_token() -> TokenRequest {
    #[cfg(not(target_arch = "wasm32"))]
    {
        TokenRequest { ready: Some(dev_user().map(|h| format!("dev:{h}")).ok_or_else(|| "not signed in".to_string())) }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let slot: std::rc::Rc<std::cell::RefCell<Option<Result<String, String>>>> = Default::default();
        let out = slot.clone();
        wasm_bindgen_futures::spawn_local(async move {
            use wasm_bindgen::JsCast;
            let result = async {
                let (auth, f) = auth_fn("serviceToken").ok_or("no window.flightSimAuth.serviceToken")?;
                let promise = f.call0(&auth).map_err(|e| format!("{e:?}"))?.dyn_into::<js_sys::Promise>().map_err(|_| "not a promise")?;
                let token = wasm_bindgen_futures::JsFuture::from(promise).await.map_err(|e| format!("{e:?}"))?;
                token.as_string().ok_or_else(|| "token isn't a string".to_string())
            }
            .await;
            *out.borrow_mut() = Some(result);
        });
        TokenRequest { slot }
    }
}

impl TokenRequest {
    pub fn poll(&mut self) -> Option<Result<String, String>> {
        #[cfg(not(target_arch = "wasm32"))]
        return self.ready.take();
        #[cfg(target_arch = "wasm32")]
        return self.slot.borrow_mut().take();
    }
}
