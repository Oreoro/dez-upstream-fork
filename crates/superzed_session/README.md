# Super Zed host session architecture

This crate is the server-owned model for a single Super Zed host. A host session owns an ordered
list of workspaces. Each workspace owns one lightweight project specification and one pane-layout
snapshot. Project IDs are dynamically allocated, nonzero, and never reused during a host session.

The implementation must use the current `~/ref/herdr` checkout as its behavioral reference. Do
not infer Herdr behavior from memory, old branches, backups, or checkpoints. Before changing the
corresponding Super Zed surface, read these current Herdr files:

- Server lifetime and client attachment: `src/server/mod.rs`, `src/server/headless.rs`,
  `src/server/client_accept.rs`, `src/server/client_transport.rs`, and `src/server/socket_paths.rs`.
- Session naming and per-session paths: `src/session.rs`.
- Pure session/workspace state: `src/app/state.rs`, `src/workspace.rs`, and
  `src/workspace/tab.rs`.
- Pane topology and stable pane identity: `src/layout.rs` and `src/pane/state.rs`.
- Snapshot, restore, and atomic persistence: `src/persist/snapshot.rs`,
  `src/persist/restore.rs`, and `src/persist/io.rs`.
- Server-facing workspace operations and events: `src/api/schema/workspaces.rs`,
  `src/app/api/workspaces.rs`, and `src/api/event_hub.rs`.
- Terminal runtime ownership for the next milestone: `src/terminal/runtime_registry.rs`,
  `src/terminal/runtime.rs`, `src/server/terminal_attach.rs`, and `src/pty/actor.rs`.
- Agent integration for its later milestone: `src/app/api/agents.rs`,
  `src/api/schema/agents.rs`, `src/pane/agent_detection.rs`, and `src/agent_resume.rs`.

Super Zed copies these Herdr boundaries:

- The persistent server owns shared session facts; the GUI is a reconnectable client.
- State is serializable independently from terminal/editor runtimes.
- Workspace, pane, and item identities are stable protocol data rather than UI indexes.
- Mutations are explicit, validated, revisioned, and persisted before becoming authoritative.
- Closing the final workspace creates a new blank workspace instead of leaving an invalid session.

Super Zed intentionally differs in these places:

- It keeps Zed's `Workspace`, `PaneGroup`, editor, project, LSP, Git, debugger, and collaboration
  implementations instead of copying Herdr's TUI or BSP implementation.
- A workspace project may contain multiple canonical root directories.
- Expensive resources are host-canonical. Overlapping projects attach to shared worktrees,
  buffers, repositories, and compatible language-server runtimes; project membership must not
  create a second filesystem watcher or second buffer for the same canonical resource.
- Milestone 1 persists file-editor tabs and Zed pane topology. Terminals remain client-owned until
  the server-owned PTY/runtime milestone, which must follow the Herdr runtime references above.
- There is one RPC connection per host, local or remote. Projects are routing scopes on that
  connection, never connection owners.

The SQLite database is host-local and authoritative for session structure. Zed's existing client
workspace database may retain presentation history, but it must not become a second authority for
the Super Zed workspace list, project IDs, or persisted pane topology.
