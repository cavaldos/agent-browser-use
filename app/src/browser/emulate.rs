//! Device emulation: phone/tablet presets, screen size, pixel ratio, touch, user agent,
//! locale, time zone, dark mode, geolocation and network speed. The settings are kept in
//! Browser.emu and applied to every tab (also tabs opened later), because the shared
//! screen is one size for everybody.

use serde_json::{json, Value};

use crate::browser::{Browser, Tab, View};
use crate::stealth::{UserAgentOverrideParams, SET_USER_AGENT_OVERRIDE};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Geo {
    pub lat: f64,
    pub lon: f64,
    pub accuracy: f64,
}

/// Emulation is the current set of overrides; the zero value means "plain desktop Chrome".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Emulation {
    pub device: String,
    /// 0 = keep the shared screen size
    pub width: i64,
    pub height: i64,
    /// device pixel ratio, 0 = 1
    pub scale: f64,
    pub mobile: bool,
    pub touch: bool,
    /// empty = Chrome's own
    pub user_agent: String,
    pub locale: String,
    pub timezone: String,
    /// light, dark or empty
    pub color_scheme: String,
    pub geo: Option<Geo>,
    /// "", offline, slow-3g, fast-3g, 4g
    pub network: String,
}

pub struct DevicePreset {
    pub name: &'static str,
    pub width: i64,
    pub height: i64,
    pub scale: f64,
    pub mobile: bool,
    pub user_agent: &'static str,
}

const IPHONE_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";

pub const DEVICE_PRESETS: [DevicePreset; 8] = [
    DevicePreset { name: "iPhone 15", width: 393, height: 852, scale: 3.0, mobile: true, user_agent: IPHONE_UA },
    DevicePreset { name: "iPhone SE", width: 375, height: 667, scale: 2.0, mobile: true, user_agent: IPHONE_UA },
    DevicePreset { name: "iPad", width: 820, height: 1180, scale: 2.0, mobile: true, user_agent: "Mozilla/5.0 (iPad; CPU OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1" },
    DevicePreset { name: "Pixel 8", width: 412, height: 915, scale: 2.625, mobile: true, user_agent: "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36" },
    DevicePreset { name: "Galaxy S23", width: 360, height: 780, scale: 3.0, mobile: true, user_agent: "Mozilla/5.0 (Linux; Android 14; SM-S911B) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36" },
    DevicePreset { name: "Galaxy Tab S9", width: 800, height: 1280, scale: 2.0, mobile: true, user_agent: "Mozilla/5.0 (Linux; Android 14; SM-X710) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36" },
    DevicePreset { name: "Laptop", width: 1366, height: 768, scale: 1.0, mobile: false, user_agent: "" },
    DevicePreset { name: "Desktop HD", width: 1920, height: 1080, scale: 1.0, mobile: false, user_agent: "" },
];

/// strings.Fields + Join "" + lower-case
fn preset_key(s: &str) -> String {
    s.split_whitespace().collect::<String>().to_lowercase()
}

pub fn find_preset(name: &str) -> Option<&'static DevicePreset> {
    let key = preset_key(name);
    DEVICE_PRESETS
        .iter()
        .find(|p| p.name.replace(' ', "").to_lowercase() == key)
}

pub fn preset_names() -> String {
    DEVICE_PRESETS
        .iter()
        .map(|p| p.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// networkProfiles: latency in ms, throughput in bytes per second.
pub fn network_profile(name: &str) -> Option<(f64, f64, f64)> {
    match name {
        "slow-3g" => Some((400.0, 50.0 * 1024.0, 50.0 * 1024.0)),
        "fast-3g" => Some((150.0, 180.0 * 1024.0, 84.0 * 1024.0)),
        "4g" => Some((60.0, 1.5 * 1024.0 * 1024.0, 750.0 * 1024.0)),
        _ => None,
    }
}

/// EmulateOpts is what browser_emulate asks for; None fields stay as they are.
#[derive(Debug, Clone, Default)]
pub struct EmulateOpts {
    pub device: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub scale: Option<f64>,
    pub mobile: Option<bool>,
    pub touch: Option<bool>,
    pub user_agent: Option<String>,
    pub locale: Option<String>,
    pub timezone: Option<String>,
    pub color_scheme: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub accuracy: Option<f64>,
    pub network: Option<String>,
}

impl EmulateOpts {
    /// `empty` (note: Accuracy is not looked at).
    pub fn is_empty(&self) -> bool {
        self.device.is_empty()
            && self.width.is_none()
            && self.height.is_none()
            && self.scale.is_none()
            && self.mobile.is_none()
            && self.touch.is_none()
            && self.user_agent.is_none()
            && self.locale.is_none()
            && self.timezone.is_none()
            && self.color_scheme.is_none()
            && self.lat.is_none()
            && self.lon.is_none()
            && self.network.is_none()
    }
}

impl Emulation {
    /// merge applies opts on top of e. A device preset comes first, so single fields can adjust it.
    /// On error the (partly changed) emulation is dropped, like `return e, err` that callers ignore.
    pub fn merge(&self, o: &EmulateOpts) -> Result<Emulation, String> {
        let mut e = self.clone();
        let d = o.device.trim();
        if !d.is_empty() {
            if d.eq_ignore_ascii_case("reset")
                || d.eq_ignore_ascii_case("none")
                || d.eq_ignore_ascii_case("desktop")
            {
                e = Emulation::default();
            } else {
                let Some(p) = find_preset(d) else {
                    return Err(format!(
                        "unknown device {d:?}. Known devices: {} (or set width, height, scale, mobile yourself; \"reset\" turns emulation off)",
                        preset_names()
                    ));
                };
                e.device = p.name.to_string();
                e.width = p.width;
                e.height = p.height;
                e.scale = p.scale;
                e.mobile = p.mobile;
                e.touch = p.mobile;
                e.user_agent = p.user_agent.to_string();
            }
        }
        if let Some(w) = o.width {
            e.width = w;
        }
        if let Some(h) = o.height {
            e.height = h;
        }
        if (o.width.is_some() || o.height.is_some() || o.scale.is_some() || o.mobile.is_some())
            && o.device.is_empty()
        {
            e.device = "custom".into();
        }
        if let Some(s) = o.scale {
            e.scale = s;
        }
        if let Some(m) = o.mobile {
            e.mobile = m;
            if o.touch.is_none() {
                e.touch = m;
            }
        }
        if let Some(t) = o.touch {
            e.touch = t;
        }
        if let Some(u) = &o.user_agent {
            e.user_agent = u.clone();
        }
        if let Some(l) = &o.locale {
            e.locale = l.clone();
        }
        if let Some(t) = &o.timezone {
            e.timezone = t.clone();
        }
        if let Some(c) = &o.color_scheme {
            match c.to_lowercase().as_str() {
                c @ ("light" | "dark") => e.color_scheme = c.to_string(),
                "" | "none" | "no-preference" | "default" => e.color_scheme = String::new(),
                _ => {
                    return Err(format!(
                        "colorScheme must be light, dark or no-preference, not {c:?}"
                    ))
                }
            }
        }
        if o.lat.is_some() || o.lon.is_some() {
            let (Some(lat), Some(lon)) = (o.lat, o.lon) else {
                return Err("give both latitude and longitude".into());
            };
            if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
                return Err("latitude must be -90..90 and longitude -180..180".into());
            }
            e.geo = Some(Geo {
                lat,
                lon,
                accuracy: o.accuracy.unwrap_or(100.0),
            });
        }
        if let Some(n) = &o.network {
            let mut lower = n.to_lowercase();
            if network_profile(&lower).is_none()
                && !["", "online", "offline", "none"].contains(&lower.as_str())
            {
                return Err(format!(
                    "network must be online, offline, slow-3g, fast-3g or 4g, not {n:?}"
                ));
            }
            if lower == "online" || lower == "none" {
                lower = String::new();
            }
            e.network = lower;
        }
        if e.scale < 0.0 || e.scale > 5.0 {
            return Err("scale (device pixel ratio) must be between 0 and 5".into());
        }
        if (e.width != 0 && !(200..=2560).contains(&e.width))
            || (e.height != 0 && !(200..=2560).contains(&e.height))
        {
            return Err("width and height must be between 200 and 2560".into());
        }
        if (e.width == 0) != (e.height == 0) {
            return Err("give width and height together".into());
        }
        Ok(e)
    }
}

/// `%q` for the plain strings used in these messages is `{:?}`; `%g` of a float is `{}`.
impl std::fmt::Display for Emulation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if *self == Emulation::default() {
            return f.write_str("No emulation (normal desktop Chrome).");
        }
        let e = self;
        let mut p: Vec<String> = Vec::new();
        p.push(format!(
            "Device: {}",
            if e.device.is_empty() { "-" } else { &e.device }
        ));
        if e.width > 0 {
            p.push(format!(
                "Screen: {}×{} @{}x",
                e.width,
                e.height,
                e.scale.max(1.0)
            ));
        }
        p.push(format!("Mobile: {}, touch: {}", e.mobile, e.touch));
        if !e.user_agent.is_empty() {
            p.push(format!("User agent: {}", e.user_agent));
        }
        if !e.locale.is_empty() {
            p.push(format!("Locale: {}", e.locale));
        }
        if !e.timezone.is_empty() {
            p.push(format!("Time zone: {}", e.timezone));
        }
        if !e.color_scheme.is_empty() {
            p.push(format!("Color scheme: {}", e.color_scheme));
        }
        if let Some(g) = &e.geo {
            p.push(format!(
                "Location: {:.5}, {:.5} (±{}m)",
                g.lat, g.lon, g.accuracy
            ));
        }
        if !e.network.is_empty() {
            p.push(format!("Network: {}", e.network));
        }
        write!(f, "### Emulation\n- {}\n", p.join("\n- "))
    }
}

/// emulation.SetDeviceMetricsOverrideParams (the fields metrics fills).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub width: i64,
    pub height: i64,
    pub device_scale_factor: f64,
    pub mobile: bool,
    pub screen_width: i64,
    pub screen_height: i64,
}

impl Metrics {
    /// cdproto's JSON: width, height, deviceScaleFactor and mobile always; the screen size when set.
    pub fn to_json(self) -> Value {
        let mut v = json!({
            "width": self.width,
            "height": self.height,
            "deviceScaleFactor": self.device_scale_factor,
            "mobile": self.mobile,
        });
        if self.screen_width != 0 {
            v["screenWidth"] = json!(self.screen_width);
        }
        if self.screen_height != 0 {
            v["screenHeight"] = json!(self.screen_height);
        }
        v
    }
}

/// metrics is the screen override for a tab: the shared screen size, unless a device sets its own.
pub fn metrics(v: View, e: &Emulation) -> Metrics {
    let mut p = Metrics {
        width: v.width,
        height: v.height,
        device_scale_factor: 1.0,
        mobile: e.mobile,
        screen_width: 0,
        screen_height: 0,
    };
    if e.mobile {
        p.screen_width = v.width;
        p.screen_height = v.height;
    } else {
        p.screen_width = 1920;
        p.screen_height = 1080;
        if v.width > p.screen_width {
            p.screen_width = v.width;
        }
        if v.height > p.screen_height {
            p.screen_height = v.height;
        }
    }
    if e.scale > 0.0 {
        p.device_scale_factor = e.scale;
    } else if !e.mobile {
        p.device_scale_factor = 2.0;
    }
    p
}

impl Browser {
    /// applyEmulation sets every override of e on one tab. Errors of single settings are ignored
    /// on purpose: a setting Chrome refuses must not stop the others.
    pub async fn apply_emulation(&self, t: &Tab, v: View, e: &Emulation) -> Result<(), String> {
        t.call(
            "Emulation.setDeviceMetricsOverride",
            metrics(v, e).to_json(),
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
        let _ = t
            .call(
                "Emulation.setTouchEmulationEnabled",
                json!({"enabled": e.touch, "maxTouchPoints": 5}),
                None,
            )
            .await;
        let mut ua = self.ua.clone();
        if !e.user_agent.is_empty() {
            ua = e.user_agent.clone();
        }
        if !ua.is_empty() {
            let mut up = UserAgentOverrideParams {
                user_agent: ua,
                ..Default::default()
            };
            if e.user_agent.is_empty() {
                // Chrome's own user agent: without its client hints Chrome would stop
                // sending Sec-CH-UA and empty navigator.userAgentData, which a real one never does.
                up.user_agent_metadata = self.ua_meta.clone();
            }
            if !e.locale.is_empty() {
                up.accept_language = e.locale.replace('_', "-");
            }
            let _ = t
                .call(
                    SET_USER_AGENT_OVERRIDE,
                    serde_json::to_value(&up).unwrap_or(Value::Null),
                    None,
                )
                .await;
        }
        // omitempty: an empty locale is an empty object (clears the override)
        let locale = if e.locale.is_empty() {
            json!({})
        } else {
            json!({"locale": e.locale})
        };
        let _ = t.call("Emulation.setLocaleOverride", locale, None).await;
        let _ = t
            .call(
                "Emulation.setTimezoneOverride",
                json!({"timezoneId": e.timezone}),
                None,
            )
            .await;
        let media = if e.color_scheme.is_empty() {
            json!({})
        } else {
            json!({"features": [{"name": "prefers-color-scheme", "value": e.color_scheme}]})
        };
        let _ = t.call("Emulation.setEmulatedMedia", media, None).await;
        match &e.geo {
            Some(g) => {
                let _ = t
                    .call(
                        "Emulation.setGeolocationOverride",
                        json!({"latitude": g.lat, "longitude": g.lon, "accuracy": g.accuracy}),
                        None,
                    )
                    .await;
            }
            None => {
                let _ = t
                    .call("Emulation.clearGeolocationOverride", json!({}), None)
                    .await;
            }
        }
        let mut np = json!({
            "offline": e.network == "offline",
            "latency": 0.0,
            "downloadThroughput": -1.0,
            "uploadThroughput": -1.0,
        });
        if let Some((lat, down, up)) = network_profile(&e.network) {
            np["latency"] = json!(lat);
            np["downloadThroughput"] = json!(down);
            np["uploadThroughput"] = json!(up);
        }
        let _ = t.call("Network.overrideNetworkState", np, None).await;
        Ok(())
    }

    /// SetEmulation applies new emulation settings to all tabs and resizes the shared screen to match.
    /// It runs from the command queue and waits for the result.
    pub async fn set_emulation(&self, o: EmulateOpts) -> Result<Emulation, String> {
        self.call(move |b| async move {
            let (e, v, tabs, active) = {
                let mut st = b.st();
                let e = st.emu.merge(&o)?;
                let mut v = st.view;
                if e.width > 0 {
                    v.width = e.width;
                    v.height = e.height;
                } else {
                    v.width = b.opt.width;
                    v.height = b.opt.height;
                }
                st.emu = e.clone();
                st.view = v;
                st.got_frame = false;
                (e, v, st.tabs.clone(), st.active.clone())
            };

            if e.geo.is_some() {
                // without permission a page's geolocation request is just refused
                let _ = b
                    .root_call(
                        "Browser.setPermission",
                        json!({"permission": {"name": "geolocation"}, "setting": "granted"}),
                        None,
                    )
                    .await;
            }
            for t in &tabs {
                let _ = b.apply_emulation(t, v, &e).await;
            }
            if let Some(active) = active {
                let _ = active.call("Page.stopScreencast", json!({}), None).await;
                b.start_screencast(&active, v).await;
            }
            b.hub.broadcast_view(v);
            b.hub.broadcast_color_scheme(e.color_scheme.clone());
            Ok(e)
        })
        .await
    }

    /// `Emulation`
    pub fn emulation(&self) -> Emulation {
        self.st().emu.clone()
    }
}
