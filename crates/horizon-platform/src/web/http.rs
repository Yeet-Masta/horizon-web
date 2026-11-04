//! HTTP for the web build (web/README.md, "HTTP"): what the game asks of a web service
//! natively with ureq (Realms, cosmetics, the resource pack sites, a server's BlueMap).
//!
//! The browser's `fetch` goes first. A site that sends no CORS headers refuses a page's
//! requests, a page on https may not fetch an `http://` address, and `fetch` drops a `Cookie`
//! header: those go through the relay's TCP tunnel instead ([`super::https`]: TLS spoken here
//! for https, so the relay carries ciphertext; an `http://` address as it is).
//!
//! `fetch`, the relay's sockets and their promises live on the page's event loop, which a
//! blocked worker never turns: a worker's [`request`] runs there ([`super::run_on_page`]) and
//! the worker waits for the answer. The page's own thread may not wait: it awaits
//! [`request_async`].
use super::https;
pub use super::https::Response;
use crate::time::Duration;
use js_sys::{Array, Object, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue, prelude::*};
use wasm_bindgen_futures::JsFuture;

/// One request.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub timeout: Duration,
    /// The relay (`ws://` or `wss://`) to go through when the browser can't reach the site;
    /// `HORIZON_WEB_RELAY` when None.
    pub relay: Option<String>,
}

impl Request {
    pub fn get(url: &str) -> Self {
        Self {
            method: "GET".into(),
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout: Duration::from_secs(20),
            relay: None,
        }
    }
    pub fn post(url: &str, body: Vec<u8>) -> Self {
        Self {
            method: "POST".into(),
            body,
            ..Self::get(url)
        }
    }
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = fetch)]
    fn js_fetch(url: &str, init: &JsValue) -> js_sys::Promise;
}

/// Why `fetch` gave no answer: the browser refused (CORS, mixed content, a forbidden
/// header) or couldn't reach the site; the relay may still.
struct Refused(String);

/// `request` with the browser's `fetch`.
async fn fetch(request: &Request) -> Result<Response, Refused> {
    let refused = |e: JsValue| {
        Refused(
            e.dyn_ref::<js_sys::Error>()
                .map_or_else(|| format!("{e:?}"), |e| String::from(e.message())),
        )
    };
    let init = Object::new();
    let set = |key: &str, value: &JsValue| {
        let _ = Reflect::set(&init, &key.into(), value);
    };
    set("method", &request.method.as_str().into());
    let headers = Object::new();
    for (name, value) in &request.headers {
        let _ = Reflect::set(&headers, &name.as_str().into(), &value.as_str().into());
    }
    set("headers", &headers);
    if !request.body.is_empty() {
        set("body", &Uint8Array::from(request.body.as_slice()));
    }
    set("mode", &"cors".into());
    set("credentials", &"omit".into());
    // `AbortSignal.timeout`: the browser gives up after the request's time.
    if let Ok(abort) = Reflect::get(&js_sys::global(), &"AbortSignal".into())
        && let Ok(timeout) = Reflect::get(&abort, &"timeout".into())
        && let Some(timeout) = timeout.dyn_ref::<js_sys::Function>()
        && let Ok(signal) = timeout.call1(&abort, &(request.timeout.as_millis() as f64).into())
    {
        set("signal", &signal);
    }
    let response = JsFuture::from(js_fetch(&request.url, &init))
        .await
        .map_err(refused)?;
    let status = Reflect::get(&response, &"status".into())
        .ok()
        .and_then(|s| s.as_f64())
        .unwrap_or(0.0) as u16;
    let mut headers = Vec::new();
    if let Ok(list) = Reflect::get(&response, &"headers".into())
        && let Ok(Some(entries)) = js_sys::try_iter(&list)
    {
        for entry in entries.flatten() {
            let pair = Array::from(&entry);
            if let (Some(name), Some(value)) = (pair.get(0).as_string(), pair.get(1).as_string()) {
                headers.push((name, value));
            }
        }
    }
    let buffer = Reflect::get(&response, &"arrayBuffer".into())
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
        .ok_or_else(|| Refused("no body".into()))?
        .call0(&response)
        .map_err(refused)?;
    let body = JsFuture::from(js_sys::Promise::from(buffer))
        .await
        .map_err(refused)?;
    Ok(Response {
        status,
        headers,
        body: Uint8Array::new(&body).to_vec(),
    })
}

/// The relay to fall back on: the request's, else `HORIZON_WEB_RELAY`.
fn relay(request: &Request) -> Option<String> {
    request
        .relay
        .clone()
        .or_else(|| crate::env::var("HORIZON_WEB_RELAY").ok())
        .map(|r| r.trim().to_owned())
        .filter(|r| r.starts_with("ws://") || r.starts_with("wss://"))
}

/// `request` from the page's thread: `fetch`, else the relay.
pub async fn request_async(request: Request) -> Result<Response, String> {
    // fetch() drops a Cookie header (Realms' session): straight to the relay.
    let cookie = request
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("cookie"));
    let refused = if cookie {
        "the browser doesn't send cookies".to_owned()
    } else {
        match fetch(&request).await {
            Ok(response) => return Ok(response),
            Err(Refused(why)) => why,
        }
    };
    let Some(relay) = relay(&request) else {
        return Err(format!(
            "The browser can't reach {} ({refused}); a WebSocket relay (HORIZON_WEB_RELAY) could",
            host(&request.url)
        ));
    };
    let headers: Vec<(&str, &str)> = request
        .headers
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_str()))
        .collect();
    https::request(
        &relay,
        &request.method,
        &request.url,
        &headers,
        &request.body,
        request.timeout,
    )
    .await
}

/// `request` from a worker, which waits: it runs on the page's thread, whose event loop has
/// `fetch` and the relay's sockets. On the page's thread: an error (it awaits
/// [`request_async`]).
pub fn request(request: Request) -> Result<Response, String> {
    if super::on_page_thread() {
        return Err("HTTP on the page's thread is async (request_async)".into());
    }
    let wait = request.timeout + Duration::from_secs(10);
    let (sender, answer) = std::sync::mpsc::channel();
    super::run_on_page(Box::new(move || {
        wasm_bindgen_futures::spawn_local(async move {
            let _ = sender.send(request_async(request).await);
        });
    }));
    crate::sync::recv_timeout(&answer, wait)
        .unwrap_or_else(|_| Err("The request got no answer in time".into()))
}

/// The host of `url`, for a message that names no secret.
fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

#[cfg(target_feature = "atomics")]
thread_local! {
    /// The page's promises [`horizon_http_check`] made, by number.
    static CHECKS: std::cell::RefCell<Vec<Option<js_sys::Function>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The smoke test's check of a worker's [`request`] (tools/web_smoke.py): a Web Worker gets
/// `url`, its request running on the page's thread, and the promise resolves to
/// `"<status> <bytes>"` or the error.
#[cfg(target_feature = "atomics")]
#[wasm_bindgen]
pub fn horizon_http_check(url: String) -> js_sys::Promise {
    js_sys::Promise::new(&mut |resolve, _| {
        let id = CHECKS.with(|checks| {
            let mut checks = checks.borrow_mut();
            checks.push(Some(resolve));
            checks.len() - 1
        });
        let url = url.clone();
        let started = crate::thread::spawn_worker("HTTP check", move || {
            let answer = match request(Request::get(&url)) {
                Ok(r) => format!("{} {}", r.status, r.body.len()),
                Err(e) => e,
            };
            super::run_on_page(Box::new(move || {
                if let Some(resolve) =
                    CHECKS.with(|c| c.borrow_mut().get_mut(id).and_then(Option::take))
                {
                    let _ = resolve.call1(&JsValue::NULL, &answer.as_str().into());
                }
            }));
        });
        if let Err(e) = started {
            if let Some(resolve) =
                CHECKS.with(|c| c.borrow_mut().get_mut(id).and_then(Option::take))
            {
                let _ = resolve.call1(&JsValue::NULL, &e.to_string().as_str().into());
            }
        }
    })
}
