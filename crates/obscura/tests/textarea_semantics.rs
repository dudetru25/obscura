// Textarea parity from #685: the element kept its parser handling and value
// semantics but had no interface of its own and no control geometry, so it
// laid out as a plain block (full containing-block width, zero height when
// empty). Playwright resolves that zero-height box to `hidden`, and
// wait_for_selector/click/fill then burn their full timeout. Both halves are
// covered here:
//
// - DOM: rows/cols reflect their attributes with the HTML defaults (2/20),
//   type is the fixed string "textarea", and the wrapper is a real
//   HTMLTextAreaElement (constructor.name, instanceof) instead of Element.
// - Layout: an empty textarea keeps an intrinsic box from rows/cols
//   (Chromium: cols=20 rows=2 -> 181x36, rows=8 -> 126 tall), is
//   inline-block, and an authored height still wins.

use std::io::{Read, Write};

use obscura::Browser;

fn spawn_server() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(mut stream) = incoming else {
                continue;
            };
            std::thread::spawn(move || {
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request);
                let body = r#"<!doctype html><html><head><title>fixture</title></head><body>
<textarea id="t"></textarea>
<textarea id="r" rows="8"></textarea>
<textarea id="c" style="height:36px"></textarea>
<textarea id="hidden" style="overflow:hidden"></textarea>
<textarea id="p">
parsed</textarea>
</body></html>"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.shutdown(std::net::Shutdown::Both);
            });
        }
    });
    format!("http://{}", addr)
}

#[tokio::test(flavor = "current_thread")]
async fn textarea_idl_matches_browser_semantics() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let base = spawn_server();

    let browser = Browser::new().unwrap();
    let mut page = browser.new_page().await.unwrap();
    page.goto(&base).await.unwrap();

    let probes = page.evaluate(
        r#"(function () {
            var t = document.getElementById('t');
            return {
                rows_default: t.rows,
                cols_default: t.cols,
                type: t.type,
                ctor: t.constructor.name,
                is_textarea: t instanceof HTMLTextAreaElement,
                is_element: t instanceof Element,
                rows_reflect: (t.rows = 5, t.getAttribute('rows')),
                rows_after_set: t.rows,
                rows_invalid: (t.setAttribute('rows', '0'), t.rows),
            };
        })()"#,
    );
    let defaults = page.evaluate(
        r#"(function () {
            var make = function (tag) { return document.createElement(tag); };
            var area = function (dflt) {
                var el = make('textarea');
                el.defaultValue = dflt;
                return [el.defaultValue, el.value, el.childNodes.length];
            };
            var p = document.getElementById('p');
            var parsed = [p.defaultValue, p.value];
            p.appendChild(document.createTextNode('!'));
            var appended = [p.defaultValue, p.value];
            var crlf = make('textarea');
            crlf.defaultValue = 'x\r\ny\rz';
            var crlf_set = make('textarea');
            crlf_set.value = 'm\r\nn';
            var dirty = make('textarea');
            dirty.defaultValue = 'init';
            dirty.value = 'a';
            dirty.defaultValue = 'b';
            var after_default = [dirty.value, dirty.defaultValue, dirty.textContent];
            dirty.textContent = 'c';
            var after_text = [dirty.value, dirty.defaultValue];
            dirty.value = 'sel';
            dirty.setSelectionRange(1, 2);
            var input = make('input');
            input.defaultValue = 'x';
            var input_set = [input.getAttribute('value'), input.value];
            input.setAttribute('value', 'y');
            var input_attr = [input.defaultValue, input.value];
            input.value = 'typed';
            input.defaultValue = 'z';
            var input_dirty = [input.value, input.getAttribute('value')];
            input.defaultValue = null;
            var input_null = input.defaultValue;
            input.removeAttribute('value');
            var box = make('input');
            box.setAttribute('type', 'checkbox');
            box.defaultChecked = true;
            var box_set = [box.hasAttribute('checked'), box.checked];
            box.checked = false;
            box.removeAttribute('checked');
            box.setAttribute('checked', '');
            var box_dirty = [box.checked, box.defaultChecked];
            box.defaultChecked = 0;
            var desc = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'defaultValue');
            return {
                accessor: !!desc && typeof desc.get === 'function' && typeof desc.set === 'function'
                    && !Object.prototype.hasOwnProperty.call(dirty, 'defaultValue'),
                init: area('init'),
                number: area(5),
                null_value: area(null),
                undefined_value: area(undefined),
                empty: area(''),
                parsed: parsed,
                appended: appended,
                crlf: [crlf.defaultValue, crlf.value],
                crlf_set: crlf_set.value,
                after_default: after_default,
                after_text: after_text,
                selection: [dirty.selectionStart, dirty.selectionEnd, dirty.textContent],
                input_set: input_set,
                input_attr: input_attr,
                input_dirty: input_dirty,
                input_null: input_null,
                input_removed: [input.defaultValue, input.value],
                box_set: box_set,
                box_dirty: box_dirty,
                box_cleared: [box.hasAttribute('checked'), box.defaultChecked, box.checked],
            };
        })()"#,
    );

    assert_eq!(probes["rows_default"], 2);
    assert_eq!(probes["cols_default"], 20);
    assert_eq!(probes["type"], "textarea");
    assert_eq!(probes["ctor"], "HTMLTextAreaElement");
    assert_eq!(probes["is_textarea"], true);
    assert_eq!(probes["is_element"], true);
    assert_eq!(probes["rows_reflect"], "5");
    assert_eq!(probes["rows_after_set"], 5);
    // rows/cols are limited to positive numbers; anything else reads back
    // as the default.
    assert_eq!(probes["rows_invalid"], 2);

    // defaultValue is the child text, not an expando. A pristine value follows
    // it; React's initTextarea writes defaultValue and then reads textContent.
    assert_eq!(
        defaults["init"],
        serde_json::json!(["init", "init", 1]),
        "defaultValue must initialize the pristine value and child text"
    );
    assert_eq!(defaults["accessor"], true);
    // DOMString coercion, not textContent's nullable conversion.
    assert_eq!(defaults["number"], serde_json::json!(["5", "5", 1]));
    assert_eq!(
        defaults["null_value"],
        serde_json::json!(["null", "null", 1])
    );
    assert_eq!(
        defaults["undefined_value"],
        serde_json::json!(["undefined", "undefined", 1])
    );
    assert_eq!(defaults["empty"], serde_json::json!(["", "", 0]));
    // The parser drops the newline right after <textarea>; dynamic child text
    // is reflected while pristine.
    assert_eq!(defaults["parsed"], serde_json::json!(["parsed", "parsed"]));
    assert_eq!(
        defaults["appended"],
        serde_json::json!(["parsed!", "parsed!"])
    );
    // The API value normalizes CRLF and CR; defaultValue stays raw.
    assert_eq!(
        defaults["crlf"],
        serde_json::json!(["x\r\ny\rz", "x\ny\nz"])
    );
    assert_eq!(defaults["crlf_set"], "m\nn");
    // A dirty value survives default changes through either API, and value
    // writes leave the child text alone.
    assert_eq!(
        defaults["after_default"],
        serde_json::json!(["a", "b", "b"])
    );
    assert_eq!(defaults["after_text"], serde_json::json!(["a", "c"]));
    assert_eq!(defaults["selection"], serde_json::json!([1, 2, "c"]));
    // Input defaults reflect the content attributes both ways.
    assert_eq!(defaults["input_set"], serde_json::json!(["x", "x"]));
    assert_eq!(defaults["input_attr"], serde_json::json!(["y", "y"]));
    assert_eq!(defaults["input_dirty"], serde_json::json!(["typed", "z"]));
    assert_eq!(defaults["input_null"], "null");
    assert_eq!(defaults["input_removed"], serde_json::json!(["", "typed"]));
    assert_eq!(defaults["box_set"], serde_json::json!([true, true]));
    assert_eq!(defaults["box_dirty"], serde_json::json!([false, true]));
    assert_eq!(
        defaults["box_cleared"],
        serde_json::json!([false, false, false])
    );
}

#[cfg(feature = "render")]
#[tokio::test(flavor = "current_thread")]
async fn textarea_gets_intrinsic_control_box() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let base = spawn_server();

    let browser = Browser::new().unwrap();
    let mut page = browser.new_page().await.unwrap();
    page.goto(&base).await.unwrap();

    let probes = page.evaluate(
        r#"(function () {
            var box = function (id) {
                var r = document.getElementById(id).getBoundingClientRect();
                return r.width.toFixed(0) + 'x' + r.height.toFixed(0);
            };
            var t = document.getElementById('t');
            return {
                empty: box('t'),
                rows8: box('r'),
                css_height: box('c'),
                hidden_overflow: box('hidden'),
                display: getComputedStyle(t).display,
                value_roundtrip: (t.value = 'hi', t.value),
            };
        })()"#,
    );

    // Chromium reference: an empty cols=20 rows=2 control is 181x36; each
    // extra row adds one 15px control line; authored height wins.
    assert_eq!(probes["empty"], "181x36");
    assert_eq!(probes["rows8"], "181x126");
    assert_eq!(probes["css_height"], "181x36");
    assert_eq!(probes["hidden_overflow"], "166x36");
    assert_eq!(probes["display"], "inline-block");
    assert_eq!(probes["value_roundtrip"], "hi");
}

// A dirty value paints from native form state, not from the child text. At
// the old base the value setter rewrote the child text, so a later default
// change repainted the control with the default. The same text then paints
// identically as a pristine default and as a dirty value, including when
// the control overflows its two rows.
#[cfg(feature = "render")]
#[tokio::test(flavor = "current_thread")]
async fn textarea_paints_dirty_value_over_default_changes() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let base = spawn_server();

    let context = std::sync::Arc::new(obscura_browser::BrowserContext::new(
        "textarea-paint".to_string(),
    ));
    let mut page = obscura_browser::Page::new("textarea-paint".to_string(), context);
    let viewport = (400.0, 400.0);
    page.set_viewport(viewport);
    page.navigate(&base).await.unwrap();

    let pristine = page.screenshot(viewport).expect("pristine screenshot");
    page.evaluate("document.getElementById('t').value = 'abc'");
    let dirty = page.screenshot(viewport).expect("dirty screenshot");
    page.evaluate("document.getElementById('t').textContent = 'xyz'");
    let default_changed = page
        .screenshot(viewport)
        .expect("default change screenshot");
    page.evaluate("document.getElementById('t').value = ''");
    let dirty_empty = page.screenshot(viewport).expect("dirty empty screenshot");
    let probes = page.evaluate(
        r#"(function () {
            var t = document.getElementById('t');
            var r = t.getBoundingClientRect();
            return [r.width.toFixed(0) + 'x' + r.height.toFixed(0), t.value, t.defaultValue];
        })()"#,
    );

    assert_ne!(dirty, pristine, "the dirty value must paint");
    assert_eq!(
        default_changed, dirty,
        "a default change must not repaint a dirty value"
    );
    assert_eq!(
        dirty_empty, pristine,
        "an empty dirty value hides the default text"
    );
    assert_eq!(probes, serde_json::json!(["181x36", "", "xyz"]));

    let metrics = r#"(function () {
        var t = document.getElementById('ab');
        var r = t.getBoundingClientRect();
        var cs = getComputedStyle(t);
        return {
            value: t.value,
            font: [cs.fontFamily, cs.fontSize, cs.lineHeight, cs.whiteSpace],
            box: [r.left, r.top, r.width, r.height],
        };
    })()"#;
    page.evaluate(
        r#"(function () {
            document.body.textContent = '';
            var t = document.createElement('textarea');
            t.id = 'ab';
            t.rows = 2;
            t.style.cssText = 'position:absolute;left:8px;top:8px';
            document.body.appendChild(t);
        })()"#,
    );
    let blank = page.screenshot(viewport).expect("blank screenshot");
    page.evaluate(
        r#"document.getElementById('ab').textContent = 'a  b\nc\nd\ne\nf'"#,
    );
    let pristine_filled = page
        .screenshot(viewport)
        .expect("pristine filled screenshot");
    let pristine_metrics = page.evaluate(metrics);
    let pristine_default = page.evaluate("document.getElementById('ab').defaultValue");
    page.evaluate(
        r#"(function () {
            var old = document.getElementById('ab');
            var t = document.createElement('textarea');
            t.rows = 2;
            t.style.cssText = old.style.cssText;
            t.appendChild(document.createTextNode(''));
            old.remove();
            t.id = 'ab';
            document.body.appendChild(t);
            t.value = 'a  b\nc\nd\ne\nf';
        })()"#,
    );
    let dirty_filled = page
        .screenshot(viewport)
        .expect("dirty filled screenshot");
    let dirty_metrics = page.evaluate(metrics);
    let dirty_default = page.evaluate("document.getElementById('ab').defaultValue");

    assert_eq!(pristine_metrics["value"], "a  b\nc\nd\ne\nf");
    assert_eq!(pristine_default, "a  b\nc\nd\ne\nf");
    assert_eq!(dirty_default, "");
    assert_eq!(
        dirty_metrics, pristine_metrics,
        "the same text must shape to the same font, box and line count"
    );
    assert_ne!(pristine_filled, blank, "the text must paint");
    assert_eq!(
        dirty_filled, pristine_filled,
        "a dirty value must paint like the same default text"
    );

    page.evaluate(
        "document.getElementById('ab').style.display = 'flex'; document.getElementById('ab').value = ''",
    );
    let flex_blank = page.screenshot(viewport).expect("blank flex textarea");
    page.evaluate("document.getElementById('ab').value = 'abc'");
    let flex_filled = page.screenshot(viewport).expect("filled flex textarea");
    assert_ne!(flex_filled, flex_blank, "a flex textarea must paint its value");
}
