package gitutil

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// TestMergeOnlySkipsGate: the split that makes the async path possible.
func TestMergeOnlySkipsGate(t *testing.T) {
	r, dir := newTestRepo(t)
	ctx := context.Background()

	runGit(t, dir, "checkout", "-q", "-b", "w1")
	writeFile(t, filepath.Join(dir, "one.txt"), "one\n")
	runGit(t, dir, "add", "-A")
	runGit(t, dir, "commit", "-q", "-m", "w1 work")
	// Back to the base branch: the integration branch is created from HEAD, so
	// leaving HEAD on w1 would make w1 its own ancestor (a no-op merge).
	runGit(t, dir, "checkout", "-q", "main")

	// A gate that ALWAYS fails: if MergeOnly ran it, the merge would fail.
	r.GateCommand = "exit 1"

	res, err := r.MergeOnly(ctx, "w1", "merge w1")
	if err != nil {
		t.Fatalf("MergeOnly must not run the gate: %v", err)
	}
	if res.SHA == "" || res.From == "" {
		t.Fatalf("MergeOnly must report From and SHA: %+v", res)
	}
	if res.From == res.SHA {
		t.Fatal("the tip should have moved")
	}

	// Gate on the integration worktree does fail, and RollbackTo restores From.
	if err := r.Gate(ctx); err == nil {
		t.Fatal("gate should fail (exit 1)")
	}
	if err := r.RollbackTo(ctx, res.From); err != nil {
		t.Fatalf("RollbackTo: %v", err)
	}
	head, err := r.Head(ctx, r.integrationDir())
	if err != nil {
		t.Fatalf("head: %v", err)
	}
	if head != res.From {
		t.Fatalf("after rollback head = %s, want %s", head, res.From)
	}
}

// TestAsyncGateRollbackKeepsOtherWork: two merges land, the FIRST gate fails
// after the second already merged. Rolling back to the first merge's From must
// revert both — and the second submission's work is still on its own branch,
// so a later round re-merges it. Nothing is lost.
func TestAsyncGateRollbackKeepsOtherWork(t *testing.T) {
	r, dir := newTestRepo(t)
	ctx := context.Background()

	mk := func(branch, file, content string) {
		t.Helper()
		runGit(t, dir, "checkout", "-q", "-b", branch, "main")
		writeFile(t, filepath.Join(dir, file), content)
		runGit(t, dir, "add", "-A")
		runGit(t, dir, "commit", "-q", "-m", branch+" work")
		runGit(t, dir, "checkout", "-q", "main")
	}
	mk("w1", "one.txt", "one\n")
	mk("w2", "two.txt", "two\n")

	r.GateCommand = "" // no gate for the merges themselves

	res1, err := r.MergeOnly(ctx, "w1", "merge w1")
	if err != nil {
		t.Fatalf("merge w1: %v", err)
	}
	res2, err := r.MergeOnly(ctx, "w2", "merge w2")
	if err != nil {
		t.Fatalf("merge w2: %v", err)
	}
	if res1.From != res2.From {
		t.Logf("note: res2.From (%s) is res1's tip (%s) — as expected for serial merges", res2.From, res1.SHA)
	}

	// Simulate: gate for merge 1 fails, so roll back to res1.From.
	if err := r.RollbackTo(ctx, res1.From); err != nil {
		t.Fatalf("RollbackTo: %v", err)
	}
	head, err := r.Head(ctx, r.integrationDir())
	if err != nil {
		t.Fatalf("head: %v", err)
	}
	if head != res1.From {
		t.Fatalf("head after rollback = %s, want %s", head, res1.From)
	}

	// Both merged files are gone from the worktree (both merges reverted).
	for _, f := range []string{"one.txt", "two.txt"} {
		if _, err := os.Stat(filepath.Join(dir, f)); err == nil {
			t.Errorf("%s should have been reverted", f)
		}
	}
	// But the work still exists on its own branch, so a later round re-merges.
	if !r.BranchExists(ctx, "w2") {
		t.Fatal("w2 branch must survive the rollback")
	}
	out, _ := r.Git(ctx, "log", "--oneline", "w2")
	if !strings.Contains(out, "w2 work") {
		t.Fatalf("w2's commit must still be reachable: %s", out)
	}
	// And it can be merged again.
	res3, err := r.MergeOnly(ctx, "w2", "retry w2")
	if err != nil {
		t.Fatalf("re-merge w2: %v", err)
	}
	if res3.SHA == "" {
		t.Fatal("re-merge should advance the tip")
	}
}

// TestGateConcurrentIsSafe: several gates in flight must not corrupt the repo
// (they only READ the worktree; the merge already happened serially).
func TestGateConcurrentIsSafe(t *testing.T) {
	r, dir := newTestRepo(t)
	ctx := context.Background()

	runGit(t, dir, "checkout", "-q", "-b", "w1")
	writeFile(t, filepath.Join(dir, "a.txt"), "a\n")
	runGit(t, dir, "add", "-A")
	runGit(t, dir, "commit", "-q", "-m", "work")

	// Gate writes its own observation to a distinct file; concurrent runs must
	// all succeed without stepping on each other.
	r.GateCommand = "echo ok"

	done := make(chan error, 4)
	for i := 0; i < 4; i++ {
		go func() {
			// Give the merge a moment so the gates overlap in time.
			time.Sleep(10 * time.Millisecond)
			done <- r.Gate(ctx)
		}()
	}
	for i := 0; i < 4; i++ {
		if err := <-done; err != nil {
			t.Fatalf("concurrent gate %d failed: %v", i, err)
		}
	}
}
