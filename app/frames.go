package main

// Cross-origin iframes.
//
// A page from another site (a Stripe checkout form, an embedded widget) has no
// contentDocument from the top page, so the page helper (agent.js) can't see
// inside it: no refs, no values, no select options.
//
// Reading it works the way Playwright does it: Page.getFrameTree lists every
// frame, cross-origin included, and Page.createIsolatedWorld gives each frame an
// execution context we can evaluate in. GrantUniversalAccess lifts the same-origin
// check inside that world, so the iframe's DOM is readable even though the page
// is not ours. Nothing about Chrome's process model has to change.
//
// Refs from a frame are prefixed with "f<N>" (f1e12 = ref e12 in iframe 1), so a
// single snapshot covers the page and every iframe in it.

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"regexp"
	"strconv"
	"strings"
	"sync"

	"github.com/chromedp/cdproto/cdp"
	"github.com/chromedp/cdproto/page"
	"github.com/chromedp/cdproto/runtime"
	"github.com/chromedp/chromedp"
)

// frameRef is a parsed ref: which frame it lives in, and the ref inside it.
type frameRef struct {
	Frame int    // 0 = the main frame, 1.. = iframe 1.. as listed
	Rest  string // the ref inside that frame ("e12"); for frame 0 the whole ref
}

// parseFrameRef splits "f1e12" into {1, "e12"}. A plain "e12" means the main frame.
func parseFrameRef(ref string) frameRef {
	if len(ref) > 2 && ref[0] == 'f' {
		i := 1
		for i < len(ref) && ref[i] >= '0' && ref[i] <= '9' {
			i++
		}
		if i > 1 && i < len(ref) {
			if n, err := strconv.Atoi(ref[1:i]); err == nil && n > 0 {
				return frameRef{Frame: n, Rest: ref[i:]}
			}
		}
	}
	return frameRef{Frame: 0, Rest: ref}
}

// childFrame is one iframe of the page: its id, its URL, and the iframe element
// that hosts it in the parent document (used to place clicks).
type childFrame struct {
	FrameID cdp.FrameID
	URL     string
	// Parent is the frame that embeds it; for a top-level iframe that is the main frame.
	Parent cdp.FrameID
}

// childFrames lists every sub-frame of the tab's main frame, in document order,
// flattened depth-first. The 1-based position is the N in f<N>.
func childFrames(ctx context.Context) ([]childFrame, cdp.FrameID, error) {
	// Page.enable makes the frame tree live: without it a frame added by script
	// after load never shows up as a child.
	if _, err := chromedp.Call(ctx, page.Enable, page.EnableParams{}); err != nil {
		return nil, "", err
	}
	res, err := chromedp.Call(ctx, page.GetFrameTree, cdp.Empty{})
	if err != nil {
		return nil, "", err
	}
	tree := res.FrameTree
	mainFrame := tree.Frame.ID
	var out []childFrame
	var walk func(n *page.FrameTree, parent cdp.FrameID)
	walk = func(n *page.FrameTree, parent cdp.FrameID) {
		if n == nil || n.Frame == nil {
			return
		}
		if n.Frame.ID != mainFrame {
			out = append(out, childFrame{FrameID: n.Frame.ID, URL: n.Frame.URL, Parent: parent})
		}
		for _, c := range n.ChildFrames {
			walk(c, n.Frame.ID)
		}
	}
	walk(tree, mainFrame)
	return out, mainFrame, nil
}

// frameCtx creates an isolated world inside frame id and returns its execution
// context, so JavaScript runs against that frame's DOM.
func frameWorld(ctx context.Context, id cdp.FrameID) (runtime.ExecutionContextID, error) {
	yes := true
	res, err := chromedp.Call(ctx, page.CreateIsolatedWorld, page.CreateIsolatedWorldParams{
		FrameID:             id,
		WorldName:           "__abu_frame",
		GrantUniveralAccess: &yes,
	})
	if err != nil {
		return 0, err
	}
	return res.ExecutionContextID, nil
}

// evalInWorld evaluates expr in an isolated world of a frame, returning the
// result as JSON text.
func evalInWorld(ctx context.Context, worldID runtime.ExecutionContextID, expr string) (json.RawMessage, error) {
	yes := true
	res, err := chromedp.Call(ctx, runtime.Evaluate, runtime.EvaluateParams{
		Expression:    expr,
		ContextID:     worldID,
		AwaitPromise:  &yes,
		ReturnByValue: &yes,
	})
	if err != nil {
		return nil, err
	}
	if res.ExceptionDetails != nil {
		msg := res.ExceptionDetails.Text
		if res.ExceptionDetails.Exception != nil && res.ExceptionDetails.Exception.Description != "" {
			msg = res.ExceptionDetails.Exception.Description
		}
		if i := strings.Index(msg, "\n    at "); i > 0 {
			msg = msg[:i]
		}
		return nil, fmt.Errorf("%s", strings.TrimPrefix(msg, "Error: "))
	}
	if res.Result == nil {
		return json.RawMessage("null"), nil
	}
	if len(res.Result.Value) > 0 {
		return json.RawMessage(res.Result.Value), nil
	}
	return json.RawMessage("null"), nil
}

// evalFrameJSON evaluates expr in frame n's world and decodes the JSON result.
func evalFrameJSON[T any](ctx context.Context, n int, expr string) (T, error) {
	var zero T
	frames, _, err := childFrames(ctx)
	if err != nil {
		return zero, err
	}
	if n < 1 || n > len(frames) {
		return zero, fmt.Errorf("iframe f%d not found: the page has %d iframe(s). Take a new snapshot (browser_snapshot) and use a ref from it", n, len(frames))
	}
	world, err := frameWorld(ctx, frames[n-1].FrameID)
	if err != nil {
		return zero, fmt.Errorf("open iframe f%d: %w", n, err)
	}
	raw, err := evalInWorld(ctx, world, expr)
	if err != nil {
		return zero, err
	}
	if err := json.Unmarshal(raw, &zero); err != nil {
		return zero, fmt.Errorf("decode iframe f%d result: %w", n, err)
	}
	return zero, nil
}

// frameDoc is one frame's snapshot.
type frameDoc struct {
	N     int
	URL   string
	Title string
	Body  string
	Err   string
}

type frameSnapOut struct {
	URL   string `json:"url"`
	Title string `json:"title"`
	Body  string `json:"body"`
}

// frameSnapExpr installs the page helper in the frame's world and snapshots it.
func frameSnapExpr() string {
	return agentJS + "\n;(async () => ({ url: location.href, title: document.title, body: __abu.snapshot() }))()"
}

// frameDocs snapshots every iframe of the tab. The main frame's body is passed
// in by the caller (it already has it).
// framesWarned keeps the site-isolation warning to once per process: it is a
// startup-configuration problem, not something that changes per page.
var framesWarned sync.Once

func (a *Agent) frameDocs(t *Tab, mainBody string) []frameDoc {
	docs := []frameDoc{{N: 0, Body: mainBody}}
	frames, _, err := childFrames(t.ctx)
	if err != nil || len(frames) == 0 {
		// No child frames is normal on a page with none, and a symptom of site
		// isolation when the page does have them: with --site-per-process a
		// cross-origin iframe runs in its own process and getFrameTree stops
		// listing it, so iframe support silently does nothing. Say so once,
		// instead of leaving someone to wonder why nothing inside an iframe works.
		if err == nil && pageHasDOMIframes(t) {
			framesWarned.Do(func() {
				log.Printf("[frames] the page has iframes but Page.getFrameTree lists none. " +
					"This happens when Chrome runs with site isolation (--site-per-process); " +
					"cross-origin iframes then cannot be read or driven. Keep " +
					"--disable-site-isolation-trials in chromeFlags (see frames.go).")
			})
		}
		return docs
	}
	for i, f := range frames {
		d := frameDoc{N: i + 1, URL: f.URL}
		func() {
			defer func() {
				if r := recover(); r != nil {
					d.Err = fmt.Sprint(r)
				}
				docs = append(docs, d)
			}()
			out, err := evalFrameJSON[frameSnapOut](t.ctx, i+1, frameSnapExpr())
			if err != nil {
				d.Err = err.Error()
				return
			}
			d.URL, d.Title, d.Body = out.URL, out.Title, out.Body
		}()
	}
	return docs
}

// ---------- Coordinates ----------

// pageHasDOMIframes reports whether the main document holds any iframe element,
// which is how we tell "no frames" apart from "frames we cannot see".
func pageHasDOMIframes(t *Tab) bool {
	n, err := evalMainJSON[int](t, `document.querySelectorAll("iframe,frame").length`)
	return err == nil && n > 0
}

// iframeOffset is an iframe element's position inside its parent document.
type iframeOffset struct {
	X float64 `json:"x"`
	Y float64 `json:"y"`
	W float64 `json:"w"`
	H float64 `json:"h"`
}

// frameOffsetExpr measures, in the current document, the iframe element shown at
// the given URL. Matching the src is what works across origins: the element is
// readable even when its document is not.
func frameOffsetExpr(url string) string {
	return `(() => {
      const want = ` + jsq(url) + `;
      const base = want.split("#")[0];
      const list = [...document.querySelectorAll("iframe,frame")];
      const f = list.find((e) => e.src === want) ||
                list.find((e) => e.src && base && (base.startsWith(e.src.split("#")[0]) || e.src.split("#")[0].startsWith(base.slice(0, 80))));
      if (!f) return null;
      const r = f.getBoundingClientRect();
      return { x: r.left, y: r.top, w: r.width, h: r.height };
    })()`
}

// frameOrigin returns where iframe n sits on the page, summing every nesting
// level: coordinates measured inside the iframe are relative to it.
func frameOrigin(t *Tab, n int) (iframeOffset, error) {
	var out iframeOffset
	frames, main, err := childFrames(t.ctx)
	if err != nil {
		return out, err
	}
	_ = main
	if n < 1 || n > len(frames) {
		return out, fmt.Errorf("iframe f%d not found", n)
	}
	// Chain from the main frame down to frame n.
	var chain []childFrame
	cur := frames[n-1]
	byID := map[cdp.FrameID]childFrame{}
	for _, f := range frames {
		byID[f.FrameID] = f
	}
	for guard := 0; guard < 32; guard++ {
		chain = append([]childFrame{cur}, chain...)
		if cur.Parent == main {
			break
		}
		parent, ok := byID[cur.Parent]
		if !ok {
			break
		}
		cur = parent
	}
	// Measure each iframe element inside its own parent document.
	for i, f := range chain {
		var off iframeOffset
		var oerr error
		if i == 0 {
			off, oerr = evalMainJSON[iframeOffset](t, frameOffsetExpr(f.URL))
		} else {
			off, oerr = evalFrameJSON[iframeOffset](t.ctx, frameIndexOf(frames, chain[i-1].FrameID), frameOffsetExpr(f.URL))
		}
		if oerr != nil {
			return out, fmt.Errorf("locate iframe f%d: %w", frameIndexOf(frames, f.FrameID), oerr)
		}
		out.X += off.X
		out.Y += off.Y
		if i == len(chain)-1 {
			out.W, out.H = off.W, off.H
		}
	}
	return out, nil
}

// evalMainJSON evaluates expr in the main frame and decodes the result.
func evalMainJSON[T any](t *Tab, expr string) (T, error) {
	var zero T
	v, err := chromedp.Run(t.ctx, chromedp.Evaluate[T](expr, chromedp.EvalAwaitPromise))
	if err != nil {
		return zero, jsError(err)
	}
	return v, nil
}

func frameIndexOf(frames []childFrame, id cdp.FrameID) int {
	for i := range frames {
		if frames[i].FrameID == id {
			return i + 1
		}
	}
	return 0
}

// ---------- Outline ----------

// joinFrameOutlines merges the page outline with the outline of every iframe.
// Refs inside iframe N are rewritten from "e12" to "fNe12", which is what
// Click/Type/SelectOption/Evaluate understand.
// maxFrameURL caps the URL in a frame header. Stripe-style iframes carry their
// whole configuration in the query string, which is noise in the outline: the
// origin plus the start of the path is enough to tell frames apart.
const maxFrameURL = 80

// frameURLShort keeps the scheme and host, then as much of the path as fits,
// and drops the query and fragment.
func frameURLShort(u string) string {
	if i := strings.IndexAny(u, "?#"); i >= 0 {
		u = u[:i]
	}
	if len(u) <= maxFrameURL {
		return u
	}
	return u[:maxFrameURL-1] + "…"
}

// maxFrameOutlines caps how many iframes get an outline in one snapshot. Pages
// with ad slots can carry a dozen or more; the rest are summarized in one line
// so the model still knows they exist.
const maxFrameOutlines = 10

func joinFrameOutlines(docs []frameDoc) string {
	var b strings.Builder
	b.WriteString(docs[0].Body)
	shown, skipped := 0, 0
	for _, d := range docs[1:] {
		if d.Err != "" || strings.TrimSpace(d.Body) == "" {
			continue
		}
		if shown >= maxFrameOutlines {
			skipped++
			continue
		}
		prefix := "f" + strconv.Itoa(d.N)
		fmt.Fprintf(&b, "\n\n--- iframe [%s] %s ---\n", prefix, frameURLShort(d.URL))
		b.WriteString(prefixFrameRefs(d.Body, prefix))
		shown++
	}
	for _, d := range docs[1:] {
		if d.Err != "" {
			fmt.Fprintf(&b, "\n\n--- iframe [f%d] unreadable: %s ---\n", d.N, d.Err)
		}
	}
	if skipped > 0 {
		fmt.Fprintf(&b, "\n\n(%d more iframe(s) with content not shown: only the first %d are listed, to keep this outline readable.)\n",
			skipped, maxFrameOutlines)
	}
	return b.String()
}

var refInText = regexp.MustCompile(`\[ref=([A-Za-z]*\d+)\]`)

func prefixFrameRefs(body, prefix string) string {
	return refInText.ReplaceAllString(body, "[ref="+prefix+"$1]")
}

// ---------- Driving a dropdown ----------

// pickOptionExpr drives a dropdown inside a frame. A real <select> is set
// directly; a custom combobox (Stripe's country picker, most design systems) is
// opened and its option clicked, because no native popup is painted in headless.
func pickOptionExpr(ref string, values []string) string {
	vals, _ := json.Marshal(values)
	return framePickJS + "\n;__abuFrame.pickOption(" + jsq(ref) + ", " + string(vals) + ")"
}

const framePickJS = `(() => {
  if (window.__abuFrame) return;
  const clean = (s) => (s || "").replace(/\s+/g, " ").trim();
  const visible = (el) => {
    const r = el.getBoundingClientRect();
    if (r.width === 0 && r.height === 0) return false;
    const st = getComputedStyle(el);
    return st.visibility !== "hidden" && st.display !== "none";
  };
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const optionEls = () => [...document.querySelectorAll('[role="option"],[role="menuitem"],li[data-value],option')].filter(visible);
  function pickOption(ref, values) {
    return (async () => {
      let el = null;
      try { el = window.__abu && window.__abu.get ? window.__abu.get(ref) : null; } catch (e) { el = null; }
      if (!el) el = document.querySelector('[role="combobox"],[aria-haspopup="listbox"],select');
      if (!el) throw new Error("no dropdown found in this iframe");
      el.scrollIntoView({ block: "center", inline: "center" });

      if (el.tagName === "SELECT") {
        const picked = [];
        for (const o of [...el.options]) {
          if (values.some((v) => v === o.value || v === o.label || clean(o.textContent) === v)) {
            if (!picked.length) el.value = o.value;
            picked.push(clean(o.textContent || o.label));
          }
        }
        if (!picked.length) throw new Error("no option matches " + values.join(", "));
        el.dispatchEvent(new Event("input", { bubbles: true }));
        el.dispatchEvent(new Event("change", { bubbles: true }));
        return picked;
      }

      el.focus();
      el.click();
      let list = optionEls();
      for (let i = 0; i < 30 && !list.length; i++) { await wait(50); list = optionEls(); }
      const want = (t) => values.some((v) => clean(t) === v || clean(t).toLowerCase() === v.toLowerCase());
      let target = list.find((o) => want(o.textContent));
      if (!target) {
        const inp = el.tagName === "INPUT" ? el : (el.querySelector && el.querySelector("input")) || document.querySelector('input[role="combobox"]');
        if (inp) {
          for (const v of values) {
            inp.focus();
            inp.value = v;
            inp.dispatchEvent(new Event("input", { bubbles: true }));
            await wait(200);
            list = optionEls();
            target = list.find((o) => want(o.textContent));
            if (target) break;
          }
        }
      }
      if (!target) {
        const seen = list.map((o) => clean(o.textContent)).slice(0, 30);
        throw new Error("no option matches " + values.join(", ") + (seen.length ? ". Options: " + seen.join(" | ") : ". The dropdown did not open"));
      }
      target.scrollIntoView({ block: "nearest" });
      target.click();
      await wait(60);
      return [clean(target.textContent)];
    })();
  }
  Object.defineProperty(window, "__abuFrame", { value: { pickOption, optionEls }, configurable: true });
})()`

// evalInFrameAs runs expr in iframe n's world and decodes the JSON result into T.
// It is the frame equivalent of evalJS: the page helper is installed there first,
// so __abu.* works with that frame's own refs.
func evalInFrameAs[T any](ctx context.Context, n int, expr string) (T, error) {
	var zero T
	frames, _, err := childFrames(ctx)
	if err != nil {
		return zero, err
	}
	if n < 1 || n > len(frames) {
		return zero, fmt.Errorf("iframe f%d not found: the page has %d iframe(s). Take a new snapshot (browser_snapshot) and use a ref from it", n, len(frames))
	}
	world, err := frameWorld(ctx, frames[n-1].FrameID)
	if err != nil {
		return zero, fmt.Errorf("open iframe f%d: %w", n, err)
	}
	raw, err := evalInWorld(ctx, world, agentJS+"\n;"+expr)
	if err != nil {
		return zero, err
	}
	if err := json.Unmarshal(raw, &zero); err != nil {
		return zero, fmt.Errorf("decode iframe f%d result: %w", n, err)
	}
	return zero, nil
}
