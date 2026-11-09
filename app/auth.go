package main

// Password protection (-pw). People get a login page and then a cookie; programs
// (MCP clients) send the password as "Authorization: Bearer <password>" (or Basic auth).

import (
	"crypto/hmac"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/hex"
	"html"
	"net/http"
	"strings"
	"sync"
	"time"
)

// authPassword is the -pw flag; empty means no password. After startup only
// setPassword changes it (the tunnel button), and everything reads it via password().
var authPassword string
var pwMu sync.RWMutex

func password() string     { pwMu.RLock(); defer pwMu.RUnlock(); return authPassword }
func setPassword(p string) { pwMu.Lock(); authPassword = p; pwMu.Unlock() }

const authCookie = "abu_auth"

// authToken is what the cookie holds: derived from the password, never the password itself.
func authToken() string {
	m := hmac.New(sha256.New, []byte(password()))
	m.Write([]byte("agent-browser-use session"))
	return hex.EncodeToString(m.Sum(nil))
}

func same(a, b string) bool { return subtle.ConstantTimeCompare([]byte(a), []byte(b)) == 1 }

func authorized(r *http.Request) bool {
	if password() == "" {
		return true
	}
	if c, err := r.Cookie(authCookie); err == nil && same(c.Value, authToken()) {
		return true
	}
	if h := r.Header.Get("Authorization"); strings.HasPrefix(h, "Bearer ") && same(strings.TrimPrefix(h, "Bearer "), password()) {
		return true
	}
	if _, pw, ok := r.BasicAuth(); ok && same(pw, password()) {
		return true
	}
	return false
}

// requireAuth wraps the whole server. Browsers that aren't logged in get the login page.
func requireAuth(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if password() == "" {
			next.ServeHTTP(w, r)
			return
		}
		if r.URL.Path == "/login" {
			login(w, r)
			return
		}
		if authorized(r) {
			next.ServeHTTP(w, r)
			return
		}
		if r.Method == http.MethodGet && strings.Contains(r.Header.Get("Accept"), "text/html") {
			loginPage(w, r.URL.RequestURI(), false)
			return
		}
		w.Header().Set("WWW-Authenticate", `Bearer realm="agent-browser-use"`)
		http.Error(w, "password required", http.StatusUnauthorized)
	})
}

func login(w http.ResponseWriter, r *http.Request) {
	next := r.FormValue("next")
	if !strings.HasPrefix(next, "/") || strings.HasPrefix(next, "//") {
		next = "/"
	}
	if r.Method != http.MethodPost {
		loginPage(w, next, false)
		return
	}
	if !same(r.FormValue("password"), password()) {
		time.Sleep(time.Second) // slows down guessing
		loginPage(w, next, true)
		return
	}
	http.SetCookie(w, &http.Cookie{Name: authCookie, Value: authToken(), Path: "/", HttpOnly: true,
		SameSite: http.SameSiteLaxMode, MaxAge: 30 * 24 * 3600})
	http.Redirect(w, r, next, http.StatusSeeOther)
}

func loginPage(w http.ResponseWriter, next string, wrong bool) {
	msg := ""
	if wrong {
		msg = `<p class="err">Wrong password</p>`
	}
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.WriteHeader(http.StatusUnauthorized)
	w.Write([]byte(`<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="color-scheme" content="dark light">
<script>try{var t=localStorage.getItem("theme")||(matchMedia("(prefers-color-scheme: light)").matches?"light":"dark");document.documentElement.dataset.theme=t}catch(e){}</script>
<title>Agent Browser Use</title>
<link rel="preconnect" href="https://fonts.googleapis.com"><link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&display=swap" rel="stylesheet">
<style>
:root{--bg:#0a0a0c;--card:#111114;--field:#0d0d10;--line:rgba(255,255,255,.22);--fg:#f4f4f5;--muted:#a1a1aa;--dim:#71717a;--accent:#6366f1;--accent-hi:#818cf8;--shadow:rgba(0,0,0,.7);--err:#f87171}
:root[data-theme="light"]{--bg:#e9eaee;--card:#fff;--field:#f1f2f6;--line:rgba(15,15,30,.28);--fg:#14141a;--muted:#52525b;--dim:#71717a;--accent:#4f46e5;--accent-hi:#4338ca;--shadow:rgba(20,20,40,.25);--err:#dc2626}
*{box-sizing:border-box}
body{margin:0;min-height:100vh;display:grid;place-items:center;padding:16px;font:14px/1.5 Inter,ui-sans-serif,system-ui,sans-serif;background:var(--bg);color:var(--fg)}
form{width:100%;max-width:384px;padding:32px;border-radius:2px;border:1px solid var(--line);background:var(--card);box-shadow:0 28px 80px var(--shadow);animation:rise .35s ease both}
@keyframes rise{from{opacity:0;transform:translateY(8px)}to{opacity:1;transform:none}}
.head{display:flex;flex-direction:column;align-items:center;text-align:center;margin-bottom:32px}
.logo{display:grid;place-items:center;width:56px;height:56px;border-radius:2px;background:var(--accent);color:#fff;margin-bottom:16px}
h1{margin:0;font-size:24px;font-weight:600;letter-spacing:-.02em}
.sub{margin:6px 0 0;color:var(--muted)}
label{display:block;margin-bottom:8px;font-size:11px;font-weight:600;letter-spacing:.06em;text-transform:uppercase;color:var(--dim)}
input[type=password]{width:100%;height:44px;padding:0 14px;font:inherit;border-radius:2px;border:1px solid var(--line);background:var(--field);color:var(--fg);outline:none;transition:border-color .15s,box-shadow .15s}
input[type=password]::placeholder{color:var(--dim)}
input[type=password]:focus{border-color:var(--accent);box-shadow:0 0 0 4px rgba(99,102,241,.18)}
button{display:flex;align-items:center;justify-content:center;gap:8px;width:100%;height:44px;margin-top:16px;font:inherit;font-size:15px;font-weight:500;color:#fff;border:0;border-radius:2px;background:var(--accent);cursor:pointer;transition:background .15s,transform .1s}
button:hover{background:var(--accent-hi)}button:active{transform:scale(.98)}
.err{margin:0 0 12px;font-size:13px;color:var(--err)}
.note{margin:20px 0 0;text-align:center;font-size:12px;color:var(--dim)}
</style></head><body><form method="post" action="/login">
<div class="head"><div class="logo"><svg width="28" height="28" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="14" rx="2"/><path d="M8 21h8M12 17v4"/><path d="m9 10 2 2 4-4"/></svg></div>
<h1>Agent Browser Use</h1><p class="sub">Watch your AI agent use the web — live.</p></div>` + msg + `
<input type="hidden" name="next" value="` + html.EscapeString(next) + `">
<label for="pw">Password</label>
<input id="pw" type="password" name="password" placeholder="Enter the room password" autofocus required>
<button type="submit">Enter room <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12h14"/><path d="m12 5 7 7-7 7"/></svg></button>
<p class="note">This room is password protected. Ask whoever shared the link.</p>
</form></body></html>`))
}
