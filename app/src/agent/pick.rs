//! The viewer's "Pick" tool: a person points at an element in the shared browser and gets a short
//! code (e.g. el-3f9a). They paste it to a coding agent, which reads the element with
//! browser_get_element and can then act on it by ref.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::agent::{eval_js_as, quote, Agent};
use crate::browser::{Browser, View};
use crate::frames::jsq;
use crate::{Client, Hub, Msg};
use serde::{Deserialize, Serialize};

/// pick.rs: `type Pick`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Pick {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub code: String,
    #[serde(rename = "ref")]
    pub ref_: String,
    pub tag: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub selector: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub html: String,
    pub rect: PickBox,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// size the rect is relative to
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<View>,
}

/// pick.rs: `type pickBox` (no omitempty).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PickBox {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

const MAX_PICKS: usize = 50;

/// keeps `picks` and `pickOrder` on the Hub under `h.mu`; here they are one process-wide store.
#[derive(Default)]
struct Picks {
    picks: HashMap<String, Pick>,
    order: Vec<String>,
}

static PICKS: Mutex<Option<Picks>> = Mutex::new(None);

fn picks() -> MutexGuard<'static, Option<Picks>> {
    PICKS.lock().unwrap_or_else(|e| e.into_inner())
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

/// pick answers a viewer's hover or click: it finds the element under the point.
pub async fn pick(hub: &Arc<Hub>, b: &Arc<Browser>, c: &Arc<Client>, m: Msg) {
    let (tabs, active, _, _) = b.state();
    let (t, v) = {
        let st = b.st();
        (st.active.clone(), st.view)
    };
    let Some(t) = t else { return };
    let x = clamp01(m.x) * v.width as f64;
    let y = clamp01(m.y) * v.height as f64;
    let Ok(p) = eval_js_as::<Option<Pick>>(&t, &format!("__abu.pick({x}, {y})")).await else {
        return;
    };
    let Some(mut p) = p else {
        hub.send_to(
            c,
            Msg {
                type_: "pick".into(),
                event: m.event,
                ..Default::default()
            },
        );
        return;
    };
    p.view = Some(v);
    if m.event == "click" {
        for ti in &tabs {
            if ti.id == active {
                p.url = ti.url.clone();
                p.title = ti.title.clone();
            }
        }
        p.code = save_pick(&p);
        if let Some(a) = crate::assets::global() {
            a.add(
                crate::assets::Asset {
                    kind: "element".into(),
                    name: p.code.clone(),
                    mime: "text/html".into(),
                    text: pick_text(&p),
                    url: p.url.clone(),
                    title: p.title.clone(),
                    by: c.user().name,
                    ..Default::default()
                },
                None,
            );
        }
    } else {
        p = Pick {
            rect: p.rect,
            view: Some(v),
            tag: p.tag,
            role: p.role,
            name: p.name,
            ..Default::default()
        };
    }
    hub.send_to(
        c,
        Msg {
            type_: "pick".into(),
            event: m.event,
            pick: Some(p),
            ..Default::default()
        },
    );
}

fn save_pick(p: &Pick) -> String {
    let mut g = picks();
    let ps = g.get_or_insert_with(Picks::default);
    let mut code;
    loop {
        let id = crate::new_id();
        code = format!("el-{}", &id[..id.len().min(4)]);
        if !ps.picks.contains_key(&code) {
            break;
        }
    }
    ps.picks.insert(code.clone(), p.clone());
    ps.order.push(code.clone());
    if ps.order.len() > MAX_PICKS {
        let old = ps.order.remove(0);
        ps.picks.remove(&old);
    }
    code
}

impl Agent {
    /// GetPicked describes an element a person picked, and gives the agent a fresh ref for it.
    pub async fn get_picked(&self, code: &str) -> Result<String, String> {
        let code = code.trim().trim_start_matches('@').trim().to_lowercase();
        let old = picks().as_ref().and_then(|p| p.picks.get(&code).cloned());
        let Some(old) = old else {
            return Err(format!("no picked element with code {code:?} (the server keeps the last {MAX_PICKS}). Ask the person to pick it again in the viewer"));
        };
        let expr = format!("__abu.resolve({}, {})", jsq(&old.ref_), jsq(&old.selector));
        let p: Option<Pick> = self
            .look(|t| async move { eval_js_as::<Option<Pick>>(&t, &expr).await })
            .await?;
        // dereferences the nil pointer if the page returns nothing; a clean error here
        let p = p.ok_or_else(|| "the element is not on the page any more".to_string())?;
        let mut s = format!("### Picked element {code}\n");
        if !old.url.is_empty() {
            s.push_str(&format!("- Page: {} ({})\n", old.url, old.title));
        }
        s.push_str(&format!(
            "- Ref: {} (use it with browser_click, browser_type, browser_hover…)\n",
            p.ref_
        ));
        let role = if p.role.is_empty() {
            String::new()
        } else {
            format!(", role {}", p.role)
        };
        s.push_str(&format!("- Tag: <{}>{role}\n", p.tag));
        if !p.name.is_empty() {
            s.push_str(&format!("- Name: {}\n", quote(&p.name)));
        }
        s.push_str(&format!("- CSS selector: {}\n", p.selector));
        s.push_str(&format!(
            "- Position: x={:.0} y={:.0}, {:.0}×{:.0} px\n",
            p.rect.x, p.rect.y, p.rect.w, p.rect.h
        ));
        if !p.text.is_empty() {
            s.push_str(&format!("- Text: {}\n", quote(&p.text)));
        }
        s.push_str(&format!("\n```html\n{}\n```\n", p.html));
        Ok(s)
    }
}

/// pickText is what the Assets tab keeps of a picked element: where it is and its markup.
fn pick_text(p: &Pick) -> String {
    let mut s = format!("<{}>", p.tag);
    if !p.name.is_empty() {
        s.push_str(&format!(" {}", quote(&p.name)));
    }
    s.push_str(&format!("\nselector: {}\n\n{}", p.selector, p.html));
    s
}
