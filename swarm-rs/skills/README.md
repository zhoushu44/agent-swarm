# Skills

本目录存放随仓库分发的 skill。拉取仓库后，把需要的 skill 链接或复制到你的
Claude Code skill 目录即可被调用：

- **全局**（所有项目可用）：`~/.claude/skills/<name>/`
- **项目级**（仅本仓库）：`<repo>/.claude/skills/<name>/`

Windows 一键安装全局 skill：

```powershell
.\skills\install.ps1
```

## 现有 skill

| 名称 | 用途 |
|---|---|
| [`swarm`](./swarm/SKILL.md) | 跑 agent-swarm 的 Rust 版（swarm-rs）：发起 run、监控、排障。说一句"用 swarm 做 X"即触发 |

## 为什么 skill 要进仓库

skill 里固化的都是**实测得来的操作知识**——哪条命令能跑、哪个坑会浪费一小时、
哪个指标有误导性。这些东西只存在于某个人的 `~/.claude/skills/` 里，
换机器或换人就丢了，而它们的保质期比代码更长。

放在仓库里的代价只是一个目录，收益是任何人 `git pull` 后都拿到同一套操作手册。
