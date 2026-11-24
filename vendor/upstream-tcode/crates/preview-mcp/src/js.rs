//! Page scripts for WebView automation using CSS selectors and synthetic DOM
//! events. Each snippet returns a JSON-serializable value; [`parse_result`]
//! decodes the WebView callback's result.

/// Report `{ url, title, loading }` for the current page.
pub const STATUS: &str = r#"(() => ({
  url: location.href,
  title: document.title,
  loading: document.readyState !== "complete"
}))()"#;

/// Build a compact DOM outline: page metadata plus an array of visible
/// interactive elements with `{ tag, role, name, selector, x, y, width, height }`.
pub const SNAPSHOT: &str = r##"(() => {
  const MAX_ELEMENTS = 100;
  const MAX_TEXT = 4000;
  const selectorFor = (element) => {
    if (element.id) return "#" + CSS.escape(element.id);
    for (const attribute of ["data-testid", "name"]) {
      const value = element.getAttribute(attribute);
      if (value) return element.tagName.toLowerCase() + "[" + attribute + "=" + JSON.stringify(value) + "]";
    }
    const buildParts = (current, parts = []) => {
      if (!current || current.nodeType !== Node.ELEMENT_NODE || parts.length >= 8) return parts;
      const parent = current.parentElement;
      const siblings = parent ? Array.from(parent.children).filter((c) => c.tagName === current.tagName) : [];
      const base = current.tagName.toLowerCase();
      const part = siblings.length > 1 ? base + ":nth-of-type(" + (siblings.indexOf(current) + 1) + ")" : base;
      return buildParts(parent, [part, ...parts]);
    };
    return buildParts(element).join(" > ");
  };
  const visible = (element) => {
    const style = getComputedStyle(element);
    const rect = element.getBoundingClientRect();
    return style.visibility !== "hidden" && style.display !== "none" && rect.width > 0 && rect.height > 0;
  };
  const elements = Array.from(document.querySelectorAll(
    "a[href],button,input,textarea,select,[role],[tabindex]"
  )).filter(visible).slice(0, MAX_ELEMENTS).map((element) => {
    const rect = element.getBoundingClientRect();
    return {
      tag: element.tagName.toLowerCase(),
      role: element.getAttribute("role"),
      name: (element.getAttribute("aria-label") || element.innerText || element.getAttribute("name") || "").slice(0, 200),
      selector: selectorFor(element),
      x: Math.round(rect.x), y: Math.round(rect.y),
      width: Math.round(rect.width), height: Math.round(rect.height)
    };
  });
  return {
    url: location.href,
    title: document.title,
    loading: document.readyState !== "complete",
    visibleText: (document.body ? document.body.innerText : "").slice(0, MAX_TEXT),
    interactiveElements: elements
  };
})()"##;

/// Dispatch synthetic pointer/mouse events at the first match's center, then
/// invoke `element.click()` when available. Returns
/// `{ ok: true }` or `{ error: "..." }`.
pub fn click(selector: &str) -> String {
    let sel = json_string(selector);
    format!(
        r#"(() => {{
  try {{
    const el = document.querySelector({sel});
    if (!el) return {{ error: "no element matches selector" }};
    el.scrollIntoView({{ block: "center", inline: "center" }});
    const rect = el.getBoundingClientRect();
    const x = rect.left + rect.width / 2;
    const y = rect.top + rect.height / 2;
    const opts = {{ bubbles: true, cancelable: true, view: window, clientX: x, clientY: y }};
    for (const type of ["pointerdown", "mousedown", "pointerup", "mouseup", "click"]) {{
      el.dispatchEvent(new MouseEvent(type, opts));
    }}
    if (typeof el.click === "function") el.click();
    return {{ ok: true }};
  }} catch (e) {{ return {{ error: String(e) }}; }}
}})()"#
    )
}

/// JS to focus `selector` and set its value to `text`, dispatching `input` and
/// `change` events (handling inputs/textareas and contenteditable). Returns
/// `{ ok: true }` or `{ error: "..." }`.
pub fn type_text(selector: &str, text: &str) -> String {
    let sel = json_string(selector);
    let txt = json_string(text);
    format!(
        r#"(() => {{
  try {{
    const el = document.querySelector({sel});
    if (!el) return {{ error: "no element matches selector" }};
    const text = {txt};
    el.focus();
    const isField = el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement;
    if (isField) {{
      const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
      const setter = Object.getOwnPropertyDescriptor(proto, "value");
      if (setter && setter.set) setter.set.call(el, text); else el.value = text;
      el.dispatchEvent(new InputEvent("input", {{ bubbles: true }}));
      el.dispatchEvent(new Event("change", {{ bubbles: true }}));
      return {{ ok: true }};
    }}
    if (el.isContentEditable) {{
      el.textContent = text;
      el.dispatchEvent(new InputEvent("input", {{ bubbles: true }}));
      return {{ ok: true }};
    }}
    return {{ error: "element is not editable" }};
  }} catch (e) {{ return {{ error: String(e) }}; }}
}})()"#
    )
}

/// JS to dispatch a keydown/keyup pair to the focused element. Printable keys
/// also update editable targets because untrusted keyboard events have no
/// browser default action.
pub fn press(key: &str, modifiers: &[String]) -> String {
    let key = json_string(key);
    let alt = modifiers.iter().any(|modifier| modifier == "Alt");
    let control = modifiers.iter().any(|modifier| modifier == "Control");
    let meta = modifiers.iter().any(|modifier| modifier == "Meta");
    let shift = modifiers.iter().any(|modifier| modifier == "Shift");
    format!(
        r#"(() => {{
  try {{
    const key = {key};
    const target = document.activeElement || document.body;
    if (!target) return {{ error: "document has no key target" }};
    const opts = {{
      key, bubbles: true, cancelable: true,
      altKey: {alt}, ctrlKey: {control}, metaKey: {meta}, shiftKey: {shift}
    }};
    target.dispatchEvent(new KeyboardEvent("keydown", opts));
    if (Array.from(key).length === 1 && !opts.ctrlKey && !opts.metaKey) {{
      if (target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement) {{
        const start = target.selectionStart == null ? target.value.length : target.selectionStart;
        const end = target.selectionEnd == null ? start : target.selectionEnd;
        const value = target.value.slice(0, start) + key + target.value.slice(end);
        const proto = target instanceof HTMLTextAreaElement
          ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
        const setter = Object.getOwnPropertyDescriptor(proto, "value");
        if (setter && setter.set) setter.set.call(target, value); else target.value = value;
        try {{ target.setSelectionRange(start + key.length, start + key.length); }} catch (_) {{}}
        target.dispatchEvent(new InputEvent("input", {{
          bubbles: true, data: key, inputType: "insertText"
        }}));
      }} else if (target.isContentEditable) {{
        const selection = getSelection();
        let range = selection && selection.rangeCount ? selection.getRangeAt(0) : null;
        if (!range || !target.contains(range.commonAncestorContainer)) {{
          range = document.createRange();
          range.selectNodeContents(target);
          range.collapse(false);
        }}
        range.deleteContents();
        const node = document.createTextNode(key);
        range.insertNode(node);
        range.setStartAfter(node);
        range.collapse(true);
        if (selection) {{
          selection.removeAllRanges();
          selection.addRange(range);
        }}
        target.dispatchEvent(new InputEvent("input", {{
          bubbles: true, data: key, inputType: "insertText"
        }}));
      }}
    }}
    if (key === "Enter" && target instanceof HTMLInputElement) {{
      const form = target.closest("form");
      if (form && typeof form.requestSubmit === "function") form.requestSubmit();
    }}
    target.dispatchEvent(new KeyboardEvent("keyup", opts));
    return {{ ok: true, key, target: target.tagName.toLowerCase() }};
  }} catch (e) {{ return {{ error: String(e) }}; }}
}})()"#
    )
}

/// JS to scroll the window or the first `selector` match and report its final
/// position.
pub fn scroll(delta_x: f64, delta_y: f64, selector: Option<&str>) -> String {
    let selector = selector
        .map(json_string)
        .unwrap_or_else(|| "null".to_string());
    format!(
        r#"(() => {{
  try {{
    const selector = {selector};
    if (selector !== null) {{
      const el = document.querySelector(selector);
      if (!el) return {{ error: "no element matches selector" }};
      el.scrollBy({{ left: {delta_x}, top: {delta_y}, behavior: "instant" }});
      return {{ scrollLeft: el.scrollLeft, scrollTop: el.scrollTop }};
    }}
    window.scrollBy({{ left: {delta_x}, top: {delta_y}, behavior: "instant" }});
    return {{ x: window.scrollX, y: window.scrollY }};
  }} catch (e) {{ return {{ error: String(e) }}; }}
}})()"#
    )
}

/// JS to test all requested wait conditions once. The UI polls this probe so
/// the WebView does not need to retain timers or callbacks across evaluations.
pub fn wait_for_probe(
    selector: Option<&str>,
    text: Option<&str>,
    url_includes: Option<&str>,
) -> String {
    let selector = selector
        .map(json_string)
        .unwrap_or_else(|| "null".to_string());
    let text = text.map(json_string).unwrap_or_else(|| "null".to_string());
    let url_includes = url_includes
        .map(json_string)
        .unwrap_or_else(|| "null".to_string());
    format!(
        r#"(() => {{
  const selector = {selector};
  const text = {text};
  const urlIncludes = {url_includes};
  const pending = [];
  if (selector !== null) {{
    try {{
      if (!document.querySelector(selector)) pending.push("selector");
    }} catch (_) {{
      pending.push("selector");
    }}
  }}
  const documentText = document.body ? (document.body.innerText || document.body.textContent || "") : "";
  if (text !== null && !documentText.includes(text)) pending.push("text");
  if (urlIncludes !== null && !location.href.includes(urlIncludes)) pending.push("urlIncludes");
  return {{ matched: pending.length === 0, url: location.href, pending }};
}})()"#
    )
}

/// Wrap an arbitrary user expression so its value is returned to the callback.
pub fn evaluate(expression: &str) -> String {
    // Parenthesize so an object-literal expression isn't parsed as a block.
    format!("(() => {{ return ({expression}); }})()")
}

/// Decode the string a WebView `evaluate_script` callback yields into JSON.
///
/// WKWebView may hand back the value already JSON-encoded, or double-encoded (a
/// JSON string whose contents are themselves JSON). We try to parse, and if we
/// land on a string that itself looks like JSON, parse one more level so
/// callers see structured data rather than an opaque string.
pub fn parse_result(raw: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::String(inner)) => {
            let trimmed = inner.trim_start();
            if trimmed.starts_with('{') || trimmed.starts_with('[') {
                serde_json::from_str::<serde_json::Value>(&inner)
                    .unwrap_or(serde_json::Value::String(inner))
            } else {
                serde_json::Value::String(inner)
            }
        }
        Ok(value) => value,
        Err(_) => serde_json::Value::String(raw.to_string()),
    }
}

/// Minimal JSON string-literal encoder for embedding a value into a JS snippet.
fn json_string(value: &str) -> String {
    serde_json::Value::String(value.to_string()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn webview_results_decode_once_or_twice_without_losing_plain_strings() {
        for (raw, expected) in [
            (
                r#"{"url":"https://x","loading":false}"#,
                json!({"url":"https://x","loading":false}),
            ),
            (r#""{\"ok\":true}""#, json!({"ok":true})),
            (r#""hello""#, json!("hello")),
            ("undefined", json!("undefined")),
        ] {
            assert_eq!(parse_result(raw), expected, "{raw}");
        }
    }

    #[test]
    fn generated_scripts_escape_each_selector_text_and_key_input() {
        let input = "\"\n\\';throw 1;//";
        let escaped = r#""\"\n\\';throw 1;//""#;
        // Each case puts the hostile value into one argument independently, so
        // an escaped sibling cannot conceal an unescaped interpolation.
        for script in [
            click(input),
            type_text(input, "text"),
            type_text("#input", input),
            press(input, &[]),
            scroll(1.0, 2.0, Some(input)),
            wait_for_probe(Some(input), None, None),
            wait_for_probe(None, Some(input), None),
            wait_for_probe(None, None, Some(input)),
        ] {
            assert!(script.contains(escaped), "{script}");
            assert!(!script.contains(input), "unescaped input: {script}");
        }
    }
}
