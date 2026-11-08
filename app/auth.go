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
	"time"
)

// authPassword is the -pw flag; empty means no password.
var authPassword string

const authCookie = "abu_auth"

// authToken is what the cookie holds: derived from the password, never the password itself.
func authToken() string {
	m := hmac.New(sha256.New, []byte(authPassword))
	m.Write([]byte("agent-browser-use session"))
	return hex.EncodeToString(m.Sum(nil))
}

func same(a, b string) bool { return subtle.ConstantTimeCompare([]byte(a), []byte(b)) == 1 }

func authorized(r *http.Request) bool {
	if authPassword == "" {
		return true
	}
	if c, err := r.Cookie(authCookie); err == nil && same(c.Value, authToken()) {
		return true
	}
	if h := r.Header.Get("Authorization"); strings.HasPrefix(h, "Bearer ") && same(strings.TrimPrefix(h, "Bearer "), authPassword) {
		return true
	}
	if _, pw, ok := r.BasicAuth(); ok && same(pw, authPassword) {
		return true
	}
	return false
}

// requireAuth wraps the whole server. Browsers that aren't logged in get the login page.
func requireAuth(next http.Handler) http.Handler {
	if authPassword == "" {
		return next
	}
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
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
	if !same(r.FormValue("password"), authPassword) {
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
	w.Write([]byte(`<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>agent-browser-use</title><style>
body{margin:0;min-height:100vh;display:grid;place-items:center;font:15px system-ui,sans-serif;background:#f4f4f5;color:#18181b}
form{background:#fff;padding:28px;border-radius:12px;box-shadow:0 2px 12px #0001;width:min(320px,calc(100vw - 32px));box-sizing:border-box}
h1{font-size:18px;margin:0 0 16px}input,button{width:100%;box-sizing:border-box;padding:10px;font:inherit;border-radius:8px}
input{border:1px solid #d4d4d8;margin-bottom:12px}button{border:0;background:#2563eb;color:#fff;cursor:pointer}.err{color:#dc2626;margin:0 0 12px}
@media (prefers-color-scheme:dark){body{background:#18181b;color:#f4f4f5}form{background:#27272a}input{background:#18181b;color:#f4f4f5;border-color:#3f3f46}}
</style></head><body><form method="post" action="/login"><h1>agent-browser-use</h1>` + msg + `
<input type="hidden" name="next" value="` + html.EscapeString(next) + `">
<input type="password" name="password" placeholder="Password" autofocus required>
<button type="submit">Enter</button></form></body></html>`))
}
