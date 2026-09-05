---
name: shade-workspaces
description: Operate Shade COW workspaces for host sessions. Use when coding work runs in Shade or needs an isolated agent workspace.
---

# Shade Workspaces

Trust Shade JSON.

1. Run `shade open <repo> --session <chat-id>`. Enter its `cwd` and apply all returned `SHADE_*` env. SDKs heartbeat; standalone CLI owners run `shade heartbeat` every 30s.
2. Use `shade context`; checkpoint risk with `shade checkpoint --reason <reason>`. Fork via `shade fork --session <child-id>`.
3. Sync, restore and dependency refresh return successors. The SDK/CLI confirms their durable handoff; adopt cwd/env together and never delete predecessors.
4. Publish with branch/message; use `--push` only when asked. On `conflict`, enter its cwd, resolve Git and run `shade resolve`.
5. On `accepted`, keep `operation_id` and resume events by cursor. On `review_required`, show only the preview and resolve with the user's merge/keep/discard choice.
6. Run `shade release`; Shade owns cleanup and private refs.
