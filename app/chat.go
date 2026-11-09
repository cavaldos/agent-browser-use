package main

// Chat with coding agents. Every agent that connects over MCP (OpenCode, Pi, Claude Code…) is a
// "seat" named after its clientInfo. People write "@opencode …" (or "@agent" for all of them) in the
// viewer's chat; the agent reads its messages with browser_read_chat and answers with browser_say.

import (
	"fmt"
	"regexp"
	"slices"
	"strings"
	"time"
)

const seatTTL = 30 * time.Minute // a seat that has not been heard from for this long leaves the list

// Seat is one connected coding agent.
type Seat struct {
	Handle    string // what people type after @
	chatRead  int64  // last chat message id it has read
	lastSeen  time.Time
	lastRead  time.Time
	listening int // browser_read_chat calls waiting right now
}

var handleBad = regexp.MustCompile(`[^a-z0-9_-]+`)
var mentionRe = regexp.MustCompile(`(?i)(?:^|\s)@([a-z0-9_-]+)`)

// registerSeat adds (or finds) the seat for a client name.
func (h *Hub) registerSeat(name string) *Seat {
	handle := strings.Trim(handleBad.ReplaceAllString(strings.ToLower(strings.TrimSpace(name)), "-"), "-")
	if handle == "" || handle == "agent" || handle == "ai" {
		handle = "coder"
	}
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.seats == nil {
		h.seats = map[string]*Seat{}
	}
	s := h.seats[handle]
	if s == nil {
		s = &Seat{Handle: handle, chatRead: h.chatSeq}
		h.seats[handle] = s
	}
	s.lastSeen = time.Now()
	h.broadcastSeats()
	return s
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
	h.broadcast(Msg{Type: "agents", Agents: &n}, nil)
}

// routeChat finds who a chat message is for: "@agent" is every seat, "@opencode" just that one.
// caller holds h.mu
func (h *Hub) routeChat(cm *ChatMsg) {
	for _, m := range mentionRe.FindAllStringSubmatch(cm.Text, -1) {
		w := strings.ToLower(m[1])
		switch {
		case w == "agent" || w == "ai":
			cm.ToBot = true
		case h.seats[w] != nil && !slices.Contains(cm.To, w):
			cm.To = append(cm.To, w)
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
