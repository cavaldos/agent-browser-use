package main

// The viewer's "Pick" tool: a person points at an element in the shared browser and gets a short
// code (e.g. el-3f9a). They paste it to a coding agent, which reads the element with
// browser_get_element and can then act on it by ref.

import (
	"fmt"
	"strings"
)

type Pick struct {
	Code     string  `json:"code,omitempty"`
	Ref      string  `json:"ref"`
	Tag      string  `json:"tag"`
	Role     string  `json:"role,omitempty"`
	Name     string  `json:"name,omitempty"`
	Selector string  `json:"selector"`
	Text     string  `json:"text,omitempty"`
	HTML     string  `json:"html,omitempty"`
	Rect     pickBox `json:"rect"`
	URL      string  `json:"url,omitempty"`
	Title    string  `json:"title,omitempty"`
	View     *View   `json:"view,omitempty"` // size the rect is relative to
}

type pickBox struct {
	X float64 `json:"x"`
	Y float64 `json:"y"`
	W float64 `json:"w"`
	H float64 `json:"h"`
}

const maxPicks = 50

// pick answers a viewer's hover or click: it finds the element under the point.
func (h *Hub) pick(c *Client, m Msg) {
	b := h.browser
	tabs, active, _, _ := b.State()
	b.mu.Lock()
	t, v := b.active, b.view
	b.mu.Unlock()
	if t == nil {
		return
	}
	x, y := clamp01(m.X)*float64(v.Width), clamp01(m.Y)*float64(v.Height)
	p, err := evalJS[*Pick](t, fmt.Sprintf("__abu.pick(%v, %v)", x, y))
	if err != nil {
		return
	}
	if p == nil {
		h.sendTo(c, Msg{Type: "pick", Event: m.Event})
		return
	}
	p.View = &v
	if m.Event == "click" {
		for _, ti := range tabs {
			if ti.ID == active {
				p.URL, p.Title = ti.URL, ti.Title
			}
		}
		p.Code = h.savePick(p)
	} else {
		p = &Pick{Rect: p.Rect, View: &v, Tag: p.Tag, Role: p.Role, Name: p.Name}
	}
	h.sendTo(c, Msg{Type: "pick", Event: m.Event, Pick: p})
}

func (h *Hub) savePick(p *Pick) string {
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.picks == nil {
		h.picks = map[string]*Pick{}
	}
	code := ""
	for {
		code = "el-" + newID()[:4]
		if h.picks[code] == nil {
			break
		}
	}
	h.picks[code] = p
	h.pickOrder = append(h.pickOrder, code)
	if len(h.pickOrder) > maxPicks {
		delete(h.picks, h.pickOrder[0])
		h.pickOrder = h.pickOrder[1:]
	}
	return code
}

// GetPicked describes an element a person picked, and gives the agent a fresh ref for it.
func (a *Agent) GetPicked(code string) (string, error) {
	code = strings.ToLower(strings.TrimSpace(strings.TrimPrefix(strings.TrimSpace(code), "@")))
	a.hub.mu.Lock()
	old := a.hub.picks[code]
	a.hub.mu.Unlock()
	if old == nil {
		return "", fmt.Errorf("no picked element with code %q (the server keeps the last %d). Ask the person to pick it again in the viewer", code, maxPicks)
	}
	var p *Pick
	err := a.look(func(t *Tab) error {
		var err error
		p, err = evalJS[*Pick](t, fmt.Sprintf("__abu.resolve(%s, %s)", jsq(old.Ref), jsq(old.Selector)))
		return err
	})
	if err != nil {
		return "", err
	}
	var s strings.Builder
	fmt.Fprintf(&s, "### Picked element %s\n", code)
	if old.URL != "" {
		fmt.Fprintf(&s, "- Page: %s (%s)\n", old.URL, old.Title)
	}
	fmt.Fprintf(&s, "- Ref: %s (use it with browser_click, browser_type, browser_hover…)\n", p.Ref)
	fmt.Fprintf(&s, "- Tag: <%s>%s\n", p.Tag, map[bool]string{true: ", role " + p.Role}[p.Role != ""])
	if p.Name != "" {
		fmt.Fprintf(&s, "- Name: %s\n", quote(p.Name))
	}
	fmt.Fprintf(&s, "- CSS selector: %s\n", p.Selector)
	fmt.Fprintf(&s, "- Position: x=%.0f y=%.0f, %.0f×%.0f px\n", p.Rect.X, p.Rect.Y, p.Rect.W, p.Rect.H)
	if p.Text != "" {
		fmt.Fprintf(&s, "- Text: %s\n", quote(p.Text))
	}
	fmt.Fprintf(&s, "\n```html\n%s\n```\n", p.HTML)
	return s.String(), nil
}
