package gitutil

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// setupConflict creates the classic contention: integration changes one line,
// the incoming branch changes the same line differently. It reuses the repo
// fixture from gitutil_test.go (branch "main", file "main.txt").
func setupConflict(t *testing.T, r *Repo, dir string) (integration, branch string) {
	t.Helper()

	// integration branch: change the line to 2
	runGit(t, dir, "checkout", "-q", "-b", "integration")
	writeFile(t, filepath.Join(dir, "main.txt"), "value = 2\n")
	runGit(t, dir, "commit", "-q", "-am", "integration change")

	// worker branch from main: change the same line to 3
	runGit(t, dir, "checkout", "-q", "-b", "worker", "main")
	writeFile(t, filepath.Join(dir, "main.txt"), "value = 3\n")
	runGit(t, dir, "commit", "-q", "-am", "worker change")
	runGit(t, dir, "checkout", "-q", "integration")

	return "integration", "worker"
}

// TestConflictedFilesExtractsThreeWayView verifies the arbitration input.
func TestConflictedFilesExtractsThreeWayView(t *testing.T) {
	r, dir := newTestRepo(t)
	ctx := context.Background()
	integration, branch := setupConflict(t, r, dir)

	// merge the worker branch into integration, expect a conflict
	cmd := exec.Command("git", "checkout", "-q", integration)
	cmd.Dir = dir
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("checkout: %v\n%s", err, out)
	}
	merge := exec.Command("git", "merge", "--no-ff", "--no-edit", branch)
	merge.Dir = dir
	_, _ = merge.CombinedOutput() // expected to conflict

	if !r.HasConflicts(ctx, dir) {
		t.Fatal("expected conflicts after merge")
	}

	files, err := r.ConflictedFiles(ctx, dir, "")
	if err != nil {
		t.Fatalf("ConflictedFiles: %v", err)
	}
	if len(files) != 1 || files[0].Path != "main.txt" {
		t.Fatalf("want 1 conflicted main.txt, got %+v", files)
	}
	cf := files[0]
	if !strings.Contains(cf.Ours, "value = 2") {
		t.Errorf("ours should hold the integration version, got %q", cf.Ours)
	}
	if !strings.Contains(cf.Theirs, "value = 3") {
		t.Errorf("theirs should hold the worker version, got %q", cf.Theirs)
	}
	if !strings.Contains(cf.Markers, "<<<<<<<") {
		t.Errorf("markers view should carry conflict markers, got %q", cf.Markers)
	}
	if !strings.Contains(cf.Base, "hello") {
		t.Errorf("base should hold the pre-branch version, got %q", cf.Base)
	}

	// The prompt must contain all three views.
	prompt := MergePromptFor(cf)
	for _, want := range []string{"BASE", "OURS", "THEIRS", "hello", "value = 2", "value = 3"} {
		if !strings.Contains(prompt, want) {
			t.Errorf("prompt missing %q", want)
		}
	}
}

// TestArbitrateConflictsResolvesAndStages drives the full path with a fake
// resolver, then commits and checks the tree.
func TestArbitrateConflictsResolvesAndStages(t *testing.T) {
	r, dir := newTestRepo(t)
	ctx := context.Background()
	_, branch := setupConflict(t, r, dir)

	cmd := exec.Command("git", "checkout", "-q", "integration")
	cmd.Dir = dir
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("checkout: %v\n%s", err, out)
	}
	merge := exec.Command("git", "merge", "--no-ff", "--no-edit", branch)
	merge.Dir = dir
	_, _ = merge.CombinedOutput()

	files, err := r.ConflictedFiles(ctx, dir, "")
	if err != nil {
		t.Fatalf("ConflictedFiles: %v", err)
	}

	// Fake resolver: keep both intents by concatenating the return statements.
	resolver := func(_ context.Context, cf ConflictFile) (string, error) {
		return "value = 2 # merged, theirs wanted 3\nextra = 3\n", nil
	}
	n, err := r.ArbitrateConflicts(ctx, dir, files, resolver)
	if err != nil {
		t.Fatalf("ArbitrateConflicts: %v", err)
	}
	if n != 1 {
		t.Fatalf("resolved %d files, want 1", n)
	}
	if r.HasConflicts(ctx, dir) {
		t.Fatal("conflicts should be gone")
	}

	// The resolved content must be on disk (the gate runs a real build).
	onDisk, err := os.ReadFile(filepath.Join(dir, "main.txt"))
	if err != nil {
		t.Fatalf("read resolved: %v", err)
	}
	if !strings.Contains(string(onDisk), "theirs wanted 3") {
		t.Fatalf("resolved content not written to disk: %q", onDisk)
	}

	// And staged, so the follow-up commit picks it up.
	staged, err := r.GitIn(ctx, dir, "diff", "--cached", "--name-only")
	if err != nil || !strings.Contains(staged, "main.txt") {
		t.Fatalf("resolved file not staged: %q (%v)", staged, err)
	}
}

// TestArbitrateRejectsMarkersAndEmpty: a resolver that does not actually merge
// must fail loudly rather than commit garbage.
func TestArbitrateRejectsMarkersAndEmpty(t *testing.T) {
	r, dir := newTestRepo(t)
	ctx := context.Background()
	_, branch := setupConflict(t, r, dir)

	cmd := exec.Command("git", "checkout", "-q", "integration")
	cmd.Dir = dir
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("checkout: %v\n%s", err, out)
	}
	merge := exec.Command("git", "merge", "--no-ff", "--no-edit", branch)
	merge.Dir = dir
	_, _ = merge.CombinedOutput()
	files, _ := r.ConflictedFiles(ctx, dir, "")

	// 1) echoes the markers back
	if _, err := r.ArbitrateConflicts(ctx, dir, files, func(_ context.Context, cf ConflictFile) (string, error) {
		return cf.Markers, nil
	}); err == nil {
		t.Fatal("must reject a resolver that leaves conflict markers")
	}

	// 2) returns nothing
	if _, err := r.ArbitrateConflicts(ctx, dir, files, func(_ context.Context, _ ConflictFile) (string, error) {
		return "   ", nil
	}); err == nil {
		t.Fatal("must reject an empty resolution")
	}
}

// TestIntegrateArbitrates: end to end — a conflict that the resolver fixes
// must reach the integration tip instead of being rejected.
func TestIntegrateArbitrates(t *testing.T) {
	r, dir := newTestRepo(t)
	ctx := context.Background()
	_, branch := setupConflict(t, r, dir)

	r.Resolver = func(_ context.Context, _ ConflictFile) (string, error) {
		return "value = 2\nextra = 3\n", nil
	}
	r.GateCommand = "" // no gate: focus on the merge path

	res, err := r.Integrate(ctx, branch, "arbitrated merge")
	if err != nil {
		t.Fatalf("Integrate should succeed via arbitration: %v", err)
	}
	if !res.Arbitrated {
		t.Fatal("result must be flagged as arbitrated")
	}
	if res.SHA == "" {
		t.Fatal("integration tip should have advanced")
	}
	// The tip must contain the arbitrated content.
	out, err := r.Git(ctx, "show", res.SHA+":main.txt")
	if err != nil {
		t.Fatalf("show: %v", err)
	}
	if !strings.Contains(out, "extra =") {
		t.Fatalf("arbitrated content missing from the tip: %q", out)
	}
}

// TestIntegrateWithoutResolverStillRejects: the default behaviour is unchanged.
func TestIntegrateWithoutResolverStillRejects(t *testing.T) {
	r, _ := newTestRepo(t)
	ctx := context.Background()
	_, branch := setupConflict(t, r, r.Dir)
	r.Resolver = nil

	if _, err := r.Integrate(ctx, branch, "should conflict"); err == nil {
		t.Fatal("without a resolver the conflict must still reject")
	}
}

// TestParseMergedFileStripsFences: models wrap output in fences despite the
// instruction; the parser must recover the content.
func TestParseMergedFileStripsFences(t *testing.T) {
	in := "```txt\nvalue = 1\n```"
	got := ParseMergedFile(in)
	if strings.Contains(got, "```") || !strings.Contains(got, "value = 1") {
		t.Fatalf("fences not stripped: %q", got)
	}

	plain := "value = 1\n"
	if got := ParseMergedFile(plain); got != plain {
		t.Fatalf("plain content must pass through untouched: %q", got)
	}

	// Structured replies are accepted too.
	structured := `{"path":"a.js","merged":"x = 1;\n","reason":"combined both"}`
	if got, err := DecodeMerged(structured); err != nil || got != "x = 1;\n" {
		t.Fatalf("DecodeMerged(structured) = %q, %v", got, err)
	}
}

// TestWriteWorktreeFileRejectsEscape: the resolver output is untrusted input.
func TestWriteWorktreeFileRejectsEscape(t *testing.T) {
	r, dir := newTestRepo(t)
	ctx := context.Background()
	for _, bad := range []string{"../escape.js", "/etc/passwd", "..\\..\\escape.js"} {
		if err := r.writeWorktreeFile(ctx, dir, bad, "x"); err == nil {
			t.Fatalf("must reject %q", bad)
		}
	}
	// A normal nested path is fine.
	if err := r.writeWorktreeFile(ctx, dir, "src/nested/ok.js", "ok\n"); err != nil {
		t.Fatalf("legit path rejected: %v", err)
	}
	if _, err := os.Stat(filepath.Join(dir, "src", "nested", "ok.js")); err != nil {
		t.Fatalf("file not written: %v", err)
	}
}
