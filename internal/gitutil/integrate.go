package gitutil

import (
	"context"
	"fmt"
	"path"
	"sort"
	"strings"
)

// RejectedArtifacts returns file names that look like the scratch files the
// gate refuses to merge: probe.*, tmp_*.*, debug_*.*, scratch/, *.log, *.tmp,
// *.bak, and leading-underscore helpers (_grep.js, _read.js) that workers
// write to make their own exploration easier.
//
// The underscore rule exists because a real run left scripts/_grep.js and
// scripts/_read.js in a worktree: legitimate helper scripts for the worker,
// but not deliverables, and they would otherwise land on the integration
// branch as noise.
func RejectedArtifacts(files []string) []string {
	var bad []string
	for _, f := range files {
		slash := strings.ReplaceAll(f, "\\", "/")
		base := strings.ToLower(path.Base(slash))
		switch {
		case strings.HasPrefix(base, "probe."),
			strings.HasPrefix(base, "tmp_"),
			strings.HasPrefix(base, "debug_"),
			strings.HasPrefix(base, "_"):
			bad = append(bad, f)
			continue
		}
		switch path.Ext(base) {
		case ".log", ".tmp", ".bak":
			bad = append(bad, f)
			continue
		}
		if strings.Contains("/"+slash, "/scratch/") {
			bad = append(bad, f)
		}
	}
	sort.Strings(bad)
	return bad
}

// ChangedFiles lists files changed between two revisions (two-dot diff).
func (r *Repo) ChangedFiles(ctx context.Context, from, to string) ([]string, error) {
	out, err := r.Git(ctx, "diff", "--name-only", from+".."+to)
	if err != nil {
		return nil, err
	}
	return splitLines(out), nil
}

// DiffAgainst returns the files a branch adds or changes relative to base,
// computed as the merge-base diff (i.e. what integrating it would bring in).
func (r *Repo) DiffAgainst(ctx context.Context, base, branch string) ([]string, error) {
	out, err := r.Git(ctx, "diff", "--name-only", base+"..."+branch)
	if err != nil {
		return nil, err
	}
	return splitLines(out), nil
}

func splitLines(s string) []string {
	var res []string
	for _, line := range strings.Split(s, "\n") {
		line = strings.TrimSpace(line)
		if line != "" {
			res = append(res, line)
		}
	}
	return res
}

// RunGate runs the configured gate command inside dir. An empty GateCommand
// means "no gate" and succeeds.
func (r *Repo) RunGate(ctx context.Context, dir string) error {
	if strings.TrimSpace(r.GateCommand) == "" {
		return nil
	}
	if dir == "" {
		dir = r.Dir
	}
	out, err := r.runShell(ctx, dir, r.GateCommand)
	if err != nil {
		return &ErrGate{Command: r.GateCommand, Output: out, Err: err}
	}
	return nil
}

func (r *Repo) runShell(ctx context.Context, dir, command string) (string, error) {
	if runtimeIsWindows() {
		return r.run(ctx, dir, "cmd", "/c", command)
	}
	return r.run(ctx, dir, "sh", "-c", command)
}

// MergeResult describes the outcome of an integration attempt.
type MergeResult struct {
	// Branch is the branch that was merged.
	Branch string
	// From is the integration tip before the merge.
	From string
	// SHA is the integration tip after the merge (empty on failure).
	SHA string
	// Files lists files changed by the merge.
	Files []string
	// Noop is true when the branch had nothing new to merge.
	Noop bool
	// Arbitrated is true when the merge conflicted and a model resolved it.
	Arbitrated bool
}

// arbitrate resolves the current conflict state in wt using r.Resolver.
func (r *Repo) arbitrate(ctx context.Context, wt, from, branch string) error {
	base, err := r.GitIn(ctx, wt, "merge-base", "HEAD", branch)
	if err != nil {
		return fmt.Errorf("gitutil: merge-base: %w", err)
	}
	files, err := r.ConflictedFiles(ctx, wt, strings.TrimSpace(base))
	if err != nil {
		return err
	}
	if len(files) == 0 {
		return fmt.Errorf("gitutil: no conflicted files to arbitrate")
	}
	if _, err := r.ArbitrateConflicts(ctx, wt, files, r.Resolver); err != nil {
		return err
	}
	// Sanity: nothing may still be unmerged.
	if r.HasConflicts(ctx, wt) {
		return fmt.Errorf("gitutil: conflicts remain after arbitration")
	}
	return nil
}

// Integrate merges branch into the integration branch and runs the gate.
//
// The merge happens in the worktree that has the integration branch checked
// out (created at <repo>/.swarm/integration when needed). If the gate fails,
// or the merge conflicts, the integration branch is reset back to its previous
// tip so a failed submission never pollutes the shared baseline.
func (r *Repo) Integrate(ctx context.Context, branch, message string) (*MergeResult, error) {
	integration := r.Integration()
	if !r.BranchExists(ctx, integration) {
		if _, err := r.Git(ctx, "branch", integration, "HEAD"); err != nil {
			return nil, fmt.Errorf("gitutil: create %s: %w", integration, err)
		}
	}
	if !r.BranchExists(ctx, branch) {
		return nil, fmt.Errorf("gitutil: branch %q does not exist", branch)
	}

	wt, err := r.integrationWorktree(ctx, integration)
	if err != nil {
		return nil, err
	}

	from, err := r.Head(ctx, wt)
	if err != nil {
		return nil, err
	}
	res := &MergeResult{Branch: branch, From: from}

	// Refuse to merge scratch artifacts: the gate would reject them anyway and
	// leaving a half-merged tree behind is worse.
	files, err := r.DiffAgainst(ctx, from, branch)
	if err != nil {
		return nil, err
	}
	if bad := RejectedArtifacts(files); len(bad) > 0 {
		return nil, fmt.Errorf("gitutil: branch %q contains scratch artifacts: %s", branch, strings.Join(bad, ", "))
	}

	// Nothing new? Then there is nothing to integrate.
	if _, err := r.GitIn(ctx, wt, "merge-base", "--is-ancestor", branch, "HEAD"); err == nil {
		res.Noop = true
		res.SHA = from
		return res, nil
	}

	mergeMsg := message
	if strings.TrimSpace(mergeMsg) == "" {
		mergeMsg = "integrate " + branch
	}
	if _, err := r.GitIn(ctx, wt, "merge", "--no-ff", "--no-edit", "-m", mergeMsg, branch); err != nil {
		// Conflict: try to arbitrate before giving up. A rejected arbitration
		// falls through to the same rollback as before, so this can only turn
		// a guaranteed failure into a possible success.
		if r.Resolver != nil && r.HasConflicts(ctx, wt) {
			if aerr := r.arbitrate(ctx, wt, from, branch); aerr == nil {
				if _, merr := r.GitIn(ctx, wt, "commit", "--no-edit"); merr == nil {
					merged, herr := r.Head(ctx, wt)
					if herr == nil {
						res.SHA = merged
						res.Arbitrated = true
						if res.Files, herr = r.ChangedFiles(ctx, from, merged); herr != nil {
							res.Files = nil
						}
						if gerr := r.RunGate(ctx, wt); gerr != nil {
							r.rollback(ctx, wt, from)
							return res, gerr
						}
						return res, nil
					}
				}
			}
			// Arbitration failed for any reason: clean the conflicted state so
			// the rollback below starts from a known-good tree.
			r.rollback(ctx, wt, from)
			return nil, fmt.Errorf("%w: %v", ErrConflict, err)
		}
		r.rollback(ctx, wt, from)
		return nil, fmt.Errorf("%w: %v", ErrConflict, err)
	}

	merged, err := r.Head(ctx, wt)
	if err != nil {
		r.rollback(ctx, wt, from)
		return nil, err
	}
	res.SHA = merged
	if res.Files, err = r.ChangedFiles(ctx, from, merged); err != nil {
		res.Files = nil
	}

	if err := r.RunGate(ctx, wt); err != nil {
		r.rollback(ctx, wt, from)
		return res, err
	}
	return res, nil
}

// rollback hard-resets the integration worktree back to sha and cleans up.
func (r *Repo) rollback(ctx context.Context, wt, sha string) {
	_, _ = r.GitIn(ctx, wt, "merge", "--abort")
	_, _ = r.GitIn(ctx, wt, "reset", "--hard", sha)
	_, _ = r.GitIn(ctx, wt, "clean", "-fdx")
}

// integrationWorktree returns a directory that has the integration branch
// checked out, creating the worktree when it does not exist yet.
func (r *Repo) integrationWorktree(ctx context.Context, integration string) (string, error) {
	if wt := r.WorktreePath(ctx, integration); wt != "" {
		return wt, nil
	}
	dir := r.IntegrationPath()
	if err := r.EnsureBranch(ctx, integration, ""); err != nil {
		return "", err
	}
	if _, err := r.Git(ctx, "worktree", "add", dir, integration); err != nil {
		if _, err2 := r.Git(ctx, "worktree", "add", "--force", dir, integration); err2 != nil {
			return "", fmt.Errorf("gitutil: add integration worktree %s: %w", dir, err)
		}
	}
	return dir, nil
}

// MergeBranchInto is a convenience helper for merging branch into target by
// name (no worktree gymnastics), used by tooling that manages its own checkout.
func (r *Repo) MergeBranchInto(ctx context.Context, target, branch, message string) error {
	if strings.TrimSpace(message) == "" {
		message = "integrate " + branch
	}
	if _, err := r.Git(ctx, "checkout", target); err != nil {
		return err
	}
	if _, err := r.Git(ctx, "merge", "--no-ff", "--no-edit", "-m", message, branch); err != nil {
		return fmt.Errorf("%w: %v", ErrConflict, err)
	}
	return nil
}
