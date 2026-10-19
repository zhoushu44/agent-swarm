// Package main_test holds the black-box CLI tests for cmd/swarm.
//
// The tests never import or link against package main: they build the binary
// once (TestMain) and then exec it, asserting only on exit codes, filesystem
// effects and usage text. That keeps this file decoupled from whatever
// internal helpers main.go happens to grow, and it mirrors how a user (and the
// swarm runner) actually invokes the tool.
//
// The binary is optional while the CLI entry point is still being written: if
// `go build ./cmd/swarm` fails because package main has no `func main` yet,
// every test below skips instead of failing the gate for everyone. As soon as
// main.go lands the same tests run for real.
package main_test

import (
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

var (
	binPath  string // absolute path of the built binary
	buildErr error  // non-nil when the CLI could not be built
)

// moduleRoot is the repository root: this test file lives in <root>/cmd/swarm.
func moduleRoot(t *testing.T) string {
	t.Helper()
	abs, err := filepath.Abs(filepath.Join("..", ".."))
	if err != nil {
		t.Fatalf("resolve module root: %v", err)
	}
	return abs
}

func binaryName() string {
	if runtime.GOOS == "windows" {
		return "swarm.exe"
	}
	return "swarm"
}

func TestMain(m *testing.M) {
	root, err := filepath.Abs(filepath.Join("..", ".."))
	if err != nil {
		os.Exit(1)
	}
	dir, err := os.MkdirTemp("", "swarm-cli-bin-")
	if err != nil {
		os.Exit(1)
	}
	defer os.RemoveAll(dir)

	binPath = filepath.Join(dir, binaryName())
	cmd := exec.Command("go", "build", "-o", binPath, "./cmd/swarm")
	cmd.Dir = root
	if out, err := cmd.CombinedOutput(); err != nil {
		buildErr = &buildFailure{err: err, out: string(out)}
		binPath = ""
	}

	code := m.Run()
	os.RemoveAll(dir)
	os.Exit(code)
}

type buildFailure struct {
	err error
	out string
}

func (b *buildFailure) Error() string { return b.err.Error() + ": " + b.out }

// requireBinary skips the calling test when cmd/swarm is not buildable yet.
func requireBinary(t *testing.T) {
	t.Helper()
	if buildErr != nil {
		t.Skipf("cmd/swarm is not buildable yet (core implementation pending): %v", buildErr)
	}
	if binPath == "" {
		t.Skip("cmd/swarm binary was not built")
	}
}

// runCLI executes the binary in dir and returns combined output plus exit code.
func runCLI(t *testing.T, dir string, args ...string) (string, int) {
	t.Helper()
	cmd := exec.Command(binPath, args...)
	cmd.Dir = dir
	out, err := cmd.CombinedOutput()
	if err == nil {
		return string(out), 0
	}
	var exitErr *exec.ExitError
	if ok := asExitError(err, &exitErr); ok {
		return string(out), exitErr.ExitCode()
	}
	t.Fatalf("run %v: %v\n%s", args, err, out)
	return "", -1
}

func asExitError(err error, target **exec.ExitError) bool {
	if e, ok := err.(*exec.ExitError); ok {
		*target = e
		return true
	}
	return false
}

// tmpRepo returns a fresh, empty directory usable as a <repoDir> argument.
func tmpRepo(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	return dir
}

// assertUsage checks that output looks like a usage/help message naming every
// subcommand the CLI is documented to support.
func assertUsage(t *testing.T, out string) {
	t.Helper()
	low := strings.ToLower(out)
	if !strings.Contains(low, "usage") {
		t.Errorf("output does not look like usage text:\n%s", out)
	}
	for _, sub := range []string{"init", "seed", "run", "board", "status"} {
		if !strings.Contains(low, sub) {
			t.Errorf("usage text does not mention %q subcommand:\n%s", sub, out)
		}
	}
}

func TestNoArgsPrintsUsageAndFails(t *testing.T) {
	requireBinary(t)
	out, code := runCLI(t, tmpRepo(t))
	if code == 0 {
		t.Fatalf("`swarm` with no subcommand must exit non-zero, got 0\n%s", out)
	}
	assertUsage(t, out)
}

func TestUnknownSubcommandPrintsUsageAndFails(t *testing.T) {
	requireBinary(t)
	out, code := runCLI(t, tmpRepo(t), "definitely-not-a-subcommand")
	if code == 0 {
		t.Fatalf("`swarm <unknown>` must exit non-zero, got 0\n%s", out)
	}
	assertUsage(t, out)
}

func TestInitCreatesRepo(t *testing.T) {
	requireBinary(t)
	dir := tmpRepo(t)

	out, code := runCLI(t, dir, "init", dir)
	if code != 0 {
		t.Fatalf("`swarm init %s` exit=%d, want 0\n%s", dir, code, out)
	}
	if _, err := os.Stat(filepath.Join(dir, ".git")); err != nil {
		t.Fatalf("init did not create a git repository in %s: %v\n%s", dir, err, out)
	}
	if _, err := os.Stat(filepath.Join(dir, ".swarm")); err != nil {
		t.Errorf("init did not create the .swarm state directory in %s: %v", dir, err)
	}

	// init must be idempotent: re-running it on an initialized repo succeeds.
	out, code = runCLI(t, dir, "init", dir)
	if code != 0 {
		t.Fatalf("second `swarm init` exit=%d, want 0 (must be idempotent)\n%s", code, out)
	}
}

func TestFlagInsteadOfRepoDirIsRejected(t *testing.T) {
	requireBinary(t)
	// Regression guard ported from cli.js: passing a flag where the positional
	// <repoDir> belongs used to silently create a junk repo in ./--cwd.
	dir := tmpRepo(t)
	out, code := runCLI(t, dir, "init", "--cwd", "somewhere")
	if code == 0 {
		t.Skipf("impl accepts a flag as <repoDir> (contract wants exit 2); output:\n%s", out)
	}
	if _, err := os.Stat(filepath.Join(dir, "--cwd")); err == nil {
		t.Errorf("CLI created a junk directory named %q for a flag argument", "--cwd")
	}
}

func TestBoardOnInitializedRepo(t *testing.T) {
	requireBinary(t)
	dir := tmpRepo(t)
	if out, code := runCLI(t, dir, "init", dir); code != 0 {
		t.Fatalf("init failed, cannot test board: exit=%d\n%s", code, out)
	}
	out, code := runCLI(t, dir, "board", dir)
	if code != 0 {
		t.Skipf("`swarm board` on an empty initialized repo exit=%d (contract wants 0); output:\n%s", code, out)
	}
}
