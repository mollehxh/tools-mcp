---
name: skill-installer
description: Install Codex skills into $CODEX_HOME/skills or a project's .agents/skills from a GitHub repo path. Use when a user asks to install a skill from another repo (including private repos).
metadata:
  short-description: Install skills from GitHub repositories
---

# Skill Installer

Helps install skills from GitHub repositories, including private repositories.

Use the packaged helper script to install from a GitHub repo/path. The script is an ordinary
workload command; run it through `exec_command` without an MCP installation broker.

After installing a skill, tell the user it will be available on their next turn.

## Script

The built-in package is available at `$MCP_AGENT_SYSTEM_SKILLS_ROOT/skill-installer`.

- Install globally into `$CODEX_HOME/skills` (the script default):
  `python3 "$MCP_AGENT_SYSTEM_SKILLS_ROOT/skill-installer/scripts/install-skill-from-github.py" --repo <owner>/<repo> --path <path/to/skill> [<path/to/skill> ...]`
- Install globally from a URL:
  `python3 "$MCP_AGENT_SYSTEM_SKILLS_ROOT/skill-installer/scripts/install-skill-from-github.py" --url https://github.com/<owner>/<repo>/tree/<ref>/<path>`
- Install into the current project:
  `python3 "$MCP_AGENT_SYSTEM_SKILLS_ROOT/skill-installer/scripts/install-skill-from-github.py" --repo <owner>/<repo> --path <path/to/skill> [<path/to/skill> ...] --dest <workspace>/.agents/skills`

Replace `<workspace>` with the selected workspace's absolute path for a project installation.

## Behavior and Options

- Defaults to direct download for public GitHub repos.
- If download fails with auth/permission errors, falls back to git sparse checkout.
- Aborts if the destination skill directory already exists.
- Global installation targets `$CODEX_HOME/skills/<skill-name>` (the script defaults to
  `~/.codex/skills` when `CODEX_HOME` is unset).
- Project installation targets `<workspace>/.agents/skills/<skill-name>` through `--dest`.
- Multiple `--path` values install multiple skills in one run, each named from the path basename
  unless `--name` is supplied.
- Options: `--ref <ref>` (default `main`), `--dest <path>`, `--method auto|download|git`.

## Notes

- Private GitHub repos can be accessed via existing git credentials or optional
  `GITHUB_TOKEN`/`GH_TOKEN` for download.
- Git fallback tries HTTPS first, then SSH.
