package prompts

import (
	"strings"
	"testing"
)

func baseOptions() Options {
	return Options{
		PeerName:   "w1",
		ServerPort: 17771,
		Mission:    "port the swarm runtime to Go",
		Harness:    "direct",
	}
}

func TestBuildWorkerPromptContainsIdentity(t *testing.T) {
	p := BuildWorkerPrompt(baseOptions())

	if !strings.Contains(p, `- peer name {PEER} = "w1"`) {
		t.Fatalf("identity line for peer missing:\n%s", tail(p, 400))
	}
	if !strings.Contains(p, "- swarm API {API} = http://127.0.0.1:17771") {
		t.Fatalf("identity line for api missing:\n%s", tail(p, 400))
	}
	if !strings.Contains(p, "MISSION (the whole swarm is working on this):\nport the swarm runtime to Go") {
		t.Fatalf("mission block missing")
	}
}

func TestBuildWorkerPromptTokenAndTurnAndBrief(t *testing.T) {
	o := baseOptions()
	o.Token = "tok-abc"
	o.Turn = 3
	o.Brief = "BOARD SNAPSHOT: 1 pending"
	p := BuildWorkerPrompt(o)

	if !strings.Contains(p, "- auth header X-Swarm-Token: tok-abc") {
		t.Fatalf("token line missing")
	}
	if !strings.HasSuffix(p, "This is turn 3.") {
		t.Fatalf("turn line should be the last thing in the prompt, got tail: %q", tail(p, 80))
	}
	briefIdx := strings.Index(p, "BOARD SNAPSHOT: 1 pending")
	identIdx := strings.Index(p, "YOUR IDENTITY")
	turnIdx := strings.Index(p, "This is turn 3.")
	if briefIdx < 0 || identIdx < 0 || turnIdx < 0 {
		t.Fatalf("missing blocks: brief=%d ident=%d turn=%d", briefIdx, identIdx, turnIdx)
	}
	if !(identIdx < briefIdx && briefIdx < turnIdx) {
		t.Fatalf("blocks out of mutability order: ident=%d brief=%d turn=%d", identIdx, briefIdx, turnIdx)
	}
}

func TestTurnZeroOmitsTurnLine(t *testing.T) {
	p := BuildWorkerPrompt(baseOptions())
	if strings.Contains(p, "This is turn") {
		t.Fatalf("turn 0 must not emit a turn line")
	}
}

// TestSharedBodyIsByteIdentical is the KV-cache property: everything before YOUR
// IDENTITY must be byte-for-byte the same for every worker, so the whole swarm
// shares that prefix in the prefix cache.
func TestSharedBodyIsByteIdentical(t *testing.T) {
	a := baseOptions()
	a.PeerName = "w1"
	a.ServerPort = 17771
	a.Token = "tok-a"
	a.Brief = "brief for w1"
	a.Turn = 4

	b := baseOptions()
	b.PeerName = "w27"
	b.ServerPort = 17771
	b.Token = "tok-b"
	b.Brief = "a completely different brief"
	b.Turn = 9

	pa, pb := BuildWorkerPrompt(a), BuildWorkerPrompt(b)
	marker := "YOUR IDENTITY"
	ia, ib := strings.Index(pa, marker), strings.Index(pb, marker)
	if ia <= 0 || ib <= 0 {
		t.Fatalf("identity marker missing")
	}
	if pa[:ia] != pb[:ib] {
		t.Fatalf("shared body differs between workers (KV-cache prefix would break)")
	}
}

func TestSharedBodyUsesPlaceholders(t *testing.T) {
	p := BuildWorkerPrompt(baseOptions())
	shared := p[:strings.Index(p, "YOUR IDENTITY")]

	if !strings.Contains(shared, "{PEER}") || !strings.Contains(shared, "{API}") {
		t.Fatalf("shared body must keep the {PEER}/{API} placeholders")
	}
	if strings.Contains(shared, "w1") {
		t.Fatalf("shared body must not contain the real peer name")
	}
	if strings.Contains(shared, "17771") {
		t.Fatalf("shared body must not contain the real API port")
	}
}

func TestToolsBlockPerHarness(t *testing.T) {
	direct := BuildWorkerPrompt(baseOptions())
	if !strings.Contains(direct, `- "http" - the ONLY way to call the swarm API`) {
		t.Fatalf("direct harness should get the http tool block")
	}
	if strings.Contains(direct, "curl -s") {
		t.Fatalf("direct harness must not be told to use curl")
	}

	o := baseOptions()
	o.Harness = "claude"
	cli := BuildWorkerPrompt(o)
	if !strings.Contains(cli, "curl -s") {
		t.Fatalf("CLI harness should get the curl instructions")
	}
	if strings.Contains(cli, `- "http" - the ONLY way`) {
		t.Fatalf("CLI harness must not be told it has an http tool")
	}
}

func TestConventionsBlockOptional(t *testing.T) {
	p := BuildWorkerPrompt(baseOptions())
	if strings.Contains(p, "PROJECT CONVENTIONS") {
		t.Fatalf("conventions block should be omitted when empty")
	}
	o := baseOptions()
	o.Conventions = "go 1.22, no cgo"
	p = BuildWorkerPrompt(o)
	if !strings.Contains(p, "PROJECT CONVENTIONS (all workers must follow):\ngo 1.22, no cgo") {
		t.Fatalf("conventions block missing")
	}
}

func TestRenderHardRulesOrderAndFormat(t *testing.T) {
	rules := []Rule{
		{ID: "zzz", Priority: 5, Category: "c", Text: "last-ish"},
		{ID: "aaa", Priority: 0, Category: "c", Text: "first"},
		{ID: "bbb", Priority: 0, Category: "c", Text: "second"},
	}
	got := RenderHardRules(rules)
	want := "- first\n- second\n- last-ish"
	if got != want {
		t.Fatalf("RenderHardRules = %q, want %q", got, want)
	}
}

func TestRenderHardRulesDoesNotMutateInput(t *testing.T) {
	rules := []Rule{
		{ID: "b", Priority: 5, Text: "b"},
		{ID: "a", Priority: 0, Text: "a"},
	}
	_ = RenderHardRules(rules)
	if rules[0].ID != "b" {
		t.Fatalf("RenderHardRules reordered the caller's slice")
	}
}

func TestDefaultRulesRenderAllRules(t *testing.T) {
	out := RenderHardRules(DefaultRules)
	lines := strings.Split(out, "\n")
	if len(lines) != len(DefaultRules) {
		t.Fatalf("rendered %d lines for %d rules", len(lines), len(DefaultRules))
	}
	for _, r := range DefaultRules {
		if !strings.Contains(out, r.Text) {
			t.Fatalf("rule %s missing from rendered output", r.ID)
		}
	}
	// priority 0 rules must come first
	if !strings.HasPrefix(out, "- One claim at a time.") {
		t.Fatalf("highest-priority rule should be first, got %q", lines[0])
	}
}

func TestValidateRulesCleanRegistry(t *testing.T) {
	res := ValidateRules(DefaultRules)
	if !res.OK {
		t.Fatalf("default rules should validate OK, errors=%v", res.Errors)
	}
	if len(res.Errors) != 0 {
		t.Fatalf("unexpected errors: %v", res.Errors)
	}
}

func TestValidateRulesTooManySamePriority(t *testing.T) {
	rules := []Rule{
		{ID: "a", Priority: 5, Category: "execution", Text: "do alpha work"},
		{ID: "b", Priority: 5, Category: "execution", Text: "do beta work"},
		{ID: "c", Priority: 5, Category: "execution", Text: "do gamma work"},
		{ID: "d", Priority: 5, Category: "execution", Text: "do delta work"},
	}
	res := ValidateRules(rules)
	if len(res.Warnings) == 0 {
		t.Fatalf("expected a warning for 4 rules sharing category:priority")
	}
	if !strings.Contains(res.Warnings[0], "execution:5") {
		t.Fatalf("warning should name the category:priority key, got %q", res.Warnings[0])
	}
}

func TestValidateRulesDetectsAlwaysVsNever(t *testing.T) {
	rules := []Rule{
		{ID: "must-commit", Priority: 0, Category: "submission", Text: "You must always commit every change before you submit."},
		{ID: "never-commit", Priority: 0, Category: "submission", Text: "You must never commit a change without review."},
	}
	res := ValidateRules(rules)
	found := false
	for _, w := range res.Warnings {
		if strings.Contains(w, "must-commit") && strings.Contains(w, "never-commit") {
			found = true
		}
	}
	if !found {
		t.Fatalf("expected an ALWAYS-vs-NEVER conflict warning, got %v", res.Warnings)
	}
}

func TestValidateRulesNoConflictAcrossCategories(t *testing.T) {
	rules := []Rule{
		{ID: "must-commit", Priority: 0, Category: "submission", Text: "You must always commit your work."},
		{ID: "never-commit", Priority: 0, Category: "execution", Text: "You must never commit scratch files."},
	}
	res := ValidateRules(rules)
	for _, w := range res.Warnings {
		if strings.Contains(w, "potential conflict") {
			t.Fatalf("rules in different categories must not be reported as conflicting: %q", w)
		}
	}
}

func TestAPIBaseURL(t *testing.T) {
	if got := APIBaseURL(17771); got != "http://127.0.0.1:17771" {
		t.Fatalf("APIBaseURL = %q", got)
	}
}

func TestPromptIsStable(t *testing.T) {
	// Same options must render the same bytes: prompt caching depends on it.
	a := BuildWorkerPrompt(baseOptions())
	b := BuildWorkerPrompt(baseOptions())
	if a != b {
		t.Fatalf("BuildWorkerPrompt is not deterministic")
	}
}

func TestPromptContainsAllFiveSteps(t *testing.T) {
	p := BuildWorkerPrompt(baseOptions())
	for _, step := range []string{
		"1. COLLECT CONTEXT",
		"2. CLAIM OR CREATE WORK",
		"3. EXECUTE",
		"4. VERIFY",
		"5. SUBMIT",
	} {
		if !strings.Contains(p, step) {
			t.Fatalf("prompt missing loop step %q", step)
		}
	}
}

func tail(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[len(s)-n:]
}
