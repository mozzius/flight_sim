// Sign in with an atproto (Bluesky) account, via atproto OAuth.
//
// Exposes (read by src/identity.rs):
//   window.FLIGHT_SIM_USER        null, or { did, handle, displayName, avatar }
//   window.flightSimAuth.available  whether sign-in works on this host
//   window.flightSimAuth.signIn(handle?)  no argument: the handle prompt;
//                                 a handle (or "" for the bsky.social entryway): redirect to its login
//   window.flightSimAuth.signOut()
//   window.flightSimAuth.serviceToken()  Promise<string>: a service-auth JWT for the game server
//   window.flightSimAuth.ready     Promise that settles once a stored session is restored (or not)
//
// Production uses the client metadata at /oauth-client-metadata.json; on
// localhost / 127.0.0.1 it's the loopback client (no metadata file needed).

const LIB = "https://esm.sh/@atproto/oauth-client-browser@0.5.14?bundle";
const APPVIEW = "https://public.api.bsky.app";
const ENTRYWAY = "https://bsky.social";
const SERVER_DID = "did:web:flight.mozzius.dev";
const JOIN = "dev.mozzius.flight.join";
// rpc permission scopes need aud to be "*" or a did#service ref, so the
// fragment-less server DID is covered by aud=* (still only this one method)
const SCOPE = `atproto rpc:${JOIN}?aud=*`;
const PROD_HOST = "flight.mozzius.dev";
const PROFILE_KEY = "flight-sim:profile";

const loopback = ["localhost", "127.0.0.1", "[::1]"].includes(location.hostname);
const available = loopback || location.hostname === PROD_HOST;

function clientId() {
  if (!loopback) return `https://${PROD_HOST}/oauth-client-metadata.json`;
  // loopback redirect URIs must be IP literals (the library moves localhost to 127.0.0.1)
  const host = location.hostname === "localhost" ? "127.0.0.1" : location.hostname;
  const redirect = `http://${host}${location.port ? ":" + location.port : ""}/`;
  return `http://localhost?redirect_uri=${encodeURIComponent(redirect)}&scope=${encodeURIComponent(SCOPE)}`;
}

window.FLIGHT_SIM_USER = null;
try {
  // the last profile, so the menu shows the handle straight away
  const cached = JSON.parse(localStorage.getItem(PROFILE_KEY) || "null");
  if (cached?.did) window.FLIGHT_SIM_USER = cached;
} catch {}

let session = null;
let clientPromise = null;
const client = () => {
  clientPromise ??= import(LIB).then(({ BrowserOAuthClient }) =>
    BrowserOAuthClient.load({ clientId: clientId(), handleResolver: APPVIEW }),
  );
  return clientPromise;
};

function setUser(user) {
  window.FLIGHT_SIM_USER = user;
  try {
    if (user) localStorage.setItem(PROFILE_KEY, JSON.stringify(user));
    else localStorage.removeItem(PROFILE_KEY);
  } catch {}
}

async function loadProfile(did) {
  const prev = window.FLIGHT_SIM_USER?.did === did ? window.FLIGHT_SIM_USER : null;
  try {
    const r = await fetch(`${APPVIEW}/xrpc/app.bsky.actor.getProfile?actor=${encodeURIComponent(did)}`);
    if (!r.ok) throw new Error(r.status);
    const p = await r.json();
    setUser({ did, handle: p.handle, displayName: p.displayName || "", avatar: p.avatar || "" });
  } catch {
    // no Bluesky profile (or the AppView is down): keep what we had, else the DID
    setUser(prev || { did, handle: did, displayName: "", avatar: "" });
  }
}

async function restore() {
  if (!available) return setUser(null);
  // nothing stored and no OAuth callback in the URL: don't load the library yet
  const callback = /[#&](state|code|error)=/.test(location.hash);
  let stored = null;
  try { stored = localStorage.getItem("@@atproto/oauth-client-browser(sub)"); } catch {}
  if (!callback && !stored) return setUser(null);
  try {
    const c = await client();
    c.addEventListener?.("deleted", () => { session = null; setUser(null); });
    const result = await c.init();
    session = result?.session || null;
    if (session) await loadProfile(session.did);
    else setUser(null);
  } catch (e) {
    // a refused login or an expired session: just play signed out
    console.warn("sign-in:", e);
    session = null;
    setUser(null);
  }
}

// ---- the handle prompt (styled like the loading overlay) ----

let prompt = null;
function showPrompt() {
  if (!prompt) {
    prompt = document.createElement("form");
    prompt.id = "signin";
    prompt.innerHTML =
      '<input name="handle" type="text" placeholder="handle.bsky.social" autocomplete="username" autocapitalize="none" autocorrect="off" spellcheck="false" enterkeyhint="go">' +
      '<button type="submit">SIGN IN</button>';
    prompt.addEventListener("submit", (e) => {
      e.preventDefault();
      const handle = prompt.handle.value.trim().replace(/^@/, "");
      prompt.querySelector("button").disabled = true;
      signIn(handle).catch((err) => {
        prompt.querySelector("button").disabled = false;
        prompt.handle.setCustomValidity(String(err?.message || err));
        prompt.handle.reportValidity();
      });
    });
    prompt.handle.addEventListener("input", () => prompt.handle.setCustomValidity(""));
    // a tap outside the box or Esc closes it
    prompt.addEventListener("pointerdown", (e) => { if (e.target === prompt) hidePrompt(); });
    prompt.addEventListener("keydown", (e) => { if (e.key === "Escape") hidePrompt(); e.stopPropagation(); });
    prompt.addEventListener("keyup", (e) => e.stopPropagation());
    document.body.appendChild(prompt);
  }
  prompt.hidden = false;
  prompt.querySelector("button").disabled = false;
  prompt.handle.focus();
}
function hidePrompt() {
  if (prompt) prompt.hidden = true;
  document.getElementById("bevy")?.focus();
}

async function signIn(handle) {
  if (!available) throw new Error("sign-in isn't available on this host");
  if (handle === undefined) return showPrompt();
  // the loopback client redirects to 127.0.0.1, and sessions are per origin: sign in from there
  if (location.hostname === "localhost") {
    location.href = location.href.replace("//localhost", "//127.0.0.1");
    return;
  }
  const c = await client();
  // an empty handle starts at the bsky.social entryway, which asks for the account itself
  await c.signIn(handle || ENTRYWAY, { scope: SCOPE });
  // (navigates away; the page comes back signed in)
}

async function signOut() {
  const s = session;
  session = null;
  setUser(null);
  try {
    if (s) await s.signOut();
  } catch (e) {
    console.warn("sign out:", e);
  }
}

async function serviceToken() {
  await ready;
  if (!session) throw new Error("signed out");
  const params = new URLSearchParams({
    aud: SERVER_DID,
    lxm: JOIN,
    exp: String(Math.floor(Date.now() / 1000) + 60),
  });
  const r = await session.fetchHandler(`/xrpc/com.atproto.server.getServiceAuth?${params}`);
  if (!r.ok) throw new Error(`getServiceAuth: ${r.status} ${await r.text()}`);
  return (await r.json()).token;
}

const ready = restore();
window.flightSimAuth = { available, signIn, signOut, serviceToken, ready };
