package main

import (
	"strings"
	"testing"
)

func TestParseFrameRef(t *testing.T) {
	cases := []struct {
		ref       string
		wantFrame int
		wantRest  string
	}{
		// Refs from the page itself have no frame prefix.
		{"e12", 0, "e12"},
		{"e1", 0, "e1"},
		{"", 0, ""},
		// A ref inside iframe N.
		{"f1e12", 1, "e12"},
		{"f1e1", 1, "e1"},
		{"f2e3", 2, "e3"},
		{"f10e4", 10, "e4"},
		{"f123e999", 123, "e999"},
		// A frame number with no ref after it is not a frame ref: keep it whole.
		{"f1", 0, "f1"},
		{"f12", 0, "f12"},
		// "f" followed by something that is not a digit is a plain ref.
		{"fe1", 0, "fe1"},
		{"fxe1", 0, "fxe1"},
		// Frame 0 is never written, so f0e1 is not a frame ref either.
		{"f0e1", 0, "f0e1"},
	}
	for _, c := range cases {
		got := parseFrameRef(c.ref)
		if got.Frame != c.wantFrame || got.Rest != c.wantRest {
			t.Errorf("parseFrameRef(%q) = {Frame:%d Rest:%q}, want {Frame:%d Rest:%q}",
				c.ref, got.Frame, got.Rest, c.wantFrame, c.wantRest)
		}
	}
}

func TestPrefixFrameRefs(t *testing.T) {
	cases := []struct {
		name   string
		body   string
		prefix string
		want   string
	}{
		{
			name:   "one ref",
			body:   "- textbox \"Email\" [ref=e1]",
			prefix: "f2",
			want:   "- textbox \"Email\" [ref=f2e1]",
		},
		{
			name:   "several refs on separate lines",
			body:   "- combobox [ref=e1]\n- listbox [ref=e2]\n- option \"Japan\" [ref=e3]",
			prefix: "f4",
			want:   "- combobox [ref=f4e1]\n- listbox [ref=f4e2]\n- option \"Japan\" [ref=f4e3]",
		},
		{
			name:   "url attributes are left alone",
			body:   "- link \"Docs\" [url=https://example.com/e12] [ref=e5]",
			prefix: "f1",
			want:   "- link \"Docs\" [url=https://example.com/e12] [ref=f1e5]",
		},
		{
			name:   "text that looks like a ref is left alone",
			body:   "- text: see ref=e7 for details [ref=e8]",
			prefix: "f3",
			want:   "- text: see ref=e7 for details [ref=f3e8]",
		},
		{
			name:   "no refs at all",
			body:   "- heading \"Country\" [level=2]\n- paragraph: hello",
			prefix: "f1",
			want:   "- heading \"Country\" [level=2]\n- paragraph: hello",
		},
		{
			name:   "empty body",
			body:   "",
			prefix: "f1",
			want:   "",
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := prefixFrameRefs(c.body, c.prefix); got != c.want {
				t.Errorf("prefixFrameRefs(%q, %q):\n got %q\nwant %q", c.body, c.prefix, got, c.want)
			}
		})
	}
}

func TestFrameURLShort(t *testing.T) {
	cases := []struct {
		name string
		url  string
		want string
	}{
		{
			name: "short url unchanged",
			url:  "https://js.stripe.com/v3/elements-inner-habanero.html",
			want: "https://js.stripe.com/v3/elements-inner-habanero.html",
		},
		{
			name: "query and fragment dropped",
			url:  "https://js.stripe.com/v3/frame.html?apiKey=pk_live_123&session=cs_live_456#frag",
			want: "https://js.stripe.com/v3/frame.html",
		},
		{
			name: "long path is truncated with an ellipsis",
			url:  "https://example.com/" + strings.Repeat("a", 200),
			want: "https://example.com/" + strings.Repeat("a", maxFrameURL-len("https://example.com/")-1) + "…",
		},
		{
			name: "empty url",
			url:  "",
			want: "",
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got := frameURLShort(c.url)
			if got != c.want {
				t.Errorf("frameURLShort(%q) = %q, want %q", c.url, got, c.want)
			}
			if n := len([]rune(got)); n > maxFrameURL {
				t.Errorf("frameURLShort(%q) is %d runes, want <= %d", c.url, n, maxFrameURL)
			}
		})
	}
}

func TestJoinFrameOutlines(t *testing.T) {
	t.Run("main frame only", func(t *testing.T) {
		got := joinFrameOutlines([]frameDoc{{N: 0, Body: "- heading \"Hello\""}})
		if got != "- heading \"Hello\"" {
			t.Errorf("got %q", got)
		}
	})

	t.Run("frames are prefixed and labelled", func(t *testing.T) {
		docs := []frameDoc{
			{N: 0, Body: "- heading \"Outer\""},
			{N: 1, URL: "https://js.stripe.com/v3/f.html?apiKey=x", Body: "- combobox [ref=e1]"},
		}
		got := joinFrameOutlines(docs)
		if !strings.Contains(got, "--- iframe [f1] https://js.stripe.com/v3/f.html ---") {
			t.Errorf("missing or untruncated header in:\n%s", got)
		}
		if !strings.Contains(got, "[ref=f1e1]") {
			t.Errorf("ref was not prefixed in:\n%s", got)
		}
		if strings.Contains(got, "apiKey") {
			t.Errorf("query string leaked into the outline:\n%s", got)
		}
	})

	t.Run("empty frames are skipped", func(t *testing.T) {
		docs := []frameDoc{
			{N: 0, Body: "- heading \"Outer\""},
			{N: 1, URL: "https://a.example/", Body: "   \n  "},
			{N: 2, URL: "https://b.example/", Body: "- text: hi"},
		}
		got := joinFrameOutlines(docs)
		if strings.Contains(got, "[f1]") {
			t.Errorf("empty frame f1 should be skipped:\n%s", got)
		}
		if !strings.Contains(got, "[f2]") {
			t.Errorf("frame f2 should be listed:\n%s", got)
		}
	})

	t.Run("unreadable frames are reported", func(t *testing.T) {
		docs := []frameDoc{
			{N: 0, Body: "- heading \"Outer\""},
			{N: 1, URL: "https://a.example/", Err: "detached"},
		}
		got := joinFrameOutlines(docs)
		if !strings.Contains(got, "unreadable: detached") {
			t.Errorf("unreadable frame not reported:\n%s", got)
		}
	})

	t.Run("frame count is capped", func(t *testing.T) {
		docs := []frameDoc{{N: 0, Body: "- heading \"Outer\""}}
		for i := 1; i <= maxFrameOutlines+3; i++ {
			docs = append(docs, frameDoc{N: i, URL: "https://f.example/", Body: "- text: x"})
		}
		got := joinFrameOutlines(docs)
		if n := strings.Count(got, "--- iframe ["); n != maxFrameOutlines {
			t.Errorf("listed %d frames, want %d", n, maxFrameOutlines)
		}
		if !strings.Contains(got, "3 more iframe(s) with content not shown") {
			t.Errorf("missing note about the frames that were left out:\n%s", got)
		}
	})
}
