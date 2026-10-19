package gitutil

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// ---- helpers -------------------------------------------------------------

func runGit(t *testing.T, dir string, args ...string) string {
	t.Helper()
	cmd := exec.Command("git", args...)
	cmd.Dir = dir
	cmd.Env = append(os.Environ(),
		"GIT_AUTHOR_NAME=test", "GIT_AUTHOR_EMAIL=test@example.com",
		"GIT_COMMITTER_NAME=test", "GIT_COMMITTER_EMAIL=test@example.com",
	)
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("git %s: %v\n%s", strings.Join(args, " "), err, out)
	}
	return string(out)
}

func writeFile(t *testing.T, path, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatalf("mkdir: %v", err)
	}
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatalf("write %s: %v", path, err)
	}
}

// newTestRepo creates a git repository with one commit on branch main.
func newTestRepo(t *testing.T) (*Repo, string) {
	t.Helper()
	dir := t.TempDir()
	runGit(t, dir, "init", "-b", "main")
	runGit(t, dir, "config", "user.name", "test")
	runGit(t, dir, "config", "user.email", "test@example.com")
	runGit(t, dir, "config", "commit.gpgsign", "false")
	writeFile(t, filepath.Join(dir, "main.txt"), "hello\n")
	runGit(t, dir, "add", "-A")
	runGit(t, dir, "commit", "-m", "initial")
	return New(dir), dir
}

// ---- pure functions ------------------------------------------------------

func TestRejectedArtifacts(t *testing.T) {
	files := []string{
		"internal/ok.go",
		"probe.go",
		"probe.txt",
		"tmp_scratch.go",
		"debug_x.go",
		"run.log",
		"a.tmp",
		"a.bak",
		"scratch/notes.md",
		"sub/scratch/deep.txt",
		"docs/readme.md",
	}
	got := RejectedArtifacts(files)
	want := []string{
		"a.bak", "a.tmp", "debug_x.go", "probe.go", "probe.txt",
		"run.log", "scratch/notes.md", "sub/scratch/deep.txt", "tmp_scratch.go",
	}
	if strings.Join(got, ",") != strings.Join(want, ",") {
		t.Fatalf("RejectedArtifacts = %v, want %v", got, want)
	}
	if extra := RejectedArtifacts([]string{"a.go", "b/c.ts"}); len(extra) != 0 {
		t.Fatalf("expected no rejects, got %v", extra)
	}
}

func TestWorkerNaming(t *testing.T) {
	r := New(".")
	if got, want := r.WorkerBranch("w1"), "swarm/swarm-w1"; got != want {
		t.Fatalf("WorkerBranch = %q, want %q", got, want)
	}
	if got, want := r.WorkerBranch("bad name/../x"), "swarm/swarm-bad-name-..-x"; got != want {
		t.Fatalf("WorkerBranch sanitised = %q, want %q", got, want)
	}
	if got := r.WorkerPath("w1"); !strings.HasSuffix(filepath.ToSlash(got), "/.swarm/worktrees/w1") {
		t.Fatalf("WorkerPath = %q", got)
	}
	if got := r.Integration(); got != "integration" {
		t.Fatalf("Integration = %q", got)
	}
	r2 := New(".")
	r2.IntegrationBranch = ""
	if got := r2.Integration(); got != "integration" {
		t.Fatalf("default Integration = %q", got)
	}
	if got := r.IntegrationPath(); !strings.HasSuffix(filepath.ToSlash(got), "/.swarm/integration") {
		t.Fatalf("IntegrationPath = %q", got)
	}
}

// ---- worktree + commit flow ---------------------------------------------

func TestAddWorktreeAndCommitAll(t *testing.T) {
	ctx := context.Background()
	r, _ := newTestRepo(t)

	path, err := r.AddWorktree(ctx, "w1", "")
	if err != nil {
		t.Fatalf("AddWorktree: %v", err)
	}
	if _, err := os.Stat(path); err != nil {
		t.Fatalf("worktree missing: %v", err)
	}
	if got := r.WorktreePath(ctx, r.WorkerBranch("w1")); got == "" {
		t.Fatal("WorktreePath did not find the worker worktree")
	}

	// Idempotent: adding again returns the same path.
	again, err := r.AddWorktree(ctx, "w1", "")
	if err != nil || !strings.EqualFold(again, path) {
		t.Fatalf("AddWorktree second call = %q, %v", again, err)
	}

	// Nothing to commit yet.
	if _, committed, err := r.CommitAll(ctx, path, "noop"); err != nil || committed {
		t.Fatalf("CommitAll on clean tree: committed=%v err=%v", committed, err)
	}

	writeFile(t, filepath.Join(path, "worker.txt"), "from w1\n")
	sha, committed, err := r.CommitAll(ctx, path, "w1: add worker.txt")
	if err != nil || !committed || sha == "" {
		t.Fatalf("CommitAll: sha=%q committed=%v err=%v", sha, committed, err)
	}

	files, err := r.DiffAgainst(ctx, "main", r.WorkerBranch("w1"))
	if err != nil {
		t.Fatalf("DiffAgainst: %v", err)
	}
	if len(files) != 1 || files[0] != "worker.txt" {
		t.Fatalf("DiffAgainst = %v", files)
	}

	if err := r.RemoveWorktree(ctx, "w1"); err != nil {
		t.Fatalf("RemoveWorktree: %v", err)
	}
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatalf("worktree dir still present: %v", err)
	}
	// The branch survives worktree removal.
	if !r.BranchExists(ctx, r.WorkerBranch("w1")) {
		t.Fatal("worker branch was deleted with the worktree")
	}
	if err := r.DeleteBranch(ctx, r.WorkerBranch("w1")); err != nil {
		t.Fatalf("DeleteBranch: %v", err)
	}
	if r.BranchExists(ctx, r.WorkerBranch("w1")) {
		t.Fatal("branch still exists after DeleteBranch")
	}
}
