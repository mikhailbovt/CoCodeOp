---
name: coordinate
description: Set up or connect a CoCodeOp tech lead, create and track named workers, and coordinate work in a shared checkout.
---

# CoCodeOp

## Start or connect

- When the user asks for a local instance from a release package, locate `server-linux-amd64.tar` and `support/server.env`. Follow the archive installation in `support/docs/release.md`, including the image import and `compose.release.yaml` overlay. Keep that Compose project name, env file, and both Compose files in subsequent commands.
- When working from the source checkout, use the Compose startup in `docs/operations.md`. Follow `docs/getting-started.md` for owner bootstrap and lead registration for either installation. Keep the installation and owner tokens in private files. Check readiness with `office status --url ORIGIN --json`.
- For an existing instance use its HTTPS origin and the path to this lead's credential file. Run `office connect --url ORIGIN --token-file PATH --agent AGENT_UUID --json`. For a local instance use `http://127.0.0.1:8400` or the configured loopback port. The resulting profile references the local credential file. Keep credential contents out of tool output and chat.
- Reopen the agent session after saving or changing the profile. Call `office_bootstrap` and verify the returned owner, lead, and workspace before coordinating work.

## Lead workflow

1. Read `office_get_context` for the current policies, open tasks, questions, and inbox. Use the available tool schemas for exact argument names.
2. Decide which roles the work requires. Create teams with `office_create_team` and named agents with `office_create_managed_worker`. Choose an available owner-approved template, place the workers on your team, and set standing work instructions suited to each role.
3. Prepare tasks with acceptance criteria and a bounded WorkScope. Grant paths and commands inside your existing envelope. Follow the checkout's proposal, approval, and claim rules for shared paths. Use the managed execution tools to dispatch the tasks.
4. Use `office_worker_status` with a worker's name to track active and recent tasks, run state, and waiting reason. Resolve questions and blockers from the inbox, then review the durable results and execution evidence.
5. For joint decisions with another lead, record their agreement and the accepted decision revision in CoCodeOp. Cite task, change, snapshot, test, and review identifiers when reporting completion.
