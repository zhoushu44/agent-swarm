package gitutil

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// Worktree describes one entry of `git worktree list --porcelain`.
type Worktree struct {
	Path   string
	Head   string
	Branch string
	Bare   bool
}

// Worktrees lists all worktrees registered in the repository.
func (r *Repo) Worktrees(ctx context.Context) ([]Worktree, error) {
	out, err := r.Git(ctx, "worktree", "list", "--porcelain")
	if err != nil {
		return nil, err
	}
	var res []Worktree
	var cur Worktree
	flush := func() {
		if cur.Path != "" {
			res = append(res, cur)
		}
		cur = Worktree{}
	}
	for _, line := range strings.Split(out, "\n") {
		line = strings.TrimRight(line, "\r")
		if line == "" {
			flush()
			continue
		}
		key, val, _ := strings.Cut(line, " ")
		switch key {
		case "worktree":
			cur.Path = val
		case "HEAD":
			cur.Head = val
		case "branch":
			cur.Branch = strings.TrimPrefix(val, "refs/heads/")
		case "bare":
			cur.Bare = true
		}
	}
	flush()
	return res, nil
}

// AddWorktree creates (or reuses) the worktree for worker, checking out its
// branch. The branch is created from base when it does not exist yet.
func (r *Repo) AddWorktree(ctx context.Context, worker, base string) (string, error) {
	path := r.WorkerPath(worker)
	branch := r.WorkerBranch(worker)

	if wts, err := r.Worktrees(ctx); err == nil {
		for _, wt := range wts {
			if !samePath(wt.Path, path) {
				continue
			}
			if wt.Branch == branch {
				return path, nil
			}
			// Stale registration on the wrong branch: drop and re-add.
			_, _ = r.Git(ctx, "worktree", "remove", "--force", path)
		}
	}

	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return "", err
	}
	if _, err := os.Stat(path); err == nil {
		// Directory exists but is not a registered worktree: clean it up.
		if err := os.RemoveAll(path); err != nil {
			return "", err
		}
	}

	if err := r.EnsureBranch(ctx, branch, base); err != nil {
		return "", err
	}
	if _, err := r.Git(ctx, "worktree", "add", path, branch); err != nil {
		if _, err2 := r.Git(ctx, "worktree", "add", "--force", path, branch); err2 != nil {
			return "", err
		}
	}
	return path, nil
}

// RemoveWorktree removes the worker's worktree, keeping its branch.
func (r *Repo) RemoveWorktree(ctx context.Context, worker string) error {
	path := r.WorkerPath(worker)
	if _, err := os.Stat(path); err != nil {
		_, _ = r.Git(ctx, "worktree", "prune")
		return nil
	}
	if _, err := r.Git(ctx, "worktree", "remove", "--force", path); err != nil {
		if rmErr := os.RemoveAll(path); rmErr != nil {
			return fmt.Errorf("gitutil: remove worktree %s: %w (git: %v)", path, rmErr, err)
		}
		_, _ = r.Git(ctx, "worktree", "prune")
		return nil
	}
	return nil
}

// PruneWorktrees drops registrations for worktrees whose directory vanished.
func (r *Repo) PruneWorktrees(ctx context.Context) error {
	_, err := r.Git(ctx, "worktree", "prune")
	return err
}

// WorktreePath returns the on-disk path of the worktree that has branch
// checked out, or "" when the branch is not checked out anywhere.
func (r *Repo) WorktreePath(ctx context.Context, branch string) string {
	wts, err := r.Worktrees(ctx)
	if err != nil {
		return ""
	}
	for _, wt := range wts {
		if wt.Branch == branch {
			return wt.Path
		}
	}
	return ""
}
