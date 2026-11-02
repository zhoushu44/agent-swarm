# 把仓库里的 skill 安装到全局 skill 目录
#
# 用法：
#   .\skills\install.ps1              # 安装全部
#   .\skills\install.ps1 -Name swarm  # 只装一个
#   .\skills\install.ps1 -Force       # 覆盖已存在的
#
# 安装后重启 Claude Code，说"用 swarm 做 X"即可触发。

[CmdletBinding()]
param(
    [string]$Name,
    [switch]$Force
)

$ErrorActionPreference = 'Stop'

$repoSkills = Join-Path $PSScriptRoot '.'
$globalSkills = Join-Path $HOME '.claude\skills'

if (-not (Test-Path $globalSkills)) {
    New-Item -ItemType Directory -Path $globalSkills -Force | Out-Null
    Write-Host "created $globalSkills"
}

$targets = if ($Name) {
    Get-ChildItem -Path $repoSkills -Directory | Where-Object { $_.Name -eq $Name }
} else {
    Get-ChildItem -Path $repoSkills -Directory | Where-Object {
        Test-Path (Join-Path $_.FullName 'SKILL.md')
    }
}

if (-not $targets) {
    Write-Warning "no skills found$(if ($Name) { " matching '$Name'" })"
    exit 1
}

foreach ($t in $targets) {
    $dest = Join-Path $globalSkills $t.Name
    if (Test-Path $dest) {
        if ($Force) {
            Remove-Item $dest -Recurse -Force
            Write-Host "replaced $($t.Name)"
        } else {
            Write-Host "skip $($t.Name) (already installed; use -Force to overwrite)"
            continue
        }
    }
    Copy-Item -Path $t.FullName -Destination $dest -Recurse -Force
    Write-Host "installed $($t.Name) -> $dest"
}

Write-Host ''
Write-Host 'Restart Claude Code, then say something like "用 swarm 跑这个任务".'
