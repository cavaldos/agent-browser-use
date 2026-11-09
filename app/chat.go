package main

// Chat with coding agents. Every agent that connects over MCP (OpenCode, Pi, Claude Code…) is a
// "seat" named after its clientInfo. People write "@opencode …" (or "@agent" for all of them) in the
// viewer's chat; the agent reads its messages with browser_read_chat and answers with browser_say.

import (
	"fmt"
	"math/rand"
	"regexp"
	"slices"
	"strings"
	"time"
)

const seatTTL = 30 * time.Minute // a seat that has not been heard from for this long leaves the list

// Seat is one connected coding agent (one MCP session). Two OpenCodes are two seats.
type Seat struct {
	Handle    string // what people type after @: client name + "#" + a random id, e.g. opencode#3f9a
	Base      string // the client name, e.g. opencode ("@opencode" reaches every OpenCode)
	chatRead  int64  // last chat message id it has read
	lastSeen  time.Time
	lastRead  time.Time
	listening int    // browser_read_chat calls waiting right now
	gone      string // why it left the room: "" still here, "self" (browser_disconnect / session ended), else the person who disconnected it
}

var handleBad = regexp.MustCompile(`[^a-z0-9_-]+`)
var mentionRe = regexp.MustCompile(`(?i)(?:^|\s)@([a-z0-9_-]+(?:#[a-z0-9]+)?)`)

func seatBase(name string) string {
	b := strings.Trim(handleBad.ReplaceAllString(strings.ToLower(strings.TrimSpace(name)), "-"), "-")
	if b == "" || b == "agent" || b == "ai" {
		b = "coder"
	}
	return b
}

// registerSeat: fresh (an MCP initialize) always gives a new seat with its own random id;
// otherwise it reuses the latest seat with that client name (a client that never initialized).
func (h *Hub) registerSeat(name string, fresh bool) *Seat {
	base := seatBase(name)
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.seats == nil {
		h.seats = map[string]*Seat{}
	}
	var s *Seat
	if !fresh {
		for _, o := range h.seats {
			if o.Base == base && (s == nil || o.lastSeen.After(s.lastSeen)) {
				s = o
			}
		}
	}
	if s == nil {
		handle := ""
		for handle == "" || h.seats[handle] != nil {
			handle = fmt.Sprintf("%s#%04x", base, rand.Intn(0x10000))
		}
		s = &Seat{Handle: handle, Base: base, chatRead: h.chatSeq}
		h.seats[handle] = s
	}
	s.lastSeen = time.Now()
	h.broadcastSeats()
	return s
}

// removeSeat takes an agent out of the room: it ended its MCP session (by "", silent), called
// browser_disconnect (by "self"), or a person disconnected it (by = their name).
func (h *Hub) removeSeat(s *Seat, by string) {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.dropSeat(s, by)
}

// caller holds h.mu
func (h *Hub) dropSeat(s *Seat, by string) {
	if h.seats[s.Handle] != s {
		return
	}
	delete(h.seats, s.Handle)
	s.gone = cmpOr(by, "self")
	h.broadcastSeats()
	if h.ctrl == h.agent && h.agent.user.ID == seatUser(s.Handle).ID {
		h.agentRelease() // give control back to whoever had it
	}
	if h.browser != nil {
		h.browser.releaseOwner(s.Handle) // its tab locks (and its sub-agents')
	}
	h.wakeAgents() // a browser_read_chat in progress returns now
	switch by {
	case "":
	case "self":
		h.addSystemChat("👋 @" + s.Handle + " disconnected from the browser")
	default:
		h.addSystemChat("🔌 " + by + " disconnected @" + s.Handle)
	}
}

// kickSeat: a person in the room disconnects a coding agent by its @handle.
// caller holds h.mu
func (h *Hub) kickSeat(handle, by string) {
	if s := h.seats[strings.TrimPrefix(handle, "@")]; s != nil {
		h.dropSeat(s, by)
	}
}

// seatGone says whether the seat may use the browser. An agent that left by itself comes back on its
// next call (same @handle); one a person disconnected is refused until it connects again (new session).
func (h *Hub) seatGone(s *Seat) error {
	h.mu.Lock()
	defer h.mu.Unlock()
	switch s.gone {
	case "":
		return nil
	case "self":
		h.rejoinSeat(s)
		return nil
	}
	return fmt.Errorf("%s disconnected you (@%s) from the shared browser. Stop using the browser tools; to use it again, the user has to reconnect you (restart this MCP server connection)", s.gone, s.Handle)
}

// rejoinSeat puts an agent that left by itself back in the room, with the same @handle if still free.
// caller holds h.mu
func (h *Hub) rejoinSeat(s *Seat) {
	for h.seats[s.Handle] != nil {
		s.Handle = fmt.Sprintf("%s#%04x", s.Base, rand.Intn(0x10000))
	}
	s.gone = ""
	s.chatRead = h.chatSeq
	s.lastSeen = time.Now()
	h.seats[s.Handle] = s
	h.broadcastSeats()
}

// caller holds h.mu
func (h *Hub) seatNames() []string {
	names := []string{}
	for k, s := range h.seats {
		if time.Since(s.lastSeen) < seatTTL || s.listening > 0 {
			names = append(names, k)
		}
	}
	slices.Sort(names)
	return names
}

// caller holds h.mu
func (h *Hub) broadcastSeats() {
	n := h.seatNames()
	if slices.Equal(n, h.seatsSent) {
		return
	}
	h.seatsSent = n
	h.broadcast(Msg{Type: "agents", Agents: &n}, nil)
}

// watchSeats drops seats that have not been heard from for seatTTL from everyone's room list.
func (h *Hub) watchSeats() {
	for range time.Tick(10 * time.Second) {
		h.mu.Lock()
		h.broadcastSeats()
		h.mu.Unlock()
	}
}

// seatUser is how a coding agent shows in the room (the web UI builds the same from the "agents" list).
func seatUser(handle string) User {
	return User{ID: "agent:" + handle, Name: handle, Color: agentUser.Color, Agent: true}
}

// agentAs makes the shared agent (the one holding control) show as this seat.
func (h *Hub) agentAs(s *Seat) {
	h.mu.Lock()
	defer h.mu.Unlock()
	s.lastSeen = time.Now()
	h.broadcastSeats()
	if u := seatUser(s.Handle); h.agent.user.ID != u.ID {
		h.agent.user = u
		if h.ctrl == h.agent {
			h.setCtrl(h.agent)
		}
	}
}

// routeChat finds who a chat message is for: "@agent" is every seat, "@opencode" just that one.
// caller holds h.mu
func (h *Hub) routeChat(cm *ChatMsg) {
	for _, m := range mentionRe.FindAllStringSubmatch(cm.Text, -1) {
		w := strings.ToLower(m[1])
		switch {
		case w == "agent" || w == "ai":
			cm.ToBot = true
		case h.seats[w] != nil:
			if !slices.Contains(cm.To, w) {
				cm.To = append(cm.To, w)
			}
		default: // "@opencode": every OpenCode
			for k, s := range h.seats {
				if s.Base == w && !slices.Contains(cm.To, k) {
					cm.To = append(cm.To, k)
				}
			}
		}
	}
}

func (cm ChatMsg) isFor(s *Seat) bool { return cm.ToBot || slices.Contains(cm.To, s.Handle) }

// unreadFor counts messages addressed to the seat that it has not read.
func (h *Hub) unreadFor(s *Seat) int {
	h.mu.Lock()
	defer h.mu.Unlock()
	n := 0
	for _, m := range h.chat {
		if m.ID > s.chatRead && m.isFor(s) {
			n++
		}
	}
	return n
}

// noticeUnheard tells the room when a message is for agents that are not listening. caller holds h.mu
func (h *Hub) noticeUnheard(cm ChatMsg) {
	var cold []string
	if cm.ToBot && len(h.seats) == 0 {
		h.addSystemChat("⚠️ No coding agent is connected to this browser yet")
		return
	}
	for k, s := range h.seats {
		if cm.isFor(s) && s.listening == 0 && time.Since(s.lastRead) > 2*time.Minute {
			cold = append(cold, "@"+k)
		}
	}
	if len(cold) > 0 {
		slices.Sort(cold)
		h.addSystemChat("⚠️ " + strings.Join(cold, ", ") + " isn’t listening yet. Ask it to call browser_read_chat (time: 60) in a loop")
	}
}

// wakeAgents releases agents waiting in browser_read_chat. caller holds h.mu
func (h *Hub) wakeAgents() {
	if h.chatWake != nil {
		close(h.chatWake)
		h.chatWake = nil
	}
}

// ReadChat returns new messages for the seat, waiting up to `wait` seconds if there are none.
func (h *Hub) ReadChat(s *Seat, wait float64, all bool) string {
	deadline := time.Now().Add(time.Duration(min(max(wait, 0), 60) * float64(time.Second)))
	for {
		h.mu.Lock()
		if s.gone != "" {
			h.mu.Unlock()
			return "You are disconnected from the browser: no more chat messages for you."
		}
		s.lastSeen = time.Now()
		var out []ChatMsg
		for _, m := range h.chat {
			if m.User == "" || m.Agent {
				continue
			}
			if all || (m.ID > s.chatRead && m.isFor(s)) {
				out = append(out, m)
			}
		}
		if len(out) > 0 || time.Now().After(deadline) {
			s.chatRead = max(s.chatRead, h.chatSeq)
			s.lastRead = time.Now()
			if len(out) > 0 && !all {
				h.addSystemChat("👀 @" + s.Handle + " read your message")
			}
			h.mu.Unlock()
			return formatChat(out, all, s)
		}
		if h.chatWake == nil {
			h.chatWake = make(chan struct{})
		}
		wake := h.chatWake
		s.listening++
		s.lastRead = time.Now()
		h.mu.Unlock()
		select {
		case <-wake:
		case <-time.After(time.Until(deadline)):
		}
		h.mu.Lock()
		s.listening--
		h.mu.Unlock()
	}
}

func formatChat(msgs []ChatMsg, all bool, s *Seat) string {
	if len(msgs) == 0 {
		if all {
			return "The chat is empty."
		}
		return fmt.Sprintf("No new messages for you. (People address you as @%s, or @agent for every agent; use time to wait for one.)", s.Handle)
	}
	var b strings.Builder
	b.WriteString("### Chat messages\n")
	for _, m := range msgs {
		to := ""
		if m.isFor(s) {
			to = " → you"
		}
		fmt.Fprintf(&b, "- [%s] %s%s: %s\n", time.UnixMilli(m.Time).Format("15:04:05"), m.User, to, m.Text)
	}
	b.WriteString("\nReply with browser_say. For an element code like el-3f9a, call browser_get_element.\n")
	return b.String()
}

// seatCount is how many coding agents are connected.
func (h *Hub) seatCount() int {
	h.mu.Lock()
	defer h.mu.Unlock()
	return len(h.seats)
}
