package harness

// Bash guard: refuse commands that edit files through a script or inline
// interpreter, instead of the write tool.
//
// Measured on a real dola run: workers wrote tmp_patch.mjs, tmp_patch2.mjs,
// tmp_t68_patch.js, tmp_addtests.js and then ran `node tmp_patch.mjs` to edit
// source files. Each edit cost ~4 tool calls (write script -> run -> verify ->
// delete) versus one `write` call. The prompt already says not to; this is the
// tool-level counterpart, because a prompt rule is advisory and the model
// still did it five times in fifteen minutes.
//
// The guard is deliberately narrow: it only blocks the patterns that are
// unambiguously "script edits a file". Running tests, builds, git, and
// read-only node -e one-liners stay allowed.

import (
	"regexp"
	"strings"
)

// scriptRunner matches an interpreter being invoked on a file that is a
// WORKER-WRITTEN helper, not part of the project. The name patterns are what
// distinguishes `node tmp_patch.mjs` (blocked) from `node src/server.js`
// (the project itself — allowed): patch scripts are always tmp_*/_*/scratch or
// named after what they do (patch/fix/edit), because that is what a worker
// improvises, whereas real project files live at stable paths and are meant to
// be executed.
var scriptRunner = regexp.MustCompile(
	`(?i)\b(node|python|python3|py|deno|bun|ts-node|perl|ruby|powershell|pwsh)\s+[^\s|;&]*[/\\]?(tmp_|_|scratch/|debug_|probe\.|patch|fix|edit)[^\s|;&]*\.(mjs|cjs|js|ts|py|pl|rb|ps1)\b`)

// inlineEdit matches interpreter flags that execute a passed-in program, when
// the program looks like it writes files (fs.writeFile / open(...,'w')).
var inlineEdit = regexp.MustCompile(`(?i)\b(node|python3?|py|perl|ruby)\s+(-e|--eval|-c)\b`)

// fsWriteHint are the calls that mean the inline program mutates files.
var fsWriteHint = regexp.MustCompile(
	`(?i)(writeFileSync|writeFile|appendFileSync|appendFile|createWriteStream|rmSync|unlinkSync|mkdirSync|renameSync|copyFileSync|\bopen\s*\([^)]*['"][wax]\+?['"])`)

// sedInPlace / perlPie match the classic in-place editors.
var inplaceEditors = regexp.MustCompile(`(?i)\b(sed|perl)\s+(-i|-[a-z]*i)`)

// GuardVerdict is the outcome of checking one bash command.
type GuardVerdict struct {
	Blocked bool
	Reason  string
	Hint    string
}

const bashGuardHint = "Use the write tool to change file contents (one call), not a script. " +
	"bash is for RUNNING things: tests, builds, git, reads."

// CheckBashCommand reports whether cmd should be refused for editing files
// through a script or inline interpreter.
func CheckBashCommand(cmd string) GuardVerdict {
	c := strings.TrimSpace(cmd)
	if c == "" {
		return GuardVerdict{}
	}
	lower := strings.ToLower(c)

	// 1) interpreter + a script file: node tmp_patch.mjs, python3 fix.py
	if m := scriptRunner.FindString(c); m != "" {
		return GuardVerdict{
			Blocked: true,
			Reason:  "running a script to edit files: " + strings.TrimSpace(m),
			Hint:    bashGuardHint,
		}
	}

	// 2) in-place editors: sed -i, perl -pi -e
	if m := inplaceEditors.FindString(c); m != "" {
		return GuardVerdict{
			Blocked: true,
			Reason:  "in-place file editing: " + strings.TrimSpace(m),
			Hint:    bashGuardHint,
		}
	}

	// 3) inline -e/-c program that writes files: node -e "fs.writeFileSync(...)"
	if inlineEdit.MatchString(lower) && fsWriteHint.MatchString(c) {
		return GuardVerdict{
			Blocked: true,
			Reason:  "inline interpreter code that writes files",
			Hint:    bashGuardHint,
		}
	}

	// 4) shell redirection into a source file: echo ... > src/x.js
	//    (allowed: > /dev/null, > *.txt, > *.log, and >> into logs)
	if blocked, why := checkRedirect(lower); blocked {
		return GuardVerdict{Blocked: true, Reason: why, Hint: bashGuardHint}
	}

	return GuardVerdict{}
}

// redirectToSource matches `>` / `>>` into a path with a source-ish extension.
var redirectToSource = regexp.MustCompile(`>{1,2}\s*["']?[^\s|&;"']+\.(mjs|cjs|js|ts|jsx|tsx|go|py|java|c|h|cc|cpp|rs|rb|php|cs|swift|kt|scala|json|ya?ml|toml|md|css|html?|xml|sql)\b`)

// safeSink matches redirections that are not source edits.
var safeSink = regexp.MustCompile(`(?i)>{1,2}\s*/dev/null\b`)

func checkRedirect(lower string) (bool, string) {
	// Strip the safe sinks first so `cmd > /dev/null 2>&1` never trips.
	cleaned := safeSink.ReplaceAllString(lower, "")
	// Output/scratch files are how workers inspect things; allow them.
	for _, ext := range []string{".txt", ".log", ".out", ".tmp", ".json.tmp"} {
		cleaned = strings.ReplaceAll(cleaned, ext, " ")
	}
	if m := redirectToSource.FindString(cleaned); m != "" {
		return true, "shell redirection into a source file: " + strings.TrimSpace(m)
	}
	return false, ""
}
