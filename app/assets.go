package main

// The viewer's Assets tab: everything the browser produced while people and agents worked, kept
// in one place so it can be looked at afterwards.
//   - screenshot: a picture taken by an agent (browser_take_screenshot) or by the Capture button
//   - copy:       text someone copied (Ctrl/Cmd+C or X) inside the shared browser
//   - element:    an element somebody picked with the Pick tool
//   - download:   a file the browser downloaded (stays in the download folder; only listed here)
//
// The list survives a restart (index.json next to the picture files). Every change is announced to
// all viewers with an "assets" message, so their lists stay current.

import (
	"encoding/json"
	"errors"
	"mime"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

const (
	maxAssets    = 200    // oldest ones are dropped beyond this
	maxAssetText = 100000 // characters kept for one copied text or element
)

type Asset struct {
	ID    string    `json:"id"`
	Kind  string    `json:"kind"` // screenshot, copy, element, download
	Name  string    `json:"name"`
	Mime  string    `json:"mime,omitempty"`
	Size  int64     `json:"size"`
	Text  string    `json:"text,omitempty"`  // copy and element: the content
	URL   string    `json:"url,omitempty"`   // page it came from
	Title string    `json:"title,omitempty"` // title of that page
	By    string    `json:"by,omitempty"`    // who made it: a person's name or "agent"
	Time  time.Time `json:"time"`

	path  string // file on disk (screenshot, download); never sent to viewers
	owned bool   // the file lives in the assets folder and goes away with the asset
}

// assetFile is an Asset as saved in index.json (it also needs the private fields).
type assetFile struct {
	Asset
	Path  string `json:"path,omitempty"`
	Owned bool   `json:"owned,omitempty"`
}

type Assets struct {
	mu   sync.Mutex
	dir  string
	list []*Asset // oldest first
	hub  *Hub
}

func newAssets(dir string, h *Hub) *Assets {
	a := &Assets{dir: dir, hub: h}
	os.MkdirAll(dir, 0o700)
	if raw, err := os.ReadFile(filepath.Join(dir, "index.json")); err == nil {
		var saved []assetFile
		if json.Unmarshal(raw, &saved) == nil {
			for _, s := range saved {
				as := s.Asset
				as.path, as.owned = s.Path, s.Owned
				a.list = append(a.list, &as)
			}
		}
	}
	return a
}

// save writes the index; the caller holds a.mu.
func (a *Assets) save() {
	out := make([]assetFile, len(a.list))
	for i, as := range a.list {
		out[i] = assetFile{Asset: *as, Path: as.path, Owned: as.owned}
	}
	raw, err := json.Marshal(out)
	if err != nil {
		return
	}
	tmp := filepath.Join(a.dir, "index.json.tmp")
	if os.WriteFile(tmp, raw, 0o600) == nil {
		os.Rename(tmp, filepath.Join(a.dir, "index.json"))
	}
}

// add records an asset. data is the content of a file the assets folder should keep (a picture);
// without it, as.path may name an existing file (a download) or the asset is only text.
func (a *Assets) add(as Asset, data []byte) *Asset {
	if a == nil {
		return nil
	}
	as.ID = "as-" + newID()[:6]
	as.Time = time.Now()
	as.Text = cut(as.Text, maxAssetText)
	if data != nil {
		ext := ".bin"
		if exts, _ := mime.ExtensionsByType(as.Mime); len(exts) > 0 {
			ext = exts[0]
		}
		if as.Mime == "image/jpeg" {
			ext = ".jpg"
		}
		file := filepath.Join(a.dir, as.ID+ext)
		if err := os.WriteFile(file, data, 0o600); err != nil {
			return nil
		}
		as.path, as.owned, as.Size = file, true, int64(len(data))
	}
	a.mu.Lock()
	// Copying the same text twice in a row is one asset, not two.
	if as.Kind == "copy" && len(a.list) > 0 {
		if last := a.list[len(a.list)-1]; last.Kind == "copy" && last.Text == as.Text {
			a.mu.Unlock()
			return last
		}
	}
	p := &as
	a.list = append(a.list, p)
	for len(a.list) > maxAssets {
		a.drop(a.list[0])
		a.list = a.list[1:]
	}
	a.save()
	a.mu.Unlock()
	a.hub.assetsChanged(p)
	return p
}

// drop removes the file of an asset the assets folder owns.
func (a *Assets) drop(as *Asset) {
	if as.owned && as.path != "" {
		os.Remove(as.path)
	}
}

func (a *Assets) get(id string) *Asset {
	a.mu.Lock()
	defer a.mu.Unlock()
	for _, as := range a.list {
		if as.ID == id {
			c := *as
			return &c
		}
	}
	return nil
}

func (a *Assets) remove(id string) bool {
	a.mu.Lock()
	defer a.mu.Unlock()
	for i, as := range a.list {
		if as.ID == id {
			a.drop(as)
			a.list = append(a.list[:i], a.list[i+1:]...)
			a.save()
			return true
		}
	}
	return false
}

func (a *Assets) clear() {
	a.mu.Lock()
	defer a.mu.Unlock()
	for _, as := range a.list {
		a.drop(as)
	}
	a.list = nil
	a.save()
}

// snapshot is the list, newest first.
func (a *Assets) snapshot() []Asset {
	a.mu.Lock()
	defer a.mu.Unlock()
	out := make([]Asset, 0, len(a.list))
	for i := len(a.list) - 1; i >= 0; i-- {
		out = append(out, *a.list[i])
	}
	return out
}

// assetsChanged tells every viewer to reload the Assets tab. as is the new asset, if there is one.
func (h *Hub) assetsChanged(as *Asset) {
	h.mu.Lock()
	defer h.mu.Unlock()
	m := Msg{Type: "assets"}
	if as != nil {
		m.Text = as.Kind
		m.ID = as.ID
	}
	h.broadcast(m, nil)
}

// pageOf is the address and title of the tab that is on screen, for the "from" line of an asset.
func (b *Browser) pageOf() (string, string) {
	if b == nil {
		return "", ""
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.active == nil {
		return "", ""
	}
	title := b.active.docTitle
	if title == "" {
		title = b.active.title
	}
	return b.active.url, title
}

// Capture takes a picture of what is on screen now (the viewer's Capture button).
func (b *Browser) Capture() ([]byte, error) {
	b.mu.Lock()
	t := b.active
	b.mu.Unlock()
	if t == nil {
		return nil, errors.New("no tab is open")
	}
	return b.shot(t)
}

func registerAssetAPI(mux *http.ServeMux, h *Hub) {
	post := func(w http.ResponseWriter, r *http.Request) bool {
		if r.Method != http.MethodPost {
			http.Error(w, "POST only", http.StatusMethodNotAllowed)
			return false
		}
		return true
	}

	// GET /api/assets -> { assets: [...] } newest first
	mux.HandleFunc("/api/assets", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(map[string]any{"assets": h.assets.snapshot()})
	})

	// GET /api/assets/file?id=…[&download=1] -> the picture, the file, or the text of the asset
	mux.HandleFunc("/api/assets/file", func(w http.ResponseWriter, r *http.Request) {
		as := h.assets.get(r.URL.Query().Get("id"))
		if as == nil {
			http.Error(w, "no such asset", http.StatusNotFound)
			return
		}
		name := safeName(as.Name)
		if name == "" {
			name = as.ID
		}
		if r.URL.Query().Get("download") != "" {
			w.Header().Set("Content-Disposition", mime.FormatMediaType("attachment", map[string]string{"filename": name}))
		}
		w.Header().Set("X-Content-Type-Options", "nosniff")
		if as.path == "" { // text
			if filepath.Ext(name) == "" {
				name += map[string]string{"element": ".html"}[as.Kind]
				if filepath.Ext(name) == "" {
					name += ".txt"
				}
			}
			w.Header().Set("Content-Type", "text/plain; charset=utf-8")
			if r.URL.Query().Get("download") != "" {
				w.Header().Set("Content-Disposition", mime.FormatMediaType("attachment", map[string]string{"filename": name}))
			}
			w.Write([]byte(as.Text))
			return
		}
		// Only pictures are shown inline: a downloaded page must never run on our origin.
		if as.Kind != "screenshot" {
			w.Header().Set("Content-Type", "application/octet-stream")
			w.Header().Set("Content-Disposition", mime.FormatMediaType("attachment", map[string]string{"filename": name}))
		} else {
			w.Header().Set("Content-Type", as.Mime)
		}
		f, err := os.Open(as.path)
		if err != nil {
			http.Error(w, "the file is gone", http.StatusGone)
			return
		}
		defer f.Close()
		fi, err := f.Stat()
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		http.ServeContent(w, r, name, fi.ModTime(), f)
	})

	// POST /api/assets/delete?id=…
	mux.HandleFunc("/api/assets/delete", func(w http.ResponseWriter, r *http.Request) {
		if !post(w, r) {
			return
		}
		if !h.assets.remove(r.URL.Query().Get("id")) {
			http.Error(w, "no such asset", http.StatusNotFound)
			return
		}
		h.assetsChanged(nil)
		w.WriteHeader(http.StatusNoContent)
	})

	// POST /api/assets/clear
	mux.HandleFunc("/api/assets/clear", func(w http.ResponseWriter, r *http.Request) {
		if !post(w, r) {
			return
		}
		h.assets.clear()
		h.assetsChanged(nil)
		w.WriteHeader(http.StatusNoContent)
	})

	// POST /api/assets/capture?by=name -> takes a screenshot of the shared screen now
	mux.HandleFunc("/api/assets/capture", func(w http.ResponseWriter, r *http.Request) {
		if !post(w, r) {
			return
		}
		if h.browser == nil {
			http.Error(w, "no browser is running", http.StatusServiceUnavailable)
			return
		}
		data, err := h.browser.Capture()
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		u, title := h.browser.pageOf()
		by := strings.TrimSpace(cut(r.URL.Query().Get("by"), 40))
		as := h.assets.add(Asset{Kind: "screenshot", Name: shotName(), Mime: "image/jpeg", URL: u, Title: title, By: by}, data)
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(as)
	})
}

func shotName() string { return "screenshot-" + time.Now().Format("150405") + ".jpg" }

// addDownload lists a finished download. The file stays where Chrome saved it.
func (a *Assets) addDownload(dl *download) {
	if a == nil || dl == nil {
		return
	}
	var size int64
	if fi, err := os.Stat(dl.Path); err == nil {
		size = fi.Size()
	}
	a.add(Asset{Kind: "download", Name: dl.Name, Mime: mime.TypeByExtension(strings.ToLower(filepath.Ext(dl.Name))),
		Size: size, URL: dl.URL, By: "browser", Text: dl.Path, path: dl.Path}, nil)
}
