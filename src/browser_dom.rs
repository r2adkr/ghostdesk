//! Reading and driving web pages by their elements instead of by screenshots.
//!
//! A screenshot costs an agent over a thousand tokens every time. These actions return a few lines
//! of text instead: what can be clicked or typed into, with a short number (`ref`) for each, so the
//! next call can say `click ref=7` rather than guess pixel coordinates from an image. Clicks and
//! typing still go through real input events, so pages behave as if a person did it.
//!
//! The page is only read: nothing here runs agent-supplied JavaScript. The scripts below are fixed
//! and get their arguments as JSON.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::browser::{BrowserWorkspace, MouseButton};

/// Shared helpers, installed once per page load (`window.__gd`).
const HELPERS: &str = r#"
(function(){
  if (window.__gd) return window.__gd;
  const gd = { n: 0 };
  const SEL = 'a[href],button,input:not([type=hidden]),select,textarea,summary,label[for],[role=button],[role=link],[role=checkbox],[role=radio],[role=tab],[role=menuitem],[role=option],[role=switch],[role=combobox],[role=textbox],[role=searchbox],[onclick],[contenteditable=""],[contenteditable="true"],[tabindex]:not([tabindex="-1"])';
  gd.SEL = SEL;
  gd.visible = (el) => {
    const r = el.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) return false;
    const cs = getComputedStyle(el);
    return cs.visibility !== 'hidden' && cs.display !== 'none' && +cs.opacity !== 0;
  };
  gd.inView = (el) => {
    const r = el.getBoundingClientRect();
    return r.bottom > 0 && r.right > 0 && r.top < innerHeight && r.left < innerWidth;
  };
  const clip = (s, n) => { s = (s || '').replace(/\s+/g, ' ').trim(); return s.length > n ? s.slice(0, n - 1) + '…' : s; };
  gd.clip = clip;
  gd.role = (el) => {
    const t = el.tagName.toLowerCase();
    const r = el.getAttribute('role');
    if (t === 'a') return 'link';
    if (t === 'button' || r === 'button') return 'button';
    if (t === 'select') return 'select';
    if (t === 'textarea') return 'textarea';
    if (t === 'input') return (!el.type || el.type === 'text') ? 'input' : el.type;
    if (t === 'summary') return 'summary';
    return r || (el.isContentEditable ? 'textbox' : t);
  };
  gd.label = (el) => {
    const t = el.tagName.toLowerCase();
    let s = el.getAttribute('aria-label') || '';
    if (!s && el.labels && el.labels.length) s = el.labels[0].innerText;
    if (!s && (t === 'input' || t === 'textarea' || t === 'select')) s = el.placeholder || el.title || el.name || '';
    if (!s && t === 'input' && /^(button|submit|reset)$/.test(el.type)) s = el.value;
    if (!s) s = el.innerText || el.textContent || '';
    if (!s) s = el.title || el.alt || (el.querySelector('img[alt]') || {}).alt || '';
    return clip(s, 60);
  };
  // every name a person could call a control by: its label, placeholder, visible text, name/id
  gd.names = (el) => {
    const out = [gd.label(el)];
    for (const x of [el.getAttribute('aria-label'), el.placeholder, el.title, el.name, el.id, el.innerText, el.value && el.type !== 'password' ? el.value : '']) if (x) out.push(clip(x, 80));
    if (el.labels) for (const l of el.labels) out.push(clip(l.innerText, 80));
    return out.map((x) => x.toLowerCase()).filter(Boolean);
  };
  gd.line = (el, ref) => {
    const t = el.tagName.toLowerCase();
    let out = '[' + ref + '] ' + gd.role(el);
    const l = gd.label(el);
    if (l) out += ' "' + l + '"';
    if (t === 'input' || t === 'textarea') {
      if (el.type === 'checkbox' || el.type === 'radio') out += el.checked ? ' ✓' : ' ☐';
      else if (el.type === 'password') out += el.value ? ' =••••' : '';
      else if (el.value) out += ' =' + JSON.stringify(clip(el.value, 40));
    } else if (t === 'select') {
      const o = el.selectedOptions && el.selectedOptions[0];
      if (o) out += ' =' + JSON.stringify(clip(o.text, 40));
    } else if (el.getAttribute('aria-checked') === 'true') out += ' ✓';
    if (el.disabled || el.getAttribute('aria-disabled') === 'true') out += ' (비활성)';
    if (t === 'a') {
      const h = el.getAttribute('href') || '';
      if (h && !h.startsWith('javascript:')) out += ' → ' + clip(h.replace(/^https?:\/\/[^/]+/, ''), 50);
    }
    return out;
  };
  // numbers elements 1..n and returns them (marks them so later calls can find them again)
  gd.number = (els) => {
    document.querySelectorAll('[data-gdref]').forEach((e) => e.removeAttribute('data-gdref'));
    els.forEach((e, i) => e.setAttribute('data-gdref', String(i + 1)));
  };
  gd.interactive = (all) => {
    const seen = new Set();
    const out = [];
    for (const el of document.querySelectorAll(SEL)) {
      if (seen.has(el) || !gd.visible(el)) continue;
      if (!all && !gd.inView(el)) continue;
      // a label wrapping its own control is the control
      if (el.tagName === 'LABEL' && el.control) continue;
      seen.add(el);
      out.push(el);
    }
    return out;
  };
  gd.find = (ref) => document.querySelector('[data-gdref="' + ref + '"]');
  window.__gd = gd;
  return gd;
})()
"#;

fn call(body: &str, args: &Value) -> String {
    format!("(function(){{ const gd = {HELPERS}; const a = {args}; {body} }})()")
}

impl BrowserWorkspace {
    async fn js(&self, body: &str, args: Value) -> Result<Value> {
        let v = self.eval(&call(body, &args)).await?;
        if let Some(e) = v["error"].as_str() {
            bail!("{e}");
        }
        Ok(v)
    }

    /// Title, address, scroll position and the clickable/typeable elements, numbered.
    ///
    /// `all`: the whole page instead of what is on screen. `max`: most elements to list.
    pub async fn dom_read(&self, all: bool, max: usize) -> Result<String> {
        let v = self
            .js(
                r#"
                const els = gd.interactive(a.all);
                const shown = els.slice(0, a.max);
                gd.number(shown);
                const lines = shown.map((e, i) => gd.line(e, i + 1));
                const h = [];
                for (const x of document.querySelectorAll('h1,h2,h3')) {
                  if (gd.visible(x) && (a.all || gd.inView(x))) h.push(gd.clip(x.innerText, 80));
                  if (h.length >= 6) break;
                }
                const de = document.documentElement;
                return { title: document.title, url: location.href, y: Math.round(scrollY), h: de.scrollHeight, vh: innerHeight,
                         total: els.length, headings: h, lines };
                "#,
                json!({ "all": all, "max": max }),
            )
            .await?;
        let lines: Vec<&str> = v["lines"].as_array().map(|l| l.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
        let mut out = format!(
            "{} — {}\n스크롤 {}/{}px (화면 높이 {})",
            v["title"].as_str().unwrap_or(""),
            v["url"].as_str().unwrap_or(""),
            v["y"],
            v["h"].as_i64().unwrap_or(0) - v["vh"].as_i64().unwrap_or(0),
            v["vh"]
        );
        if let Some(h) = v["headings"].as_array().filter(|h| !h.is_empty()) {
            out.push_str(&format!("\n제목: {}", h.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" | ")));
        }
        out.push('\n');
        out.push_str(&lines.join("\n"));
        let total = v["total"].as_u64().unwrap_or(0) as usize;
        if total > lines.len() {
            out.push_str(&format!("\n… 외 {}개 (scope=page 또는 find 로 찾기)", total - lines.len()));
        }
        if lines.is_empty() {
            out.push_str("(화면에 누를 수 있는 요소가 없어요. text 로 본문을 읽거나 스크롤해 보세요)");
        }
        Ok(out)
    }

    /// Visible text of the page (or of one element), whitespace collapsed and cut to `max` characters.
    pub async fn dom_text(&self, selector: Option<&str>, max: usize) -> Result<String> {
        let v = self
            .js(
                r#"
                let root = document.body;
                if (a.selector) {
                  try { root = document.querySelector(a.selector); } catch (e) { return { error: '선택자가 올바르지 않아요: ' + a.selector }; }
                  if (!root) return { error: '선택자에 맞는 요소가 없어요: ' + a.selector };
                }
                const raw = (root.innerText || root.textContent || '').replace(/[ \t]+/g, ' ').replace(/\n\s*\n+/g, '\n').trim();
                return { text: raw.slice(0, a.max), len: raw.length };
                "#,
                json!({ "selector": selector, "max": max }),
            )
            .await?;
        let text = v["text"].as_str().unwrap_or_default();
        let len = v["len"].as_u64().unwrap_or(0) as usize;
        let shown = text.chars().count();
        Ok(if len > shown { format!("{text}\n… (전체 {len}자 중 {shown}자. max_chars 를 늘리거나 selector 로 좁히세요)") } else { text.to_string() })
    }

    /// Elements whose label or text contains `query`, numbered like `dom_read`.
    pub async fn dom_find(&self, query: &str) -> Result<String> {
        let v = self
            .js(
                r#"
                const q = a.query.toLowerCase();
                const found = [];
                const seen = new Set();
                const push = (el) => { if (!seen.has(el) && gd.visible(el)) { seen.add(el); found.push(el); } };
                for (const el of gd.interactive(true)) {
                  if ((gd.label(el) + ' ' + (el.innerText || '') + ' ' + (el.value || '') + ' ' + (el.name || '') + ' ' + (el.id || '')).toLowerCase().includes(q)) push(el);
                }
                // plain text that isn't a control (headings, prices, messages): the smallest element holding it
                if (found.length < 8) {
                  const w = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
                  while (w.nextNode() && found.length < 12) {
                    const n = w.currentNode;
                    if (n.nodeValue.toLowerCase().includes(q) && n.parentElement && !/^(script|style|noscript)$/i.test(n.parentElement.tagName)) push(n.parentElement);
                  }
                }
                const top = found.slice(0, 12);
                const marked = top;
                gd.number(marked);
                return { total: found.length, lines: marked.map((e, i) => {
                  const base = gd.line(e, i + 1);
                  return gd.SEL && e.matches(gd.SEL) ? base : '[' + (i + 1) + '] 글 "' + gd.clip(e.innerText, 80) + '"';
                }) };
                "#,
                json!({ "query": query }),
            )
            .await?;
        let lines: Vec<&str> = v["lines"].as_array().map(|l| l.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
        if lines.is_empty() {
            return Ok(format!("'{query}' 가 들어간 요소를 못 찾았어요 (화면 밖이면 scroll, 로딩 중이면 wait_for 를 써 보세요)"));
        }
        Ok(lines.join("\n"))
    }

    /// Center of the element for `target` (a `ref` number or, failing that, a text to find),
    /// scrolled into view. Returns the point and a short description.
    async fn dom_locate(&self, target: &Value, text: Option<&str>) -> Result<((f64, f64), String)> {
        let v = self
            .js(
                r#"
                let el = null;
                if (a.ref) {
                  el = gd.find(a.ref);
                  if (!el) return { error: '번호 ' + a.ref + ' 를 찾을 수 없어요. 페이지가 바뀌었다면 read 로 다시 번호를 받으세요' };
                } else if (a.text) {
                  const q = a.text.toLowerCase();
                  let best = null, rank = 9;
                  for (const e of gd.interactive(true)) {
                    for (const l of gd.names(e)) {
                      const r = l === q ? 0 : l.startsWith(q) ? 1 : l.includes(q) ? 2 : 9;
                      if (r < rank) { best = e; rank = r; }
                    }
                  }
                  if (!best) {
                    const some = gd.interactive(false).slice(0, 6).map((e) => '"' + gd.label(e) + '"').filter((x) => x !== '""').join(', ');
                    return { error: "'" + a.text + "' 라는 이름의 요소를 못 찾았어요. 화면에 있는 것: " + some + " (find 로 찾아 보세요)" };
                  }
                  el = best;
                } else return { error: 'ref(번호) 또는 label(이름) 이 필요해요' };
                el.scrollIntoView({ block: 'center', inline: 'center' });
                const r = el.getBoundingClientRect();
                const x = Math.min(Math.max(r.left + r.width / 2, 1), innerWidth - 1);
                const y = Math.min(Math.max(r.top + r.height / 2, 1), innerHeight - 1);
                const top = document.elementFromPoint(x, y);
                const covered = top && top !== el && !el.contains(top) && !top.contains(el);
                return { x, y, desc: gd.line(el, a.ref || '·').replace(/^\[[^\]]*\] /, ''), covered: covered ? gd.clip(top.innerText || top.tagName, 40) : '' };
                "#,
                json!({ "ref": target.as_u64(), "text": text }),
            )
            .await?;
        let (x, y) = (v["x"].as_f64().ok_or_else(|| anyhow!("요소 위치를 못 구했어요"))?, v["y"].as_f64().unwrap_or(0.0));
        let mut desc = v["desc"].as_str().unwrap_or_default().to_string();
        if let Some(c) = v["covered"].as_str().filter(|c| !c.is_empty()) {
            desc.push_str(&format!(" (다른 요소 '{c}' 에 가려져 있을 수 있어요)"));
        }
        Ok(((x, y), desc))
    }

    pub async fn dom_click(&self, target: &Value, text: Option<&str>, double: bool) -> Result<String> {
        let ((x, y), desc) = self.dom_locate(target, text).await?;
        self.click(Some((x, y)), MouseButton::Left, if double { 2 } else { 1 }, 0).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(format!("눌렀어요: {desc}"))
    }

    /// Clicks the field, replaces its content with `value` and optionally presses Enter.
    pub async fn dom_fill(&self, target: &Value, text: Option<&str>, value: &str, submit: bool) -> Result<String> {
        let ((x, y), desc) = self.dom_locate(target, text).await?;
        self.click(Some((x, y)), MouseButton::Left, 1, 0).await?;
        // select what is there so typing replaces it
        let _ = self
            .eval(
                "(function(){ const e = document.activeElement; if (!e) return; if (e.select) { try { e.select(); } catch (_) {} } \
                 else if (e.isContentEditable) document.execCommand('selectAll'); })()",
            )
            .await;
        if value.is_empty() {
            self.key("Delete", 1).await?;
        } else {
            self.type_text(value).await?;
        }
        if submit {
            self.key("Return", 1).await?;
        }
        Ok(format!("입력했어요: {desc}{}", if submit { " + Enter" } else { "" }))
    }

    /// Chooses an option of a `<select>` by its text or value.
    pub async fn dom_select(&self, target: &Value, text: Option<&str>, option: &str) -> Result<String> {
        let _ = self.dom_locate(target, text).await?;
        let v = self
            .js(
                r#"
                const el = a.ref ? gd.find(a.ref) : Array.from(document.querySelectorAll('select')).find((s) => gd.visible(s) && gd.names(s).some((n) => n.includes((a.label || '').toLowerCase())));
                if (!el || el.tagName !== 'SELECT') return { error: '목록(select) 요소가 아니에요. 직접 누르고 고르려면 click 을 쓰세요' };
                const q = a.option.toLowerCase();
                const opts = Array.from(el.options);
                const o = opts.find((x) => x.text.trim().toLowerCase() === q || x.value.toLowerCase() === q) || opts.find((x) => x.text.toLowerCase().includes(q));
                if (!o) return { error: '선택지에 없어요. 있는 것: ' + opts.slice(0, 12).map((x) => x.text.trim()).join(' | ') };
                el.value = o.value;
                el.dispatchEvent(new Event('input', { bubbles: true }));
                el.dispatchEvent(new Event('change', { bubbles: true }));
                return { chosen: o.text.trim() };
                "#,
                json!({ "ref": target.as_u64(), "label": text, "option": option }),
            )
            .await?;
        Ok(format!("골랐어요: {}", v["chosen"].as_str().unwrap_or(option)))
    }

    pub async fn dom_scroll_to(&self, target: &Value, text: Option<&str>) -> Result<String> {
        let (_, desc) = self.dom_locate(target, text).await?;
        Ok(format!("화면에 보이게 했어요: {desc}"))
    }

    /// Waits until `text` is on the page (or gone, with `gone`), up to `secs` seconds.
    pub async fn dom_wait_for(&self, text: &str, gone: bool, secs: f64) -> Result<String> {
        let start = Instant::now();
        let limit = Duration::from_secs_f64(secs.clamp(0.5, 60.0));
        loop {
            let present = self
                .js("return { p: (document.body.innerText || '').toLowerCase().includes(a.q) };", json!({ "q": text.to_lowercase() }))
                .await
                .map(|v| v["p"].as_bool().unwrap_or(false))
                .unwrap_or(false);
            if present != gone {
                return Ok(if gone { format!("'{text}' 가 사라졌어요") } else { format!("'{text}' 가 나타났어요") });
            }
            if start.elapsed() >= limit {
                bail!("{}초 안에 '{text}' 가 {}", limit.as_secs_f64(), if gone { "사라지지 않았어요" } else { "나타나지 않았어요" });
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    /// Links on the page: text and address, to read a list of results without opening each.
    pub async fn dom_links(&self, max: usize) -> Result<String> {
        let v = self
            .js(
                r#"
                const out = [];
                const seen = new Set();
                for (const e of document.querySelectorAll('a[href]')) {
                  if (!gd.visible(e)) continue;
                  const h = e.href;
                  if (!h || h.startsWith('javascript:') || seen.has(h)) continue;
                  seen.add(h);
                  out.push(gd.clip(e.innerText || e.title || (e.querySelector('img[alt]') || {}).alt || '', 60) + ' → ' + h);
                }
                return { total: out.length, lines: out.slice(0, a.max) };
                "#,
                json!({ "max": max }),
            )
            .await?;
        let lines: Vec<&str> = v["lines"].as_array().map(|l| l.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
        let total = v["total"].as_u64().unwrap_or(0) as usize;
        let mut out = lines.join("\n");
        if total > lines.len() {
            out.push_str(&format!("\n… 외 {}개", total - lines.len()));
        }
        Ok(out)
    }
}
