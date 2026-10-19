# swarm-clean - clean up locked/stale swarm worktrees on Windows.
# Use when a swarm run stalls because .swarm/worktrees/wN is held by a live
# process (editor, terminal, antivirus, or a zombie git.exe). Safe by default:
# kills only zombie git processes (empty CommandLine), prunes the worktree
# registry, and removes leftover worktree dirs. The db is kept unless -ResetDB.
param(
    # Project root that contains the .swarm directory. Defaults to current dir.
    [string]$WorkDir = (Get-Location).Path,
    # Also delete .swarm/swarm.db (or state.db) to fully reseed. DESTRUCTIVE - loses board state.
    [switch]$ResetDB
)

$ErrorActionPreference = 'Stop'
$swarmDir = Join-Path $WorkDir '.swarm'
$wtDir    = Join-Path $swarmDir 'worktrees'

if (-not (Test-Path $swarmDir)) {
    Write-Host "No .swarm directory under $WorkDir - nothing to clean."
    exit 0
}

Write-Host "[1/4] Killing zombie git processes (empty CommandLine)..."
$killed = 0
Get-CimInstance Win32_Process -Filter "Name='git.exe'" -ErrorAction SilentlyContinue |
    Where-Object { [string]::IsNullOrWhiteSpace($_.CommandLine) } |
    ForEach-Object {
        try { Stop-Process -Id $_.ProcessId -Force -ErrorAction Stop; $killed++ } catch {}
    }
Write-Host "      killed $killed zombie git process(es)."

Write-Host "[2/4] Pruning git worktree registry..."
Push-Location $WorkDir
try { git worktree prune 2>$null } catch {}
Pop-Location

Write-Host "[3/4] Removing leftover worktree dirs under $wtDir ..."
$removed = 0
if (Test-Path $wtDir) {
    Get-ChildItem $wtDir -Directory -ErrorAction SilentlyContinue | ForEach-Object {
        try { Remove-Item -Recurse -Force $_.FullName -ErrorAction Stop; $removed++ }
        catch { Write-Warning "      still locked: $($_.FullName) - a live process (editor/terminal/AV) is holding it. Close it and re-run." }
    }
}
Write-Host "      removed $removed worktree dir(s)."

if ($ResetDB) {
    Write-Host "[4/4] ResetDB specified - deleting swarm state db ..."
    foreach ($db in @('swarm.db', 'state.db')) {
        Remove-Item (Join-Path $swarmDir $db) -Force -ErrorAction SilentlyContinue
    }
    Write-Host "      db deleted. Reseed before next run."
} else {
    Write-Host "[4/4] swarm db kept (pass -ResetDB to wipe board state)."
}

Write-Host "Done."
