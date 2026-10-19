package gitutil

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func commitWorkerFile(t *testing.T, r *Repo, worker, name, content, msg string) string {
	t.Helper()
	ctx := context.Background()
	path, err := r.AddWorktree(ctx, worker, "")
	if err != nil {
		t.Fatalf("AddWorktree(%s): %v", worker, err)
	}
	writeFile(t, filepath.Join(path, name), content)
	if _, _, err := r.CommitAll(ctx, path, msg); err != nil {
		t.Fatalf("CommitAll(%s): %v", worker, err)
	}
	return path
}

func TestIntegrateMergesAndRunsGate(t *testing.T) {
	ctx := context.Background()
	r, _ := newTestRepo(t)
	r.GateCommand = "git rev-parse HEAD" // succeeds

	commitWorkerFile(t, r, "w1", "worker.txt", "from w1\n", "w1: add worker.txt")

	res, err := r.Integrate(ctx, r.WorkerBranch("w1"), "integrate w1")
	if err != nil {
		t.Fatalf("Integrate: %v", err)
	}
	if res.Noop || res.SHA == "" || res.SHA == res.From {
		t.Fatalf("unexpected result: %+v", res)
	}
	if len(res.Files) != 1 || res.Files[0] != "worker.txt" {
		t.Fatalf("files = %v", res.Files)
	}

	// The integration branch really moved, and the file is on it.
	got, err := r.RevParse(ctx, "refs/heads/integration")
	if err != nil {
		t.Fatalf("RevParse integration: %v", err)
	}
	if got != res.SHA {
		t.Fatalf("integration tip = %s, want %s", got, res.SHA)
	}
	if _, err := os.Stat(filepath.Join(r.IntegrationPath(), "worker.txt")); err != nil {
		t.Fatalf("merged file missing from integration worktree: %v", err)
	}

	// Merging the same branch again is a no-op.
	res2, err := r.Integrate(ctx, r.WorkerBranch("w1"), "integrate w1 again")
	if err != nil {
		t.Fatalf("second Integrate: %v", err)
	}
	if !res2.Noop || res2.SHA != res.SHA {
		t.Fatalf("expected noop, got %+v", res2)
	}
}

func TestIntegrateGateFailureRollsBack(t *testing.T) {
	ctx := context.Background()
	r, _ := newTestRepo(t)
	r.GateCommand = "git rev-parse --verify refs/heads/does-not-exist" // fails

	before, err := r.RevParse(ctx, "refs/heads/main")
	if err != nil {
		t.Fatalf("RevParse: %v", err)
	}

	commitWorkerFile(t, r, "w1", "worker.txt", "from w1\n", "w1: add worker.txt")

	res, err := r.Integrate(ctx, r.WorkerBranch("w1"), "integrate w1")
	if err == nil {
		t.Fatalf("expected gate failure, got %+v", res)
	}
	var gateErr *ErrGate
	if !errors.As(err, &gateErr) {
		t.Fatalf("expected *ErrGate, got %T: %v", err, err)
	}

	// The integration branch must be back at its original tip.
	after, err := r.RevParse(ctx, "refs/heads/integration")
	if err != nil {
		t.Fatalf("RevParse integration: %v", err)
	}
	if after != before {
		t.Fatalf("integration moved despite gate failure: %s -> %s", before, after)
	}

	wt := r.IntegrationPath()
	dirty, err := r.IsDirty(ctx, wt)
	if err != nil {
		t.Fatalf("IsDirty: %v", err)
	}
	if dirty {
		t.Fatal("integration worktree left dirty after rollback")
	}
	if _, err := os.Stat(filepath.Join(wt, "worker.txt")); !os.IsNotExist(err) {
		t.Fatalf("merged file left behind after rollback: %v", err)
	}
}

func TestIntegrateRejectsScratchArtifacts(t *testing.T) {
	ctx := context.Background()
	r, _ := newTestRepo(t)
	r.GateCommand = "git rev-parse HEAD"

	mainTip, err := r.RevParse(ctx, "refs/heads/main")
	if err != nil {
		t.Fatalf("RevParse main: %v", err)
	}

	commitWorkerFile(t, r, "w1", "tmp_probe.go", "package tmp\n", "w1: scratch")

	if _, err := r.Integrate(ctx, r.WorkerBranch("w1"), "integrate w1"); err == nil {
		t.Fatal("expected Integrate to reject scratch artifacts")
	} else if !strings.Contains(err.Error(), "scratch") {
		t.Fatalf("unexpected error: %v", err)
	}

	// Nothing was merged: the integration branch is still at the main tip.
	tip, err := r.RevParse(ctx, "refs/heads/integration")
	if err != nil {
		t.Fatalf("RevParse integration: %v", err)
	}
	if tip != mainTip {
		t.Fatalf("integration tip = %s, want untouched %s", tip, mainTip)
	}
}

func TestIntegrateUnknownBranch(t *testing.T) {
	ctx := context.Background()
	r, _ := newTestRepo(t)
	if _, err := r.Integrate(ctx, "swarm/swarm-nobody", ""); err == nil {
		t.Fatal("expected error for unknown branch")
	}
}

func TestCommitAllAndHead(t *testing.T) {
	ctx := context.Background()
	r, dir := newTestRepo(t)

	head, err := r.Head(ctx, "")
	if err != nil || head == "" {
		t.Fatalf("Head: %q %v", head, err)
	}
	branch, err := r.CurrentBranch(ctx)
	if err != nil || branch != "main" {
		t.Fatalf("CurrentBranch = %q, %v", branch, err)
	}

	writeFile(t, filepath.Join(dir, "second.txt"), "two\n")
	sha, committed, err := r.CommitAll(ctx, "", "add second.txt")
	if err != nil || !committed || sha == head {
		t.Fatalf("CommitAll: sha=%q committed=%v err=%v", sha, committed, err)
	}
	if got, err := r.Head(ctx, ""); err != nil || got != sha {
		t.Fatalf("Head after commit = %q, %v", got, err)
	}

	files, err := r.ChangedFiles(ctx, head, sha)
	if err != nil {
		t.Fatalf("ChangedFiles: %v", err)
	}
	if len(files) != 1 || files[0] != "second.txt" {
		t.Fatalf("ChangedFiles = %v", files)
	}

	if !IsRepo(dir) {
		t.Fatal("IsRepo = false for a real repository")
	}
	if IsRepo(filepath.Join(dir, "nope")) {
		t.Fatal("IsRepo = true for a missing directory")
	}

	// RunGate with no command configured is a no-op success.
	if err := r.RunGate(ctx, dir); err != nil {
		t.Fatalf("RunGate(empty) = %v", err)
	}
	r.GateCommand = "git rev-parse --verify refs/heads/does-not-exist"
	if err := r.RunGate(ctx, dir); err == nil {
		t.Fatal("RunGate should fail for a failing command")
	}
}
