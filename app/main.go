package main

import (
	"embed"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"io/fs"
	"log"
	"math/rand"
	"net"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/gorilla/websocket"
)

//go:embed static
var staticFiles embed.FS

// version is set at build time by scripts/build.sh (-ldflags "-X main.version=vX.Y.Z").
var version = "dev"

// ---------- Data ----------

type User struct {
	ID    string `json:"id"`
	Name  string `json:"name"`
	Color string `json:"color"`
	Agent bool   `json:"agent,omitempty"` // the AI agent (MCP), not a person
}

type ChatMsg struct {
	User  string   `json:"user"`
	Color string   `json:"color"`
	Text  string   `json:"text"`
	Time  int64    `json:"time"`
	Agent bool     `json:"agent,omitempty"`
	ID    int64    `json:"id,omitempty"`      // order in the room; coding agents read from here
	ToBot bool     `json:"toAgent,omitempty"` // addressed to every agent with @agent
	To    []string `json:"to,omitempty"`      // addressed to these agents, e.g. "opencode"
}

// JSON message between the web page and the server.
type Msg struct {
	Type   string    `json:"type"`
	ID     string    `json:"id,omitempty"`
	Text   string    `json:"text,omitempty"`
	X      float64   `json:"x,omitempty"`
	Y      float64   `json:"y,omitempty"`
	On     bool      `json:"on,omitempty"`
	You    *User     `json:"you,omitempty"`
	User   *User     `json:"user,omitempty"`
	Users  []User    `json:"users,omitempty"`
	Chat   []ChatMsg `json:"chat,omitempty"`
	Agents *[]string `json:"agents,omitempty"` // handles of the connected coding agents
	Msg    *ChatMsg  `json:"msg,omitempty"`

	// Shared browser
	Enabled bool      `json:"enabled,omitempty"`
	Ctrl    *User     `json:"ctrl,omitempty"`    // who is in control
	Blocked *User     `json:"blocked,omitempty"` // who blocked the agent from taking control (nil = not blocked)
	Tabs    []TabInfo `json:"tabs,omitempty"`
	Active  string    `json:"active,omitempty"`
	View    *View     `json:"view,omitempty"`
	Index   int       `json:"index,omitempty"`
	URL     string    `json:"url,omitempty"`
	Area    string    `json:"area,omitempty"`
	Event   string    `json:"event,omitempty"`
	Button  int       `json:"button,omitempty"`
	Buttons int       `json:"buttons,omitempty"`
	Count   int       `json:"count,omitempty"`
	Mods    int       `json:"mods,omitempty"`
	Key     string    `json:"key,omitempty"`
	Code    string    `json:"code,omitempty"`
	KeyCode int       `json:"keyCode,omitempty"`
	DX      float64   `json:"dx,omitempty"`
	DY      float64   `json:"dy,omitempty"`

	// Picked page element (type "pick", Event hover|click)
	Pick *Pick `json:"pick,omitempty"`

	// What the AI agent is doing (type "agent")
	Act      string `json:"act,omitempty"` // step, click, key, say
	Rect     *Rect  `json:"rect,omitempty"`
	TabID    string `json:"tabId,omitempty"`
	WorkerID string `json:"workerId,omitempty"`
}

type outMsg struct {
	bin  bool
	data []byte
}

type Client struct {
	user  User
	conn  *websocket.Conn
	send  chan outMsg // JSON messages and audio, in order
	frame chan []byte // only the newest frame; slow connections skip old ones

	// guarded by Hub.mu
	audio     bool // wants audio
	paused    bool // stream paused: no frames, no audio
	lastEmote time.Time
}

type Hub struct {
	mu       sync.Mutex
	clients  map[*Client]bool
	chat     []ChatMsg
	dataFile string
	browser  *Browser
	ctrl     *Client // who controls the shared browser

	agent     *Client // the AI agent: has no connection, shown in the room once it acts
	agentOn   bool
	agentLast time.Time
	agentBusy bool    // a tool call is running (it may be waiting for the page): not idle
	agentFrom *Client // the person the agent took control from; gets it back when the agent is done
	blocked   *User   // set while someone has blocked the agent from taking control
	chatSeq   int64
	seats     map[string]*Seat // connected coding agents, by @handle
	seatsSent []string         // the agent list everyone has
	chatWake  chan struct{}    // closed when a message for the agent arrives
	picks     map[string]*Pick // elements people picked in the viewer, by code
	pickOrder []string
	humanLast time.Time // last time the person in control sent input (the agent waits while they are active)
}

// Bright colors that read well on a dark background.
var colors = []string{"#f87171", "#fb923c", "#facc15", "#4ade80", "#2dd4bf", "#38bdf8", "#818cf8", "#c084fc", "#f472b6", "#a3e635"}

var emotes = []string{"😍", "😴", "🤣", "🎉", "😱", "😄", "👍", "❤️"}

func newID() string { return fmt.Sprintf("%x", rand.Int63()) }

// ---------- Persistence (chat history) ----------

type saved struct {
	Chat []ChatMsg `json:"chat"`
}

// Chat history is only kept in a file when -data is given; otherwise it lives in memory.
func (h *Hub) load() {
	if h.dataFile == "" {
		return
	}
	b, err := os.ReadFile(h.dataFile)
	if err != nil {
		return
	}
	var s saved
	if json.Unmarshal(b, &s) == nil && s.Chat != nil {
		h.chat = s.Chat
		for _, m := range h.chat {
			h.chatSeq = max(h.chatSeq, m.ID)
		}
	}
}

// caller holds h.mu
func (h *Hub) save() {
	if h.dataFile == "" {
		return
	}
	b, _ := json.MarshalIndent(saved{h.chat}, "", "  ")
	if err := os.WriteFile(h.dataFile, b, 0644); err != nil {
		log.Println("save failed:", err)
	}
}

// ---------- Sending ----------

func trySend(c *Client, m outMsg) {
	select {
	case c.send <- m:
	default: // client too slow, drop
	}
}

// caller holds h.mu
func (h *Hub) broadcast(m Msg, except *Client) {
	b, _ := json.Marshal(m)
	for c := range h.clients {
		if c != except {
			trySend(c, outMsg{data: b})
		}
	}
}

func (h *Hub) sendTo(c *Client, m Msg) {
	h.mu.Lock()
	defer h.mu.Unlock()
	if !h.clients[c] { // already left
		return
	}
	b, _ := json.Marshal(m)
	trySend(c, outMsg{data: b})
}

func (h *Hub) systemChat(text string) {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.addSystemChat(text)
}

func (h *Hub) broadcastTabs(tabs []TabInfo, active string) {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.broadcast(Msg{Type: "tabs", Tabs: tabs, Active: active}, nil)
}

func (h *Hub) broadcastView(v View) {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.broadcast(Msg{Type: "view", View: &v}, nil)
}

func (h *Hub) broadcastFrame(pkt []byte) {
	h.mu.Lock()
	defer h.mu.Unlock()
	for c := range h.clients {
		if !c.paused {
			pushFrame(c, pkt)
		}
	}
}

func (h *Hub) broadcastAudio(pkt []byte) {
	h.mu.Lock()
	defer h.mu.Unlock()
	for c := range h.clients {
		if c.audio && !c.paused {
			trySend(c, outMsg{bin: true, data: pkt})
		}
	}
}

func pushFrame(c *Client, pkt []byte) {
	select { // drop the old frame that hasn't gone out yet
	case <-c.frame:
	default:
	}
	select {
	case c.frame <- pkt:
	default:
	}
}

func (h *Hub) usersLocked() []User {
	us := []User{}
	for c := range h.clients {
		us = append(us, c.user)
	}
	return us // coding agents come separately, in "agents"
}

// ---------- Handling client messages ----------

func (h *Hub) handle(c *Client, m Msg) {
	h.mu.Lock()
	defer h.mu.Unlock()

	switch m.Type {
	case "cursor":
		h.broadcast(Msg{Type: "cursor", ID: c.user.ID, X: m.X, Y: m.Y, Area: m.Area}, c)

	case "audio":
		c.audio = m.On

	case "pause":
		c.paused = m.On
		if !c.paused && h.browser != nil {
			if _, _, f, _ := h.browser.State(); f != nil {
				pushFrame(c, f)
			}
		}

	case "emote":
		if !slices.Contains(emotes, m.Text) || time.Since(c.lastEmote) < 250*time.Millisecond {
			return
		}
		c.lastEmote = time.Now()
		h.broadcast(Msg{Type: "emote", Text: m.Text, User: &c.user}, nil)

	case "chat":
		if m.Text == "" {
			return
		}
		if r := []rune(m.Text); len(r) > 1000 {
			m.Text = string(r[:1000])
		}
		cm := ChatMsg{User: c.user.Name, Color: c.user.Color, Text: m.Text, Time: time.Now().UnixMilli()}
		h.routeChat(&cm)
		h.pushChat(cm)
		if cm.ToBot || len(cm.To) > 0 {
			h.wakeAgents()
		}
		h.broadcast(Msg{Type: "chat", Msg: &cm}, nil)
		if cm.ToBot || len(cm.To) > 0 {
			h.noticeUnheard(cm)
		}
		h.save()

	case "pick":
		if h.browser != nil && (m.Event == "hover" || m.Event == "click") {
			go h.pick(c, m)
		}

	case "control_take":
		h.humanLast = time.Now()
		if h.ctrl != c {
			h.agentFrom = nil
			h.setCtrl(c)
		}

	case "agent_kick": // a person disconnects one coding agent (m.ID = its @handle)
		h.kickSeat(m.ID, c.user.Name)

	case "agent_block": // while blocked, the agent can't take control (and gives it back now if it has it)
		if m.On && h.blocked == nil {
			u := c.user
			h.blocked = &u
			h.broadcast(Msg{Type: "block", Blocked: h.blocked}, nil)
			h.addSystemChat("🛑 " + c.user.Name + " blocked the agent")
			if h.ctrl == h.agent {
				h.agentRelease()
			}
		} else if !m.On && h.blocked != nil {
			h.blocked = nil
			h.broadcast(Msg{Type: "block"}, nil)
			h.addSystemChat("✅ " + c.user.Name + " let the agent take control again")
		}

	case "control_release":
		if h.ctrl == c {
			h.setCtrl(nil)
		}

	case "nav", "back", "forward", "reload", "stop", "mouse", "key", "text",
		"tab_new", "tab_close", "tab_activate", "tab_move", "tab_mute", "view":
		if h.browser == nil {
			return
		}
		if h.ctrl == nil && m.Type != "view" && !(m.Type == "mouse" && m.Event == "move") { // nobody drives: whoever acts first takes control
			h.agentFrom = nil
			h.setCtrl(c)
		}
		if h.ctrl != c { // only whoever is in control can drive
			return
		}
		h.humanLast = time.Now()
		b := h.browser
		switch m.Type {
		case "nav":
			b.Navigate(m.URL)
		case "back":
			b.History(-1)
		case "forward":
			b.History(1)
		case "reload":
			b.Reload()
		case "stop":
			b.Stop()
		case "mouse":
			b.Mouse(m)
		case "key":
			b.Key(m, func(s string) { h.sendTo(c, Msg{Type: "clipboard", Text: s}) })
		case "text":
			b.Text(m.Text)
		case "tab_new":
			b.NewTab(m.URL)
		case "tab_close":
			b.CloseTab(m.ID)
		case "tab_activate":
			b.ActivateTab(m.ID)
		case "tab_move":
			b.MoveTab(m.ID, m.Index)
		case "tab_mute":
			b.MuteTab(m.ID)
		case "view":
			if m.View != nil {
				b.SetView(*m.View)
			}
		}
	}
}

func (h *Hub) pushChat(cm ChatMsg) {
	h.chatSeq++
	cm.ID = h.chatSeq
	h.chat = append(h.chat, cm)
	if len(h.chat) > 200 {
		h.chat = h.chat[len(h.chat)-200:]
	}
}

// caller holds h.mu
func (h *Hub) addSystemChat(text string) {
	cm := ChatMsg{Text: text, Time: time.Now().UnixMilli()}
	h.pushChat(cm)
	h.broadcast(Msg{Type: "chat", Msg: &cm}, nil)
}

// caller holds h.mu
func (h *Hub) setCtrl(c *Client) {
	h.ctrl = c
	var u *User
	if c != nil {
		u = &c.user
	}
	h.broadcast(Msg{Type: "control", Ctrl: u}, nil)
}

// ---------- WebSocket ----------

var upgrader = websocket.Upgrader{CheckOrigin: func(r *http.Request) bool { return true }}

func (h *Hub) serveWS(w http.ResponseWriter, r *http.Request) {
	name := r.URL.Query().Get("name")
	if name == "" {
		http.Error(w, "name required", http.StatusBadRequest)
		return
	}
	if len([]rune(name)) > 30 {
		name = string([]rune(name)[:30])
	}
	conn, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		return
	}

	c := &Client{
		user:  User{ID: newID(), Name: name, Color: colors[rand.Intn(len(colors))]},
		conn:  conn,
		send:  make(chan outMsg, 512),
		frame: make(chan []byte, 1),
	}

	// Lock order is always h.mu before Browser.mu; the browser never calls the hub while holding its lock.
	init := Msg{Type: "init", You: &c.user}
	var frame []byte
	if h.browser != nil {
		tabs, active, f, v := h.browser.State()
		init.Enabled, init.Tabs, init.Active, init.View, frame = true, tabs, active, &v, f
	}

	h.mu.Lock()
	h.clients[c] = true
	init.Users, init.Chat = h.usersLocked(), h.chat
	seats := h.seatNames()
	init.Agents = &seats
	if h.ctrl != nil {
		init.Ctrl = &h.ctrl.user
	}
	init.Blocked = h.blocked
	initMsg, _ := json.Marshal(init)
	c.send <- outMsg{data: initMsg}
	if frame != nil {
		pushFrame(c, frame)
	}
	h.broadcast(Msg{Type: "user_join", User: &c.user}, c)
	h.mu.Unlock()
	log.Printf("+ %s joined", name)

	go c.writeLoop()
	c.readLoop(h)

	h.mu.Lock()
	delete(h.clients, c)
	if h.ctrl == c {
		h.setCtrl(nil)
	}
	if h.agentFrom == c {
		h.agentFrom = nil
	}
	h.broadcast(Msg{Type: "user_leave", ID: c.user.ID}, nil)
	h.mu.Unlock()
	close(c.send)
	log.Printf("- %s left", name)
}

func (c *Client) readLoop(h *Hub) {
	defer c.conn.Close()
	c.conn.SetReadLimit(64 * 1024)
	c.conn.SetReadDeadline(time.Now().Add(60 * time.Second))
	c.conn.SetPongHandler(func(string) error {
		c.conn.SetReadDeadline(time.Now().Add(60 * time.Second))
		return nil
	})
	for {
		var m Msg
		if err := c.conn.ReadJSON(&m); err != nil {
			return
		}
		h.handle(c, m)
	}
}

func (c *Client) writeLoop() {
	ping := time.NewTicker(30 * time.Second)
	defer ping.Stop()
	write := func(typ int, b []byte) bool {
		c.conn.SetWriteDeadline(time.Now().Add(10 * time.Second))
		if err := c.conn.WriteMessage(typ, b); err != nil {
			c.conn.Close()
			return false
		}
		return true
	}
	for {
		select {
		case m, ok := <-c.send:
			if !ok {
				return
			}
			typ := websocket.TextMessage
			if m.bin {
				typ = websocket.BinaryMessage
			}
			if !write(typ, m.data) {
				return
			}
		case f := <-c.frame:
			if !write(websocket.BinaryMessage, f) {
				return
			}
		case <-ping.C:
			if !write(websocket.PingMessage, nil) {
				return
			}
		}
	}
}

// ---------- main ----------

func main() {
	// `agent-browser-use mcp …` speaks MCP on stdin/stdout; stdout then carries only MCP messages.
	stdio := len(os.Args) > 1 && os.Args[1] == "mcp"
	args := os.Args[1:]
	out := io.Writer(os.Stdout)
	if stdio {
		args, out = os.Args[2:], os.Stderr
	}

	fl := flag.NewFlagSet(appName, flag.ExitOnError)
	fl.Usage = func() {
		fmt.Fprintf(fl.Output(), "Usage:\n  %[1]s [flags]       web UI + MCP over HTTP at /mcp\n  %[1]s mcp [flags]   MCP over stdio (also serves the web UI)\n\nFlags:\n", appName)
		fl.PrintDefaults()
	}
	port := fl.Int("port", 3000, "HTTP port")
	fl.IntVar(port, "p", 3000, "HTTP port (short for -port)")
	fl.StringVar(&authPassword, "pw", "", "password needed to open the web UI and use /mcp (empty = no password)")
	data := fl.String("data", "", "file to keep the chat history in across restarts (empty = memory only)")
	noBrowser := fl.Bool("no-browser", false, "disable the shared browser")
	noAudio := fl.Bool("no-audio", false, "disable shared browser audio")
	var bo BrowserOptions
	fl.IntVar(&bo.Width, "width", 1280, "shared screen width")
	fl.IntVar(&bo.Height, "height", 720, "shared screen height")
	fl.IntVar(&bo.Quality, "quality", 70, "image quality 10-95 (lower = less bandwidth)")
	fl.IntVar(&bo.MaxTabs, "max-tabs", 12, "maximum number of open tabs")
	fl.StringVar(&bo.StartURL, "start", "https://www.google.com", "page for new tabs")
	fl.StringVar(&bo.Profile, "profile", "chrome-profile", "folder for the shared browser's cookies/logins")
	fl.StringVar(&bo.ChromePath, "chrome", "", "path to the browser executable (empty = auto-detect)")
	engine := fl.String("engine", "auto", "browser to run: chromium, chrome, edge, brave, vivaldi, headless-shell (lightest); auto = Chromium, else the first one installed")
	fl.StringVar(&bo.DownloadDir, "downloads", "", "folder for downloaded files (empty = \"downloads\" next to the profile folder)")
	fl.StringVar(&bo.UploadDir, "uploads", "", "folder the agent may upload files from (empty = \"uploads\" next to the profile folder)")
	fl.StringVar(&bo.StateDir, "states", "", "folder for saved logins/sessions (empty = \"states\" next to the profile folder)")
	var ao AgentOptions
	fl.Float64Var(&ao.Speed, "agent-speed", 0, "pace of the AI agent's cursor and typing, for people who want to follow it: 0 instant, 1 normal, 2 slower")
	fl.BoolVar(&ao.OpenViewer, "open", true, "open the web UI in your browser when the agent starts and nobody is watching")
	fl.DurationVar(&ao.Idle, "agent-idle", 15*time.Second, "give control back (to whoever had it before) after the agent has been idle this long (0 = never)")
	noMCP := fl.Bool("no-mcp", false, "disable the MCP endpoint /mcp")
	mcpRemote := fl.Bool("mcp-remote", false, "allow /mcp from other machines (default: this machine only)")
	tunnelOn := fl.Bool("tunnel", false, "share the web UI on a public https URL (built-in Cloudflare tunnel, no router setup); sets a password if you gave none")
	tunnelSub := fl.String("subdomain", "", "with -tunnel: name for the public URL (empty = random)")
	tunnelBackend := fl.String("tunnel-backend", "", "with -tunnel: tunnel backend URL (empty = "+defaultTunnelBackend+")")
	showVersion := fl.Bool("version", false, "print the version and exit")
	fl.Parse(args)
	if *showVersion {
		fmt.Fprintln(os.Stdout, appName, version)
		return
	}
	if fl.NArg() > 0 { // e.g. `make run -p 8080`: make takes -p itself and only "8080" arrives here
		fmt.Fprintf(os.Stderr, "Unexpected arguments: %s\nFlags start with -, e.g. ./run -p 8080 -pw secret (with make: make run p=8080 pw=secret)\n", strings.Join(fl.Args(), " "))
		os.Exit(2)
	}
	bo.Audio = !*noAudio
	log.SetOutput(os.Stderr)

	// An MCP client starts us from any folder, so keep our files in one place there.
	if stdio {
		set := map[string]bool{}
		fl.Visit(func(f *flag.Flag) { set[f.Name] = true })
		if home, err := os.UserHomeDir(); err == nil {
			dir := filepath.Join(home, "."+appName)
			os.MkdirAll(dir, 0755)
			if !set["profile"] {
				bo.Profile = filepath.Join(dir, "chrome-profile")
			}
		}
	}

	local := fmt.Sprintf("http://localhost:%d", *port)
	ln, err := net.Listen("tcp", fmt.Sprintf("0.0.0.0:%d", *port))
	if err != nil {
		if stdio && probeMCP(local+"/mcp") { // another copy already runs the browser: use it
			proxyStdio(local + "/mcp")
			return
		}
		log.Fatalf("Port %d is busy: %v (use -port)", *port, err)
	}

	tm := &TunnelManager{port: *port, backend: *tunnelBackend}
	tunnels = tm

	h := &Hub{clients: map[*Client]bool{}, chat: []ChatMsg{}, dataFile: *data, agent: &Client{user: agentUser}}
	h.load()

	mcp := &MCPServer{}
	if !*noBrowser {
		eng, err := resolveEngine(*engine, bo.ChromePath)
		if err != nil {
			log.Fatal(err)
		}
		bo.Engine = eng
		// Each browser keeps its own profile: a profile written by a newer or
		// different browser can refuse to open (or get damaged) in another one.
		if eng.Name != "chrome" {
			bo.Profile += "-" + eng.Name
		}
		if abs, err := filepath.Abs(bo.Profile); err == nil {
			bo.Profile = abs
		}
		base := filepath.Dir(bo.Profile)
		for dir, name := range map[*string]string{&bo.DownloadDir: "downloads", &bo.UploadDir: "uploads", &bo.StateDir: "states"} {
			if *dir == "" {
				*dir = filepath.Join(base, name)
			} else if abs, err := filepath.Abs(*dir); err == nil {
				*dir = abs
			}
		}
		b, err := startBrowser(h, bo)
		if err != nil {
			log.Fatalf("Could not start %s: %v\n(Pick another -engine, pass -chrome <path>, or run with -no-browser)", bo.Engine.Label, err)
		}
		h.browser = b
		ao.ViewerURL = local + "/?join"
		mcp.agent = newAgent(h, b, ao)
		fmt.Fprintf(out, "Shared browser started (headless %s: %s)\n", bo.Engine.Label, bo.Engine.Path)
	}

	sub, _ := fs.Sub(staticFiles, "static")
	mux := http.NewServeMux()
	mux.Handle("/", http.FileServer(http.FS(sub)))
	mux.HandleFunc("/ws", h.serveWS)
	registerStateAPI(mux, mcp)
	registerTunnelAPI(mux, tm)
	if !*noMCP {
		mux.HandleFunc("/mcp", mcp.ServeHTTP(*mcpRemote))
	}

	fmt.Fprintf(out, "%s is running:\n  %s\n", appName, local)
	for _, ip := range lanIPs() {
		fmt.Fprintf(out, "  http://%s:%d   (share this link on your network)\n", ip, *port)
	}
	if !*noMCP {
		fmt.Fprintf(out, "MCP endpoint for AI agents: %s/mcp\n", local)
	}
	if password() != "" {
		fmt.Fprintln(out, "Password protection is on (MCP clients: send \"Authorization: Bearer <password>\")")
	}

	if *tunnelOn {
		fmt.Fprintln(out, "Opening public tunnel...")
		if err := tm.Start(*tunnelSub); err != nil {
			fmt.Fprintf(out, "Tunnel failed: %v\n(the app keeps running locally)\n", err)
		}
	}

	// Close Chrome on the way out, so no process is left running.
	quit := make(chan struct{}, 1)
	stop := func() {
		select {
		case quit <- struct{}{}:
		default:
		}
	}
	go func() { log.Println(http.Serve(ln, requireAuth(mux))); stop() }()
	if st := tm.Status(); st.On {
		fmt.Fprintf(out, "Public URL: %s\n  MCP: %s/mcp\n", st.URL, st.URL)
		if st.Generated {
			fmt.Fprintf(out, "  Password (generated; set your own with -pw): %s\n", st.Password)
		}
	}
	sig := make(chan os.Signal, 1)
	signal.Notify(sig, os.Interrupt, syscall.SIGTERM)
	go func() { <-sig; stop() }()
	if stdio {
		go func() { mcp.serveStdio(); stop() }() // the MCP client closed stdin: it is done with us
	}
	<-quit
	tm.Stop()
	if h.browser != nil {
		h.browser.Close()
	}
}

func lanIPs() []string {
	var ips []string
	addrs, _ := net.InterfaceAddrs()
	for _, a := range addrs {
		if ipn, ok := a.(*net.IPNet); ok && !ipn.IP.IsLoopback() && ipn.IP.To4() != nil {
			ips = append(ips, ipn.IP.String())
		}
	}
	return ips
}
