package prompts

// toolsDirect is the TOOLS block for the direct harness, which has a dedicated
// http tool. It is byte-identical for every worker: it only ever mentions the
// {PEER} placeholder, never a real peer name.
const toolsDirect = `TOOLS: you have three tools.
- "http" - the ONLY way to call the swarm API. Pass ONE string field "request" holding the full JSON request, e.g.
    http({request:"{\"method\":\"GET\",\"path\":\"/board\"}"})
    http({request:"{\"method\":\"GET\",\"path\":\"/board\",\"status\":\"pending\"}"})
    http({request:"{\"method\":\"POST\",\"path\":\"/claim\",\"itemId\":\"<id>\",\"peer\":\"{PEER}\"}"})
    -> returns {token, item, history}: the full task package - item detail AND its past findings/failures. Do NOT re-fetch /item or /context/search for it.
    http({request:"{\"method\":\"POST\",\"path\":\"/submit\",\"token\":\"<token>\",\"peer\":\"{PEER}\",\"commitSha\":\"<sha>\"}"})
    http({request:"{\"method\":\"POST\",\"path\":\"/context\",\"peer\":\"{PEER}\",\"kind\":\"FACT\",\"text\":\"...\" ,\"itemId\":\"<id>\"}"})
  Every POST parameter (itemId, peer, token, commitSha, ...) MUST be inside that JSON string. NEVER use curl/wget for the API (they hang on this host).
- "write" - create or overwrite a file in your worktree. ALWAYS use this for writing files; NEVER write files through the shell (cmd.exe truncates multi-line content and mangles quotes/parens - it will cost you several extra steps to fix).
    write({file:"{\"path\":\"src/m3.js\",\"content\":\"function f(x) {\n  return x;\n}\nmodule.exports = { f };\n\"}"})
  The "file" field is ONE string containing JSON: {"path":"<relative path>","content":"<ENTIRE file text, newlines as \n>"}. Content is written verbatim.
- "bash" - runs in your worktree. The shell is Windows cmd.exe: use dir/type/copy/echo, chain with &&, no ls/cat/head/grep, no 2>/dev/null. Never run interactive commands (they hang).
  Use it for git, running tests, listing files - NOT for writing files (use "write" for that).`

// toolsCLI is the TOOLS block for CLI harnesses (pi/claude/codex), which only have
// file and shell tools: API calls must go through curl (pi's bash is Git Bash).
// Like toolsDirect it stays byte-identical across workers.
const toolsCLI = `TOOLS: you have file and shell tools in your worktree: read, edit, write, bash (and on Windows also powershell).
- Call the swarm API from bash using curl. bash runs Git Bash (Unix syntax: ls/cat/grep/sed and single quotes all work). Keep each JSON payload on one line inside single quotes:
    curl -s "{API}/board"
    curl -s "{API}/digest?peer={PEER}&since=0"
    curl -s -X POST "{API}/claim" -H "Content-Type: application/json" -d '{"itemId":"<id>","peer":"{PEER}"}'
    -> returns {token, item, history}: the full task package - item detail AND its past findings/failures. Do NOT re-fetch /item or /context/search for it.
    curl -s -X POST "{API}/submit" -H "Content-Type: application/json" -d '{"token":"<token>","peer":"{PEER}","commitSha":"<sha>"}'
    curl -s -X POST "{API}/context" -H "Content-Type: application/json" -d '{"peer":"{PEER}","kind":"FACT","text":"...","itemId":"<id>"}'
  If curl is unavailable, use the powershell tool instead:
    Invoke-RestMethod -Method Post -Uri "{API}/claim" -ContentType "application/json" -Body '{"itemId":"<id>","peer":"{PEER}"}'
  Always pass -s to curl; never add -i; never run interactive commands (they hang).`
