# AGENTS.md / CLAUDE.md snippet

Paste the block below into your project's `AGENTS.md` or `CLAUDE.md` so agents
open Shade workspaces instead of creating their own linked worktrees. It only
redirects worktrees an *agent* would create; see
[MIGRATING.md](MIGRATING.md) for what it does not cover.

---

```markdown
## Workspaces

This project uses Shade for isolated agent workspaces. Do not run
`git worktree add`.

- **Start work:** `shade open <absolute-repo-path> --session <task-id>` from the
  main repository, optionally with `--base <branch-or-commit>`. Enter the `cwd`
  it returns and export every `SHADE_*` variable from its `env`. Never `cd` into
  another session's workspace.
- **Keep the lease:** `shade open` starts a keepalive tied to the agent process
  that called it (`claude`, `codex`, `cursor`, `cursor-agent`, `kimi`,
  `zumith`). From a plain shell, a script or CI, pass `--owner-pid <pid>` or
  `--owner-name <name>`, or call `shade heartbeat` at least every 120 seconds.
- **Check state:** `shade context` for the workspace (HEAD, changes,
  `dependencies.state`), `shade status --session <id>` for the lifecycle.
- **Save a risky point:** `shade checkpoint --reason <reason>`.
- **Park a branch:** `shade sleep`. It checkpoints, then gives the tree back to
  the filesystem so the branch costs no disk. `shade wake --session <id>`
  rebuilds it at a new `cwd` — read the new `cwd` and `env` from the result,
  never reuse the old ones.
- **Come back after going idle:** `shade attach --session <id>`. A session
  without a heartbeat for 120 seconds goes `dormant`, which is normal and loses
  nothing.
- **Ship:** `shade publish --branch <branch> --message <message>`. Add `--push`
  only when the user asks.
- **Never run `shade release` on your own.** It is the only command that ends a
  session and makes its work collectible. Ask the user first, every time. Use
  `shade sleep` when you merely want the disk back.
- Gitignored build output (`node_modules`, `target`, `dist`) is not preserved
  across `sleep`/`wake`; Shade re-links the shared dependency layer instead. Do
  not store anything you need in a gitignored directory.
```

---

## Turning off Claude Code's own worktree feature

Claude Code has a native worktree feature, so a project that uses Shade should
switch it off rather than end up with both. There is no single toggle; two
settings and one habit cover it.

Add to `.claude/settings.json` (project) or `~/.claude/settings.json` (user):

```json
{
  "worktree": {
    "bgIsolation": "none"
  },
  "permissions": {
    "deny": ["EnterWorktree"]
  }
}
```

- `worktree.bgIsolation: "none"` stops background sessions from being isolated
  into a worktree; they work in the checkout directly, which is what you want
  when Shade provides the isolation.
- Denying the `EnterWorktree` tool stops Claude from creating or entering a
  worktree during a session.
- Do not start the CLI with `--worktree` / `-w`, which creates one at startup.

Verified against the Claude Code settings reference
(`https://code.claude.com/docs/en/settings-reference.md`), the worktrees page
(`https://code.claude.com/docs/en/worktrees.md`) and the tools reference
(`https://code.claude.com/docs/en/tools-reference.md`). Check the version you
run: these keys are current documentation, not a stable contract.
