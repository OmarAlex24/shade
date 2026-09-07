---
name: shade-workspaces
description: Operate Shade COW workspaces for host sessions. Use when coding work runs in Shade or needs an isolated agent workspace.
---

# Shade Workspaces

Trust Shade JSON.

1. Run `shade open <repo> --session <chat-id>`. Enter its `cwd` and apply all `SHADE_*` env. Shade keeps the lease alive while your process runs.
2. `shade status --session <id>` reports lifecycle: `attach` resumes an idle session, `sleep` frees its disk, `wake` rebuilds it at a new `cwd`. Only `shade release` deletes work.
3. Use `shade context`; checkpoint risk with `shade checkpoint --reason <reason>`. Fork via `shade fork --session <child-id>`.
4. Sync, restore, refresh and wake return successors: adopt cwd/env together, never delete predecessors.
5. Publish with branch/message; `--push` only if asked. On `conflict`, enter its cwd, resolve Git, run `shade resolve`.
6. On `accepted`, keep `operation_id`; resume events by cursor. On `review_required`, show only the preview; take the user's merge/keep/discard choice.
7. Run `shade release` when done; Shade owns cleanup.
