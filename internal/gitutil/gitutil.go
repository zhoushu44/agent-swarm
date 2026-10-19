// Package gitutil provides the git plumbing used by the swarm runner:
// per-worker worktrees, branch management and the integration step
// (merge a worker branch into the integration branch, run gate tests and
// roll the merge back when the gate fails).
//
// It is a Go port of the Repo class in src/gitutil.js.
package gitutil

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"runtime"
	"strings"
	"time"
)

// ErrConflict is returned when a merge cannot be completed because of
// conflicting changes, or because the integration checkout is not clean.
var ErrConflict = errors.New("gitutil: merge conflict")

// ErrGate is returned when the integration gate command exits non-zero.
type ErrGate struct {
	Command string
	Output  string
	Err     error
}

func (e *ErrGate) Error() string {
	msg := e.Output
	if len(msg) > 2000 {
		msg = msg[:2000] + "...(truncated)"
	}
	return fmt.Sprintf("gitutil: gate %q failed: %v\n%s", e.Command, e.Err, msg)
}

// Unwrap exposes the underlying exec error.
func (e *ErrGate) Unwrap() error { return e.Err }

// Repo is a handle on a git repository plus the conventions the swarm uses:
// one branch per worker (swarm/<prefix>-<worker>) checked out in its own
// worktree under <repo>/.swarm/worktrees/<worker>.
type Repo struct {
	// Dir is the path of the main repository (the one holding .git).
	Dir string
	// WorktreeRoot is where per-worker worktrees are created.
	WorktreeRoot string
	// BranchPrefix is used to build worker branch names.
	BranchPrefix string
	// IntegrationBranch is the branch workers get merged into.
	IntegrationBranch string
	// GateCommand is the shell command run after a merge, e.g. "go test ./...".
	GateCommand string
	// Timeout bounds every git / gate invocation. Zero means five minutes.
	Timeout time.Duration
}

// New returns a Repo rooted at dir with the swarm defaults applied.
func New(dir string) *Repo {
	abs, err := filepath.Abs(dir)
	if err != nil {
		abs = dir
	}
	return &Repo{
		Dir:               abs,
		WorktreeRoot:      filepath.Join(abs, ".swarm", "worktrees"),
		BranchPrefix:      "swarm",
		IntegrationBranch: "integration",
		Timeout:           5 * time.Minute,
	}
}

func (r *Repo) timeout() time.Duration {
	if r.Timeout <= 0 {
		return 5 * time.Minute
	}
	return r.Timeout
}

// Integration returns the configured integration branch name.
func (r *Repo) Integration() string {
	if strings.TrimSpace(r.IntegrationBranch) == "" {
		return "integration"
	}
	return r.IntegrationBranch
}

// WorkerBranch returns the branch name used for a worker.
func (r *Repo) WorkerBranch(worker string) string {
	prefix := strings.TrimSpace(r.BranchPrefix)
	if prefix == "" {
		prefix = "swarm"
	}
	return prefix + "/" + prefix + "-" + sanitizeRef(worker)
}

// WorkerPath returns the worktree path used for a worker.
func (r *Repo) WorkerPath(worker string) string {
	root := strings.TrimSpace(r.WorktreeRoot)
	if root == "" {
		root = filepath.Join(r.Dir, ".swarm", "worktrees")
	}
	return filepath.Join(root, sanitizeRef(worker))
}

var refUnsafe = regexp.MustCompile("[^A-Za-z0-9._-]+")

func sanitizeRef(s string) string {
	s = strings.TrimSpace(s)
	s = refUnsafe.ReplaceAllString(s, "-")
	s = strings.Trim(s, "-.")
	if s == "" {
		return "worker"
	}
	return s
}

// run executes a command inside dir and returns its combined output.
func (r *Repo) run(ctx context.Context, dir string, name string, args ...string) (string, error) {
	if ctx == nil {
		ctx = context.Background()
	}
	c, cancel := context.WithTimeout(ctx, r.timeout())
	defer cancel()

	cmd := exec.CommandContext(c, name, args...)
	cmd.Dir = dir
	var buf bytes.Buffer
	cmd.Stdout = &buf
	cmd.Stderr = &buf
	err := cmd.Run()
	out := buf.String()
	if err != nil {
		return out, fmt.Errorf("%s %s: %w\n%s", name, strings.Join(args, " "), err, strings.TrimSpace(out))
	}
	return out, nil
}

// Git runs a raw git command in the repository root.
func (r *Repo) Git(ctx context.Context, args ...string) (string, error) {
	return r.run(ctx, r.Dir, "git", args...)
}

// GitIn runs a raw git command inside dir.
func (r *Repo) GitIn(ctx context.Context, dir string, args ...string) (string, error) {
	return r.run(ctx, dir, "git", args...)
}

// IsRepo reports whether dir looks like a git repository.
func IsRepo(dir string) bool {
	_, err := os.Stat(filepath.Join(dir, ".git"))
	return err == nil
}

// CurrentBranch returns the branch checked out in the repository root.
func (r *Repo) CurrentBranch(ctx context.Context) (string, error) {
	out, err := r.Git(ctx, "rev-parse", "--abbrev-ref", "HEAD")
	if err != nil {
		return "", err
	}
	return strings.TrimSpace(out), nil
}

// Head returns the commit SHA of HEAD in dir (repository root when empty).
func (r *Repo) Head(ctx context.Context, dir string) (string, error) {
	if dir == "" {
		dir = r.Dir
	}
	out, err := r.run(ctx, dir, "git", "rev-parse", "HEAD")
	if err != nil {
		return "", err
	}
	return strings.TrimSpace(out), nil
}

// RevParse resolves a revision to a SHA.
func (r *Repo) RevParse(ctx context.Context, rev string) (string, error) {
	out, err := r.Git(ctx, "rev-parse", "--verify", "--quiet", rev)
	if err != nil {
		return "", err
	}
	return strings.TrimSpace(out), nil
}

// BranchExists reports whether refs/heads/<branch> exists.
func (r *Repo) BranchExists(ctx context.Context, branch string) bool {
	_, err := r.Git(ctx, "rev-parse", "--verify", "--quiet", "refs/heads/"+branch)
	return err == nil
}

// EnsureBranch creates branch pointing at base (HEAD when empty) if missing.
func (r *Repo) EnsureBranch(ctx context.Context, branch, base string) error {
	if r.BranchExists(ctx, branch) {
		return nil
	}
	if strings.TrimSpace(base) == "" {
		base = "HEAD"
	}
	_, err := r.Git(ctx, "branch", branch, base)
	return err
}

// DeleteBranch force-deletes a local branch. Missing branches are ignored.
func (r *Repo) DeleteBranch(ctx context.Context, branch string) error {
	if !r.BranchExists(ctx, branch) {
		return nil
	}
	_, err := r.Git(ctx, "branch", "-D", branch)
	return err
}

// IsDirty reports whether dir has uncommitted changes.
func (r *Repo) IsDirty(ctx context.Context, dir string) (bool, error) {
	if dir == "" {
		dir = r.Dir
	}
	out, err := r.run(ctx, dir, "git", "status", "--porcelain")
	if err != nil {
		return false, err
	}
	return strings.TrimSpace(out) != "", nil
}

// CommitAll stages everything (except .gitignore'd paths) and commits it.
// It returns the new SHA; when there is nothing to commit it returns the
// current HEAD and committed=false.
func (r *Repo) CommitAll(ctx context.Context, dir, message string) (sha string, committed bool, err error) {
	if dir == "" {
		dir = r.Dir
	}
	if _, err := r.run(ctx, dir, "git", "add", "-A"); err != nil {
		return "", false, err
	}
	out, err := r.run(ctx, dir, "git", "status", "--porcelain")
	if err != nil {
		return "", false, err
	}
	if strings.TrimSpace(out) == "" {
		sha, err = r.Head(ctx, dir)
		return sha, false, err
	}
	if _, err := r.run(ctx, dir, "git",
		"-c", "user.name=swarm", "-c", "user.email=swarm@local",
		"commit", "-m", message); err != nil {
		return "", false, err
	}
	sha, err = r.Head(ctx, dir)
	return sha, err == nil, err
}

func samePath(a, b string) bool {
	aa, err1 := filepath.Abs(a)
	bb, err2 := filepath.Abs(b)
	if err1 != nil || err2 != nil {
		return a == b
	}
	if runtime.GOOS == "windows" {
		return strings.EqualFold(filepath.Clean(aa), filepath.Clean(bb))
	}
	return filepath.Clean(aa) == filepath.Clean(bb)
}
