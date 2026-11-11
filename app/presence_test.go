package main

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/gorilla/websocket"
)

// Cursor moves and sound fill a viewer's outgoing queue when its connection is slow.
// Presence messages (someone joined or left) must still reach that viewer, otherwise
// its member list silently goes out of date.
func TestPresenceSurvivesCursorFlood(t *testing.T) {
	h := &Hub{clients: map[*Client]bool{}}
	mover := &Client{user: User{ID: "mover", Name: "mover"}, send: make(chan outMsg, 8), frame: make(chan []byte, 1)}
	slow := &Client{user: User{ID: "slow", Name: "slow"}, send: make(chan outMsg, 8), frame: make(chan []byte, 1)}
	h.clients[mover] = true
	h.clients[slow] = true

	h.mu.Lock()
	for i := 0; i < 100; i++ {
		h.broadcast(Msg{Type: "cursor", ID: "mover", X: float64(i)}, mover)
	}
	h.broadcast(Msg{Type: "user_join", User: &User{ID: "new", Name: "new"}}, nil)
	h.mu.Unlock()

	joined := false
	for len(slow.send) > 0 {
		if strings.Contains(string((<-slow.send).data), `"user_join"`) {
			joined = true
		}
	}
	if !joined {
		t.Fatal("user_join was dropped for a viewer whose queue was full of cursor moves")
	}
}

// Over a real WebSocket: a viewer sees others join and leave, and gets a snapshot when it connects.
func TestPresenceOverWebSocket(t *testing.T) {
	h := &Hub{clients: map[*Client]bool{}, chat: []ChatMsg{}, agent: &Client{}}
	srv := httptest.NewServer(http.HandlerFunc(h.serveWS))
	defer srv.Close()
	base := "ws" + strings.TrimPrefix(srv.URL, "http") + "/?name="

	alice := dialViewer(t, base+"alice")
	defer alice.Close()
	waitType(t, alice, "init")

	bob := dialViewer(t, base+"bob")
	waitType(t, alice, "user_join")
	bob.Close()
	waitType(t, alice, "user_leave")
}

func dialViewer(t *testing.T, url string) *websocket.Conn {
	t.Helper()
	c, _, err := websocket.DefaultDialer.Dial(url, nil)
	if err != nil {
		t.Fatal(err)
	}
	return c
}

// waitType reads text messages until one has the wanted type.
func waitType(t *testing.T, c *websocket.Conn, want string) {
	t.Helper()
	c.SetReadDeadline(time.Now().Add(3 * time.Second))
	for {
		_, raw, err := c.ReadMessage()
		if err != nil {
			t.Fatalf("waiting for %q: %v", want, err)
		}
		var m Msg
		if json.Unmarshal(raw, &m) == nil && m.Type == want {
			return
		}
	}
}
