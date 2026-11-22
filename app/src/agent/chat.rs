//! Chat with coding agents. Every agent that connects over MCP (OpenCode, Pi, Claude Code…) is a
//! "seat" named after its clientInfo. People write "@opencode …" (or "@agent" for all of them) in the
//! viewer's chat; the agent reads its messages with browser_read_chat and answers with browser_say.

// ReadChat and formatChat (the MCP-facing half) use `HubState::chat_wake` (a watch counter) in place
// of closed-channel wake-up: subscribe under the lock, wait on `changed` after unlocking.

use crate::{agent, ChatMsg, Hub, HubState, Msg, User};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// a seat that has not been heard from for this long leaves the list
pub const SEAT_TTL: Duration = Duration::from_secs(30 * 60);

/// Seat is one connected coding agent (one MCP session). Two OpenCodes are two seats.
#[derive(Debug)]
pub struct Seat {
    /// what people type after @: client name + "#" + a random id, e.g. opencode#3f9a
    pub handle: String,
    /// the client name, e.g. opencode ("@opencode" reaches every OpenCode)
    pub base: String,
    /// last chat message id it has read
    pub chat_read: i64,
    pub last_seen: Instant,
    pub last_read: Option<Instant>,
    /// browser_read_chat calls waiting right now
    pub listening: i64,
    /// why it left the room: "" still here, "self" (browser_disconnect / session ended), else the person who disconnected it
    pub gone: String,
}

/// shares `*Seat`; the map and the MCP session both hold it. Lock it only while holding the hub
/// lock, so it can never deadlock.
pub type SeatRef = Arc<Mutex<Seat>>;
pub type Seats = HashMap<String, SeatRef>;

fn lock(s: &SeatRef) -> std::sync::MutexGuard<'_, Seat> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

/// `handleBad.ReplaceAllString(lower(trim(name)), "-")` then trim "-"; "" / agent / ai => coder.
pub fn seat_base(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    let mut b = String::new();
    let mut in_bad = false;
    for c in lower.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
            b.push(c);
            in_bad = false;
        } else if !in_bad {
            b.push('-');
            in_bad = true;
        }
    }
    let b = b.trim_matches('-');
    if b.is_empty() || b == "agent" || b == "ai" {
        "coder".into()
    } else {
        b.into()
    }
}

fn new_handle(base: &str) -> String {
    format!("{}#{:04x}", base, rand::random::<u16>())
}

/// seatUser is how a coding agent shows in the room (the web UI builds the same from the "agents" list).
pub fn seat_user(handle: &str) -> User {
    User {
        id: format!("agent:{handle}"),
        name: handle.to_string(),
        color: agent::agent_user().color,
        agent: true,
    }
}

impl ChatMsg {
    /// `cm.isFor(s)`.
    pub fn is_for(&self, s: &Seat) -> bool {
        self.to_bot || self.to.contains(&s.handle)
    }
}

/// `mentionRe` = `(?i)(?:^|\s)@([a-z0-9_-]+(?:#[a-z0-9]+)?)`, all matches, as lower-cased words.
fn mentions(text: &str) -> Vec<String> {
    let c: Vec<char> = text.chars().collect();
    let is_space = |ch: char| matches!(ch, ' ' | '\t' | '\n' | '\r' | '\x0c');
    let word = |ch: char| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-';
    let mut out = Vec::new();
    let mut i = 0; // end of the previous match
    while i < c.len() {
        // find the '@' of the next match
        let mut p = i;
        let found = loop {
            if p >= c.len() {
                break None;
            }
            if c[p] == '@' && ((p == 0 && i == 0) || (p > i && is_space(c[p - 1]))) {
                break Some(p);
            }
            p += 1;
        };
        let Some(at) = found else { break };
        let mut e = at + 1;
        while e < c.len() && word(c[e]) {
            e += 1;
        }
        if e == at + 1 {
            i = at + 1; // "@" alone: no match here
            continue;
        }
        let mut end = e;
        if end < c.len() && c[end] == '#' {
            let mut f = end + 1;
            while f < c.len() && c[f].is_ascii_alphanumeric() {
                f += 1;
            }
            if f > end + 1 {
                end = f;
            }
        }
        out.push(c[at + 1..end].iter().collect::<String>().to_lowercase());
        i = end;
    }
    out
}

impl HubState {
    /// caller holds h.mu
    pub fn seat_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .seats
            .iter()
            .filter(|(_, s)| {
                let s = lock(s);
                s.last_seen.elapsed() < SEAT_TTL || s.listening > 0
            })
            .map(|(k, _)| k.clone())
            .collect();
        names.sort();
        names
    }

    /// caller holds h.mu
    pub fn broadcast_seats(&mut self) {
        let n = self.seat_names();
        if n == self.seats_sent {
            return;
        }
        self.seats_sent = n.clone();
        self.broadcast(
            &Msg {
                type_: "agents".into(),
                agents: Some(n),
                ..Default::default()
            },
            None,
        );
    }

    /// dropSeat: caller holds h.mu
    pub fn drop_seat(&mut self, hub: &Hub, handle: &str, seat: &SeatRef, by: &str) {
        match self.seats.get(handle) {
            Some(s) if Arc::ptr_eq(s, seat) => {}
            _ => return,
        }
        self.seats.remove(handle);
        lock(seat).gone = if by.is_empty() {
            "self".into()
        } else {
            by.into()
        };
        self.broadcast_seats();
        if self.ctrl_is_agent() && self.agent.user().id == seat_user(handle).id {
            hub.hooks_agent_release(self); // give control back to whoever had it
        }
        if let Some(b) = hub.browser() {
            b.release_owner(handle); // its tab locks (and its sub-agents')
        }
        self.wake_agents(); // a browser_read_chat in progress returns now
        match by {
            "" => {}
            "self" => self.add_system_chat(&format!("👋 @{handle} disconnected from the browser")),
            _ => self.add_system_chat(&format!("🔌 {by} disconnected @{handle}")),
        }
    }

    /// kickSeat: a person in the room disconnects a coding agent by its @handle. caller holds h.mu
    pub fn kick_seat(&mut self, hub: &Hub, handle: &str, by: &str) {
        let handle = handle.strip_prefix('@').unwrap_or(handle);
        if let Some(s) = self.seats.get(handle).cloned() {
            self.drop_seat(hub, handle, &s, by);
        }
    }

    /// routeChat finds who a chat message is for: "@agent" is every seat, "@opencode" just that one.
    /// caller holds h.mu
    pub fn route_chat(&self, cm: &mut ChatMsg) {
        for w in mentions(&cm.text) {
            if w == "agent" || w == "ai" {
                cm.to_bot = true;
            } else if self.seats.contains_key(&w) {
                if !cm.to.contains(&w) {
                    cm.to.push(w);
                }
            } else {
                // "@opencode": every OpenCode
                for (k, s) in &self.seats {
                    if lock(s).base == w && !cm.to.contains(k) {
                        cm.to.push(k.clone());
                    }
                }
            }
        }
    }

    /// noticeUnheard tells the room when a message is for agents that are not listening. caller holds h.mu
    pub fn notice_unheard(&mut self, cm: &ChatMsg) {
        let mut cold = Vec::new();
        if cm.to_bot && self.seats.is_empty() {
            self.add_system_chat("⚠️ No coding agent is connected to this browser yet");
            return;
        }
        for (k, s) in &self.seats {
            let s = lock(s);
            // time.Since(zero) is huge, so a seat that never read counts as cold
            let cold_read = s
                .last_read
                .is_none_or(|t| t.elapsed() > Duration::from_secs(120));
            if cm.is_for(&s) && s.listening == 0 && cold_read {
                cold.push(format!("@{k}"));
            }
        }
        if !cold.is_empty() {
            cold.sort();
            self.add_system_chat(&format!(
                "⚠️ {} isn’t listening yet. Ask it to call browser_read_chat (time: 60) in a loop",
                cold.join(", ")
            ));
        }
    }

    /// wakeAgents releases agents waiting in browser_read_chat. caller holds h.mu
    pub fn wake_agents(&mut self) {
        self.chat_wake.send_modify(|v| *v += 1);
    }

    /// agentAs makes the shared agent (the one holding control) show as this seat. caller holds h.mu
    pub fn agent_as(&mut self, handle: &str, seat: &SeatRef) {
        lock(seat).last_seen = Instant::now();
        self.broadcast_seats();
        let u = seat_user(handle);
        if self.agent.user().id != u.id {
            self.agent.set_user(u);
            if self.ctrl_is_agent() {
                let a = self.agent.clone();
                self.set_ctrl(Some(&a));
            }
        }
    }
}

impl Hub {
    /// registerSeat: fresh (an MCP initialize) always gives a new seat with its own random id;
    /// otherwise it reuses the latest seat with that client name (a client that never initialized).
    pub fn register_seat(&self, name: &str, fresh: bool) -> SeatRef {
        let base = seat_base(name);
        let mut st = self.lock();
        let mut found: Option<SeatRef> = None;
        if !fresh {
            for o in st.seats.values() {
                let (ob, seen) = {
                    let g = lock(o);
                    (g.base.clone(), g.last_seen)
                };
                if ob == base && found.as_ref().is_none_or(|f| seen > lock(f).last_seen) {
                    found = Some(o.clone());
                }
            }
        }
        let s = found.unwrap_or_else(|| {
            let mut handle = new_handle(&base);
            while st.seats.contains_key(&handle) {
                handle = new_handle(&base);
            }
            let s = Arc::new(Mutex::new(Seat {
                handle: handle.clone(),
                base: base.clone(),
                chat_read: st.chat_seq,
                last_seen: Instant::now(),
                last_read: None,
                listening: 0,
                gone: String::new(),
            }));
            st.seats.insert(handle, s.clone());
            s
        });
        lock(&s).last_seen = Instant::now();
        st.broadcast_seats();
        s
    }

    /// removeSeat takes an agent out of the room: it ended its MCP session (by "", silent), called
    /// browser_disconnect (by "self"), or a person disconnected it (by = their name).
    pub fn remove_seat(&self, seat: &SeatRef, by: &str) {
        let mut st = self.lock();
        let handle = lock(seat).handle.clone();
        st.drop_seat(self, &handle, seat, by);
    }

    /// seatGone says whether the seat may use the browser. An agent that left by itself comes back on its
    /// next call (same @handle); one a person disconnected is refused until it connects again (new session).
    pub fn seat_gone(&self, seat: &SeatRef) -> Result<(), String> {
        let mut st = self.lock();
        let (gone, handle) = {
            let g = lock(seat);
            (g.gone.clone(), g.handle.clone())
        };
        match gone.as_str() {
            "" => Ok(()),
            "self" => {
                st.rejoin_seat(seat);
                Ok(())
            }
            by => Err(format!("{by} disconnected you (@{handle}) from the shared browser. Stop using the browser tools; to use it again, the user has to reconnect you (restart this MCP server connection)")),
        }
    }

    /// seatCount is how many coding agents are connected.
    pub fn seat_count(&self) -> usize {
        self.lock().seats.len()
    }

    /// unreadFor counts messages addressed to the seat that it has not read.
    pub fn unread_for(&self, seat: &SeatRef) -> usize {
        let st = self.lock();
        let s = lock(seat);
        st.chat
            .iter()
            .filter(|m| m.id > s.chat_read && m.is_for(&s))
            .count()
    }

    /// watchSeats drops seats that have not been heard from for seatTTL from everyone's room list.
    pub async fn watch_seats(self: Arc<Self>) {
        let mut t = tokio::time::interval(Duration::from_secs(10));
        t.tick().await; // time.Tick first fires after 10s
        loop {
            t.tick().await;
            self.lock().broadcast_seats();
        }
    }
}

impl HubState {
    /// rejoinSeat puts an agent that left by itself back in the room, with the same @handle if still free.
    /// caller holds h.mu
    pub fn rejoin_seat(&mut self, seat: &SeatRef) {
        let mut s = lock(seat);
        while self.seats.contains_key(&s.handle) {
            s.handle = new_handle(&s.base);
        }
        s.gone = String::new();
        s.chat_read = self.chat_seq;
        s.last_seen = Instant::now();
        self.seats.insert(s.handle.clone(), seat.clone());
        drop(s);
        self.broadcast_seats();
    }
}

/// Local time of a Unix time in seconds as (year, month, day, hour, minute, second): 's
/// `time.Unix(..).Format(..)` prints in the local zone.
pub fn local_tm(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    #[cfg(unix)]
    {
        // SAFETY: localtime_r only writes into the zeroed tm we pass.
        unsafe {
            let t = secs as libc::time_t;
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&t, &mut tm);
            (
                tm.tm_year + 1900,
                (tm.tm_mon + 1) as u32,
                tm.tm_mday as u32,
                tm.tm_hour as u32,
                tm.tm_min as u32,
                tm.tm_sec as u32,
            )
        }
    }
    #[cfg(not(unix))]
    {
        // UTC fallback (prints local time)
        let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
        let z = days + 719468;
        let era = z.div_euclid(146097);
        let doe = z.rem_euclid(146097);
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        (
            y as i32,
            m as u32,
            d as u32,
            (rem / 3600) as u32,
            (rem % 3600 / 60) as u32,
            (rem % 60) as u32,
        )
    }
}

impl Hub {
    /// ReadChat returns new messages for the seat, waiting up to `wait` seconds if there are none.
    ///
    /// The wait is async and holds no lock: the hub lock is released before awaiting the wake-up
    /// (`chat_wake`, subscribed to *under* the lock so a message that arrives in between is not missed).
    pub async fn read_chat(&self, seat: &SeatRef, wait: f64, all: bool) -> String {
        let deadline = Instant::now() + Duration::from_secs_f64(wait.clamp(0.0, 60.0));
        loop {
            let mut wake = {
                let mut st = self.lock();
                {
                    let mut s = lock(seat);
                    if !s.gone.is_empty() {
                        return "You are disconnected from the browser: no more chat messages for you."
                            .into();
                    }
                    s.last_seen = Instant::now();
                    let out: Vec<ChatMsg> = st
                        .chat
                        .iter()
                        .filter(|m| {
                            !m.user.is_empty()
                                && !m.agent
                                && (all || (m.id > s.chat_read && m.is_for(&s)))
                        })
                        .cloned()
                        .collect();
                    if !out.is_empty() || Instant::now() >= deadline {
                        s.chat_read = s.chat_read.max(st.chat_seq);
                        s.last_read = Some(Instant::now());
                        let handle = s.handle.clone();
                        let out_s = format_chat(&out, all, &s);
                        drop(s);
                        if !out.is_empty() && !all {
                            st.add_system_chat(&format!("👀 @{handle} read your message"));
                        }
                        return out_s;
                    }
                    s.listening += 1;
                    s.last_read = Some(Instant::now());
                }
                st.chat_wake.subscribe()
            };
            tokio::select! {
                _ = wake.changed() => {}
                _ = tokio::time::sleep_until(deadline.into()) => {}
            }
            let _st = self.lock();
            lock(seat).listening -= 1;
        }
    }
}

/// formatChat
pub fn format_chat(msgs: &[ChatMsg], all: bool, s: &Seat) -> String {
    if msgs.is_empty() {
        if all {
            return "The chat is empty.".into();
        }
        return format!("No new messages for you. (People address you as @{}, or @agent for every agent; use time to wait for one.)", s.handle);
    }
    let mut b = String::from("### Chat messages\n");
    for m in msgs {
        let to = if m.is_for(s) { " → you" } else { "" };
        let (_, _, _, hh, mm, ss) = local_tm(m.time.div_euclid(1000));
        b.push_str(&format!(
            "- [{hh:02}:{mm:02}:{ss:02}] {}{to}: {}\n",
            m.user, m.text
        ));
    }
    b.push_str("\nReply with browser_say. For an element code like el-3f9a, call browser_get_element; for a flow id like flow-3fa9c1, call browser_get_flow.\n");
    b
}
