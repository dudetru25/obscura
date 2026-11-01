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
            for _ in 0..24 {
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
    let unicode_body = r#"{"message":"hello, café"}"#;
    let cases = [
        ("POST", serde_json::to_string(unicode_body).unwrap(), Some(unicode_body), Some(unicode_body)),
        ("POST", "''".to_string(), Some(""), None),
        ("POST", "new Uint8Array([255,0])".to_string(), None, None),
        ("GET", "undefined".to_string(), Some(""), None),
    ];
    let mut id = 2;
    for (method, body, expected_wire, expected_post_data) in cases {
        let injection = cdp(&mut ctx, id, "Page.addScriptToEvaluateOnNewDocument", json!({
            "source":format!("globalThis.__bodyResponse=fetch('/api/body.json',{{method:'{method}',body:{body}}}).then(response=>response.json());"),
        }), session).await;
        id += 1;
        ctx.pending_events.clear();
        // Navigation runs the existing network-event serializer. Direct
        // dispatch does not run the WebSocket server's post-command drain.
        cdp(&mut ctx, id, "Page.navigate", json!({"url":base,"waitUntil":"networkidle0"}), session).await;
        id += 1;
        let response = cdp(&mut ctx, id, "Runtime.evaluate", json!({
            "expression":"globalThis.__bodyResponse",
            "awaitPromise":true,"returnByValue":true,
        }), session).await;
        id += 1;
        cdp(&mut ctx, id, "Page.removeScriptToEvaluateOnNewDocument", json!({"identifier":injection["identifier"]}), session).await;
        id += 1;
        assert!(ctx.pending_events.iter().filter(|event| event.method == "Network.requestWillBeSent"
            && event.params["request"]["method"] == "GET")
            .all(|event| event.params["request"].get("postData").is_none()), "GET requests must omit postData");
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
    let page = ctx.get_session_page_mut(&Some(session.to_string())).unwrap();
    page.sync_js_network_events();
    assert!(page.network_events.iter().any(|event|
        event.request_id == request_id && event.status == 200 && event.intercepted),
        "continuation must preserve the transport request's response identity");
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

// EventSource fixture: streams stay open until a test barrier releases the
// next write, so delivery before EOF is observable.
#[derive(Default)]
struct StreamFixture {
    requests: std::sync::Mutex<Vec<String>>,
    send_data: tokio::sync::Notify,
    connected: tokio::sync::Notify,
    send_change: tokio::sync::Notify,
    after_close: tokio::sync::Notify,
    stream_closed: tokio::sync::Notify,
}

impl StreamFixture {
    fn record(&self, entry: String) {
        self.requests.lock().unwrap().push(entry);
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    // Records whether the client closed the stream connection.
    async fn record_peer_close(&self, socket: &mut tokio::net::TcpStream, path: &str) {
        let mut buffer = [0u8; 256];
        let closed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while matches!(socket.read(&mut buffer).await, Ok(length) if length > 0) {}
        }).await;
        self.record(format!("{path} {}", if closed.is_ok() { "closed" } else { "still open after 10s" }));
        self.stream_closed.notify_one();
    }
}

async fn read_request_head(socket: &mut tokio::net::TcpStream) -> (String, String, std::collections::HashMap<String, String>) {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 2048];
    while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
        let length = socket.read(&mut chunk).await.unwrap();
        assert!(length > 0, "fixture request headers must complete");
        bytes.extend_from_slice(&chunk[..length]);
        assert!(bytes.len() <= 16_384, "fixture request headers are bounded");
    }
    let request = String::from_utf8(bytes).unwrap();
    let mut request_line = request.lines().next().unwrap().split_whitespace();
    let method = request_line.next().unwrap().to_string();
    let path = request_line.next().unwrap().to_string();
    let headers = request.lines().skip(1).filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string())).collect();
    (method, path, headers)
}

async fn serve_event_streams() -> (String, u16, std::sync::Arc<StreamFixture>) {
    const STREAM_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
    const CORS_STREAM_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n";
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = std::sync::Arc::new(StreamFixture::default());
    let server = fixture.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let fixture = server.clone();
            tokio::spawn(async move {
                let (method, path, headers) = read_request_head(&mut socket).await;
                let last_event_id = headers.get("last-event-id");
                if method == "OPTIONS" {
                    fixture.record(format!("OPTIONS {path}"));
                    socket.write_all(b"HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: last-event-id, x-probe\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                    return;
                }
                if path != "/" && path != "/next" {
                    fixture.record(match last_event_id {
                        Some(id) => format!("{path} last-event-id={id}"),
                        None => path.clone(),
                    });
                }
                let reply = |status: u16, content_type: &str, body: &str| format!(
                    "HTTP/1.1 {status} Response\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len());
                match path.as_str() {
                    "/sse" => {
                        // Headers arrive alone, so `open` must not wait for data.
                        socket.write_all(STREAM_HEAD.as_bytes()).await.unwrap();
                        fixture.send_data.notified().await;
                        socket.write_all(b": open\n\ndata: hello\n\nevent: ready\nid: r1\ndata: go\n\n").await.unwrap();
                        fixture.send_change.notified().await;
                        socket.write_all(b"event: change\ndata: next\n\n").await.unwrap();
                        fixture.after_close.notified().await;
                        // The client may already have reset the closed stream.
                        let _ = socket.write_all(b"event: late\ndata: late\n\n").await;
                        fixture.record_peer_close(&mut socket, "/sse").await;
                    }
                    "/frame-hold" => {
                        fixture.connected.notify_one();
                        fixture.record_peer_close(&mut socket, "/frame-hold").await;
                    }
                    "/frame-sse" => {
                        socket.write_all(format!("{CORS_STREAM_HEAD}retry: 1\ndata: one\n\n").as_bytes()).await.unwrap();
                        fixture.record_peer_close(&mut socket, "/frame-sse").await;
                    }
                    "/sse-navigate" => {
                        socket.write_all(format!("{STREAM_HEAD}event: ready\ndata: go\n\n").as_bytes()).await.unwrap();
                        fixture.record_peer_close(&mut socket, "/sse-navigate").await;
                    }
                    "/reconnect" if last_event_id.is_none() => {
                        socket.write_all(format!("{STREAM_HEAD}\u{feff}: comment\r\nretry: 1\r\nid: a1\r\ndata: line1\rdata: line2\r\n\r\n").as_bytes()).await.unwrap();
                        socket.write_all(b"data: caf\xC3").await.unwrap();
                        socket.write_all(b"\xA9\n\nid: bad\0x\ndata: nul\n\ndata: partial").await.unwrap();
                        socket.shutdown().await.unwrap();
                    }
                    "/reconnect" => {
                        socket.write_all(format!("{STREAM_HEAD}data: second\n\n").as_bytes()).await.unwrap();
                        fixture.record_peer_close(&mut socket, "/reconnect").await;
                    }
                    "/cors-reconnect" if last_event_id.is_none() => {
                        socket.write_all(format!("{CORS_STREAM_HEAD}id: c1\nretry: 1\ndata: first\n\n").as_bytes()).await.unwrap();
                        socket.shutdown().await.unwrap();
                    }
                    "/cors-reconnect" => {
                        socket.write_all(format!("{CORS_STREAM_HEAD}data: second\n\n").as_bytes()).await.unwrap();
                        fixture.record_peer_close(&mut socket, "/cors-reconnect").await;
                    }
                    "/cors-preflight" => socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 8\r\nConnection: close\r\n\r\napproved").await.unwrap(),
                    "/status" => socket.write_all(reply(500, "text/event-stream", "data: x\n\n").as_bytes()).await.unwrap(),
                    "/mime" => socket.write_all(reply(200, "text/plain", "data: x\n\n").as_bytes()).await.unwrap(),
                    "/cors" | "/blocked" => socket.write_all(format!("{STREAM_HEAD}data: x\n\n").as_bytes()).await.unwrap(),
                    "/api/during" => socket.write_all(reply(200, "text/plain", "during").as_bytes()).await.unwrap(),
                    "/api/refetch" => socket.write_all(reply(200, "text/plain", "refetched").as_bytes()).await.unwrap(),
                    "/api/after-close" => {
                        fixture.after_close.notify_one();
                        fixture.stream_closed.notified().await;
                        socket.write_all(reply(200, "text/plain", "after").as_bytes()).await.unwrap();
                    }
                    _ => socket.write_all(reply(200, "text/html", "<html><body>events</body></html>").as_bytes()).await.unwrap(),
                }
            });
        }
    });
    (format!("http://{addr}/"), addr.port(), fixture)
}

async fn event_stream_page(base: &str, session: &str) -> CdpContext {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let mut ctx = CdpContext::new();
    let page = ctx.create_page();
    ctx.sessions.insert(session.to_string(), page);
    cdp(&mut ctx, 1, "Page.navigate", json!({"url":base,"waitUntil":"load"}), session).await;
    ctx
}

// A bounded wait for a page promise. A timeout reports what the fixture
// server saw, which names the missing stream step.
async fn await_page(ctx: &mut CdpContext, id: u64, expression: &str, session: &str, fixture: &StreamFixture) -> Value {
    let response = dispatch(&CdpRequest {
        id, method: "Runtime.evaluate".into(), session_id: Some(session.into()),
        params: json!({"expression":expression,"awaitPromise":true,"returnByValue":true,"timeout":10_000}),
    }, ctx).await;
    assert!(response.error.is_none(), "`{expression}` did not settle: {:?}; server saw {:?}", response.error, fixture.requests());
    response.result.unwrap()["result"]["value"].clone()
}

#[tokio::test(flavor = "current_thread")]
async fn event_source_delivers_before_eof_keeps_fetch_live_and_close_cancels() {
    let (base, _port, fixture) = serve_event_streams().await;
    let session = "event-source";
    let mut ctx = event_stream_page(&base, session).await;
    cdp(&mut ctx, 2, "Runtime.evaluate", json!({"expression":r#"
        globalThis.__log = [];
        globalThis.__reported = [];
        globalThis.reportError = (error) => __reported.push(error.message);
        globalThis.__source = new EventSource('/sse');
        __source.onopen = (event) => {
            __log.push('open:' + event.target.readyState + ':' + (event.currentTarget === __source));
            throw new Error('event-source-handler');
        };
        __source.addEventListener('open', () => { throw new Error('event-source-listener'); });
        __source.onmessage = (event) => __log.push('message:' + event.data + ':' + (event.target === __source) + ':' + (event.currentTarget === __source));
        __source.onerror = () => __log.push('error:' + __source.readyState);
        globalThis.__opened = new Promise((resolve) => __source.addEventListener('open', () => resolve(__log.slice())));
        globalThis.__ready = new Promise((resolve) => __source.addEventListener('ready', (event) => {
            __log.push('ready:' + event.data + ':' + event.lastEventId + ':' + (event.origin === location.origin));
            globalThis.__during = fetch('/api/during').then((response) => response.text())
                .then((text) => { __log.push('fetch:' + text); resolve(__log.slice()); });
        }));
        globalThis.__changed = new Promise((resolve) => __source.addEventListener('change', async (event) => {
            await __during;
            __log.push('change:' + event.data + ':' + event.lastEventId);
            __log.push('refetch:' + await (await fetch('/api/refetch')).text());
            __source.close();
            __log.push('closed:' + __source.readyState);
            __log.push('after:' + await (await fetch('/api/after-close')).text());
            resolve(__log.slice());
        }));
        undefined
    "#}), session).await;

    // The server has sent only the response headers.
    let opened = await_page(&mut ctx, 7, "__opened", session, &fixture).await;
    assert_eq!(fixture.requests(), ["/sse"]);
    assert_eq!(opened, json!(["open:1:true"]), "open must follow the headers, before any data");
    let reported = cdp(&mut ctx, 8, "Runtime.evaluate", json!({"expression":"__reported","returnByValue":true}), session).await;
    assert_eq!(reported["result"]["value"], json!(["event-source-handler", "event-source-listener"]), "callback errors must be reported without stopping later listeners");
    fixture.send_data.notify_one();

    // The server has sent `ready` and holds the stream open.
    let ready = await_page(&mut ctx, 3, "__ready", session, &fixture).await;
    assert_eq!(ready, json!(["open:1:true", "message:hello:true:true", "ready:go:r1:true", "fetch:during"]));
    assert_eq!(fixture.requests(), ["/sse", "/api/during"], "the stream must still be open");
    let live = cdp(&mut ctx, 4, "Runtime.evaluate", json!({"expression":"[__source.readyState, 6 * 7]","returnByValue":true}), session).await;
    assert_eq!(live["result"]["value"], json!([1, 42]), "evaluate must stay live while the stream is open");

    fixture.send_change.notify_one();
    let changed = await_page(&mut ctx, 5, "__changed", session, &fixture).await;
    assert_eq!(changed, json!([
        "open:1:true", "message:hello:true:true", "ready:go:r1:true", "fetch:during",
        "change:next:r1", "refetch:refetched", "closed:2", "after:after",
    ]));
    // /api/after-close answers only after the server saw the stream close.
    assert_eq!(fixture.requests(), ["/sse", "/api/during", "/api/refetch", "/api/after-close", "/sse closed"]);
    let after = cdp(&mut ctx, 6, "Runtime.evaluate", json!({"expression":"[__source.readyState, __log.length]","returnByValue":true}), session).await;
    assert_eq!(after["result"]["value"], json!([2, 8]), "a closed source must not deliver late events");
}

#[tokio::test(flavor = "current_thread")]
async fn event_source_parses_the_stream_and_reconnects_with_last_event_id() {
    let (base, _port, fixture) = serve_event_streams().await;
    let session = "event-source-reconnect";
    let mut ctx = event_stream_page(&base, session).await;
    let log = await_page(&mut ctx, 2, r#"new Promise((resolve) => {
        const log = [];
        const source = new EventSource('/reconnect');
        source.onopen = () => log.push('open:' + source.readyState);
        source.onerror = () => log.push('error:' + source.readyState);
        source.onmessage = (event) => {
            log.push(event.data + '|' + event.lastEventId);
            if (event.data === 'second') { source.close(); resolve(log); }
        };
    })"#, session, &fixture).await;
    assert_eq!(log, json!([
        "open:1", "line1\nline2|a1", "caf\u{e9}|a1", "nul|a1", "error:0", "open:1", "second|a1",
    ]), "BOM, CR/CRLF, comments, split UTF-8, NUL ids and the incomplete EOF event follow the HTML parser");
    tokio::time::timeout(std::time::Duration::from_secs(10), fixture.stream_closed.notified()).await
        .unwrap_or_else(|_| panic!("close must end the reconnected stream; server saw {:?}", fixture.requests()));
    assert_eq!(fixture.requests(), ["/reconnect", "/reconnect last-event-id=a1", "/reconnect closed"]);
}

#[tokio::test(flavor = "current_thread")]
async fn event_source_fails_closed_for_status_mime_cors_and_blocked_urls() {
    let (base, port, fixture) = serve_event_streams().await;
    let session = "event-source-policy";
    let mut ctx = event_stream_page(&base, session).await;
    cdp(&mut ctx, 2, "Network.setBlockedURLs", json!({"urls":["/blocked"]}), session).await;
    let cases = [
        ("/status".to_string(), "a non-200 status"),
        ("/mime".to_string(), "a MIME type other than text/event-stream"),
        (format!("http://localhost:{port}/cors"), "a cross-origin stream without CORS approval"),
        ("/blocked".to_string(), "a blocked URL"),
    ];
    for (id, (url, reason)) in (3..).zip(cases) {
        let result = await_page(&mut ctx, id, &format!(r#"new Promise((resolve) => {{
            const log = [];
            const source = new EventSource({url:?});
            source.onopen = () => log.push('open');
            source.onmessage = () => log.push('message');
            source.onerror = () => resolve([source.readyState, log]);
        }})"#), session, &fixture).await;
        assert_eq!(result, json!([2, []]), "{reason} must fail the connection without reconnecting");
    }
    assert_eq!(fixture.requests(), ["/status", "/mime", "/cors"], "blocked URLs must not reach the network");
}

#[tokio::test(flavor = "current_thread")]
async fn navigation_cancels_an_open_event_source() {
    let (base, _port, fixture) = serve_event_streams().await;
    let session = "event-source-navigation";
    let mut ctx = event_stream_page(&base, session).await;
    let ready = await_page(&mut ctx, 2, r#"new Promise((resolve) => {
        new EventSource('/sse-navigate').addEventListener('ready', (event) => resolve(event.data));
    })"#, session, &fixture).await;
    assert_eq!(ready, "go");
    cdp(&mut ctx, 3, "Page.navigate", json!({"url":format!("{base}next"),"waitUntil":"load"}), session).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), fixture.stream_closed.notified()).await
        .unwrap_or_else(|_| panic!("navigation must close the stream; server saw {:?}", fixture.requests()));
    assert_eq!(fixture.requests(), ["/sse-navigate", "/sse-navigate closed"]);
}

// The host keeps the old frame realm and its source. Retiring the frame must
// still close the connecting or open stream and deliver nothing to the realm.
#[tokio::test(flavor = "current_thread")]
async fn retiring_a_frame_stops_its_event_source_without_late_callbacks() {
    let (base, port, fixture) = serve_event_streams().await;
    let session = "event-source-frame";
    let mut ctx = event_stream_page(&base, session).await;
    let cases = [
        ("/frame-hold", "__frame.remove()", vec![], "removing a frame while its stream waits for headers"),
        ("/frame-sse", "__frame.src = '/next'", vec!["open:1", "message:1"], "replacing the src of a frame with an open stream"),
    ];
    for (id, (path, retire, delivered, reason)) in (10..).step_by(10).zip(cases) {
        let seen = fixture.requests().len();
        cdp(&mut ctx, id, "Runtime.evaluate", json!({"expression":format!(r#"
            globalThis.__frame = document.body.appendChild(document.createElement('iframe'));
            globalThis.__oldWindow = __frame.contentWindow;
            globalThis.__oldSource = new __oldWindow.EventSource('http://127.0.0.1:{port}{path}');
            globalThis.__frameLog = [];
            for (const type of ['open', 'message', 'error']) {{
                __oldSource['on' + type] = () => __frameLog.push(type + ':' + __oldSource.readyState);
            }}
            globalThis.__delivered = new Promise((resolve) => __oldSource.addEventListener('message', () => resolve(__frameLog.slice())));
            undefined
        "#)}), session).await;
        if delivered.is_empty() {
            tokio::time::timeout(std::time::Duration::from_secs(10), fixture.connected.notified()).await
                .unwrap_or_else(|_| panic!("{reason}: the request must arrive; server saw {:?}", fixture.requests()));
        } else {
            assert_eq!(await_page(&mut ctx, id + 1, "__delivered", session, &fixture).await, json!(delivered), "{reason}");
        }
        cdp(&mut ctx, id + 2, "Runtime.evaluate", json!({"expression":retire}), session).await;
        // The fixture records "still open after 10s" if the client keeps the stream.
        tokio::time::timeout(std::time::Duration::from_secs(15), fixture.stream_closed.notified()).await
            .unwrap_or_else(|_| panic!("{reason}: the fixture must record the stream end; server saw {:?}", fixture.requests()));
        let during = await_page(&mut ctx, id + 3, "fetch('/api/during').then((response) => response.text())", session, &fixture).await;
        assert_eq!(during, "during", "{reason}: the page must stay live");
        assert_eq!(fixture.requests()[seen..], [path.to_string(), format!("{path} closed"), "/api/during".into()],
            "{reason}: the stream must close without a reconnect");
        let log = cdp(&mut ctx, id + 4, "Runtime.evaluate", json!({"expression":"__frameLog","returnByValue":true}), session).await;
        assert_eq!(log["result"]["value"], json!(delivered), "{reason}: the retired realm must get no late events");
    }
}

// The CDP server rejects malformed base64 before it resolves an interception.
// The op's own checks are proven at the resolver seam that server uses.
#[tokio::test(flavor = "current_thread")]
async fn fulfilled_event_source_opens_only_for_a_decodable_body_within_the_cap() {
    use base64::Engine;
    let (base, _port, fixture) = serve_event_streams().await;
    let session = "event-source-fulfill";
    let mut ctx = event_stream_page(&base, session).await;
    let (intercept_tx, mut intercepted) = tokio::sync::mpsc::unbounded_channel();
    ctx.intercept_tx = Some(intercept_tx);
    cdp(&mut ctx, 2, "Fetch.enable", json!({"patterns":[{"urlPattern":"*/api/fulfilled"}]}), session).await;
    let encode = |body: &str| base64::engine::general_purpose::STANDARD.encode(body);
    let cases = [
        (encode("data: ok\n\n"), None, json!([2, ["open:1", "message:ok"]]), "a decodable fulfilled stream"),
        ("!!!".to_string(), None, json!([2, []]), "a malformed base64 body"),
        (encode(&format!("data: {}\n\n", "x".repeat(80))), Some("64"), json!([2, []]), "a decoded body above the cap"),
    ];
    for (id, (body_base64, max_body_bytes, expected, reason)) in (3..).zip(cases) {
        if let Some(max) = max_body_bytes {
            // The op reads the cap per request. nextest runs each test in its own process.
            std::env::set_var("OBSCURA_FETCH_MAX_BODY_BYTES", max);
        }
        let fulfill = async {
            let request: obscura_js::ops::InterceptedRequest = intercepted.recv().await
                .expect("the stream request must reach the interception boundary");
            assert!(request.resolver.send(obscura_js::ops::InterceptResolution::Fulfill {
                status: 200,
                headers: std::collections::HashMap::from([("Content-Type".into(), "text/event-stream".into())]),
                body: String::new(), body_base64,
            }).is_ok(), "{reason}: the fulfillment must be delivered");
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            tokio::join!(await_page(&mut ctx, id, r#"new Promise((resolve) => {
                const log = [];
                const source = new EventSource('/api/fulfilled');
                source.onopen = () => log.push('open:' + source.readyState);
                source.onmessage = (event) => {
                    log.push('message:' + event.data);
                    source.close();
                    resolve([source.readyState, log]);
                };
                source.onerror = () => resolve([source.readyState, log]);
            })"#, session, &fixture), fulfill)
        }).await.unwrap_or_else(|_| panic!("{reason}: the fulfilled connection must settle"));
        assert_eq!(result, expected, "{reason}");
    }
    assert!(fixture.requests().is_empty(), "fulfilled streams must not reach the network: {:?}", fixture.requests());
}

// Last-Event-ID is not a CORS-safelisted header, but EventSource sends it
// without a preflight. An ordinary fetch with an unsafe header still sends one.
#[tokio::test(flavor = "current_thread")]
async fn approved_cross_origin_event_source_reconnects_without_a_preflight() {
    let (base, port, fixture) = serve_event_streams().await;
    let session = "event-source-cors-reconnect";
    let mut ctx = event_stream_page(&base, session).await;
    let log = await_page(&mut ctx, 2, &format!(r#"new Promise((resolve) => {{
        const log = [];
        const source = new EventSource('http://localhost:{port}/cors-reconnect');
        source.onerror = () => log.push('error:' + source.readyState);
        source.onmessage = (event) => {{
            log.push(event.data + '|' + event.lastEventId);
            if (event.data === 'second') {{ source.close(); resolve(log); }}
        }};
    }})"#), session, &fixture).await;
    assert_eq!(log, json!(["first|c1", "error:0", "second|c1"]));
    tokio::time::timeout(std::time::Duration::from_secs(10), fixture.stream_closed.notified()).await
        .unwrap_or_else(|_| panic!("close must end the reconnected stream; server saw {:?}", fixture.requests()));
    let probe = await_page(&mut ctx, 3, &format!(
        "fetch('http://localhost:{port}/cors-preflight', {{headers: {{'X-Probe': '1'}}}}).then((response) => response.text())"),
        session, &fixture).await;
    assert_eq!(probe, "approved");
    assert_eq!(fixture.requests(), [
        "/cors-reconnect", "/cors-reconnect last-event-id=c1", "/cors-reconnect closed",
        "OPTIONS /cors-preflight", "/cors-preflight",
    ]);
}

// networkidle0 waits until the page HTTP client has no active request. An open
// stream must not count, or readiness waits for the idle deadline.
#[tokio::test(flavor = "current_thread")]
async fn an_open_event_source_does_not_hold_network_idle() {
    let (base, _port, fixture) = serve_event_streams().await;
    let session = "event-source-idle";
    let mut ctx = event_stream_page(&base, session).await;
    cdp(&mut ctx, 2, "Page.addScriptToEvaluateOnNewDocument", json!({"source":r#"
        globalThis.__live = new EventSource('/sse-navigate');
        globalThis.__ready = new Promise((resolve) => __live.addEventListener('ready', () => resolve(__live.readyState)));
    "#}), session).await;
    cdp(&mut ctx, 3, "Page.navigate", json!({"url":format!("{base}next"),"waitUntil":"networkidle0"}), session).await;
    assert_eq!(await_page(&mut ctx, 4, "__ready", session, &fixture).await.as_f64(), Some(1.0), "the stream must be open");
    let page = ctx.get_session_page_mut(&Some(session.to_string())).unwrap();
    assert_eq!(page.http_client.active_requests(), 0, "the open stream must not count as an active request");
    assert_eq!(fixture.requests(), ["/sse-navigate"], "the stream must still be open");
    cdp(&mut ctx, 5, "Runtime.evaluate", json!({"expression":"__live.close()"}), session).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), fixture.stream_closed.notified()).await
        .unwrap_or_else(|_| panic!("close must end the stream; server saw {:?}", fixture.requests()));
    assert_eq!(fixture.requests(), ["/sse-navigate", "/sse-navigate closed"]);
}

#[cfg(feature = "stealth")]
async fn stealth_context() -> (CdpContext, String, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
    let mut ctx = CdpContext::new();
    ctx.default_context = std::sync::Arc::new(obscura_browser::BrowserContext::with_storage_and_network(
        "stealth-events".into(), None, true, None, None, true,
    ));
    let page = ctx.create_page();
    let session = "stealth-session".to_string();
    ctx.sessions.insert(session.clone(), page);
    let (base, observed) = serve().await;
    cdp(&mut ctx, 1, "Page.navigate", json!({"url":base,"waitUntil":"networkidle0"}), &session).await;
    (ctx, session, observed)
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
    let (mut ctx, session, _observed) = stealth_context().await;
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
    let (mut ctx, session, _observed) = stealth_context().await;
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
    let (mut ctx, session, _observed) = stealth_context().await;
    let request_id = response_request_id(&ctx, "/api/data.json").expect("retention opt-out must not disable observation");
    let response = dispatch(&CdpRequest { id: 2, method: "Network.getResponseBody".into(),
        params: json!({"requestId":request_id}), session_id: Some(session) }, &mut ctx).await;
    assert!(response.error.is_some(), "opt-out must not retain the body");
}

#[cfg(feature = "stealth")]
#[tokio::test(flavor = "current_thread")]
async fn stealth_response_body_buffer_evicts_old_requests() {
    std::env::set_var("OBSCURA_NETWORK_BODY_BUFFER_ENTRIES", "2");
    let (mut ctx, session, _observed) = stealth_context().await;
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
    let (mut ctx, session, _observed) = stealth_context().await;
    let response = cdp(&mut ctx, 2, "Runtime.evaluate", json!({"expression":"fetch('/api/missing.json').then(async r=>{await r.text();return r.status})","awaitPromise":true,"returnByValue":true}), &session).await;
    assert_eq!(response["result"]["value"].as_f64(), Some(404.0));
    let page = ctx.get_session_page_mut(&Some(session.clone())).unwrap();
    page.sync_js_network_events();
    let event = page.network_events.pop().expect("HTTP error response must be recorded");
    assert_eq!(event.status, 404);
    assert!(event.url.ends_with("/api/missing.json"));
    assert_eq!(cdp(&mut ctx, 3, "Network.getResponseBody", json!({"requestId":event.request_id}), &session).await["body"], r#"{"error":"missing"}"#);
}

#[cfg(feature = "stealth")]
#[tokio::test(flavor = "current_thread")]
async fn stealth_event_source_fails_closed_without_a_request() {
    let (mut ctx, session, observed) = stealth_context().await;
    let seen = observed.lock().unwrap().len();
    let response = cdp(&mut ctx, 2, "Runtime.evaluate", json!({"expression":r#"new Promise((resolve) => {
        const log = [];
        const source = new EventSource('/api/stream');
        source.onopen = () => log.push('open');
        source.onmessage = () => log.push('message');
        source.onerror = () => resolve([source.readyState, log]);
    })"#, "awaitPromise":true, "returnByValue":true}), &session).await;
    assert_eq!(response["result"]["value"], json!([2, []]), "the stealth transport must fail EventSource closed");
    assert_eq!(observed.lock().unwrap().len(), seen, "the stealth transport must not send the stream request");
}
