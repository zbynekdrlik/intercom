//! `fetch` for the page: the status and the body text, or `Err` when nothing answered. Never a
//! console line: every failure becomes page state.

use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Headers, Request, RequestCache, RequestInit, Response};

/// An answer: the HTTP status and the body.
pub struct Answer {
    pub ok: bool,
    pub text: String,
}

async fn send(url: &str, init: &RequestInit) -> Result<Answer, ()> {
    let window = web_sys::window().ok_or(())?;
    let request = Request::new_with_str_and_init(url, init).map_err(|_| ())?;
    let response: Response = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|_| ())?
        .dyn_into()
        .map_err(|_| ())?;
    let text = JsFuture::from(response.text().map_err(|_| ())?)
        .await
        .map_err(|_| ())?
        .as_string()
        .unwrap_or_default();
    Ok(Answer {
        ok: response.ok(),
        text,
    })
}

/// `GET` without the HTTP cache (`cache: "no-store"`).
pub async fn get_fresh(url: &str) -> Result<Answer, ()> {
    let init = RequestInit::new();
    init.set_method("GET");
    init.set_cache(RequestCache::NoStore);
    send(url, &init).await
}

/// `PUT` a JSON body.
pub async fn put_json(url: &str, body: &str) -> Result<Answer, ()> {
    let init = RequestInit::new();
    init.set_method("PUT");
    let headers = Headers::new().map_err(|_| ())?;
    headers
        .set("Content-Type", "application/json")
        .map_err(|_| ())?;
    init.set_headers(&headers);
    init.set_body(&wasm_bindgen::JsValue::from_str(body));
    send(url, &init).await
}
