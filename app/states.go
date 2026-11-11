package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"time"

	"github.com/chromedp/cdproto/cdp"
	"github.com/chromedp/cdproto/network"
	"github.com/chromedp/cdproto/page"
	"github.com/chromedp/cdproto/storage"
	"github.com/chromedp/chromedp"
)

// ---------- Saved sessions (logins) ----------
//
// A session is every cookie of the browser plus the localStorage/sessionStorage of each site open
// in a tab. Saved under a name it can be loaded again later; exported to a JSON file it can be
// imported into the browser on another machine, which is then signed in to the same sites
// (Facebook, Gmail, GitHub…) without logging in again.

type stCookie struct {
	Name         string                      `json:"name"`
	Value        string                      `json:"value"`
	Domain       string                      `json:"domain"` // a leading dot = also its subdomains; none = that host only
	Path         string                      `json:"path"`
	Expires      float64                     `json:"expires"`
	Session      bool                        `json:"session"`
	HTTPOnly     bool                        `json:"httpOnly"`
	Secure       bool                        `json:"secure"`
	SameSite     string                      `json:"sameSite,omitempty"`
	PartitionKey *network.CookiePartitionKey `json:"partitionKey,omitempty"`
}

// kvMap is one storage area. It reads both {"key": "value"} and Playwright's [{"name", "value"}].
type kvMap map[string]string

func (m *kvMap) UnmarshalJSON(b []byte) error {
	var obj map[string]string
	if err := json.Unmarshal(b, &obj); err == nil {
		*m = obj
		return nil
	}
	var arr []struct{ Name, Value string }
	if err := json.Unmarshal(b, &arr); err != nil {
		return err
	}
	*m = kvMap{}
	for _, kv := range arr {
		(*m)[kv.Name] = kv.Value
	}
	return nil
}

// siteStorage is the localStorage and sessionStorage of one site.
type siteStorage struct {
	Origin  string `json:"origin"`
	Local   kvMap  `json:"localStorage,omitempty"`
	Session kvMap  `json:"sessionStorage,omitempty"`
}

type savedState struct {
	Saved   time.Time  `json:"saved"`
	URL     string     `json:"url"`
	Cookies []stCookie `json:"cookies"`
	// Storage of the page that was open when saving. Kept so older versions can still read the file.
	Origin  string `json:"origin"`
	Local   kvMap  `json:"localStorage"`
	Session kvMap  `json:"sessionStorage"`
	// Storage of every site that was open in a tab.
	Origins []siteStorage `json:"origins,omitempty"`
}

// sites lists the storage to restore, one entry per site.
func (st savedState) sites() []siteStorage {
	var out []siteStorage
	seen := map[string]bool{}
	add := func(s siteStorage) {
		if !strings.HasPrefix(s.Origin, "http") || seen[s.Origin] || len(s.Local)+len(s.Session) == 0 {
			return
		}
		seen[s.Origin] = true
		out = append(out, s)
	}
	for _, s := range st.Origins {
		add(s)
	}
	add(siteStorage{st.Origin, st.Local, st.Session})
	return out
}

// stateBundle is the portable file produced by export and read by import: one or more saved
// states, so a whole set of logins can be carried to another machine in a single JSON file.
type stateBundle struct {
	Version  int                   `json:"version"`
	App      string                `json:"app"`
	Exported time.Time             `json:"exported"`
	States   map[string]savedState `json:"states"`
}

const bundleVersion = 2 // 2: every open site's storage (origins)

var nameRe = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$`)

func (a *Agent) statePath(name string) (string, error) {
	if !nameRe.MatchString(name) {
		return "", errors.New("name is required: letters, digits, - _ . (up to 64)")
	}
	return filepath.Join(a.b.opt.StateDir, name+".json"), nil
}

// ---------- Capture ----------

const dumpStorageJS = `JSON.stringify({origin: location.origin,
  local: Object.fromEntries(Object.keys(localStorage).map(k => [k, localStorage.getItem(k)])),
  session: Object.fromEntries(Object.keys(sessionStorage).map(k => [k, sessionStorage.getItem(k)]))})`

// readStorage returns the storage of the page open in t.
func readStorage(t *Tab) (siteStorage, error) {
	ctx, cancel := context.WithTimeout(t.ctx, 5*time.Second)
	defer cancel()
	raw, err := chromedp.Run(ctx, chromedp.Evaluate[string](dumpStorageJS))
	if err != nil {
		return siteStorage{}, jsError(err)
	}
	var d struct {
		Origin         string
		Local, Session map[string]string
	}
	if err := json.Unmarshal([]byte(raw), &d); err != nil {
		return siteStorage{}, err
	}
	return siteStorage{Origin: d.Origin, Local: d.Local, Session: d.Session}, nil
}

// openTabs returns the tabs with the URL each one shows.
func (b *Browser) openTabs() ([]*Tab, []string) {
	b.mu.Lock()
	defer b.mu.Unlock()
	urls := make([]string, len(b.tabs))
	for i, t := range b.tabs {
		urls[i] = t.url
	}
	return slices.Clone(b.tabs), urls
}

func originOf(raw string) string {
	u, err := url.Parse(raw)
	if err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" {
		return ""
	}
	return u.Scheme + "://" + u.Host
}

// captureState reads the whole login state of the running browser: every cookie (all sites) and
// the storage of each site open in a tab.
func (a *Agent) captureState() (savedState, error) {
	cookies, err := a.allCookies()
	if err != nil {
		return savedState{}, err
	}
	st := savedState{Saved: time.Now(), URL: a.pageURL(), Local: kvMap{}, Session: kvMap{}}
	now := float64(time.Now().Unix())
	for _, c := range cookies {
		if !c.Session && c.Expires > 0 && c.Expires < now {
			continue
		}
		st.Cookies = append(st.Cookies, stCookie{c.Name, c.Value, c.Domain, c.Path, c.Expires, c.Session, c.HTTPOnly, c.Secure, string(c.SameSite), c.PartitionKey})
	}
	tabs, urls := a.b.openTabs()
	seen := map[string]int{}
	for i, t := range tabs {
		if originOf(urls[i]) == "" {
			continue
		}
		s, err := readStorage(t)
		if err != nil || originOf(s.Origin) == "" {
			continue
		}
		if j, ok := seen[s.Origin]; ok { // the same site in two tabs: keep both
			prev := &st.Origins[j]
			if prev.Local == nil {
				prev.Local = kvMap{}
			}
			if prev.Session == nil {
				prev.Session = kvMap{}
			}
			maps.Copy(prev.Local, s.Local)
			maps.Copy(prev.Session, s.Session)
			continue
		}
		seen[s.Origin] = len(st.Origins)
		st.Origins = append(st.Origins, s)
	}
	if j, ok := seen[originOf(st.URL)]; ok {
		cur := st.Origins[j]
		st.Origin, st.Local, st.Session = cur.Origin, cur.Local, cur.Session
	}
	return st, nil
}

// ---------- Restore ----------

func sameSiteOf(s string) network.CookieSameSite {
	switch strings.ToLower(s) {
	case "strict":
		return network.CookieSameSiteStrict
	case "lax":
		return network.CookieSameSiteLax
	case "none", "no_restriction":
		return network.CookieSameSiteNone
	}
	return ""
}

func cookieParam(c stCookie) *network.CookieParam {
	p := &network.CookieParam{Name: c.Name, Value: c.Value, Path: cmpOr(c.Path, "/"), Secure: c.Secure, HTTPOnly: c.HTTPOnly, SameSite: sameSiteOf(c.SameSite), PartitionKey: c.PartitionKey}
	if strings.HasPrefix(c.Domain, ".") {
		p.Domain = c.Domain
	} else { // host-only: set it through its URL, or Chrome would widen it to the subdomains (and refuse __Host- cookies)
		scheme := "http"
		if c.Secure {
			scheme = "https"
		}
		p.URL = scheme + "://" + c.Domain + p.Path
	}
	if !c.Session && c.Expires > 0 {
		p.Expires = cdp.TimeSinceEpoch(c.Expires)
	}
	return p
}

// setCookies puts the cookies into the browser. It returns how many were set and how many Chrome refused.
func (a *Agent) setCookies(cookies []stCookie) (int, int) {
	now := float64(time.Now().Unix())
	var params []*network.CookieParam
	for _, c := range cookies {
		if c.Name == "" || c.Domain == "" || (!c.Session && c.Expires > 0 && c.Expires < now) {
			continue
		}
		params = append(params, cookieParam(c))
	}
	if len(params) == 0 {
		return 0, 0
	}
	set := func(ps []*network.CookieParam) error {
		ctx, cancel := cmdCtx(a.b.rootCtx())
		defer cancel()
		_, err := chromedp.CallBrowser(ctx, storage.SetCookies, storage.SetCookiesParams{Cookies: ps})
		return err
	}
	if set(params) != nil {
		// Chrome refuses the whole batch when one cookie is malformed: set them one by one instead.
		for _, p := range params {
			set([]*network.CookieParam{p})
		}
	}
	// Chrome also drops some cookies silently (SameSite=None without Secure…): count what it kept.
	have, err := a.allCookies()
	if err != nil {
		return len(params), 0
	}
	kept := map[string]bool{}
	for _, c := range have {
		kept[c.Name+"|"+c.Domain+"|"+c.Path] = true
	}
	ok := 0
	for _, p := range params {
		domain := p.Domain
		if domain == "" {
			if u, err := url.Parse(p.URL); err == nil {
				domain = u.Hostname()
			}
		}
		if kept[p.Name+"|"+domain+"|"+p.Path] {
			ok++
		}
	}
	return ok, len(params) - ok
}

// writeStorage fills the storage of the page open in t, which must be on site s.
func writeStorage(t *Tab, s siteStorage, withSession bool) error {
	l, _ := json.Marshal(s.Local)
	ss := []byte("null")
	if withSession {
		ss, _ = json.Marshal(s.Session)
	}
	o, _ := json.Marshal(s.Origin)
	expr := fmt.Sprintf(`(() => {
  if (location.origin !== %s) throw new Error("wrong site: " + location.origin);
  for (const [k, v] of Object.entries(%s || {})) localStorage.setItem(k, v);
  for (const [k, v] of Object.entries(%s || {})) sessionStorage.setItem(k, v);
  return true;
})()`, o, l, ss)
	ctx, cancel := context.WithTimeout(t.ctx, 10*time.Second)
	defer cancel()
	_, err := chromedp.Run(ctx, chromedp.Evaluate[bool](expr))
	return jsError(err)
}

// openOrigin loads a light page of the site in t (its robots.txt), just so its storage can be written.
func openOrigin(t *Tab, origin string) error {
	for _, p := range []string{"/robots.txt", "/"} {
		ctx, cancel := context.WithTimeout(t.ctx, 20*time.Second)
		_, err := chromedp.Run(ctx, chromedp.Navigate(origin+p))
		got := ""
		if err == nil {
			got, err = chromedp.Run(ctx, chromedp.Evaluate[string]("location.origin"))
		}
		cancel()
		if err == nil && got == origin {
			return nil
		}
	}
	return errors.New("could not open " + origin)
}

// writeSites restores the storage of each site. A site open in a tab is written in place; the
// others are opened one after another in a temporary background tab (sessionStorage belongs to a
// tab, so only localStorage survives for those).
func (a *Agent) writeSites(sites []siteStorage) (done, missed []string) {
	if len(sites) == 0 {
		return nil, nil
	}
	tabs, urls := a.b.openTabs()
	open := map[string]*Tab{}
	for i, t := range tabs {
		if o := originOf(urls[i]); o != "" && open[o] == nil {
			open[o] = t
		}
	}
	var rest []siteStorage
	for _, s := range sites {
		if t := open[s.Origin]; t != nil && writeStorage(t, s, true) == nil {
			done = append(done, s.Origin)
		} else if len(s.Local) > 0 {
			rest = append(rest, s)
		}
	}
	if len(rest) == 0 {
		return done, missed
	}
	var tmp *Tab
	err := a.b.call(func() error {
		a.b.mu.Lock()
		full := len(a.b.tabs) >= a.b.opt.MaxTabs
		a.b.mu.Unlock()
		if full {
			return fmt.Errorf("tab limit (%d) reached", a.b.opt.MaxTabs)
		}
		var err error
		tmp, err = a.b.createTab()
		return err
	})
	if err != nil || tmp == nil {
		for _, s := range rest {
			missed = append(missed, s.Origin)
		}
		return done, missed
	}
	defer a.b.call(func() error { a.b.closeTab(tmp.id); return nil })
	for _, s := range rest {
		if openOrigin(tmp, s.Origin) == nil && writeStorage(tmp, s, false) == nil {
			done = append(done, s.Origin)
		} else {
			missed = append(missed, s.Origin)
		}
	}
	return done, missed
}

// applyState puts a saved session into the running browser: its cookies, then each site's storage.
// The caller reloads the page so it picks up the login.
func (a *Agent) applyState(st savedState) (string, error) {
	set, refused := a.setCookies(st.Cookies)
	sites := st.sites()
	if set == 0 && len(sites) == 0 {
		if refused > 0 {
			return "", fmt.Errorf("the browser refused all %d cookies", refused)
		}
		return "", errors.New("the session holds no cookies or storage")
	}
	done, missed := a.writeSites(sites)
	msg := fmt.Sprintf("%d cookies on %d sites, storage of %d sites", set, len(siteNames(st.Cookies)), len(done))
	if refused > 0 {
		msg += fmt.Sprintf(" (%d cookies refused by the browser)", refused)
	}
	if len(missed) > 0 {
		msg += fmt.Sprintf(" (could not restore the storage of %s)", strings.Join(missed, ", "))
	}
	return msg, nil
}

// reloadVisible reloads the tab people are looking at, so a just-imported login shows up.
func (a *Agent) reloadVisible() {
	if !strings.HasPrefix(a.pageURL(), "http") {
		return
	}
	a.b.onTab(func(t *Tab) error {
		ctx, cancel := cmdCtx(t.ctx)
		defer cancel()
		_, err := chromedp.Call(ctx, page.Reload, page.ReloadParams{})
		return err
	})
}

// mergeStates folds several sessions into one, later ones winning.
func mergeStates(states map[string]savedState) savedState {
	names := slices.Sorted(maps.Keys(states))
	var out savedState
	for _, n := range names {
		st := states[n]
		out.Cookies = append(out.Cookies, st.Cookies...)
		out.Origins = append(out.Origins, st.sites()...)
	}
	return out
}

// ---------- Files ----------

type stateInfo struct {
	Name    string    `json:"name"`
	Cookies int       `json:"cookies"`
	Sites   []string  `json:"sites"`
	Origin  string    `json:"origin"`
	Saved   time.Time `json:"saved"`
}

// siteNames lists the sites the cookies belong to (google.com for mail.google.com), sorted.
func siteNames(cookies []stCookie) []string {
	set := map[string]bool{}
	for _, c := range cookies {
		parts := strings.Split(strings.TrimPrefix(c.Domain, "."), ".")
		n := 2
		if len(parts) >= 3 && len(parts[len(parts)-2]) <= 3 { // facebook.com.vn, bbc.co.uk
			n = 3
		}
		if len(parts) > n {
			parts = parts[len(parts)-n:]
		}
		if h := strings.Join(parts, "."); h != "" {
			set[h] = true
		}
	}
	return slices.Sorted(maps.Keys(set))
}

func (a *Agent) stateNames() []string {
	var out []string
	ents, _ := os.ReadDir(a.b.opt.StateDir)
	for _, e := range ents {
		n := strings.TrimSuffix(e.Name(), ".json")
		if !e.IsDir() && strings.HasSuffix(e.Name(), ".json") && nameRe.MatchString(n) {
			out = append(out, n)
		}
	}
	slices.Sort(out)
	return out
}

// listStates returns metadata for every saved state, for the settings UI.
func (a *Agent) listStates() []stateInfo {
	out := []stateInfo{}
	for _, n := range a.stateNames() {
		if st, err := a.readState(n); err == nil {
			out = append(out, stateInfo{Name: n, Cookies: len(st.Cookies), Sites: siteNames(st.Cookies), Origin: st.Origin, Saved: st.Saved})
		}
	}
	return out
}

// readState reads and parses one saved state file by name.
func (a *Agent) readState(name string) (savedState, error) {
	var st savedState
	p, err := a.statePath(name)
	if err != nil {
		return st, err
	}
	data, err := os.ReadFile(p)
	if err != nil {
		return st, fmt.Errorf("no saved state called %q (use action list)", name)
	}
	if err := json.Unmarshal(data, &st); err != nil {
		return st, fmt.Errorf("state %q is damaged: %v", name, err)
	}
	return st, nil
}

func (a *Agent) writeState(name string, st savedState) error {
	p, err := a.statePath(name)
	if err != nil {
		return err
	}
	data, _ := json.MarshalIndent(st, "", "  ")
	return os.WriteFile(p, data, 0o600)
}

func newBundle() stateBundle {
	return stateBundle{Version: bundleVersion, App: appName, Exported: time.Now(), States: map[string]savedState{}}
}

// buildBundle gathers one saved state (name given) or every saved state (name empty) into a bundle.
func (a *Agent) buildBundle(name string) (stateBundle, error) {
	bundle := newBundle()
	if name != "" {
		st, err := a.readState(name)
		if err != nil {
			return bundle, err
		}
		bundle.States[name] = st
		return bundle, nil
	}
	for _, n := range a.stateNames() {
		if st, err := a.readState(n); err == nil {
			bundle.States[n] = st
		}
	}
	if len(bundle.States) == 0 {
		return bundle, errors.New("no saved states to export (save one first)")
	}
	return bundle, nil
}

// liveBundle captures the logins of the running browser right now into a bundle.
func (a *Agent) liveBundle() (stateBundle, error) {
	st, err := a.captureState()
	if err != nil {
		return stateBundle{}, err
	}
	if len(st.Cookies) == 0 && len(st.Origins) == 0 {
		return stateBundle{}, errors.New("the browser has no logins to export yet: sign in to a site first")
	}
	bundle := newBundle()
	bundle.States["browser-"+time.Now().Format("20060102-1504")] = st
	return bundle, nil
}

// extCookie is one cookie as browser extensions export them (Cookie-Editor, EditThisCookie).
type extCookie struct {
	Name           string  `json:"name"`
	Value          string  `json:"value"`
	Domain         string  `json:"domain"`
	Path           string  `json:"path"`
	ExpirationDate float64 `json:"expirationDate"`
	Expires        float64 `json:"expires"`
	HostOnly       bool    `json:"hostOnly"`
	HTTPOnly       bool    `json:"httpOnly"`
	Secure         bool    `json:"secure"`
	Session        bool    `json:"session"`
	SameSite       string  `json:"sameSite"`
}

func (c extCookie) state() stCookie {
	domain := c.Domain
	if c.HostOnly {
		domain = strings.TrimPrefix(domain, ".")
	}
	exp := c.ExpirationDate
	if exp == 0 {
		exp = c.Expires
	}
	return stCookie{Name: c.Name, Value: c.Value, Domain: domain, Path: cmpOr(c.Path, "/"), Expires: exp, Session: c.Session || exp <= 0, HTTPOnly: c.HTTPOnly, Secure: c.Secure, SameSite: c.SameSite}
}

// parseSessions reads an import file: a bundle exported here (or a single saved state), the cookie
// list of the Cookie-Editor / EditThisCookie extensions, or a Playwright storageState, so logins can
// also come from an ordinary Chrome, Edge or Firefox.
func parseSessions(data []byte, fallbackName string) (map[string]savedState, error) {
	name := fallbackName
	if !nameRe.MatchString(name) {
		name = "imported"
	}
	data = bytes.TrimSpace(bytes.TrimPrefix(data, []byte("\xef\xbb\xbf")))
	bad := errors.New("not a sessions file: expected a file exported here, a Cookie-Editor cookie list or a Playwright storageState")
	if len(data) > 0 && data[0] == '[' {
		var list []extCookie
		if err := json.Unmarshal(data, &list); err != nil {
			return nil, bad
		}
		st := savedState{Saved: time.Now()}
		for _, c := range list {
			if c.Name != "" && c.Domain != "" {
				st.Cookies = append(st.Cookies, c.state())
			}
		}
		if len(st.Cookies) == 0 {
			return nil, errors.New("the file holds no cookies")
		}
		return map[string]savedState{name: st}, nil
	}
	var bundle stateBundle
	if json.Unmarshal(data, &bundle) == nil && len(bundle.States) > 0 {
		return bundle.States, nil
	}
	var one savedState
	if json.Unmarshal(data, &one) == nil && (len(one.Cookies) > 0 || len(one.sites()) > 0) {
		return map[string]savedState{name: one}, nil
	}
	return nil, bad
}

// saveBundle parses an import file and writes every usable state into the states folder,
// returning the names it saved.
func (a *Agent) saveBundle(data []byte, fallbackName string) ([]string, error) {
	states, err := parseSessions(data, fallbackName)
	if err != nil {
		return nil, err
	}
	var names []string
	for name, st := range states {
		if nameRe.MatchString(name) && a.writeState(name, st) == nil {
			names = append(names, name)
		}
	}
	if len(names) == 0 {
		return nil, errors.New("nothing imported: the file held no usable states")
	}
	slices.Sort(names)
	return names, nil
}

// importAndApply saves every session of an import file and loads them all into the running
// browser, which is then signed in without logging in again.
func (a *Agent) importAndApply(data []byte, fallbackName string) ([]string, string, error) {
	names, err := a.saveBundle(data, fallbackName)
	if err != nil {
		return nil, "", err
	}
	states := map[string]savedState{}
	for _, n := range names {
		if st, err := a.readState(n); err == nil {
			states[n] = st
		}
	}
	msg, err := a.applyState(mergeStates(states))
	if err != nil {
		return names, "", err
	}
	a.reloadVisible()
	return names, msg, nil
}

// ---------- browser_state tool ----------

// State saves or restores the whole login state (cookies + storage of the open sites) under a name.
func (a *Agent) State(action, name, path string) (string, error) {
	switch action {
	case "", "list":
		var s strings.Builder
		s.WriteString("### Saved states\n")
		infos := a.listStates()
		for _, st := range infos {
			fmt.Fprintf(&s, "- %s: %d cookies on %d sites, saved %s\n", st.Name, st.Cookies, len(st.Sites), st.Saved.Format("2006-01-02 15:04"))
		}
		if len(infos) == 0 {
			s.WriteString("None. Save one with action save and a name.\n")
		}
		return s.String(), nil
	case "export":
		return a.exportStates(name, path)
	case "import":
		return a.importStates(path)
	}
	path, err := a.statePath(name)
	if err != nil {
		return "", err
	}
	switch action {
	case "save":
		st, err := a.captureState()
		if err != nil {
			return "", err
		}
		if err := a.writeState(name, st); err != nil {
			return "", err
		}
		a.announce("💾", "Save session "+quote(name), nil)
		return fmt.Sprintf("Saved state %q: %d cookies on %d sites, storage of %d open sites.", name, len(st.Cookies), len(siteNames(st.Cookies)), len(st.Origins)), nil

	case "load":
		st, err := a.readState(name)
		if err != nil {
			return "", err
		}
		if err := a.hub.agentTake(); err != nil {
			return "", err
		}
		a.announce("💾", "Load session "+quote(name), nil)
		msg, err := a.applyState(st)
		if err != nil {
			return "", err
		}
		if strings.HasPrefix(a.pageURL(), "http") {
			if err := a.Reload(); err != nil {
				return "", err
			}
		}
		return fmt.Sprintf("Loaded state %q: %s.", name, msg), nil

	case "delete":
		if err := os.Remove(path); err != nil {
			return "", fmt.Errorf("no saved state called %q", name)
		}
		return "Deleted state " + name + ".", nil
	}
	return "", fmt.Errorf("action must be save, load, list, delete, export or import, not %q", action)
}

// exportStates bundles one saved state (name given) or every saved state (name empty) into a single
// JSON file that can be copied to another machine and read back with import. If path is empty the
// file is written into the downloads folder.
func (a *Agent) exportStates(name, path string) (string, error) {
	bundle, err := a.buildBundle(name)
	if err != nil {
		return "", err
	}
	if path == "" {
		fname := "states-export-" + time.Now().Format("20060102-150405") + ".json"
		path = filepath.Join(cmpOr(a.b.opt.DownloadDir, a.b.opt.StateDir), fname)
	} else if !strings.HasSuffix(path, ".json") {
		path += ".json"
	}
	data, _ := json.MarshalIndent(bundle, "", "  ")
	if err := os.WriteFile(path, data, 0o600); err != nil {
		return "", err
	}
	abs, _ := filepath.Abs(path)
	a.announce("📤", fmt.Sprintf("Export %d session(s)", len(bundle.States)), nil)
	return fmt.Sprintf("Exported %d session(s) to %s. Copy this file to the other machine and run action import with its path there; the secrets inside let anyone log in as you, so keep it safe and delete it when done.", len(bundle.States), abs), nil
}

// importStates reads a bundle written by export and saves every state inside it into the states
// folder, ready to be restored with action load.
func (a *Agent) importStates(path string) (string, error) {
	if path == "" {
		return "", errors.New("path is required: the JSON file produced by action export")
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return "", fmt.Errorf("cannot read %q: %v", path, err)
	}
	names, err := a.saveBundle(data, strings.TrimSuffix(filepath.Base(path), ".json"))
	if err != nil {
		return "", err
	}
	a.announce("📥", fmt.Sprintf("Import %d session(s)", len(names)), nil)
	return fmt.Sprintf("Imported %d session(s): %s. Load one with action load and its name to sign in without logging in again.", len(names), strings.Join(names, ", ")), nil
}
