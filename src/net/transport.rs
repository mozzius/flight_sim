//! A WebSocket that never blocks the game: binary frames in and out,
//! polled once a frame. The browser's own WebSocket on the web, and
//! tokio-tungstenite on a background thread natively (`ws://` and `wss://`,
//! rustls with bundled webpki roots).

pub enum TransportEvent {
    Open,
    Message(Vec<u8>),
    Closed,
}

pub use imp::Transport;

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use std::sync::Mutex;

    use futures_util::{SinkExt, StreamExt};
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
    use tokio_tungstenite::tungstenite::Message;

    use super::TransportEvent;

    pub struct Transport {
        tx: UnboundedSender<Vec<u8>>,
        rx: Mutex<UnboundedReceiver<TransportEvent>>,
    }

    impl Transport {
        pub fn connect(url: &str) -> Self {
            let (tx, mut out_rx) = unbounded_channel::<Vec<u8>>();
            let (in_tx, rx) = unbounded_channel();
            let url = url.to_string();
            std::thread::spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
                    let _ = in_tx.send(TransportEvent::Closed);
                    return;
                };
                rt.block_on(async move {
                    let ws = match tokio_tungstenite::connect_async(url.as_str()).await {
                        Ok((ws, _)) => ws,
                        Err(e) => {
                            bevy::log::warn!("couldn't connect to {url}: {e}");
                            let _ = in_tx.send(TransportEvent::Closed);
                            return;
                        }
                    };
                    let _ = in_tx.send(TransportEvent::Open);
                    let (mut sink, mut stream) = ws.split();
                    loop {
                        tokio::select! {
                            out = out_rx.recv() => match out {
                                Some(bytes) => {
                                    if sink.send(Message::Binary(bytes.into())).await.is_err() {
                                        break;
                                    }
                                }
                                // the game dropped the connection
                                None => {
                                    let _ = sink.close().await;
                                    break;
                                }
                            },
                            frame = stream.next() => match frame {
                                Some(Ok(Message::Binary(b))) => {
                                    let _ = in_tx.send(TransportEvent::Message(b.to_vec()));
                                }
                                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                                _ => {}
                            },
                        }
                    }
                    let _ = in_tx.send(TransportEvent::Closed);
                });
            });
            Self { tx, rx: Mutex::new(rx) }
        }

        pub fn send(&self, bytes: Vec<u8>) {
            let _ = self.tx.send(bytes);
        }

        pub fn poll(&self) -> Vec<TransportEvent> {
            let mut rx = self.rx.lock().unwrap();
            std::iter::from_fn(|| rx.try_recv().ok()).collect()
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod imp {
    use std::{cell::RefCell, rc::Rc};

    use wasm_bindgen::{JsCast, prelude::Closure};
    use web_sys::{BinaryType, MessageEvent, WebSocket};

    use super::TransportEvent;

    /// The socket lives in a thread-local (the web is single-threaded, and
    /// JS objects can't go in a Bevy resource); this is a handle to it.
    pub struct Transport {
        id: u32,
    }

    struct Socket {
        id: u32,
        ws: Option<WebSocket>,
        inbox: Rc<RefCell<Vec<TransportEvent>>>,
        _callbacks: Vec<Closure<dyn FnMut(web_sys::Event)>>,
    }

    thread_local! {
        static SOCKET: RefCell<Option<Socket>> = const { RefCell::new(None) };
        static NEXT: RefCell<u32> = const { RefCell::new(0) };
    }

    impl Transport {
        pub fn connect(url: &str) -> Self {
            let id = NEXT.with(|n| {
                *n.borrow_mut() += 1;
                *n.borrow()
            });
            let inbox: Rc<RefCell<Vec<TransportEvent>>> = Default::default();
            let mut callbacks = vec![];
            let ws = match WebSocket::new(url) {
                Ok(ws) => {
                    ws.set_binary_type(BinaryType::Arraybuffer);
                    let on = |inbox: &Rc<RefCell<Vec<TransportEvent>>>, f: fn(web_sys::Event) -> Option<TransportEvent>| {
                        let inbox = inbox.clone();
                        Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
                            if let Some(ev) = f(e) {
                                inbox.borrow_mut().push(ev);
                            }
                        })
                    };
                    let open = on(&inbox, |_| Some(TransportEvent::Open));
                    let message = on(&inbox, |e| {
                        let data = e.dyn_into::<MessageEvent>().ok()?.data();
                        let buf = data.dyn_into::<js_sys::ArrayBuffer>().ok()?;
                        Some(TransportEvent::Message(js_sys::Uint8Array::new(&buf).to_vec()))
                    });
                    let close = on(&inbox, |_| Some(TransportEvent::Closed));
                    ws.set_onopen(Some(open.as_ref().unchecked_ref()));
                    ws.set_onmessage(Some(message.as_ref().unchecked_ref()));
                    ws.set_onclose(Some(close.as_ref().unchecked_ref()));
                    callbacks.extend([open, message, close]);
                    Some(ws)
                }
                Err(e) => {
                    bevy::log::warn!("couldn't open a WebSocket to {url}: {e:?}");
                    inbox.borrow_mut().push(TransportEvent::Closed);
                    None
                }
            };
            SOCKET.with(|s| {
                if let Some(old) = s.borrow_mut().replace(Socket { id, ws, inbox, _callbacks: callbacks }) {
                    old.shut();
                }
            });
            Self { id }
        }

        fn with<R>(&self, f: impl FnOnce(&Socket) -> R) -> Option<R> {
            SOCKET.with(|s| s.borrow().as_ref().filter(|s| s.id == self.id).map(f))
        }

        pub fn send(&self, bytes: Vec<u8>) {
            self.with(|s| {
                if let Some(ws) = &s.ws
                    && ws.ready_state() == WebSocket::OPEN
                {
                    let _ = ws.send_with_u8_array(&bytes);
                }
            });
        }

        pub fn poll(&self) -> Vec<TransportEvent> {
            self.with(|s| std::mem::take(&mut *s.inbox.borrow_mut())).unwrap_or_default()
        }
    }

    impl Socket {
        fn shut(&self) {
            if let Some(ws) = &self.ws {
                ws.set_onopen(None);
                ws.set_onmessage(None);
                ws.set_onclose(None);
                let _ = ws.close();
            }
        }
    }

    impl Drop for Transport {
        fn drop(&mut self) {
            SOCKET.with(|s| {
                let mut s = s.borrow_mut();
                if s.as_ref().is_some_and(|s| s.id == self.id)
                    && let Some(old) = s.take()
                {
                    old.shut();
                }
            });
        }
    }
}
