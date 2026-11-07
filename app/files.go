package main

// Files that go in and out of the shared browser.
//   - Downloads: Chrome saves them into the download folder; we track progress so the
//     agent (browser_downloads) can wait for a file and read it.
//   - Uploads: browser_file_upload only reads from the upload and download folders, so an
//     agent cannot hand a page any other file of the machine.

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/chromedp/cdproto/browser"
)

type download struct {
	GUID     string
	URL      string
	Name     string // suggested file name
	Path     string
	State    string // inProgress, completed, canceled
	Received float64
	Total    float64
	Started  time.Time
}

type downloads struct {
	mu     sync.Mutex
	list   []*download
	byGUID map[string]*download
}

func newDownloads() *downloads { return &downloads{byGUID: map[string]*download{}} }

// event handles Chrome's download events; the return value is set once when a download completes.
func (d *downloads) event(ev any, dir string) *download {
	d.mu.Lock()
	defer d.mu.Unlock()
	switch e := ev.(type) {
	case *browser.EventDownloadWillBegin:
		dl := &download{GUID: e.GUID, URL: e.URL, Name: filepath.Base(e.SuggestedFilename), State: "inProgress", Started: time.Now()}
		d.list = append(d.list, dl)
		d.byGUID[e.GUID] = dl
		if len(d.list) > 100 {
			delete(d.byGUID, d.list[0].GUID)
			d.list = d.list[1:]
		}
	case *browser.EventDownloadProgress:
		dl := d.byGUID[e.GUID]
		if dl == nil {
			return nil
		}
		was := dl.State
		dl.Received, dl.Total, dl.State = e.ReceivedBytes, e.TotalBytes, string(e.State)
		if e.FilePath != "" {
			dl.Path = e.FilePath
		}
		if dl.State == "completed" && was != "completed" {
			if dl.Path == "" {
				dl.Path = filepath.Join(dir, dl.Name)
			}
			return dl
		}
	}
	return nil
}

func (d *downloads) snapshot() []download {
	d.mu.Lock()
	defer d.mu.Unlock()
	out := make([]download, len(d.list))
	for i, dl := range d.list {
		out[i] = *dl
	}
	return out
}

func (dl download) line(i int) string {
	switch dl.State {
	case "completed":
		fi, err := os.Stat(dl.Path)
		size := ""
		if err == nil {
			size = ", " + humanSize(fi.Size())
		}
		return fmt.Sprintf("- #%d %s: done%s → %s\n  from %s", i, dl.Name, size, dl.Path, shortURL(dl.URL))
	case "canceled":
		return fmt.Sprintf("- #%d %s: canceled (from %s)", i, dl.Name, shortURL(dl.URL))
	}
	prog := humanSize(int64(dl.Received))
	if dl.Total > 0 {
		prog += fmt.Sprintf(" of %s", humanSize(int64(dl.Total)))
	}
	return fmt.Sprintf("- #%d %s: downloading (%s) from %s", i, dl.Name, prog, shortURL(dl.URL))
}

// within reports whether p (after resolving links) lies inside root.
func within(root, p string) bool {
	r, err := filepath.EvalSymlinks(root)
	if err != nil {
		return false
	}
	q, err := filepath.EvalSymlinks(p)
	if err != nil {
		return false
	}
	rel, err := filepath.Rel(r, q)
	return err == nil && rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator)) && !filepath.IsAbs(rel)
}

// uploadPaths checks the paths an agent wants to upload: existing files inside the upload or download folder.
func (b *Browser) uploadPaths(paths []string) ([]string, error) {
	var out []string
	for _, p := range paths {
		if !filepath.IsAbs(p) { // a bare name means a file in the upload folder, then the download folder
			cand := []string{filepath.Join(b.opt.UploadDir, p), filepath.Join(b.opt.DownloadDir, p)}
			i := slices.IndexFunc(cand, func(c string) bool { _, err := os.Stat(c); return err == nil })
			if i < 0 {
				return nil, fmt.Errorf("file %q not found in the upload folder %s or the download folder %s", p, b.opt.UploadDir, b.opt.DownloadDir)
			}
			p = cand[i]
		}
		fi, err := os.Stat(p)
		if err != nil || fi.IsDir() {
			return nil, fmt.Errorf("file %q does not exist", p)
		}
		if !within(b.opt.UploadDir, p) && !within(b.opt.DownloadDir, p) {
			return nil, fmt.Errorf("%q is outside the upload folder (%s) and the download folder (%s). Put the file there first", p, b.opt.UploadDir, b.opt.DownloadDir)
		}
		out = append(out, p)
	}
	if len(out) == 0 {
		return nil, errors.New("paths is required: file names in the upload folder, or full paths inside it")
	}
	return out, nil
}

// safeName reduces a name to a plain file name.
func safeName(name string) string {
	name = filepath.Base(strings.ReplaceAll(strings.TrimSpace(name), "\\", "/"))
	if name == "." || name == "/" || name == ".." {
		return ""
	}
	return name
}
