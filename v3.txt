// Package prompts builds the worker prompt for the swarm runtime.
//
// Ported from swarm/src/prompts.js. Design notes kept from the JS original:
//
//   - The five-step cooperation loop lives in the prompt, not in the runtime; the
//     runtime only injects the API address and the worker identity.
//   - No global state (board contents, what other agents did) is embedded: each
//     agent fetches that itself every turn, so prompt size is independent of the
//     number of agents and the system scales.
//   - KV-cache / prefix-cache friendly: everything in the shared body (protocol,
//     rules, tools) that varies per worker (peer name, API port, token) is written
//     as the {PEER}/{API} placeholders so that body is byte-for-byte identical for
//     every worker (Agensh paper section 2.3). Volatile content is ordered
//     tail-wards by mutability: shared body -> YOUR IDENTITY (fixed per worker) ->
//     TURN BRIEF (per turn) -> turn number.
package prompts

import (
	"fmt"
	"regexp"
	"sort"
	"strings"
)

// Rule is one HARD RULE plus the metadata needed for conflict detection.
// Priority: 0 = highest (safety red line), 5 = medium, 10 = low (advisory).
type Rule struct {
	ID        string
	Priority  int
	Category  string
	Text      string
	Rationale string
}

// DefaultRules is the rule registry. To add a rule, register it here; then
// ValidateRules picks it up for conflict detection at startup.
var DefaultRules = []Rule{
	{
		ID:        "one-claim-at-a-time",
		Priority:  0,
		Category:  "claiming",
		Text:      "One claim at a time. Never touch files outside your worktree.",
		Rationale: "Concurrency safety: multiple workers editing the same file causes integration conflicts.",
	},
	{
		ID:        "no-self-done",
		Priority:  0,
		Category:  "submission",
		Text:      "Never mark your own work done - submission + gate decides that.",
		Rationale: "Prevents a worker from bypassing the gate and self-declaring completion.",
	},
	{
		ID:        "accept-409",
		Priority:  1,
		Category:  "claiming",
		Text:      "If claim/submit returns 409, accept it and move on; races are normal, not errors.",
		Rationale: "409 on claim/submit is normal contention between workers, not a system error.",
	},
	{
		ID:        "small-verifiable-turns",
		Priority:  5,
		Category:  "execution",
		Text:      "Keep each turn focused: finish something small and verifiable rather than half-finishing something big.",
		Rationale: "Small steps lower integration conflict probability and give faster gate feedback.",
	},
	{
		ID:        "no-scratch-files",
		Priority:  0,
		Category:  "submission",
		Text:      "NEVER commit scratch/debug files: no probe.*, tmp_*.*, debug_*.*, scratch/, *.log, *.tmp, *.bak. Delete them BEFORE git commit; the gate rejects merges containing them, and every rejected merge wastes a full turn. Only commit real deliverables (source, tests, docs, config).",
		Rationale: "Scratch files pollute history and are rejected by the gate, wasting turns.",
	},
	{
		ID:        "idle-when-no-work",
		Priority:  5,
		Category:  "execution",
		Text:      "When the board has no suitable work left for you, reply exactly: IDLE",
		Rationale: "Spinning idle burns API quota; an explicit IDLE lets the runner back off.",
	},
}

// ValidationResult is the outcome of ValidateRules.
type ValidationResult struct {
	OK       bool
	Warnings []string
	Errors   []string
}

var (
	reAlways = regexp.MustCompile(`(?i)\b(always|must|required)\b`)
	reNever  = regexp.MustCompile(`(?i)\b(never|forbidden|must not)\b`)
	reWord   = regexp.MustCompile(`\b\w{4,}\b`)
)

// ValidateRules statically checks the rule registry for contradictions.
//
// Two kinds of conflict are detected:
//  1. too many rules sharing the same category:priority (>3): the worker cannot
//     tell which to follow first;
//  2. mutually exclusive keywords ("always X" vs "never X", "must first" vs
//     "must last") on rules of the same category that mention the same word.
//
// The result is OK unless hard errors are found; soft conflicts are warnings.
func ValidateRules(rules []Rule) ValidationResult {
	warnings := []string{}
	errors := []string{}

	// 1. same category + priority with too many rules.
	byCatPri := map[string][]string{}
	order := []string{}
	for _, r := range rules {
		key := fmt.Sprintf("%s:%d", r.Category, r.Priority)
		if _, ok := byCatPri[key]; !ok {
			order = append(order, key)
		}
		byCatPri[key] = append(byCatPri[key], r.ID)
	}
	for _, key := range order {
		ids := byCatPri[key]
		if len(ids) > 3 {
			warnings = append(warnings, fmt.Sprintf(
				"category:priority %s has %d rules (>3): %s - worker may not know which to follow first",
				key, len(ids), strings.Join(ids, ", ")))
		}
	}

	// 2. mutually exclusive keywords.
	for i := 0; i < len(rules); i++ {
		for j := i + 1; j < len(rules); j++ {
			a, b := rules[i], rules[j]
			if a.Category != b.Category {
				continue
			}
			aAlways, aNever := reAlways.MatchString(a.Text), reNever.MatchString(a.Text)
			bAlways, bNever := reAlways.MatchString(b.Text), reNever.MatchString(b.Text)
			if !((aAlways && bNever) || (aNever && bAlways)) {
				continue
			}
			// Coarse detection: if both rules share a noun (e.g. "commit"),
			// they may contradict each other.
			common := commonWords(a.Text, b.Text)
			if len(common) == 0 {
				continue
			}
			if len(common) > 3 {
				common = common[:3]
			}
			warnings = append(warnings, fmt.Sprintf(
				"potential conflict between %s and %s (category=%s): one says ALWAYS/MUST, other says NEVER/MUST NOT, both mention %s",
				a.ID, b.ID, a.Category, strings.Join(common, "/")))
		}
	}

	return ValidationResult{OK: len(errors) == 0, Warnings: warnings, Errors: errors}
}

// commonWords returns the distinct 4+ letter lowercase words shared by a and b,
// in the order they first appear in b.
func commonWords(a, b string) []string {
	inA := map[string]bool{}
	for _, w := range reWord.FindAllString(strings.ToLower(a), -1) {
		inA[w] = true
	}
	seen := map[string]bool{}
	out := []string{}
	for _, w := range reWord.FindAllString(strings.ToLower(b), -1) {
		if inA[w] && !seen[w] {
			seen[w] = true
			out = append(out, w)
		}
	}
	return out
}

// RenderHardRules renders the HARD RULES block sorted by priority, then ID.
// The output format is stable (KV-cache prefix hits depend on it) and does not
// vary per worker or per turn.
func RenderHardRules(rules []Rule) string {
	sorted := make([]Rule, len(rules))
	copy(sorted, rules)
	sort.SliceStable(sorted, func(i, j int) bool {
		if sorted[i].Priority != sorted[j].Priority {
			return sorted[i].Priority < sorted[j].Priority
		}
		return sorted[i].ID < sorted[j].ID
	})
	lines := make([]string, 0, len(sorted))
	for _, r := range sorted {
		lines = append(lines, "- "+r.Text)
	}
	return strings.Join(lines, "\n")
}

// Options configures BuildWorkerPrompt.
type Options struct {
	// PeerName is this worker's peer name (the {PEER} value in YOUR IDENTITY).
	PeerName string
	// ServerPort is the swarm API port; the base URL is http://127.0.0.1:<port>.
	ServerPort int
	// Token is optional; when set it is exposed as the X-Swarm-Token auth header.
	Token string
	// Mission is the whole-swarm objective.
	Mission string
	// Conventions are optional project-wide conventions.
	Conventions string
	// Turn is the 1-based turn number (0 omits the trailing turn line).
	Turn int
	// Brief is the per-turn TURN BRIEF block (empty on the first turn).
	Brief string
	// Harness selects the TOOLS block: "direct" gets the dedicated http tool,
	// anything else (pi/claude/codex) gets the curl-based CLI block.
	Harness string
	// Rules overrides DefaultRules when non-nil.
	Rules []Rule
}

// APIBaseURL returns the swarm API base URL for a server port.
func APIBaseURL(port int) string { return fmt.Sprintf("http://127.0.0.1:%d", port) }
