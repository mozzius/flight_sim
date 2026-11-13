//! Multiplayer's view of the player: thin glue over [`crate::identity`], which
//! owns who's signed in and mints the service-auth token.
//!
//! The server URL: `FLIGHT_SIM_SERVER` (`?server=` on the web), else
//! [`DEFAULT_SERVER`].

pub const DEFAULT_SERVER: &str = "wss://mp.flight.mozzius.dev";

/// The WebSocket URL (`<server>/ws`).
pub fn server_url() -> String {
    let base = crate::environment::setting("FLIGHT_SIM_SERVER").unwrap_or_else(|| DEFAULT_SERVER.to_string());
    let base = base.trim_end_matches('/');
    if base.ends_with("/ws") { base.to_string() } else { format!("{base}/ws") }
}

/// Whether the menu offers multiplayer at all: always on the web (it can
/// sign in), natively only for a dev user (`FLIGHT_SIM_DEV_USER`).
pub fn available() -> bool {
    cfg!(target_arch = "wasm32") || crate::identity::current_user().is_some()
}

/// A token on its way ([`crate::identity::service_token`] is async on the web).
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
        // natively it's just an env var: ready at once
        TokenRequest { ready: Some(bevy::tasks::block_on(crate::identity::service_token())) }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let slot: std::rc::Rc<std::cell::RefCell<Option<Result<String, String>>>> = Default::default();
        let out = slot.clone();
        wasm_bindgen_futures::spawn_local(async move {
            *out.borrow_mut() = Some(crate::identity::service_token().await);
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
