// Regression for issue #406: requests initiated by page JS (fetch/XHR/dynamic
// resource) must emit Network.requestWillBeSent / responseReceived so
// Puppeteer/Playwright `page.on('request'|'response')` observe them. On main
// only the static navigation subresources surfaced; a `fetch()` fired from the
// page produced no CDP Network event, so clients captured zero XHR/JSON
// responses (this is also the root cause of the Aviasales half of #394).

use obscura_cdp::dispatch::{dispatch, CdpContext};
use obscura_cdp::types::CdpRequest;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// Initial inline fetches must carry native metadata before navigation commits.
async fn serve() -> (String, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    for listener in [listener, peer] {
        let observed = observed.clone();
        tokio::spawn(async move {
            for _ in 0..12 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let observed = observed.clone();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let mut chunk = [0u8; 2048];
                    let header_end = loop {
                        let length = socket.read(&mut chunk).await.unwrap();
                        assert!(length > 0, "fixture request headers must complete");
                        bytes.extend_from_slice(&chunk[..length]);
                        assert!(bytes.len() <= 16_384, "fixture request headers are bounded");
                        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                            break end + 4;
                        }
                    };
                    let request = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                    let path = request.lines().next().unwrap().split_whitespace().nth(1).unwrap();
                    let headers = request.lines().skip(1).filter_map(|line| line.split_once(':'))
                        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim())).collect::<std::collections::HashMap<_, _>>();
                    let content_length = headers.get("content-length").map(|value| value.parse::<usize>().unwrap()).unwrap_or(0);
                    assert!(content_length <= 8192, "fixture request body is bounded");
                    while bytes.len() - header_end < content_length {
                        let length = socket.read(&mut chunk).await.unwrap();
                        assert!(length > 0, "fixture request body must complete");
                        bytes.extend_from_slice(&chunk[..length]);
                    }
                    let request_body = String::from_utf8(bytes[header_end..header_end + content_length].to_vec()).ok();
                    let site = headers.get("sec-fetch-site").copied().unwrap_or("");
                    let cookie = headers.get("cookie").is_some_and(|value|
                        value.split(';').any(|item| item.trim() == "metadata-session=fixture"));
                    if path.starts_with("/api/") {
                        observed.lock().unwrap().push(json!({"path":path,"site":site,"cookie":cookie}));
                    }
                    let redirect = match path {
                        "/api/start.json" if site == "same-origin" && cookie => Some("/api/data.json".to_string()),
                        "/api/same-site-start" => Some(format!("http://{peer_addr}/api/same-site-hop")),
                        "/api/cross-site-start" => Some(format!("http://localhost:{}/api/cross-site-hop", peer_addr.port())),
                        "/api/same-site-hop" | "/api/cross-site-hop" => Some(format!("http://{addr}/api/site-return.json")),
                        _ => None,
                    };
                    let (status, content_type, body) = match path {
                        "/api/start.json" if redirect.is_none() => (403, "application/json", r#"{"error":"cookie-auth metadata missing"}"#.to_string()),
                        "/api/data.json" => (200, "application/json", r#"{"value":42}"#.to_string()),
                        "/api/body.json" => (200, "application/json", json!({"body":request_body}).to_string()),
                        "/api/missing.json" => (404, "application/json", r#"{"error":"missing"}"#.to_string()),
                        "/api/site-return.json" => (200, "application/json", json!({"site":site,"cookie":cookie}).to_string()),
                        _ => (200, "text/html", r#"<html><head></head><body>
<div id="r">stage1</div>
<script>
window.__done = new Promise(function (resolve) {
  fetch("/api/start.json")
    .then(function (r) { return r.json(); })
    .then(function (d) { document.getElementById("r").textContent = "got:" + d.value; resolve("ok"); })
    .catch(function (e) { resolve("err:" + e); });
});
</script>
</body></html>"#.to_string()),
                    };
                    let response = if let Some(location) = redirect {
                        format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nAccess-Control-Allow-Origin: http://{addr}\r\nAccess-Control-Allow-Credentials: true\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    } else {
                        let cookie_header = if content_type == "text/html" {
                            "Set-Cookie: metadata-session=fixture; HttpOnly; SameSite=Lax; Path=/\r\n"
                        } else {
                            ""
                        };
                        format!("HTTP/1.1 {status} Response\r\nContent-Type: {content_type}\r\n{cookie_header}Access-Control-Allow-Origin: http://{addr}\r\nAccess-Control-Allow-Credentials: true\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                    };
                    socket.write_all(response.as_bytes()).await.unwrap();
                });
            }
        });
    }
    (format!("http://{addr}/"), observed)
}

async fn cdp(ctx: &mut CdpContext, id: u64, method: &str, params: Value, session_id: &str) -> Value {
    let resp = dispatch(
        &CdpRequest {
            id,
            method: method.to_string(),
            params,
            session_id: Some(session_id.to_string()),
        },
        ctx,
    )
    .await;
    assert!(resp.error.is_none(), "CDP {method} failed: {:?}", resp.error);
    resp.result.unwrap_or_else(|| json!({}))
}

// Collect the request URLs from every Network.requestWillBeSent currently
// queued in ctx.pending_events, then clear the queue.
fn drain_request_urls(ctx: &mut CdpContext) -> Vec<String> {
    let urls = ctx
        .pending_events
        .iter()
        .filter(|e| e.method == "Network.requestWillBeSent")
        .filter_map(|e| e.params.get("request").and_then(|r| r.get("url")).and_then(|u| u.as_str()).map(str::to_string))
        .collect();
    ctx.pending_events.clear();
    urls
}

// The requestId that Network.responseReceived reported for the given URL.
fn response_request_id(ctx: &CdpContext, url_needle: &str) -> Option<String> {
    ctx.pending_events
        .iter()
        .find(|e| {
            e.method == "Network.responseReceived"
                && e.params
                    .get("response")
                    .and_then(|r| r.get("url"))
                    .and_then(|u| u.as_str())
                    .map(|u| u.contains(url_needle))
                    .unwrap_or(false)
        })
        .and_then(|e| e.params.get("requestId").and_then(|v| v.as_str()).map(str::to_string))
}

#[tokio::test(flavor = "current_thread")]
async fn js_fetch_emits_network_request_and_response() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let (base, observed) = serve().await;
    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    let session_id = "session-1";
    ctx.sessions.insert(session_id.to_string(), page_id.clone());

    // An ordinary fetch() is not load-delaying in Chromium: `load` may fire
    // while its response is still pending. Ask for networkidle0 explicitly so
    // this output-level assertion observes the completed request without
    // turning every load navigation into an implicit global settle.
    cdp(
        &mut ctx,
        1,
        "Page.navigate",
        json!({"url": base, "waitUntil": "networkidle0"}),
        session_id,
    )
    .await;

    assert_eq!(*observed.lock().unwrap(), vec![
        json!({"path":"/api/start.json","site":"same-origin","cookie":true}),
        json!({"path":"/api/data.json","site":"same-origin","cookie":true}),
    ], "inline fetch must send cookie-auth metadata without an interception repair");

    // The final fetched JSON URL must appear as a requestWillBeSent event.
    let request_urls = ctx
        .pending_events
        .iter()
        .filter(|e| e.method == "Network.requestWillBeSent")
        .filter_map(|e| e.params.get("request").and_then(|r| r.get("url")).and_then(|u| u.as_str()).map(str::to_string))
        .collect::<Vec<_>>();
    assert!(
        request_urls.iter().any(|u| u.contains("/api/data.json")),
        "script-initiated fetch must emit Network.requestWillBeSent; saw {request_urls:?}"
    );
    // And its response body must be resolvable via the same requestId, so a
    // client can read the captured JSON.
    let request_id = response_request_id(&ctx, "/api/data.json")
        .expect("fetch must emit Network.responseReceived with a requestId");
    let body = cdp(
        &mut ctx,
        2,
        "Network.getResponseBody",
        json!({"requestId": request_id}),
        session_id,
    )
    .await;
    assert_eq!(
        body.get("body").and_then(|b| b.as_str()),
        Some("{\"value\":42}"),
        "Network.getResponseBody must return the script-fetched JSON"
    );

    for (id, kind, expected_site, peer_cookie) in [
        (3, "same-site", "same-site", true),
        (4, "cross-site", "cross-site", false),
    ] {
        observed.lock().unwrap().clear();
        let response = cdp(&mut ctx, id, "Runtime.evaluate", json!({
            "expression":format!("fetch('/api/{kind}-start',{{credentials:'include'}}).then(response=>response.json())"),
            "awaitPromise":true,"returnByValue":true,
        }), session_id).await;
        assert_eq!(response["result"]["value"], json!({"site":expected_site,"cookie":true}));
        assert_eq!(*observed.lock().unwrap(), vec![
            json!({"path":format!("/api/{kind}-start"),"site":"same-origin","cookie":true}),
            json!({"path":format!("/api/{kind}-hop"),"site":expected_site,"cookie":peer_cookie}),
            json!({"path":"/api/site-return.json","site":expected_site,"cookie":true}),
        ], "the redirect URL list must retain its least-trusted site");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn js_fetch_post_body_is_observable_without_interception() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let (base, _observed) = serve().await;
    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    let session = "body-session";
    ctx.sessions.insert(session.to_string(), page_id);
    cdp(&mut ctx, 1, "Page.navigate", json!({"url":base,"waitUntil":"networkidle0"}), session).await;
    assert!(ctx.pending_events.iter().filter(|event| event.method == "Network.requestWillBeSent")
        .all(|event| event.params["request"].get("postData").is_none()), "initial GET requests must omit postData");

    let unicode_body = r#"{"message":"hello, café"}"#;
    let cases = [
        ("POST", serde_json::to_string(unicode_body).unwrap(), Some(unicode_body), Some(unicode_body)),
        ("POST", "''".to_string(), Some(""), None),
        ("POST", "new Uint8Array([255,0])".to_string(), None, None),
        ("GET", "undefined".to_string(), Some(""), None),
    ];
    let mut id = 2;
    for (method, body, expected_wire, expected_post_data) in cases {
        ctx.pending_events.clear();
        let response = cdp(&mut ctx, id, "Runtime.evaluate", json!({
            "expression":format!("fetch('/api/body.json',{{method:'{method}',body:{body}}}).then(response=>response.json())"),
            "awaitPromise":true,"returnByValue":true,
        }), session).await;
        id += 1;
        assert_eq!(response["result"]["value"], json!({"body":expected_wire}), "the real server must receive the expected body");
        let request = ctx.pending_events.iter().find(|event| event.method == "Network.requestWillBeSent"
            && event.params["request"]["url"].as_str().is_some_and(|url| url.ends_with("/api/body.json")))
            .expect("runtime request must emit its start event");
        assert_eq!(request.params["request"]["method"], method);
        if let Some(expected) = expected_post_data {
            assert_eq!(request.params["request"]["postData"], expected, "CDP must preserve the exact UTF-8 request body");
        } else {
            assert!(request.params["request"].get("postData").is_none(), "absent, empty and binary request bodies must omit postData");
        }
        let request_id = &request.params["requestId"];
        assert!(ctx.pending_events.iter().any(|event| event.method == "Network.responseReceived"
            && &event.params["requestId"] == request_id && event.params["response"]["status"] == 200),
            "response identity must match the body-bearing request");
    }

    let (intercept_tx, mut intercepted) = tokio::sync::mpsc::unbounded_channel();
    ctx.intercept_tx = Some(intercept_tx);
    cdp(&mut ctx, id, "Fetch.enable", json!({"patterns":[{"urlPattern":format!("{base}api/body.json")}]}), session).await;
    id += 1;
    ctx.pending_events.clear();
    let replacement_body = r#"{"message":"continued, café"}"#;
    let continuation = async {
        let request: obscura_js::ops::InterceptedRequest = intercepted.recv().await.expect("POST must reach the existing interception boundary");
        assert_eq!(request.method, "POST");
        let request_id = request.request_id;
        assert!(request.resolver.send(obscura_js::ops::InterceptResolution::Continue {
            url: None, method: None, headers: None, body: Some(replacement_body.as_bytes().to_vec()),
        }).is_ok(), "the intercepted body rewrite must be delivered");
        request_id
    };
    let (response, request_id) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(cdp(&mut ctx, id, "Runtime.evaluate", json!({
            "expression":format!("fetch('/api/body.json',{{method:'POST',body:{}}}).then(response=>response.json())", serde_json::to_string(unicode_body).unwrap()),
            "awaitPromise":true,"returnByValue":true,
        }), session), continuation)
    }).await.expect("the intercepted POST and body rewrite must complete");
    assert_eq!(response["result"]["value"], json!({"body":replacement_body}), "the server must receive the continued body rather than the original body");
    assert!(ctx.pending_events.iter().any(|event| event.method == "Network.responseReceived"
        && event.params["requestId"] == request_id && event.params["response"]["status"] == 200),
        "continuation must preserve the request's response identity");
}

#[tokio::test(flavor = "current_thread")]
async fn navigation_without_script_fetch_is_unaffected() {
    // A page that issues no script fetch must still emit exactly its document
    // request, proving the #406 change adds nothing spurious.
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = socket.read(&mut buf).await.unwrap();
        let body = "<html><body>plain</body></html>";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(resp.as_bytes()).await;
    });
    let base = format!("http://{addr}/");

    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    let session_id = "session-1";
    ctx.sessions.insert(session_id.to_string(), page_id.clone());

    cdp(&mut ctx, 1, "Page.navigate", json!({"url": base, "waitUntil": "load"}), session_id).await;

    let urls = drain_request_urls(&mut ctx);
    assert!(
        urls.iter().any(|u| u == &base || u.starts_with(&base)),
        "the document request must still be emitted; saw {urls:?}"
    );
    assert!(
        !urls.iter().any(|u| u.contains("/api/")),
        "no spurious script-fetch events for a page that makes none; saw {urls:?}"
    );
}

#[cfg(feature = "stealth")]
async fn stealth_context() -> (CdpContext, String, String) {
    let mut ctx = CdpContext::new();
    ctx.default_context = std::sync::Arc::new(obscura_browser::BrowserContext::with_storage_and_network(
        "stealth-events".into(), None, true, None, None, true,
    ));
    let page = ctx.create_page();
    let session = "stealth-session".to_string();
    ctx.sessions.insert(session.clone(), page);
    let (base, _observed) = serve().await;
    cdp(&mut ctx, 1, "Page.navigate", json!({"url":base,"waitUntil":"networkidle0"}), &session).await;
    (ctx, session, base)
}

#[cfg(feature = "stealth")]
fn recorded_live_request_id(ctx: &mut CdpContext, session: &str) -> String {
    // dispatch() does not run the WebSocket server's post-command drain.
    let page = ctx.get_session_page_mut(&Some(session.to_string())).unwrap();
    page.sync_js_network_events();
    let event = page.network_events.pop().expect("scripted request recorded by the transport");
    assert!(event.url.ends_with("/api/data.json"));
    assert_eq!(event.status, 200);
    assert_eq!(event.method, "GET");
    assert_eq!(event.body_size, 12);
    event.request_id
}

#[cfg(feature = "stealth")]
#[tokio::test(flavor = "current_thread")]
async fn stealth_fetch_records_redirect_identity_and_readable_response_body() {
    let (mut ctx, session, _base) = stealth_context().await;
    let request_id = response_request_id(&ctx, "/api/data.json").expect("stealth response event");
    let events = ctx.pending_events.iter().filter(|event|
        event.method.starts_with("Network.") && event.params["requestId"] == request_id
    ).collect::<Vec<_>>();
    assert_eq!(events.iter().map(|event| event.method.as_str()).collect::<Vec<_>>(),
        ["Network.requestWillBeSent", "Network.responseReceived", "Network.loadingFinished"]);
    assert_eq!(events[1].params["response"]["status"], 200);
    assert_eq!(events[2].params["encodedDataLength"], 12);
    let body = cdp(&mut ctx, 2, "Network.getResponseBody", json!({"requestId":request_id}), &session).await;
    assert_eq!(body["body"], r#"{"value":42}"#);
    assert_eq!(body["base64Encoded"], false);
}

#[cfg(feature = "stealth")]
#[tokio::test(flavor = "current_thread")]
async fn stealth_xhr_records_events_after_navigation() {
    let (mut ctx, session, _base) = stealth_context().await;
    ctx.pending_events.clear();
    let response = cdp(&mut ctx, 2, "Runtime.evaluate", json!({
        "expression":"new Promise((resolve,reject)=>{let xhr=new XMLHttpRequest();xhr.open('GET','/api/data.json');xhr.onload=()=>resolve(xhr.responseText);xhr.onerror=reject;xhr.send()})",
        "awaitPromise":true,"returnByValue":true
    }), &session).await;
    assert_eq!(response["result"]["value"], r#"{"value":42}"#);
    let request_id = recorded_live_request_id(&mut ctx, &session);
    let body = cdp(&mut ctx, 3, "Network.getResponseBody", json!({"requestId":request_id}), &session).await;
    assert_eq!(body["body"], r#"{"value":42}"#);
}

#[cfg(feature = "stealth")]
#[tokio::test(flavor = "current_thread")]
async fn stealth_events_survive_disabled_response_body_retention() {
    std::env::set_var("OBSCURA_NETWORK_BODY_BUFFER_ENTRIES", "0");
    let (mut ctx, session, _base) = stealth_context().await;
    let request_id = response_request_id(&ctx, "/api/data.json").expect("retention opt-out must not disable observation");
    let response = dispatch(&CdpRequest { id: 2, method: "Network.getResponseBody".into(),
        params: json!({"requestId":request_id}), session_id: Some(session) }, &mut ctx).await;
    assert!(response.error.is_some(), "opt-out must not retain the body");
}

#[cfg(feature = "stealth")]
#[tokio::test(flavor = "current_thread")]
async fn stealth_response_body_buffer_evicts_old_requests() {
    std::env::set_var("OBSCURA_NETWORK_BODY_BUFFER_ENTRIES", "2");
    let (mut ctx, session, _base) = stealth_context().await;
    let oldest = response_request_id(&ctx, "/api/data.json").expect("first response");
    let mut latest = String::new();
    for id in 2..6 {
        ctx.pending_events.clear();
        let response = cdp(&mut ctx, id, "Runtime.evaluate", json!({"expression":"fetch('/api/data.json').then(r=>r.text())", "awaitPromise":true, "returnByValue":true}), &session).await;
        assert_eq!(response["result"]["value"], r#"{"value":42}"#);
        latest = recorded_live_request_id(&mut ctx, &session);
    }
    let evicted = dispatch(&CdpRequest { id: 6, method: "Network.getResponseBody".into(),
        params: json!({"requestId":oldest}), session_id: Some(session.clone()) }, &mut ctx).await;
    assert!(evicted.error.is_some(), "bounded cache must evict old responses");
    assert_eq!(cdp(&mut ctx, 7, "Network.getResponseBody", json!({"requestId":latest}), &session).await["body"], r#"{"value":42}"#);
}

#[cfg(feature = "stealth")]
#[tokio::test(flavor = "current_thread")]
async fn stealth_http_error_response_keeps_status_and_body() {
    let (mut ctx, session, _base) = stealth_context().await;
    let response = cdp(&mut ctx, 2, "Runtime.evaluate", json!({"expression":"fetch('/api/missing.json').then(async r=>{await r.text();return r.status})","awaitPromise":true,"returnByValue":true}), &session).await;
    assert_eq!(response["result"]["value"].as_f64(), Some(404.0));
    let page = ctx.get_session_page_mut(&Some(session.clone())).unwrap();
    page.sync_js_network_events();
    let event = page.network_events.pop().expect("HTTP error response must be recorded");
    assert_eq!(event.status, 404);
    assert!(event.url.ends_with("/api/missing.json"));
    assert_eq!(cdp(&mut ctx, 3, "Network.getResponseBody", json!({"requestId":event.request_id}), &session).await["body"], r#"{"error":"missing"}"#);
}
