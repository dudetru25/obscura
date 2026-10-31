//! `Input.dispatchKeyEvent` interpolates the `key`/`code` params into a
//! generated `KeyboardEvent(...)` snippet. They must be escaped for BOTH
//! backslash and single-quote (issue #433): Chrome sends `key: "\\"` (U+005C)
//! when the backslash key is pressed, and quote-only escaping turns that into
//! `key:'\'` — the backslash escapes the closing quote, the literal runs on,
//! and the whole `page.evaluate` is a syntax error, so the `keydown` is
//! silently never dispatched. Regression test: the backslash key must arrive.

use obscura_cdp::dispatch::{dispatch, CdpContext};
use obscura_cdp::types::CdpRequest;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// Serves a page that records the `key` of the last keydown event on the body.
async fn serve_page() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await.unwrap();
            let body = r#"<html><body>
<input id="i">
<textarea id="a"></textarea>
<script>
window.__keys = [];
document.body.addEventListener('keydown', function (e) { window.__keys.push(e.key + '|' + e.code); });
</script>
</body></html>"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(resp.as_bytes()).await;
        });
    });
    format!("http://{addr}/")
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

#[tokio::test(flavor = "current_thread")]
async fn dispatch_key_event_escapes_backslash_in_key_and_code() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let url = serve_page().await;
    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    let session_id = "session-1";
    ctx.sessions.insert(session_id.to_string(), page_id.clone());

    cdp(&mut ctx, 1, "Page.navigate", json!({"url": url, "waitUntil": "load"}), session_id).await;

    // The backslash key: Chrome sends key="\" (a single backslash) code="Backslash".
    cdp(
        &mut ctx,
        2,
        "Input.dispatchKeyEvent",
        json!({"type": "keyDown", "key": "\\", "code": "Backslash"}),
        session_id,
    )
    .await;

    // A key whose name itself contains a quote AND a backslash, to exercise the
    // ordering of the two replacements.
    cdp(
        &mut ctx,
        3,
        "Input.dispatchKeyEvent",
        json!({"type": "keyDown", "key": "a", "code": "KeyA"}),
        session_id,
    )
    .await;

    let v = cdp(
        &mut ctx,
        4,
        "Runtime.evaluate",
        json!({"expression": "JSON.stringify(window.__keys)", "returnByValue": true}),
        session_id,
    )
    .await;

    let keys: Vec<String> =
        serde_json::from_str(v["result"]["value"].as_str().unwrap()).unwrap();
    assert_eq!(
        keys,
        vec!["\\|Backslash".to_string(), "a|KeyA".to_string()],
        "the backslash key must be dispatched, not dropped by a malformed snippet"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dispatch_key_event_exposes_legacy_virtual_key_codes() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let url = serve_page().await;
    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    let session_id = "session-1";
    ctx.sessions.insert(session_id.to_string(), page_id);

    cdp(&mut ctx, 1, "Page.navigate", json!({"url": url, "waitUntil": "load"}), session_id).await;
    cdp(
        &mut ctx,
        2,
        "Runtime.evaluate",
        json!({
            "expression": "window.__codes = []; for (const type of ['keydown', 'keypress', 'keyup']) document.body.addEventListener(type, e => __codes.push([type, e.keyCode, e.which, e.charCode, e.shiftKey]))",
            "returnByValue": true
        }),
        session_id,
    ).await;
    for (id, event_type) in [(3, "keyDown"), (4, "keyUp")] {
        cdp(
            &mut ctx,
            id,
            "Input.dispatchKeyEvent",
            json!({"type": event_type, "key": "Enter", "code": "Enter", "text": "\r", "windowsVirtualKeyCode": 13, "modifiers": 8}),
            session_id,
        ).await;
    }
    let value = cdp(
        &mut ctx,
        5,
        "Runtime.evaluate",
        json!({"expression": "JSON.stringify(__codes)", "returnByValue": true}),
        session_id,
    ).await;
    assert_eq!(
        value["result"]["value"],
        r#"[["keydown",13,13,0,true],["keypress",13,13,13,true],["keyup",13,13,0,true]]"#
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dispatch_key_event_preserves_modifier_combinations() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let url = serve_page().await;
    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    let session_id = "session-1";
    ctx.sessions.insert(session_id.to_string(), page_id);
    cdp(&mut ctx, 1, "Page.navigate", json!({"url": url, "waitUntil": "load"}), session_id).await;
    cdp(&mut ctx, 2, "Runtime.evaluate", json!({
        "expression": "window.__modifiers = []; for (const type of ['keydown','keyup']) document.addEventListener(type, e => __modifiers.push([type, e.altKey, e.ctrlKey, e.metaKey, e.shiftKey, ...['Alt','Control','Meta','Shift'].map(k => e.getModifierState(k))]))"
    }), session_id).await;
    let mut expected = Vec::new();
    let mut id = 3;
    for modifiers in 0..16 {
        for (event_type, dom_type) in [("rawKeyDown", "keydown"), ("keyUp", "keyup")] {
            cdp(&mut ctx, id, "Input.dispatchKeyEvent", json!({
                "type": event_type, "key": "o", "code": "KeyO",
                "windowsVirtualKeyCode": 79, "modifiers": modifiers
            }), session_id).await;
            id += 1;
            let flags = [1, 2, 4, 8].map(|mask| modifiers & mask != 0);
            expected.push(json!([dom_type, flags[0], flags[1], flags[2], flags[3],
                flags[0], flags[1], flags[2], flags[3]]));
        }
    }
    let result = cdp(&mut ctx, id, "Runtime.evaluate", json!({
        "expression": "JSON.stringify(__modifiers)", "returnByValue": true
    }), session_id).await;
    let actual: Value = serde_json::from_str(result["result"]["value"].as_str().unwrap()).unwrap();
    assert_eq!(actual, json!(expected));
}

#[tokio::test(flavor = "current_thread")]
async fn delete_key_removes_the_selected_range() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let url = serve_page().await;
    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    let session_id = "session-1";
    ctx.sessions.insert(session_id.to_string(), page_id);
    cdp(&mut ctx, 1, "Page.navigate", json!({"url": url, "waitUntil": "load"}), session_id).await;
    cdp(
        &mut ctx,
        2,
        "Runtime.evaluate",
        json!({
            "expression": "i.focus(); i.value = 'abcd'; i.setSelectionRange(1, 3); window.__inputs = 0; i.addEventListener('input', () => __inputs++)",
            "returnByValue": true
        }),
        session_id,
    ).await;
    cdp(
        &mut ctx,
        3,
        "Input.dispatchKeyEvent",
        json!({"type": "keyDown", "key": "Delete", "code": "Delete", "windowsVirtualKeyCode": 46}),
        session_id,
    ).await;
    let value = cdp(
        &mut ctx,
        4,
        "Runtime.evaluate",
        json!({"expression": "JSON.stringify({value: i.value, start: i.selectionStart, inputs: __inputs})", "returnByValue": true}),
        session_id,
    ).await;
    assert_eq!(value["result"]["value"], r#"{"value":"ad","start":1,"inputs":1}"#);
}

#[tokio::test(flavor = "current_thread")]
async fn tab_key_moves_focus_through_enabled_visible_stops() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let url = serve_page().await;
    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    let session_id = "session-1";
    ctx.sessions.insert(session_id.to_string(), page_id);
    cdp(&mut ctx, 1, "Page.navigate", json!({"url": url, "waitUntil": "load"}), session_id).await;
    let setup = cdp(&mut ctx, 2, "Runtime.evaluate", json!({
        "expression": r#"document.body.innerHTML = `
            <input id="search" value="query">
            <input id="negative" tabindex="-1"><input disabled><input type="hidden">
            <div inert><button>Inert</button></div>
            <div hidden><button>Hidden</button></div>
            <dialog><button>Closed dialog</button></dialog>
            <div style="display:none"><button>No box</button></div>
            <div style="visibility:hidden"><button>Invisible</button></div>
            <fieldset disabled><input></fieldset>
            <button id="new-page">New page</button>
            <input id="transparent" style="opacity:0">
            <fieldset disabled><legend><button id="legend">Legend</button></legend><button>Disabled fieldset</button></fieldset>
            <input id="positive-later" tabindex="3">
            <button id="positive-first" tabindex="1">First</button>
            <button id="positive-tie" tabindex="1">Tie</button>`;
            window.__tabTransitions = [];
            window.__tabKeyTargets = [];
            document.addEventListener('focusout', event => __tabTransitions.push([event.type, event.target.id, event.relatedTarget?.id ?? '']));
            document.addEventListener('focusin', event => __tabTransitions.push([event.type, event.target.id, event.relatedTarget?.id ?? '']));
            document.addEventListener('keydown', event => {
                if (event.key !== 'Tab') return;
                __tabKeyTargets.push([event.type, event.target.id]);
                if (window.__cancelTab) event.preventDefault();
            });
            document.addEventListener('keyup', event => {
                if (event.key === 'Tab') __tabKeyTargets.push([event.type, event.target.id]);
            });
            'ready'"#,
        "returnByValue": true
    }), session_id).await;
    assert_eq!(setup["result"]["value"], "ready");

    let cases = [
        ("search", 0, false, "new-page"),
        ("new-page", 8, false, "search"),
        ("negative", 0, false, "new-page"),
        ("negative", 8, false, "search"),
        ("positive-first", 0, false, "positive-tie"),
        ("positive-tie", 0, false, "positive-later"),
        ("positive-later", 0, false, "search"),
        ("new-page", 0, false, "transparent"),
        ("transparent", 0, false, "legend"),
        ("legend", 0, false, ""),
        ("", 0, false, "positive-first"),
        ("positive-first", 8, false, ""),
        ("", 8, false, "legend"),
        ("search", 0, true, "search"),
        ("search", 1, false, "search"),
        ("search", 2, false, "search"),
        ("search", 4, false, "search"),
    ];
    let mut id = 3;
    for (case_index, (start, modifiers, cancelled, expected)) in cases.into_iter().enumerate() {
        cdp(&mut ctx, id, "Runtime.evaluate", json!({
            "expression": format!(
                "(() => {{ const start = document.getElementById({}); if (start) start.focus(); else document.activeElement.blur(); window.__cancelTab = {cancelled}; __tabTransitions.length = 0; __tabKeyTargets.length = 0; }})()",
                serde_json::to_string(start).unwrap()
            )
        }), session_id).await;
        id += 1;
        for event_type in [if case_index % 2 == 0 { "keyDown" } else { "rawKeyDown" }, "keyUp"] {
            cdp(&mut ctx, id, "Input.dispatchKeyEvent", json!({
                "type": event_type, "key": "Tab", "code": "Tab", "text": "\t",
                "windowsVirtualKeyCode": 9, "modifiers": modifiers
            }), session_id).await;
            id += 1;
        }
        let value = cdp(&mut ctx, id, "Runtime.evaluate", json!({
            "expression": "JSON.stringify({active: document.activeElement.id, transitions: __tabTransitions, keys: __tabKeyTargets, value: document.getElementById('search').value})",
            "returnByValue": true
        }), session_id).await;
        id += 1;
        let actual: Value = serde_json::from_str(value["result"]["value"].as_str().unwrap()).unwrap();
        let mut transitions = Vec::new();
        if start != expected {
            if !start.is_empty() {
                transitions.push(json!(["focusout", start, expected]));
            }
            if !expected.is_empty() {
                transitions.push(json!(["focusin", expected, start]));
            }
        }
        assert_eq!(actual, json!({
            "active": expected,
            "transitions": transitions,
            "keys": [["keydown", start], ["keyup", expected]],
            "value": "query"
        }), "Tab case {case_index}: {start:?}, modifiers={modifiers}, cancelled={cancelled}");
    }
}

// The same hazard one layer over: on the inserted *text* rather than on the key
// name. `insert_text_js` interpolates the text into the same kind of
// single-quoted literal. #433 escaped the backslash and the quote and stopped
// there, but a raw newline ends a JS string literal just as a stray quote does,
// so the snippet is a syntax error and the character is silently dropped.
//
// The `char` type is the path that reaches it with a newline: the `keyDown`
// branch skips insertion for "\r" and "\n" because `key == "Enter"` handles
// those, and `char` has no such guard, so a client entering a line break in a
// textarea this way loses it with no error reported anywhere.
#[tokio::test(flavor = "current_thread")]
async fn dispatch_key_event_char_carries_a_newline_into_a_textarea() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let url = serve_page().await;
    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    let session_id = "session-1";
    ctx.sessions.insert(session_id.to_string(), page_id.clone());

    cdp(&mut ctx, 1, "Page.navigate", json!({"url": url, "waitUntil": "load"}), session_id).await;
    cdp(
        &mut ctx,
        2,
        "Runtime.evaluate",
        json!({
            "expression": "(function () { document.getElementById('a').focus(); return 'ok'; })()",
            "returnByValue": true,
        }),
        session_id,
    )
    .await;

    for (id, ch) in [(3u64, "a"), (4, "\n"), (5, "b")] {
        cdp(
            &mut ctx,
            id,
            "Input.dispatchKeyEvent",
            json!({"type": "char", "text": ch}),
            session_id,
        )
        .await;
    }

    let v = cdp(
        &mut ctx,
        6,
        "Runtime.evaluate",
        json!({
            "expression": "JSON.stringify(document.getElementById('a').value)",
            "returnByValue": true,
        }),
        session_id,
    )
    .await;
    assert_eq!(
        v["result"]["value"].as_str().unwrap_or_default(),
        r#""a\nb""#,
        "a newline sent as a char must reach the field, not be dropped by a malformed snippet"
    );
}

// #577: Playwright's fill() focuses the field in page and then types the whole
// value with one Input.insertText call. The method must exist and drive the
// same snippet the key-event text path uses, so quotes, backslashes, and
// newlines survive intact.
#[tokio::test(flavor = "current_thread")]
async fn insert_text_types_into_the_focused_field() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let url = serve_page().await;
    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    let session_id = "session-1";
    ctx.sessions.insert(session_id.to_string(), page_id.clone());

    cdp(&mut ctx, 1, "Page.navigate", json!({"url": url, "waitUntil": "load"}), session_id).await;
    cdp(
        &mut ctx,
        2,
        "Runtime.evaluate",
        json!({"expression": "document.getElementById('i').focus()", "returnByValue": true}),
        session_id,
    )
    .await;
    cdp(
        &mut ctx,
        3,
        "Input.insertText",
        json!({"text": "he'll\\o\nbye"}),
        session_id,
    )
    .await;

    let v = cdp(
        &mut ctx,
        4,
        "Runtime.evaluate",
        json!({
            "expression": "JSON.stringify(document.getElementById('i').value)",
            "returnByValue": true,
        }),
        session_id,
    )
    .await;
    assert_eq!(
        v["result"]["value"].as_str().unwrap_or_default(),
        r#""he'll\\o\nbye""#,
        "insertText must type the full text with quotes, backslashes, and newlines intact"
    );
}
