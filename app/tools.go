package main

// Agent actions for the extra MCP tools: console and network inspection, downloads and
// uploads, cookies and storage, saved sessions, snapshot diffs and device emulation.

import (
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/chromedp/cdproto/cdp"
	"github.com/chromedp/cdproto/dom"
	"github.com/chromedp/cdproto/network"
	"github.com/chromedp/cdproto/page"
	cdpruntime "github.com/chromedp/cdproto/runtime"
	"github.com/chromedp/cdproto/storage"
	"github.com/chromedp/chromedp"
)

// ---------- Console and network ----------

// activeObs is the observer of the tab the agent is looking at.
func (a *Agent) activeObs() *observer {
	a.b.mu.Lock()
	defer a.b.mu.Unlock()
	if a.tabID != "" {
		for _, t := range a.b.tabs {
			if t.id == a.tabID {
				return t.obs
			}
		}
		return nil
	}
	if a.b.active == nil {
		return nil
	}
	return a.b.active.obs
}

func (a *Agent) Console(level string, limit int, clear, history bool) (string, error) {
	o := a.activeObs()
	if o == nil {
		return "", errors.New("no tab is open")
	}
	a.announce("🪵", "Read the console", nil)
	return o.consoleReport(level, limit, clear, history), nil
}

// Network lists requests, or one request in detail (with its body) when id > 0.
func (a *Agent) Network(f netFilter, id int, body, clear bool) (string, error) {
	o := a.activeObs()
	if o == nil {
		return "", errors.New("no tab is open")
	}
	if id <= 0 {
		a.announce("📡", "Check network requests", nil)
		return o.networkReport(f, clear), nil
	}
	n, ok := o.find(id)
	if !ok {
		return "", fmt.Errorf("no request with id %d (it may have been cleared; list requests first)", id)
	}
	a.announce("📡", fmt.Sprintf("Inspect request #%d", id), nil)
	out := n.detail()
	if !body {
		return out, nil
	}
	if !n.HasBody {
		return out + "\nNo response body (the request is not finished, failed, or is a redirect).\n", nil
	}
	var data []byte
	err := a.look(func(t *Tab) error {
		res, err := chromedp.Call(t.ctx, network.GetResponseBody, network.GetResponseBodyParams{RequestID: n.reqID})
		if err == nil {
			data = res.Body
		}
		return err
	})
	if err != nil {
		return out + "\nResponse body not available: " + err.Error() + "\n", nil
	}
	if !utf8.Valid(data) {
		return out + fmt.Sprintf("\nResponse body is binary (%s).\n", humanSize(int64(len(data)))), nil
	}
	text := string(data)
	if len(text) > 20000 {
		text = text[:20000] + "\n… (cut)"
	}
	return out + "\nResponse body:\n```\n" + text + "\n```\n", nil
}

// Alerts describes new console errors and failed requests, to append to a tool result.
func (a *Agent) Alerts() string {
	o := a.activeObs()
	if o == nil {
		return ""
	}
	s, n := o.alerts()
	if n > 0 {
		a.hub.agentLog(fmt.Sprintf("⚠️ %d problem(s) on the page (console errors or failed requests)", n))
	}
	return s
}

// ---------- Downloads and uploads ----------

func (a *Agent) Downloads(action, name string, seconds float64) (string, error) {
	list := func() string {
		dls := a.b.dl.snapshot()
		a.dlSeen = len(dls)
		var s strings.Builder
		fmt.Fprintf(&s, "### Downloads (folder: %s)\n", a.b.opt.DownloadDir)
		if len(dls) == 0 {
			s.WriteString("No downloads yet.\n")
		}
		for i, d := range dls {
			s.WriteString(d.line(i+1) + "\n")
		}
		return s.String()
	}
	switch action {
	case "", "list":
		return list(), nil
	case "wait":
		if seconds <= 0 {
			seconds = 30
		}
		seconds = min(seconds, 120)
		a.announce("⏳", "Wait for a download", nil)
		end := time.Now().Add(time.Duration(seconds * float64(time.Second)))
		for time.Now().Before(end) {
			dls := a.b.dl.snapshot()
			active := slices.ContainsFunc(dls, func(d download) bool { return d.State == "inProgress" })
			if !active && len(dls) > a.dlSeen { // something new finished (maybe already, while the click was settling)
				return list(), nil
			}
			time.Sleep(200 * time.Millisecond)
		}
		return "", fmt.Errorf("no download finished within %.0fs. If the page should have started one, check the click; current state:\n%s", seconds, list())
	case "read":
		path, err := a.downloadFile(name)
		if err != nil {
			return "", err
		}
		data, err := os.ReadFile(path)
		if err != nil {
			return "", err
		}
		a.announce("📄", "Read downloaded file "+quote(filepath.Base(path)), nil)
		if len(data) > 60000 || !utf8.Valid(data) {
			return fmt.Sprintf("%s is %s and not plain text (or too big to show). It is saved at %s", filepath.Base(path), humanSize(int64(len(data))), path), nil
		}
		return string(data), nil
	}
	return "", fmt.Errorf("action must be list, wait or read, not %q", action)
}

// downloadFile finds a downloaded file by name (the newest one when name is empty).
func (a *Agent) downloadFile(name string) (string, error) {
	dir := a.b.opt.DownloadDir
	if name == "" {
		dls := a.b.dl.snapshot()
		for i := len(dls) - 1; i >= 0; i-- {
			if dls[i].State == "completed" {
				return dls[i].Path, nil
			}
		}
		return "", errors.New("no finished download yet")
	}
	n := safeName(name)
	if n == "" {
		return "", fmt.Errorf("invalid file name %q", name)
	}
	p := filepath.Join(dir, n)
	if _, err := os.Stat(p); err != nil {
		return "", fmt.Errorf("%q is not in the download folder %s", n, dir)
	}
	return p, nil
}

type UploadFile struct {
	Name string `json:"name"`
	Data string `json:"data"` // base64
}

// Upload gives files to a <input type=file>. ref can be the input or something that belongs to it (button, label).
func (a *Agent) Upload(ref, element string, paths []string, files []UploadFile) (string, error) {
	if ref == "" {
		return "", errors.New("ref is required: the file input (or its button/label) from a snapshot")
	}
	for _, f := range files { // files sent inline are stored in the upload folder first
		n := safeName(f.Name)
		data, err := base64.StdEncoding.DecodeString(f.Data)
		if n == "" || err != nil {
			return "", fmt.Errorf("files: %q needs a plain file name and base64 data", f.Name)
		}
		if len(data) > 20<<20 {
			return "", fmt.Errorf("files: %q is bigger than 20 MB", n)
		}
		if err := os.WriteFile(filepath.Join(a.b.opt.UploadDir, n), data, 0o600); err != nil {
			return "", err
		}
		paths = append(paths, n)
	}
	abs, err := a.b.uploadPaths(paths)
	if err != nil {
		return "", err
	}
	names := make([]string, len(abs))
	for i, p := range abs {
		names[i] = filepath.Base(p)
	}
	a.announce("📎", "Upload "+quote(strings.Join(names, ", "))+" to "+cmpOr(quote(element), ref), nil)
	err = a.act(func(t *Tab) error {
		res, err := chromedp.Call(t.ctx, cdpruntime.Evaluate, cdpruntime.EvaluateParams{Expression: agentJS + "\n;__abu.fileInput(" + jsq(ref) + ")"})
		if err != nil {
			return jsError(err)
		}
		if res.ExceptionDetails != nil {
			return exceptionError(res.ExceptionDetails)
		}
		if res.Result == nil || res.Result.ObjectID == "" {
			return errors.New("could not find the file input")
		}
		_, err = chromedp.Call(t.ctx, dom.SetFileInputFiles, dom.SetFileInputFilesParams{Files: abs, ObjectID: res.Result.ObjectID})
		return err
	})
	if err != nil {
		return "", err
	}
	a.settle(3 * time.Second)
	return "Uploaded " + strings.Join(names, ", "), nil
}

func exceptionError(d *cdpruntime.ExceptionDetails) error {
	msg := d.Text
	if d.Exception != nil && d.Exception.Description != "" {
		msg, _, _ = strings.Cut(d.Exception.Description, "\n    at ")
	}
	return errors.New(strings.TrimPrefix(msg, "Error: "))
}

// ---------- Cookies and storage ----------

// pageURL is the address of the tab the agent is on.
func (a *Agent) pageURL() string {
	tabs, active, _, _ := a.b.State()
	if a.tabID != "" {
		active = a.tabID
	}
	for _, t := range tabs {
		if t.ID == active {
			return t.URL
		}
	}
	return ""
}

type CookieArgs struct {
	Action, URL, Name, Value, Domain, Path, SameSite string
	Secure, HTTPOnly                                 *bool
	Expires                                          float64
	All, Full                                        bool
}

func cookieLine(c *network.Cookie, full bool) string {
	v := c.Value
	if !full && utf8.RuneCountInString(v) > 60 {
		v = string([]rune(v)[:60]) + "…"
	}
	var flags []string
	if c.Secure {
		flags = append(flags, "Secure")
	}
	if c.HTTPOnly {
		flags = append(flags, "HttpOnly")
	}
	if c.SameSite != "" {
		flags = append(flags, "SameSite="+string(c.SameSite))
	}
	exp := "session"
	if !c.Session && c.Expires > 0 {
		exp = "expires " + time.Unix(int64(c.Expires), 0).Format("2006-01-02 15:04")
	}
	info := append([]string{c.Domain + c.Path}, flags...)
	return fmt.Sprintf("- %s=%s  (%s; %s)", c.Name, v, strings.Join(info, " "), exp)
}

func (a *Agent) allCookies() ([]*network.Cookie, error) {
	res, err := chromedp.CallBrowser(a.b.rootCtx(), storage.GetCookies, storage.GetCookiesParams{})
	return res.Cookies, err
}

func (a *Agent) Cookies(c CookieArgs) (string, error) {
	cur := a.pageURL()
	switch c.Action {
	case "", "list":
		a.announce("🍪", "Read cookies", nil)
		var cookies []*network.Cookie
		var err error
		if c.All {
			cookies, err = a.allCookies()
		} else {
			u := cmpOr(c.URL, cur)
			if !strings.HasPrefix(u, "http") {
				return "", errors.New("open a web page first, or pass url / all: true")
			}
			err = a.look(func(t *Tab) error {
				res, err := chromedp.Call(t.ctx, network.GetCookies, network.GetCookiesParams{URLs: []string{u}})
				cookies = res.Cookies
				return err
			})
		}
		if err != nil {
			return "", err
		}
		if c.Name != "" {
			cookies = slices.DeleteFunc(cookies, func(x *network.Cookie) bool { return x.Name != c.Name })
		}
		slices.SortFunc(cookies, func(x, y *network.Cookie) int { return strings.Compare(x.Domain+x.Name, y.Domain+y.Name) })
		var s strings.Builder
		fmt.Fprintf(&s, "### Cookies (%d)\n", len(cookies))
		for _, k := range cookies {
			s.WriteString(cookieLine(k, c.Full) + "\n")
		}
		return s.String(), nil

	case "set":
		if c.Name == "" {
			return "", errors.New("name is required")
		}
		p := network.SetCookieParams{Name: c.Name, Value: c.Value, Domain: c.Domain, Path: c.Path, Secure: c.Secure, HTTPOnly: c.HTTPOnly}
		if c.Domain == "" {
			p.URL = cmpOr(c.URL, cur)
			if !strings.HasPrefix(p.URL, "http") {
				return "", errors.New("open a web page first, or pass url or domain")
			}
		}
		switch strings.ToLower(c.SameSite) {
		case "":
		case "strict":
			p.SameSite = network.CookieSameSiteStrict
		case "lax":
			p.SameSite = network.CookieSameSiteLax
		case "none":
			p.SameSite = network.CookieSameSiteNone
		default:
			return "", fmt.Errorf("sameSite must be Strict, Lax or None, not %q", c.SameSite)
		}
		if c.Expires > 0 {
			p.Expires = cdp.TimeSinceEpoch(c.Expires)
		}
		a.announce("🍪", "Set cookie "+quote(c.Name), nil)
		if err := a.act(func(t *Tab) error { _, err := chromedp.Call(t.ctx, network.SetCookie, p); return err }); err != nil {
			return "", err
		}
		return "Cookie " + c.Name + " set.", nil

	case "delete":
		if c.Name == "" {
			return "", errors.New("name is required (use action clear to remove many)")
		}
		p := network.DeleteCookiesParams{Name: c.Name, Domain: c.Domain, Path: c.Path}
		if c.Domain == "" {
			p.URL = cmpOr(c.URL, cur)
		}
		a.announce("🍪", "Delete cookie "+quote(c.Name), nil)
		if err := a.act(func(t *Tab) error { _, err := chromedp.Call(t.ctx, network.DeleteCookies, p); return err }); err != nil {
			return "", err
		}
		return "Cookie " + c.Name + " deleted.", nil

	case "clear":
		if c.Domain != "" {
			a.announce("🍪", "Clear cookies of "+c.Domain, nil)
		} else {
			a.announce("🍪", "Clear all cookies", nil)
		}
		if c.Domain == "" {
			if err := a.act(func(t *Tab) error {
				_, err := chromedp.Call(t.ctx, network.ClearBrowserCookies, cdp.Empty{})
				return err
			}); err != nil {
				return "", err
			}
			return "All cookies cleared. You are signed out everywhere.", nil
		}
		cookies, err := a.allCookies()
		if err != nil {
			return "", err
		}
		want := strings.TrimPrefix(strings.ToLower(c.Domain), ".")
		n := 0
		err = a.act(func(t *Tab) error {
			for _, k := range cookies {
				d := strings.TrimPrefix(strings.ToLower(k.Domain), ".")
				if d == want || strings.HasSuffix(d, "."+want) {
					if _, err := chromedp.Call(t.ctx, network.DeleteCookies, network.DeleteCookiesParams{Name: k.Name, Domain: k.Domain, Path: k.Path}); err != nil {
						return err
					}
					n++
				}
			}
			return nil
		})
		return fmt.Sprintf("%d cookie(s) of %s cleared.", n, c.Domain), err
	}
	return "", fmt.Errorf("action must be list, set, delete or clear, not %q", c.Action)
}

// Storage reads and writes localStorage / sessionStorage of the current page.
func (a *Agent) Storage(action, kind, key, value string) (string, error) {
	area := "localStorage"
	switch strings.ToLower(kind) {
	case "", "local":
	case "session":
		area = "sessionStorage"
	default:
		return "", fmt.Errorf("type must be local or session, not %q", kind)
	}
	var expr string
	switch action {
	case "", "list":
		expr = `JSON.stringify(Object.fromEntries(Object.keys(` + area + `).map(k => [k, ` + area + `.getItem(k)])))`
	case "get":
		expr = area + ".getItem(" + jsq(key) + ")"
	case "set":
		if key == "" {
			return "", errors.New("key is required")
		}
		expr = "(" + area + ".setItem(" + jsq(key) + ", " + jsq(value) + "), 'ok')"
	case "remove":
		expr = "(" + area + ".removeItem(" + jsq(key) + "), 'ok')"
	case "clear":
		expr = "(" + area + ".clear(), 'ok')"
	default:
		return "", fmt.Errorf("action must be list, get, set, remove or clear, not %q", action)
	}
	if (action == "get" || action == "remove") && key == "" {
		return "", errors.New("key is required")
	}
	a.announce("🗄️", fmt.Sprintf("%s %s", cmpOr(action, "list"), area), nil)
	var out any
	err := a.act(func(t *Tab) error {
		var err error
		out, err = evalJS[any](t, expr)
		return err
	})
	if err != nil {
		return "", err
	}
	switch action {
	case "", "list":
		var m map[string]string
		json.Unmarshal([]byte(fmt.Sprint(out)), &m)
		keys := make([]string, 0, len(m))
		for k := range m {
			keys = append(keys, k)
		}
		slices.Sort(keys)
		var s strings.Builder
		fmt.Fprintf(&s, "### %s of %s (%d)\n", area, a.pageURL(), len(keys))
		for _, k := range keys {
			fmt.Fprintf(&s, "- %s = %s\n", k, oneLine(m[k], 200))
		}
		return s.String(), nil
	case "get":
		if out == nil {
			return "(no such key)", nil
		}
		return fmt.Sprint(out), nil
	}
	return fmt.Sprint(out), nil
}

// ---------- Snapshot diff ----------

type mark struct{ snap, url string }

// SnapshotDiff shows what changed on the page compared with an earlier snapshot: the last one the
// agent saw (baseline "last"), or one saved under a name with save. save keeps the current page as a new baseline.
func (a *Agent) SnapshotDiff(baseline, save string) (string, error) {
	var cur string
	if err := a.evalRetry(func(t *Tab) error {
		var err error
		cur, err = evalJS[string](t, "__abu.snapshot()")
		return err
	}); err != nil {
		return "", err
	}
	url := a.pageURL()
	var prev, prevURL string
	if baseline == "" || baseline == "last" {
		if a.last == "" {
			return "", errors.New("there is no earlier snapshot to compare with yet: call browser_snapshot first, or use save to name a baseline")
		}
		prev = a.last
		if _, u, ok := strings.Cut(a.lastKey, " "); ok {
			prevURL = u
		}
		baseline = "the last snapshot"
	} else {
		m, ok := a.marks[baseline]
		if !ok {
			names := make([]string, 0, len(a.marks))
			for k := range a.marks {
				names = append(names, k)
			}
			slices.Sort(names)
			return "", fmt.Errorf("no baseline called %q. Saved baselines: %s", baseline, cmpOr(strings.Join(names, ", "), "none (create one with save)"))
		}
		prev, prevURL = m.snap, m.url
		baseline = "baseline " + quote(baseline)
	}

	var s strings.Builder
	fmt.Fprintf(&s, "### Snapshot diff against %s\n", baseline)
	if prevURL != "" && prevURL != url {
		fmt.Fprintf(&s, "- URL changed: %s → %s\n", prevURL, url)
	}
	a.announce("🔍", "Compare the page with "+baseline, nil)
	if d, ok := diffOutline(prev, cur, true); ok {
		s.WriteString(d)
	} else {
		fmt.Fprintf(&s, "Too much changed to list (%d lines before, %d now). Call browser_snapshot for the whole page.\n", strings.Count(prev, "\n")+1, strings.Count(cur, "\n")+1)
	}
	tabs, active, _, _ := a.b.State()
	key := ""
	for _, t := range tabs {
		if t.ID == active {
			key = t.ID + " " + t.URL
		}
	}
	a.last, a.lastKey = cur, key // the agent has now seen this page
	if save != "" {
		if !nameRe.MatchString(save) {
			return "", errors.New("save: use letters, digits, - _ . only")
		}
		if a.marks == nil {
			a.marks = map[string]mark{}
		}
		if _, exists := a.marks[save]; !exists && len(a.marks) >= 20 {
			return "", errors.New("too many saved baselines (20): reuse a name")
		}
		a.marks[save] = mark{cur, url}
		fmt.Fprintf(&s, "\nSaved the current page as baseline %s.\n", quote(save))
	}
	return s.String(), nil
}

// ---------- Device emulation ----------

func (a *Agent) Emulate(o EmulateOpts) (string, error) {
	if o.empty() || strings.EqualFold(o.Device, "list") {
		return a.b.Emulation().String() + "\nDevices: " + presetNames() + "\n", nil
	}
	if err := a.hub.agentTake(); err != nil {
		return "", err
	}
	label := cmpOr(o.Device, "custom settings")
	if strings.EqualFold(o.Device, "reset") {
		label = "default desktop"
	}
	a.announce("📱", "Emulate "+label, nil)
	prev := a.b.Emulation()
	e, err := a.b.SetEmulation(o)
	if err != nil {
		return "", err
	}
	note := ""
	if e.Width != prev.Width || e.Height != prev.Height || e.Mobile != prev.Mobile || e.UserAgent != prev.UserAgent || e.Scale != prev.Scale || e.Locale != prev.Locale {
		if u := a.pageURL(); strings.HasPrefix(u, "http") {
			if err := a.look(func(t *Tab) error { _, err := chromedp.Call(t.ctx, page.Reload, page.ReloadParams{}); return err }); err == nil {
				a.settle(15 * time.Second)
				note = "The page was reloaded so the new device takes effect.\n"
			}
		}
	}
	return e.String() + "\n" + note, nil
}
