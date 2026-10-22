package gitutil

// Three-way merge arbitration.
//
// When `git merge --no-ff` conflicts, the old behaviour was: roll back and
// hand the item back to the pool, where a worker redoes the whole task from
// scratch (another full LLM round, another gate run). On a real run, 1 of 8
// submissions died that way.
//
// Instead: extract the conflicting hunks as a three-way view (base / ours =
// integration tip / theirs = the worker branch), ask a model to produce the
// merged file, and write it back. The gate still runs afterwards, so a bad
// arbitration is caught exactly like a bad hand-merge — it never reaches the
// integration tip.
//
// This is deliberately NOT a general merge engine: it only fires on conflicts,
// only touches conflicted files, and always defers to the gate. If the model
// fails or the result does not compile, we fall back to the old rollback.

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// ConflictFile is one conflicted path with its three-way content.
type ConflictFile struct {
	Path    string `json:"path"`
	Base    string `json:"base,omitempty"`  // merge-base version (may be empty for add/add)
	Ours    string `json:"ours"`            // integration tip version
	Theirs  string `json:"theirs"`          // worker branch version
	Markers string `json:"markers"`         // the file as git left it, with <<<<<<< markers
}

// MergeResolver produces a merged file for a conflicted path.
// It is a function (not an interface) so tests can inject a fake and the
// production wiring stays in cmd/swarm.
type MergeResolver func(ctx context.Context, cf ConflictFile) (string, error)

// maxConflictFileBytes caps how much of one file we send to the model.
// A conflicted 100k-line generated file is not worth arbitrating.
const maxConflictFileBytes = 64 * 1024

// maxConflictFiles caps how many files one merge may arbitrate. Beyond this
// the change is a rewrite, not a conflict — roll back and let a human decide.
const maxConflictFiles = 6

// ConflictedFiles returns the paths git currently reports as unmerged in wt,
// with the three-way content for each.
func (r *Repo) ConflictedFiles(ctx context.Context, wt, base string) ([]ConflictFile, error) {
	out, err := r.GitIn(ctx, wt, "diff", "--name-only", "--diff-filter=U")
	if err != nil {
		return nil, fmt.Errorf("gitutil: list conflicts: %w", err)
	}
	paths := splitLines(out)
	if len(paths) == 0 {
		return nil, nil
	}
	if len(paths) > maxConflictFiles {
		return nil, fmt.Errorf("gitutil: %d conflicted files exceeds the arbitration cap of %d", len(paths), maxConflictFiles)
	}

	files := make([]ConflictFile, 0, len(paths))
	for _, p := range paths {
		cf := ConflictFile{Path: p}

		// :1/:2/:3 stages give base/ours/theirs directly.
		if b, err := r.GitIn(ctx, wt, "show", ":1:"+p); err == nil {
			cf.Base = b
		}
		if o, err := r.GitIn(ctx, wt, "show", ":2:"+p); err == nil {
			cf.Ours = o
		}
		if t, err := r.GitIn(ctx, wt, "show", ":3:"+p); err == nil {
			cf.Theirs = t
		}
		// The working-tree copy carries the conflict markers, which is the most
		// useful single view for the model. During a conflict the :0: stage is
		// the merged-but-unresolved blob, so read it from disk instead when the
		// plumbing does not have it.
		if m, err := r.GitIn(ctx, wt, "show", ":0:"+p); err == nil && strings.Contains(m, "<<<<<<<") {
			cf.Markers = m
		} else if disk, derr := os.ReadFile(filepath.Join(wt, filepath.FromSlash(p))); derr == nil && strings.Contains(string(disk), "<<<<<<<") {
			cf.Markers = string(disk)
		} else {
			// Last resort: rebuild the marker view from the stages we have.
			cf.Markers = buildMarkerView(cf)
		}

		if len(cf.Ours)+len(cf.Theirs) > maxConflictFileBytes {
			return nil, fmt.Errorf("gitutil: conflicted file %s is too large to arbitrate", p)
		}
		files = append(files, cf)
	}
	return files, nil
}

// HasConflicts reports whether wt has unmerged entries.
func (r *Repo) HasConflicts(ctx context.Context, wt string) bool {
	out, err := r.GitIn(ctx, wt, "diff", "--name-only", "--diff-filter=U")
	return err == nil && strings.TrimSpace(out) != ""
}

// writeWorktreeFile replaces path in wt with content and stages it.
//
// Path is validated to stay inside wt (the resolver output is model-generated,
// so it is untrusted input). Line endings are written verbatim: git normalises
// on stage, and rewriting them here would corrupt CRLF repos.
func (r *Repo) writeWorktreeFile(ctx context.Context, wt, path, content string) error {
	clean := filepath.Clean(filepath.FromSlash(path))
	if filepath.IsAbs(clean) || strings.HasPrefix(clean, "..") {
		return fmt.Errorf("gitutil: refusing to write outside the worktree: %q", path)
	}
	dest := filepath.Join(wt, clean)
	wtAbs, err := filepath.Abs(wt)
	if err != nil {
		return err
	}
	destAbs, err := filepath.Abs(dest)
	if err != nil {
		return err
	}
	if destAbs != wtAbs && !strings.HasPrefix(destAbs, wtAbs+string(os.PathSeparator)) {
		return fmt.Errorf("gitutil: path escapes the worktree: %q", path)
	}
	if err := os.MkdirAll(filepath.Dir(destAbs), 0o755); err != nil {
		return fmt.Errorf("gitutil: mkdir for %s: %w", path, err)
	}
	if err := os.WriteFile(destAbs, []byte(content), 0o644); err != nil {
		return fmt.Errorf("gitutil: write %s: %w", path, err)
	}
	if _, err := r.GitIn(ctx, wt, "add", "--", path); err != nil {
		return fmt.Errorf("gitutil: stage %s: %w", path, err)
	}
	return nil
}

// buildMarkerView reconstructs a conflict-marker view from the three stages.
// Used only when git did not leave a marker blob for us to read.
func buildMarkerView(cf ConflictFile) string {
	var b strings.Builder
	b.WriteString("<<<<<<< ours (integration)\n")
	b.WriteString(cf.Ours)
	if !strings.HasSuffix(cf.Ours, "\n") {
		b.WriteString("\n")
	}
	b.WriteString("=======\n")
	b.WriteString(cf.Theirs)
	if !strings.HasSuffix(cf.Theirs, "\n") {
		b.WriteString("\n")
	}
	b.WriteString(">>>>>>> theirs (incoming)\n")
	return b.String()
}

// ArbitrateConflicts asks resolver to merge each conflicted file, writes the
// results, and stages them. It returns the number of files resolved.
//
// The caller is responsible for committing (or aborting) afterwards.
func (r *Repo) ArbitrateConflicts(ctx context.Context, wt string, files []ConflictFile, resolver MergeResolver) (int, error) {
	if resolver == nil {
		return 0, fmt.Errorf("gitutil: no merge resolver configured")
	}
	resolved := 0
	for _, cf := range files {
		merged, err := resolver(ctx, cf)
		if err != nil {
			return resolved, fmt.Errorf("gitutil: arbitrate %s: %w", cf.Path, err)
		}
		text := strings.TrimSpace(merged)
		if text == "" {
			return resolved, fmt.Errorf("gitutil: arbitrate %s: resolver returned empty content", cf.Path)
		}
		// A resolver that just echoes the markers back has not merged anything.
		if strings.Contains(text, "<<<<<<<") || strings.Contains(text, ">>>>>>>") {
			return resolved, fmt.Errorf("gitutil: arbitrate %s: resolver left conflict markers in place", cf.Path)
		}
		if err := r.writeWorktreeFile(ctx, wt, cf.Path, merged); err != nil {
			return resolved, err
		}
		if _, err := r.GitIn(ctx, wt, "add", "--", cf.Path); err != nil {
			return resolved, fmt.Errorf("gitutil: stage %s: %w", cf.Path, err)
		}
		resolved++
	}
	return resolved, nil
}

// MergePromptFor renders the arbitration prompt for one conflicted file.
// Exported so the production resolver (and its tests) share the exact wording.
func MergePromptFor(cf ConflictFile) string {
	var b strings.Builder
	b.WriteString("You are resolving a git merge conflict. Three versions of the same file are shown below.\n\n")
	b.WriteString("RULES:\n")
	b.WriteString("- Output ONLY the complete merged file content. No prose, no markdown fences, no explanation.\n")
	b.WriteString("- Keep every change from BOTH sides. If both sides touch the same lines, combine them so both intents survive.\n")
	b.WriteString("- Never leave conflict markers (<<<<<<< ======= >>>>>>>) in the output.\n")
	b.WriteString("- Preserve the file's existing style, imports and formatting.\n\n")
	b.WriteString("PATH: " + cf.Path + "\n\n")

	if strings.TrimSpace(cf.Base) != "" {
		b.WriteString("=== BASE (before both branches) ===\n" + truncateForPrompt(cf.Base) + "\n\n")
	}
	b.WriteString("=== OURS (integration branch) ===\n" + truncateForPrompt(cf.Ours) + "\n\n")
	b.WriteString("=== THEIRS (incoming change) ===\n" + truncateForPrompt(cf.Theirs) + "\n\n")
	if strings.TrimSpace(cf.Markers) != "" && cf.Markers != cf.Ours {
		b.WriteString("=== FILE WITH CONFLICT MARKERS (what git produced) ===\n" + truncateForPrompt(cf.Markers) + "\n\n")
	}
	b.WriteString("Output the merged file now:")
	return b.String()
}

func truncateForPrompt(s string) string {
	if len(s) <= maxConflictFileBytes {
		return s
	}
	return s[:maxConflictFileBytes] + "\n...[truncated]"
}

// ParseMergedFile cleans a model reply into file content: strips a leading
// markdown fence if the model added one despite the instruction.
//
// It deliberately does NOT trim: a file's trailing newline is content, and
// silently dropping it shows up as a spurious diff line in every later merge.
func ParseMergedFile(reply string) string {
	text := strings.Trim(reply, "\r\n")
	if !strings.HasPrefix(text, "```") {
		return text + "\n"
	}
	lines := strings.Split(text, "\n")
	if len(lines) > 0 {
		lines = lines[1:]
	}
	for len(lines) > 0 && strings.TrimSpace(lines[len(lines)-1]) == "```" {
		lines = lines[:len(lines)-1]
	}
	out := strings.Join(lines, "\n")
	if !strings.HasSuffix(out, "\n") {
		out += "\n"
	}
	return out
}

// MergeResultJSON is the schema a resolver may return instead of raw content.
// Kept for resolvers that prefer structured output.
type MergeResultJSON struct {
	Path   string `json:"path"`
	Merged string `json:"merged"`
	Reason string `json:"reason,omitempty"`
}

// DecodeMerged accepts either raw file content or {"merged": "..."}.
func DecodeMerged(reply string) (string, error) {
	text := strings.TrimSpace(reply)
	if strings.HasPrefix(text, "{") {
		var obj MergeResultJSON
		if err := json.Unmarshal([]byte(text), &obj); err == nil && strings.TrimSpace(obj.Merged) != "" {
			return obj.Merged, nil
		}
	}
	return ParseMergedFile(text), nil
}
