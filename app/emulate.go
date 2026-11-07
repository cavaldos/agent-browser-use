package main

// Device emulation: phone/tablet presets, screen size, pixel ratio, touch, user agent,
// locale, time zone, dark mode, geolocation and network speed. The settings are kept in
// Browser.emu and applied to every tab (also tabs opened later), because the shared
// screen is one size for everybody.

import (
	"fmt"
	"slices"
	"strings"

	"github.com/chromedp/cdproto/browser"
	"github.com/chromedp/cdproto/cdp"
	"github.com/chromedp/cdproto/emulation"
	"github.com/chromedp/cdproto/network"
	"github.com/chromedp/cdproto/page"
	"github.com/chromedp/chromedp"
)

type Geo struct{ Lat, Lon, Accuracy float64 }

// Emulation is the current set of overrides; the zero value means "plain desktop Chrome".
type Emulation struct {
	Device      string
	Width       int // 0 = keep the shared screen size
	Height      int
	Scale       float64 // device pixel ratio, 0 = 1
	Mobile      bool
	Touch       bool
	UserAgent   string // empty = Chrome's own
	Locale      string
	Timezone    string
	ColorScheme string // light, dark or empty
	Geo         *Geo
	Network     string // "", offline, slow-3g, fast-3g, 4g
}

type devicePreset struct {
	Name          string
	Width, Height int
	Scale         float64
	Mobile        bool
	UserAgent     string
}

var devicePresets = []devicePreset{
	{"iPhone 15", 393, 852, 3, true, "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1"},
	{"iPhone SE", 375, 667, 2, true, "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1"},
	{"iPad", 820, 1180, 2, true, "Mozilla/5.0 (iPad; CPU OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1"},
	{"Pixel 8", 412, 915, 2.625, true, "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36"},
	{"Galaxy S23", 360, 780, 3, true, "Mozilla/5.0 (Linux; Android 14; SM-S911B) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36"},
	{"Galaxy Tab S9", 800, 1280, 2, true, "Mozilla/5.0 (Linux; Android 14; SM-X710) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36"},
	{"Laptop", 1366, 768, 1, false, ""},
	{"Desktop HD", 1920, 1080, 1, false, ""},
}

func findPreset(name string) *devicePreset {
	key := strings.ToLower(strings.Join(strings.Fields(name), ""))
	for i, p := range devicePresets {
		if strings.ToLower(strings.ReplaceAll(p.Name, " ", "")) == key {
			return &devicePresets[i]
		}
	}
	return nil
}

func presetNames() string {
	names := make([]string, len(devicePresets))
	for i, p := range devicePresets {
		names[i] = p.Name
	}
	return strings.Join(names, ", ")
}

// networkProfiles: latency in ms, throughput in bytes per second.
var networkProfiles = map[string]struct{ latency, down, up float64 }{
	"slow-3g": {400, 50 * 1024, 50 * 1024},
	"fast-3g": {150, 180 * 1024, 84 * 1024},
	"4g":      {60, 1.5 * 1024 * 1024, 750 * 1024},
}

// EmulateOpts is what browser_emulate asks for; nil fields stay as they are.
type EmulateOpts struct {
	Device      string
	Width       *int
	Height      *int
	Scale       *float64
	Mobile      *bool
	Touch       *bool
	UserAgent   *string
	Locale      *string
	Timezone    *string
	ColorScheme *string
	Lat, Lon    *float64
	Accuracy    *float64
	Network     *string
}

func (o EmulateOpts) empty() bool {
	return o.Device == "" && o.Width == nil && o.Height == nil && o.Scale == nil && o.Mobile == nil && o.Touch == nil &&
		o.UserAgent == nil && o.Locale == nil && o.Timezone == nil && o.ColorScheme == nil && o.Lat == nil && o.Lon == nil && o.Network == nil
}

// merge applies opts on top of e. A device preset comes first, so single fields can adjust it.
func (e Emulation) merge(o EmulateOpts) (Emulation, error) {
	if d := strings.TrimSpace(o.Device); d != "" {
		if strings.EqualFold(d, "reset") || strings.EqualFold(d, "none") || strings.EqualFold(d, "desktop") {
			e = Emulation{}
		} else {
			p := findPreset(d)
			if p == nil {
				return e, fmt.Errorf("unknown device %q. Known devices: %s (or set width, height, scale, mobile yourself; \"reset\" turns emulation off)", d, presetNames())
			}
			e.Device, e.Width, e.Height, e.Scale, e.Mobile, e.Touch, e.UserAgent = p.Name, p.Width, p.Height, p.Scale, p.Mobile, p.Mobile, p.UserAgent
		}
	}
	if o.Width != nil {
		e.Width = *o.Width
	}
	if o.Height != nil {
		e.Height = *o.Height
	}
	if (o.Width != nil || o.Height != nil || o.Scale != nil || o.Mobile != nil) && o.Device == "" {
		e.Device = "custom"
	}
	if o.Scale != nil {
		e.Scale = *o.Scale
	}
	if o.Mobile != nil {
		e.Mobile = *o.Mobile
		if o.Touch == nil {
			e.Touch = *o.Mobile
		}
	}
	if o.Touch != nil {
		e.Touch = *o.Touch
	}
	if o.UserAgent != nil {
		e.UserAgent = *o.UserAgent
	}
	if o.Locale != nil {
		e.Locale = *o.Locale
	}
	if o.Timezone != nil {
		e.Timezone = *o.Timezone
	}
	if o.ColorScheme != nil {
		switch c := strings.ToLower(*o.ColorScheme); c {
		case "light", "dark":
			e.ColorScheme = c
		case "", "none", "no-preference", "default":
			e.ColorScheme = ""
		default:
			return e, fmt.Errorf("colorScheme must be light, dark or no-preference, not %q", *o.ColorScheme)
		}
	}
	if o.Lat != nil || o.Lon != nil {
		if o.Lat == nil || o.Lon == nil {
			return e, fmt.Errorf("give both latitude and longitude")
		}
		if *o.Lat < -90 || *o.Lat > 90 || *o.Lon < -180 || *o.Lon > 180 {
			return e, fmt.Errorf("latitude must be -90..90 and longitude -180..180")
		}
		acc := 100.0
		if o.Accuracy != nil {
			acc = *o.Accuracy
		}
		e.Geo = &Geo{*o.Lat, *o.Lon, acc}
	}
	if o.Network != nil {
		n := strings.ToLower(*o.Network)
		if _, ok := networkProfiles[n]; !ok && !slices.Contains([]string{"", "online", "offline", "none"}, n) {
			return e, fmt.Errorf("network must be online, offline, slow-3g, fast-3g or 4g, not %q", *o.Network)
		}
		if n == "online" || n == "none" {
			n = ""
		}
		e.Network = n
	}
	if e.Scale < 0 || e.Scale > 5 {
		return e, fmt.Errorf("scale (device pixel ratio) must be between 0 and 5")
	}
	if (e.Width != 0 && (e.Width < 200 || e.Width > 2560)) || (e.Height != 0 && (e.Height < 200 || e.Height > 2560)) {
		return e, fmt.Errorf("width and height must be between 200 and 2560")
	}
	if (e.Width == 0) != (e.Height == 0) {
		return e, fmt.Errorf("give width and height together")
	}
	return e, nil
}

func (e Emulation) String() string {
	if e == (Emulation{}) {
		return "No emulation (normal desktop Chrome)."
	}
	var p []string
	p = append(p, "Device: "+cmpOr(e.Device, "-"))
	if e.Width > 0 {
		p = append(p, fmt.Sprintf("Screen: %d×%d @%gx", e.Width, e.Height, max(e.Scale, 1)))
	}
	p = append(p, fmt.Sprintf("Mobile: %t, touch: %t", e.Mobile, e.Touch))
	if e.UserAgent != "" {
		p = append(p, "User agent: "+e.UserAgent)
	}
	if e.Locale != "" {
		p = append(p, "Locale: "+e.Locale)
	}
	if e.Timezone != "" {
		p = append(p, "Time zone: "+e.Timezone)
	}
	if e.ColorScheme != "" {
		p = append(p, "Color scheme: "+e.ColorScheme)
	}
	if e.Geo != nil {
		p = append(p, fmt.Sprintf("Location: %.5f, %.5f (±%gm)", e.Geo.Lat, e.Geo.Lon, e.Geo.Accuracy))
	}
	if e.Network != "" {
		p = append(p, "Network: "+e.Network)
	}
	return "### Emulation\n- " + strings.Join(p, "\n- ") + "\n"
}

// metrics is the screen override for a tab: the shared screen size, unless a device sets its own.
func metrics(v View, e Emulation) emulation.SetDeviceMetricsOverrideParams {
	p := emulation.SetDeviceMetricsOverrideParams{Width: int64(v.Width), Height: int64(v.Height), DeviceScaleFactor: 1, Mobile: e.Mobile}
	if e.Scale > 0 {
		p.DeviceScaleFactor = e.Scale
	}
	return p
}

// applyEmulation sets every override of e on one tab. Errors of single settings are ignored
// on purpose: a setting Chrome refuses must not stop the others.
func (b *Browser) applyEmulation(t *Tab, v View, e Emulation) error {
	ctx := t.ctx
	if _, err := chromedp.Call(ctx, emulation.SetDeviceMetricsOverride, metrics(v, e)); err != nil {
		return err
	}
	chromedp.Call(ctx, emulation.SetTouchEmulationEnabled, emulation.SetTouchEmulationEnabledParams{Enabled: e.Touch, MaxTouchPoints: 5})
	ua := b.ua
	if e.UserAgent != "" {
		ua = e.UserAgent
	}
	if ua != "" {
		up := emulation.SetUserAgentOverrideParams{UserAgent: ua}
		if e.Locale != "" {
			up.AcceptLanguage = strings.ReplaceAll(e.Locale, "_", "-")
		}
		chromedp.Call(ctx, emulation.SetUserAgentOverride, up)
	}
	chromedp.Call(ctx, emulation.SetLocaleOverride, emulation.SetLocaleOverrideParams{Locale: e.Locale})
	chromedp.Call(ctx, emulation.SetTimezoneOverride, emulation.SetTimezoneOverrideParams{TimezoneID: e.Timezone})
	var feats []*emulation.MediaFeature
	if e.ColorScheme != "" {
		feats = append(feats, &emulation.MediaFeature{Name: "prefers-color-scheme", Value: e.ColorScheme})
	}
	chromedp.Call(ctx, emulation.SetEmulatedMedia, emulation.SetEmulatedMediaParams{Features: feats})
	if e.Geo != nil {
		chromedp.Call(ctx, emulation.SetGeolocationOverride, emulation.SetGeolocationOverrideParams{Latitude: &e.Geo.Lat, Longitude: &e.Geo.Lon, Accuracy: &e.Geo.Accuracy})
	} else {
		chromedp.Call(ctx, emulation.ClearGeolocationOverride, cdp.Empty{})
	}
	np := network.OverrideNetworkStateParams{Offline: e.Network == "offline", Latency: 0, DownloadThroughput: -1, UploadThroughput: -1}
	if pr, ok := networkProfiles[e.Network]; ok {
		np.Latency, np.DownloadThroughput, np.UploadThroughput = pr.latency, pr.down, pr.up
	}
	chromedp.Call(ctx, network.OverrideNetworkState, np)
	return nil
}

// SetEmulation applies new emulation settings to all tabs and resizes the shared screen to match.
// It runs from the command queue and waits for the result.
func (b *Browser) SetEmulation(o EmulateOpts) (Emulation, error) {
	var result Emulation
	err := b.call(func() error {
		b.mu.Lock()
		e, err := b.emu.merge(o)
		if err != nil {
			b.mu.Unlock()
			return err
		}
		v := b.view
		if e.Width > 0 {
			v.Width, v.Height = e.Width, e.Height
		} else {
			v.Width, v.Height = b.opt.Width, b.opt.Height
		}
		b.emu, b.view = e, v
		tabs := slices.Clone(b.tabs)
		active := b.active
		b.gotFrame = false
		b.mu.Unlock()
		result = e

		if e.Geo != nil { // without permission a page's geolocation request is just refused
			chromedp.CallBrowser(b.root, browser.SetPermission, browser.SetPermissionParams{
				Permission: &browser.PermissionDescriptor{Name: "geolocation"}, Setting: browser.PermissionSettingGranted})
		}
		for _, t := range tabs {
			b.applyEmulation(t, v, e)
		}
		if active != nil {
			chromedp.Call(active.ctx, page.StopScreencast, cdp.Empty{})
			b.startScreencast(active, v)
		}
		b.hub.broadcastView(v)
		return nil
	})
	return result, err
}

func (b *Browser) Emulation() Emulation {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.emu
}
