package prompts

import (
	"flag"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// update rewrites the golden files instead of asserting against them:
//
//	go test ./internal/prompts/ -run TestPromptGolden -update
//
// Regenerate only when the prompt body changes ON PURPOSE: the golden files are
// the regression guard for the KV-cache property (the shared body must stay
// byte-identical so the whole swarm keeps hitting the same prefix cache).
var update = flag.Bool("update", false, "rewrite testdata golden prompt files")

type goldenCase struct {
	name string
	opts Options
}

// goldenCases mirrors the option matrix used to cross-check the JS original
// (swarm/refgen_w7.mjs), so a Go-vs-JS diff can be replayed case by case.
func goldenCases() []goldenCase {
	return []goldenCase{
		{
			name: "full",
			opts: Options{
				PeerName:    "w6",
				ServerPort:  17771,
				Token:       "tok-abc",
				Mission:     "Build the thing",
				Conventions: "Go 1.22, no scratch files",
				Turn:        3,
				Brief:       "BOARD SNAPSHOT: 2 pending / 0 done.",
				Harness:     "direct",
			},
		},
		{
			name: "empty-cli",
			opts: Options{
				PeerName:   "w1",
				ServerPort: 8080,
				Harness:    "cli",
			},
		},
		{
			name: "minimal-direct",
			opts: Options{
				PeerName:   "w9",
				ServerPort: 9090,
				Mission:    "m",
				Harness:    "direct",
			},
		},
		{
			name: "brief-only",
			opts: Options{
				PeerName:    "w2",
				ServerPort:  1234,
				Token:       "zzz",
				Mission:     "Mission",
				Conventions: "Conv",
				Turn:        1,
				Brief:       "BRIEF",
				Harness:     "direct",
			},
		},
	}
}

// normalizeEOL converts CRLF line endings to LF.
//
// The prompt builder always emits LF, but the golden fixtures live in git and
// on a Windows checkout with core.autocrlf=true (or any editor that saves
// CRLF) they arrive here with CRLF. Without this the byte comparison below
// fails on every case even though the prompt is correct. .gitattributes pins
// testdata/*.golden to LF so a clean checkout is already correct; this is the
// belt-and-braces half of the fix so the test cannot be broken by a stray
// tool rewriting the fixtures.
func normalizeEOL(s string) string {
	if !strings.Contains(s, "\r\n") {
		return s
	}
	return strings.ReplaceAll(s, "\r\n", "\n")
}

func TestPromptGolden(t *testing.T) {
	for _, c := range goldenCases() {
		c := c
		t.Run(c.name, func(t *testing.T) {
			got := BuildWorkerPrompt(c.opts)
			path := filepath.Join("testdata", c.name+".golden")

			if *update {
				if err := os.MkdirAll("testdata", 0o755); err != nil {
					t.Fatalf("mkdir testdata: %v", err)
				}
				// Always write LF so the committed fixture is platform neutral.
				if err := os.WriteFile(path, []byte(normalizeEOL(got)), 0o644); err != nil {
					t.Fatalf("write golden: %v", err)
				}
				return
			}

			raw, err := os.ReadFile(path)
			if err != nil {
				t.Fatalf("read golden %s: %v (run `go test ./internal/prompts/ -run TestPromptGolden -update` to regenerate)", path, err)
			}
			want := normalizeEOL(string(raw))
			if got != want {
				i := firstDiff(got, want)
				t.Fatalf("prompt for case %q changed at byte %d\n got: %q\nwant: %q",
					c.name, i, sliceAround(got, i, 80), sliceAround(want, i, 80))
			}
		})
	}
}

// TestGoldenSharedPrefixIsIdenticalAcrossCases is the scaling property: every
// golden case must share the exact same bytes before YOUR IDENTITY, regardless of
// peer, port, token, harness, mission or brief.
func TestGoldenSharedPrefixIsIdenticalAcrossCases(t *testing.T) {
	var prefix string
	for _, c := range goldenCases() {
		p := BuildWorkerPrompt(c.opts)
		i := indexOf(p, "YOUR IDENTITY")
		if i <= 0 {
			t.Fatalf("case %q has no YOUR IDENTITY marker", c.name)
		}
		if prefix == "" {
			prefix = p[:i]
			continue
		}
		if p[:i] != prefix {
			t.Fatalf("case %q shared prefix differs at byte %d", c.name, firstDiff(p[:i], prefix))
		}
	}
}

func firstDiff(a, b string) int {
	n := len(a)
	if len(b) < n {
		n = len(b)
	}
	for i := 0; i < n; i++ {
		if a[i] != b[i] {
			return i
		}
	}
	return n
}

func sliceAround(s string, i, n int) string {
	lo := i - n
	if lo < 0 {
		lo = 0
	}
	hi := i + n
	if hi > len(s) {
		hi = len(s)
	}
	return s[lo:hi]
}

func indexOf(s, sub string) int {
	for i := 0; i+len(sub) <= len(s); i++ {
		if s[i:i+len(sub)] == sub {
			return i
		}
	}
	return -1
}
