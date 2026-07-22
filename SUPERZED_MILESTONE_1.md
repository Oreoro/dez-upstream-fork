# Super Zed Milestone 1 Implementation Contract

This document is both the implementation plan and the definition of done. A checkpoint may be
reported as complete when its own gate passes. **Milestone 1 may not be reported as complete until
every required acceptance test in this document passes.**

The implementation starts from the current working tree only. Never inspect Git branches, Git
history, backups, checkpoints, or old implementations. Herdr behavior must be read from the current
checkout at `/Users/maxktz/ref/herdr`.

## Goal

Deliver one coherent Super Zed shell in which each persistent host server owns its workspaces,
project scopes, and saved editor layout while Zed remains the editor UI.

The finished application must provide:

- One automatically connected local host and manually connected SSH hosts.
- One persistent server process and one RPC connection per connected host.
- Multiple ordered workspaces per host in the existing global sidebar that also contains chats.
- One Zed `Workspace` projection per server workspace, with Zed panes and editors in the content
  area.
- A project scope containing zero or more ordered root directories in every workspace.
- Server persistence of workspace identity, order, active workspace, project roots, pane topology,
  editor tabs, active pane/item, pinned state, and preview state.
- Shared host resources for overlapping project scopes.
- A detached host server that survives GUI exit and restores the same session on reconnect.

This is a usable vertical slice, not the final Super Zed product.

## Scope limits

Milestone 1 includes:

- macOS GUI;
- Unix local and SSH servers;
- one GUI process, one application window, and one unnamed/default session per host;
- one local host plus at least one SSH host in automated or configured acceptance testing;
- file editor items and empty panes in saved layouts;
- existing Zed editor, project panel, Git, search, LSP, task, and debugger behavior inside each
  workspace;
- generic workspace labels derived from stable IDs;
- create, activate, close, set project roots, add project root, and replace layout operations.

Milestone 1 does **not** include:

- server-owned PTYs or terminal process persistence;
- terminal screen streaming;
- Herdr tabs or Herdr's BSP layout;
- agent integration;
- workspace rename or reorder UI;
- simultaneous GUI clients;
- multiple Super Zed windows;
- named host sessions;
- unsaved buffer contents across restart;
- migration from any installed Zed database or user-data namespace;
- a custom replacement sidebar;
- rendered-frame streaming from the server.

Existing terminals may work while the GUI is attached. They may disappear on detach or relaunch,
but their disappearance must not corrupt the saved pane topology.

## Non-negotiable design rules

1. **One production path.** Super Zed startup, workspace creation, activation, closing, project
   opening, and restoration have exactly one production path: the host-session path described here.
   There is no fallback to Zed workspace restoration and no second local-workspace path.
2. **The server is authoritative.** The GUI never commits workspace membership, order, project
   roots, or layout on its own. It projects a validated server snapshot.
3. **The host owns the connection.** A project borrows the host RPC client for routed project
   traffic. A project never connects, reconnects, displays host status, or shuts down the host.
4. **One client mutation queue per host.** Create, activate, close, project-root, and layout writes
   are serialized against one current session revision. UI handlers never perform their own
   `GetSuperzedSession`/`MutateSuperzedSession` sequence.
5. **One reconciler.** Only the host-session reconciler may add, remove, reorder, or activate GPUI
   workspace projections in response to server state.
6. **No silent failures.** User operations either finish and reconcile the returned snapshot or
   display a concrete error. Do not detach fallible user actions with log-only error handling.
7. **No parallel metadata model.** Stable host, workspace, and project identity is attached when a
   projection is constructed. Do not use `EntityId` as persistent identity and do not maintain an
   optional `EntityId -> SuperzedWorkspaceMetadata` side table.
8. **No client topology authority.** Host-session workspaces do not restore or save their list,
   project IDs, or pane topology through `WorkspaceDb`. The recent-project database may remain a
   path-history source only.
9. **No legacy UI semantics.** Selecting a project never implicitly closes or detaches a workspace.
   Clicking a workspace row only activates it. Closing occurs only through the close command.
10. **No visible pre-connection UI.** The application creates and shows no window, loading screen,
    connection modal, or error dialog before the local host handshake, snapshot validation, project
    initialization, and initial reconciliation succeed. Startup failure logs a clear error and exits
    nonzero.
11. **Prefer deletion and reuse.** Replace conflicting Super Zed glue instead of adding adapters on
    top of it. Keep Zed's editor/project components and Herdr's ownership behavior; do not create a
    second editor, project system, sidebar, transport, or persistence model.

Test-only constructors may use an in-memory host-session implementation. Production code may not
construct a standalone or legacy `MultiWorkspace`.

## Source-reference contract

Before implementing a row, read the listed files from the two current working trees. The
implementation report for that checkpoint must name the files consulted. Do not copy behavior from
memory.

| Concern | Herdr behavioral reference | Super Zed integration seam |
| --- | --- | --- |
| Single owner, ordered workspaces, create/switch/close | `src/app/state.rs`, `src/app/actions.rs`, `src/app/creation.rs`, `src/workspace.rs` | `crates/superzed_session`, `crates/workspace/src/host_session.rs`, `multi_workspace.rs` |
| Dirty state, capture, save, restore | `src/app/session.rs`, `src/persist/snapshot.rs`, `src/persist/io.rs`, `src/persist/restore.rs` | `crates/superzed_session/src/persistence.rs`, `Workspace::serialize_workspace` |
| Detached server and client handshake | `src/server/headless.rs`, `src/server/client_accept.rs`, `src/server/client_transport.rs`, `src/server/clients.rs`, `src/client/mod.rs`, `src/protocol/wire.rs` | `crates/remote_server/src/server.rs`, `superzed_host.rs`, `crates/remote/src/remote_client.rs` |
| Explicit workspace commands | `src/app/api/workspaces.rs`, `src/api/schema/workspaces.rs`, `src/api/event_hub.rs` | `SessionMutation`, `GetSuperzedSession`, `MutateSuperzedSession` |
| Sidebar behavior | `src/ui/sidebar.rs`, `src/app/input/sidebar.rs` | existing `crates/sidebar/src/sidebar.rs` |
| Runtime/state separation | `src/terminal/runtime_registry.rs`, `src/terminal/runtime.rs`, `src/server/terminal_attach.rs` | server project registry and `HostResourceRegistry`; PTYs remain deferred |
| Zed pane projection | Herdr is not the source for this | `crates/workspace/src/pane_group.rs`, `workspace.rs` serialization/restoration |
| Zed project RPC and resources | Herdr is not the source for this | `crates/project`, `crates/remote_server/src/headless_project.rs` |

Copy these Herdr rules:

- The persistent server owns shared session facts and runtimes.
- The client attaches to an already running server or starts one, then receives authoritative state.
- Create, activate, and close are explicit mutations of one state owner.
- Detaching a client does not destroy the server state.
- Serializable topology is separate from reconstructed runtime objects.
- Closing removes the workspace and its owned runtime state; it is never a UI-only detach.

Do not copy these Herdr details:

- terminal rendering, raw input, or rendered-frame transport;
- terminal runtimes in this milestone;
- the tab layer;
- the binary BSP tree;
- agent detection or resume;
- JSON persistence or Herdr snapshot migrations.

## Required architecture

```text
SuperZed application shell (one MultiWorkspace window)
├── connected HostSessionClient: local
├── connected HostSessionClient: SSH host A
└── active (HostId, WorkspaceId)

HostSessionClient
├── HostId and display name
├── one strong RemoteClient connection owner
├── latest validated HostSessionSnapshot
├── one serialized mutation queue
├── WorkspaceId -> WorkspaceProjection
└── reconcile(snapshot)

WorkspaceProjection
├── required HostWorkspaceIdentity
│   ├── HostId
│   ├── WorkspaceId
│   └── ProjectId
├── Entity<Project> remote proxy
└── Entity<Workspace> with PaneGroup and editor items

Host server
├── HostSessionDb
├── authoritative HostSessionSnapshot
├── ProjectId -> HeadlessProject
└── HostResourceRegistry
    ├── canonical worktrees and filesystem scanners
    ├── canonical buffers
    ├── Git repositories by common directory
    └── compatible language-server processes
```

### HostSessionClient

Add one logical client component, preferably `crates/workspace/src/host_session.rs`. Do not add a
new crate or a new shell.

It owns:

- the host identity and connection;
- the last validated server snapshot;
- the stable projection map;
- the in-flight mutation queue;
- connection/reconnection state exposed to the global sidebar;
- all reconciliation of server snapshots into `MultiWorkspace`.

Its public mutation API is the only API used by production UI code:

```text
create_workspace(project_spec)
activate_workspace(workspace_id)
close_workspace(workspace_id)
set_project_roots(workspace_id, project_spec)
add_project_root(workspace_id, root)
replace_layout(workspace_id, expected_layout_revision, layout)
```

Each call:

1. enters the per-host queue;
2. builds a mutation using the controller's current revision;
3. sends it once;
4. validates the returned full snapshot;
5. reconciles that snapshot;
6. completes only after the affected projection is usable;
7. returns a UI-visible error on failure.

There is one supported GUI client per host in this milestone. Therefore no server-push event bus is
required: a full snapshot response after each serialized mutation plus a full snapshot on reconnect
is sufficient. Do not add a second synchronization protocol.

### Server model and database

Keep `superzed_session` UI-independent. It owns stable IDs, snapshot validation, pure mutations, and
SQLite schema/migrations.

The authoritative snapshot contains:

- monotonic session revision;
- monotonic project ID allocator;
- active workspace ID;
- ordered non-empty workspace list;
- for each workspace: stable ID, nonzero project ID, ordered project roots, layout revision, and
  typed layout;
- for each root: requested absolute path and canonical absolute identity;
- for each layout: stable pane/item IDs, Zed n-ary axes and flexes, focused pane, active item,
  file path, pinned state, and preview state.

There is no workspace-name field and no rename/move mutation in Milestone 1. Ordering changes only
through create and close. A future schema migration may add naming and explicit reorder behavior.

The model must reject duplicate/missing IDs, invalid active references, invalid roots, invalid axes
or flexes, multiple focused panes, stale session revisions, and stale layout revisions.

Each accepted mutation has one authoritative commit path:

1. canonicalize inputs that require filesystem identity;
2. clone and validate the next snapshot;
3. persist the complete logical change in one SQLite transaction;
4. publish the new in-memory snapshot;
5. reconcile derived project runtimes from that snapshot;
6. return the snapshot; derived runtime failures use the existing project error/status path.

If validation or persistence fails, the mutation fails and the old snapshot remains authoritative.
Runtime reconstruction failure does not roll back or rewrite committed session facts: the affected
root remains visible with an error and can recover later. Missing roots are never silently removed.

Closing a workspace removes its workspace row, project scope, saved layout, and all workspace-owned
state. The project runtime is dropped. Shared runtime resources are released when no remaining
project references them. Closing the final workspace atomically creates and activates one blank
replacement workspace.

The database is per host and separate from the GUI's Zed database. Runtime entities, sockets,
process IDs, watchers, buffers, repositories, and language servers are reconstructed, not persisted.

### Project scopes and host resources

Every workspace owns a distinct `ProjectId` and ordered `ProjectSpec`. Changing one workspace's
roots never changes another workspace's visible roots, settings, tasks, debugger state, search
state, or lightweight scope indexes.

Expensive resources are canonical per host:

- Worktree/scanner key: canonical root path.
- Buffer key: canonical worktree identity plus relative path.
- Git key: canonical Git common-directory path.
- LSP key: canonical worktree identity, adapter/name, toolchain, binary settings, initialization
  options, and settings fingerprint.

Two overlapping projects must share the same worktree entity/scanner, buffer entity, repository
entity, and compatible language-server process. Open-document reference counting must produce one
`didOpen` and one final `didClose` per shared server/document pair. Different project scopes must
still expose only their own ordered roots.

Do not broaden this checkpoint into a general rewrite of Zed stores. Extend the existing
`HostResourceRegistry` and the existing `LanguageServerSeed` startup seam only as required by the
sharing tests.

### Project RPC routing

`SuperzedHost` owns `ProjectId -> Entity<HeadlessProject>`. It creates, updates, and drops project
scopes while the process-level filesystem, HTTP client, Node runtime, languages, extension host,
and host resource registry are initialized once.

All host-session project traffic carries a real `ProjectId`. No Super Zed request may fall back to
`REMOTE_SERVER_PROJECT_ID`. Host-level handlers must find the target project and reject unknown IDs;
they must not accidentally execute against the first project that registered a handler.

The client project proxy is constructed with a required host client and project ID. It may retain a
clone of the RPC handle for entity subscriptions, but the `HostSessionClient` remains the connection
lifecycle owner. Dropping a project or GUI must not send `ShutdownRemoteServer`.

### Local and SSH lifecycle

Local and SSH use the same persistent host-server behavior:

- connect to the stable host-session socket if reachable;
- otherwise remove only stale socket/PID artifacts and start the server;
- never kill a live process based only on a stale PID file;
- complete the handshake and load/validate the database before accepting workspace operations;
- reject a second simultaneous full GUI client with a clear protocol error;
- remain alive after GUI disconnect;
- reconnect to the same unnamed session;
- stop only through an explicit stop-server operation.

The local host connects automatically. Saved SSH host descriptions may be shown disconnected, but
the user explicitly reconnects them in Milestone 1. Disconnecting SSH hides its workspace section
without deleting remote data. If its workspace was active, the local host's last active workspace
becomes active.

### GPUI projection

The reconciler applies a validated snapshot by stable ID:

1. create missing project proxies and workspace entities;
2. refresh changed project roots through the existing remote project resync path;
3. restore a changed layout by stable pane/item IDs;
4. order projections exactly like the snapshot;
5. activate the snapshot's active workspace;
6. remove projections absent from the snapshot only after the new state is ready.

Reconciliation must preserve an existing `Entity<Workspace>` when its ID remains present. Switching
workspaces therefore activates an existing entity rather than reopening a project.

`Workspace::serialize_workspace` remains the capture seam. Host-backed workspaces enqueue
`replace_layout`; they never save the same topology to `WorkspaceDb`. Debounced layout writes use
GPUI executors and enter the same per-host mutation queue as every other mutation.

### Exact project-opening semantics

The phrase "Open Project" is not an internal operation in Super Zed. UI surfaces translate to these
explicit host mutations:

| User action | Required result |
| --- | --- |
| Open Recent / choose a folder while current workspace is blank | Set that same workspace's project roots. Preserve its `WorkspaceId`. |
| Open Recent / choose a folder while current workspace is non-blank | If an existing workspace has the exact canonical `ProjectSpec`, activate it. Otherwise create and activate a new workspace with that spec. Never close the current workspace. |
| Add Folder to Project | Append the canonical root to the current workspace's `ProjectSpec`; preserve its workspace and project IDs. |
| Choose a file under a current root | Open the file in the current workspace without changing project roots. |
| Choose a file outside all current roots | Use its parent directory as the requested project root, following the same blank/non-blank rule, then open the file. |
| New Workspace (`+`) | Create and activate one blank server workspace after the active workspace. |
| Click workspace row | Activate that exact existing workspace only. |
| Click workspace close button | Send `close_workspace`; remove the row only after the server commits and reconciliation succeeds. |

All entry points must use these semantics: menu actions, command palette, title bar, existing global
sidebar, Recent Projects modal, Finder path prompt, CLI/open listener, and macOS open events.

Production Super Zed action handlers must not call legacy `MultiWorkspace::open_project`,
`find_or_create_local_workspace`, `Workspace::open_workspace_for_paths`, or create a new local
workspace through `with_active_or_new_workspace`.

### Existing global sidebar and title bar

Modify the existing global sidebar that contains chats. Do not create or register a second sidebar.

It renders one section per connected host:

- host name and connection status in the section header;
- one `+` button in the header;
- ordered workspace rows;
- generic `Workspace <stable short ID>` labels, using the first eight hex characters of the
  persisted `WorkspaceId`;
- active-row styling from the snapshot;
- a close button on the right of every workspace row.

There are no rename handlers, text fields, drag reorder, or up/down buttons in this milestone. A
row click cannot enter rename mode. The close button stops propagation so it cannot activate or
rename the row accidentally.

Connection state belongs to the host header. The title bar must not render the local host as a
per-project remote server or show a red/green server icon for host-session projects.

### Product startup and keymaps

`cargo run -p superzed` uses the normal Super Zed user-data directory. A custom user-data directory
is not required for normal development.

The product must:

- use only Super Zed names, paths, remote artifacts, and databases;
- never read or migrate installed Zed databases;
- register all product actions before loading shipped and user keymaps;
- parse the shipped default, Vim, specific override, and initial keymaps without errors;
- report genuinely invalid user keymaps normally rather than suppressing diagnostics.

The automated startup fixture uses an isolated temporary Super Zed data directory so extensions,
credentials, or unrelated user settings cannot contaminate acceptance results. It must produce no
Super-Zed-owned error logs. External MCP credentials, third-party themes, authentication, or broken
project symlinks are not hidden or treated as host-session failures.

## Conflicting current code that must be replaced

The current working tree contains useful model/server pieces but an invalid mixed client shell. The
implementation must replace, not wrap, these patterns:

- optional `superzed_metadata` on `Workspace`;
- `MultiWorkspace::superzed_metadata: HashMap<EntityId, ...>`;
- unused workspace-name state and rename/move session mutations;
- direct mutation RPCs scattered through `MultiWorkspace`, `Workspace`, sidebar handlers, and open
  listeners;
- production `detach_workspace` used as a substitute for server close;
- Super Zed UI routes through legacy `open_project`, `find_or_create_local_workspace`,
  `open_workspace_for_paths`, or legacy workspace creation;
- project-owned connection status in the title bar;
- synthetic tests that manually inject metadata instead of attaching a host session;
- fire-and-forget project-root reconciliation;
- any startup fallback to legacy Zed workspace restoration.

Underlying Zed helpers may remain for upstream code only if no Super Zed production action can
reach them. Do not duplicate them under new names.

## Implementation checkpoints and hard gates

Implement one checkpoint at a time. Add the failing acceptance test first, make it pass, run the
checkpoint command, and review the diff before continuing. A checkpoint completion is not Milestone
1 completion.

### Checkpoint 1: Pure server contract

- Finish snapshot validation and SQLite transaction behavior.
- Remove workspace-name state and rename/move mutations from the Milestone 1 model.
- Keep the compact `GetSuperzedSession`, `ResolveSuperzedProjectSpec`, and
  `MutateSuperzedSession` protocol; do not add parallel per-operation RPCs.
- Make close-last replacement and project ID allocation deterministic and tested.
- Ensure a failed mutation leaves both database and in-memory snapshot unchanged.

Gate:

- model mutation tests cover create, activate, close, close-last, roots, layout, stale revisions,
  and validation;
- database reopen returns the exact committed snapshot;
- failure-injection proves atomic rollback.

### Checkpoint 2: One local host-session client path

- Add `HostSessionClient` and its serialized mutation queue.
- Make production `MultiWorkspace` require the local host session.
- Attach stable identity during projection construction.
- Implement the sole reconciler.
- Remove optional metadata and scattered direct mutations from production paths.
- Keep the application window hidden until initial reconciliation completes.

Gate:

- a GPUI integration test attaches through the real host protocol and renders one blank local
  workspace;
- three sequential and three concurrently triggered `+` actions produce exactly the expected
  workspace count and order;
- every resulting workspace row switches to an already existing entity;
- no test manually injects Super Zed metadata.

### Checkpoint 3: Complete local UI behavior and persistence

- Render host/workspace rows in the existing global sidebar.
- Add close buttons and remove rename/reorder behavior.
- Convert every project/folder/file entry point to the exact semantics table.
- Route layout capture through the mutation queue.
- Remove title-bar per-project local-host status.
- Fix product-caused keymap diagnostics.

Gate:

- GPUI tests click the actual `+`, workspace row, close, Open Recent, and folder-open UI actions;
- opening into a blank workspace preserves its ID;
- opening a different project from a non-blank workspace preserves the old workspace and creates a
  new one;
- exact existing projects activate instead of duplicate;
- Add Folder changes only the active workspace;
- close remains deleted after client and server restart;
- workspace order, active ID, roots, panes, flexes, editor items, pinned/preview state survive
  restart;
- shipped keymaps load with zero errors after real product action registration.

### Checkpoint 4: Dynamic projects and host resource sharing

- Route all project traffic by real project ID.
- Complete canonical worktree, buffer, Git repository, and compatible LSP sharing.
- Retain per-project visible roots and lightweight state.
- Drop a closed project's runtime and release unused shared resources.

Gate:

- two overlapping projects share one worktree/scanner, buffer, repository, and compatible fake LSP
  process;
- editing a shared file through either project immediately updates the same buffer entity;
- each project panel still shows only its own ordered roots;
- roots changed in one project do not change the other;
- one mock host connection performs file, search, LSP, Git, task, and debugger requests against two
  project IDs without cross-routing or use of `REMOTE_SERVER_PROJECT_ID`.

### Checkpoint 5: SSH through the same path

- Represent each connected host with the same `HostSessionClient` abstraction.
- Connect SSH manually through the existing Zed transport and stable host-session identity.
- Group sidebar rows by host.
- Implement disconnect/reconnect without deletion.

Gate:

- a transport-backed integration test runs local and SSH host sessions simultaneously over two
  connections in one window;
- mutations on one host never alter the other host's snapshot;
- SSH disconnect hides only that host and activates the local fallback;
- reconnect restores the same SSH workspace IDs and project roots;
- a second simultaneous full GUI client is rejected without disturbing the first;
- the remote server process survives GUI/client exit.

### Checkpoint 6: Milestone acceptance harness

Add `script/test-superzed-milestone-1` as the single repeatable acceptance command. It must run the
focused model, database, server, project routing/sharing, GPUI, keymap, process lifecycle, and SSH
tests. It must fail fast and return nonzero if any required scenario fails.

The script may use temporary user-data and server-state directories. It must not mutate the normal
developer profile. It must not skip a required test by name. Scheduler-sensitive GPUI tests use
GPUI executor timers and record a reproducible seed on failure; retries may not mask failures.

Gate:

```sh
./script/test-superzed-milestone-1
./script/clippy -p superzed_session -p remote_server -p remote -p project -p workspace \
  -p sidebar -p recent_projects -p title_bar -p proto -p superzed
cargo fmt --all -- --check
cargo check -p superzed --all-targets
```

For the real SSH process test, the script may require an explicit environment variable naming a
configured Unix SSH target. The automated fake-transport SSH test is always required. Milestone 1
cannot be declared complete until the real configured SSH test has also passed once and its exact
command/result is reported.

## End-to-end acceptance scenarios

The acceptance harness must verify these scenarios through production entry points, not by directly
editing snapshots or injecting UI metadata:

1. Start with no server or database. No UI appears before connection. One blank local workspace
   appears after successful attach.
2. Create three local workspaces from the sidebar. Every click succeeds, order is stable, and each
   row activates its existing entity.
3. In blank workspace A, choose a two-directory project. Workspace A keeps its ID and shows both
   directories.
4. From non-blank workspace A, open a different recent project. Workspace A remains; workspace B is
   created and activated.
5. Reopen A's exact project. A is activated; no duplicate workspace is created.
6. Add a shared directory to B. A's roots do not change. Shared files use the same server resources
   and buffer state.
7. Split panes, resize them, open/pin/preview files, and focus a pane. Detach and relaunch the GUI.
   The same IDs, order, projects, and editor layout return.
8. Close B with its row close button. Its project and saved data disappear. Relaunch both GUI and
   server; B does not return.
9. Close the final workspace. Exactly one new blank workspace is atomically created and displayed.
10. Connect one SSH host, create two remote workspaces, open projects, disconnect, and reconnect.
    Local state remains untouched and remote state returns.
11. Exit the GUI. Both host servers stay alive. Relaunch and attach without a loading window or
    connection modal.
12. Start with a stale PID file pointing at an unrelated live process. Super Zed does not kill that
    process and still starts or attaches correctly.
13. Load all shipped keymaps after product initialization. No keymap error notification appears.
14. Exercise buffer, search, LSP, Git, task, and debugger operations in two project scopes on one
    host. Every request reaches the requested project ID.

## Review guardrails

- Use the existing crates and seams listed above. The only expected new handwritten source module
  is the host-session client/controller if no suitable existing module can hold it.
- Do not add a new sidebar crate, persistence crate, RPC framework, project abstraction, or app
  binary.
- Do not preserve two implementations behind a setting or fallback.
- Do not solve a failed test by weakening, deleting, ignoring, or retrying it.
- Do not use manual testing as the recurring verification mechanism. Manual launch is optional
  confirmation after the automated harness passes.
- If a checkpoint requires a broad refactor outside the named seams, stop that checkpoint and
  report the precise architectural conflict. Do not stockpile compatibility wrappers.
- After each checkpoint, report changed files, deleted alternate paths, exact commands/results, and
  the next incomplete checkpoint.

## Completion report contract

An agent may say **"Checkpoint N complete; Milestone 1 incomplete"** after that checkpoint's gate
passes.

An agent may say **"Milestone 1 complete"** only when all of the following are included in the same
report:

- every checkpoint is complete;
- `script/test-superzed-milestone-1` passed;
- formatting, clippy, and all-target checks passed;
- the real configured Unix SSH acceptance test passed;
- no required test was skipped;
- no known bug remains in any acceptance scenario;
- the report identifies any unrelated external-profile warnings separately from Super Zed errors.

Compilation, model tests, a synthetic sidebar test, a process-lifecycle test, or a visually plausible
UI is not sufficient evidence by itself.
