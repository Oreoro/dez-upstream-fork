# Super Zed Fork Design

## Status and authority

This document defines what Super Zed is, how its major concepts relate, and which behavior is
correct. It is product and architecture specification, not an implementation plan or a report of
the current code.

Read only the current working trees when researching the design:

- Super Zed and Zed code: `/Users/maxktz/dev/zed`
- Herdr reference code: `/Users/maxktz/ref/herdr`

## Product identity

Super Zed is not a conventional editor with an embedded terminal. It is a workspace-first,
client/server development environment intended to replace both:

- an editor such as Neovim; and
- a terminal multiplexer such as tmux or herdr

Zed is the base because its editor is already the desired editor. Super Zed must retain Zed's high-quality editing surface and features, including:

- Vim mode;
- language servers and the complete LSP editing experience;
- Git integration, diffs, and blame;
- debugger support;
- project search;
- symbol navigation and rename;
- file trees and file operations;
- panes, editor tabs/items, pickers, and navigation;
- tasks, extensions, themes, and other editor capabilities that remain compatible with this
  architecture.

The fork replaces Zed's application shell, workspace ownership, remote-project model, and server
lifecycle where they conflict with a persistent multiplexer. It does not rewrite the editor.

There is one Super Zed product on this fork. A second runnable legacy Zed product, legacy startup
mode, or compatibility shell is not a requirement.

Super Zed uses its own product name, user-data namespace, logs, server artifacts, and databases.
Normal development runs use that default namespace without requiring a custom data directory.

## Core principles

1. **Workspace-first, not project-first.** A workspace exists independently and owns a mutable
   project scope, pane layout, and eventually persistent terminals and agents.
2. **The host owns the connection.** There is one persistent server and one logical connection per
   host, not per project or workspace.
3. **The server is authoritative.** Persistent session facts and long-running runtimes belong to
   the host server. The GUI is a local GPUI projection of typed server state.
4. **One operation has one production path.** Do not retain legacy and Super Zed implementations
   side by side, behind settings, or as fallbacks.
5. **Reuse expensive resources at the correct level.** Workspaces and project scopes stay
   independent while compatible host resources are shared.
6. **Explicit actions have narrow effects.** Connecting creates a host connection. Workspace
   controls create or close workspaces. Folder selection changes only the current workspace's
   project roots.
7. **Detach is not shutdown.** Closing the GUI, a project projection, or a workspace must not stop
   a persistent host server.
8. **Prefer a small coherent refactor over compatibility glue.** Upstream mergeability is useful,
   but it never justifies duplicate ownership models or dead production paths.
9. **Behavior is copied before it is invented.** Multiplexer and agent behavior must be compared
   with the current Herdr source before implementation.
10. **Automated end-to-end evidence defines completion.** Compilation or a visually plausible UI
    is not proof that a workflow works.

## Conceptual model

```text
Super Zed GUI
└── one global application shell and content window
    ├── global sidebar
    │   ├── chats and future global features
    │   └── connected hosts
    │       └── ordered workspaces
    └── active workspace content
        └── Zed PaneGroup
            └── panes
                └── editor, terminal, and other items

Host
├── one persistent host server
├── one logical GUI connection
├── one authoritative host session
│   └── ordered workspaces
├── project-scope registry
└── shared host-resource registries
```

The terms in this hierarchy are not interchangeable.

### Single application shell

Super Zed has exactly one OS-level GPUI content window and one `MultiWorkspace` application shell.
All hosts and workspaces are projected inside that shell. A workspace is an in-application
multiplexer unit; it is never represented by another OS window.

The single shell is a product-wide invariant, not a preference enforced by individual callers:

- one application-level owner is the only production code allowed to construct the
  `MultiWorkspace` window;
- that owner reuses and activates the existing window for startup, macOS Dock reopen, file-open,
  CLI/open events, recent projects, SSH connection, and every other entry point;
- concurrent attempts to start or reopen the GUI share one pending creation and resolve to the
  same window;
- every production `MultiWorkspace` is constructed with an authoritative `HostSessionClient` and
  every contained `Workspace` has a stable host-workspace identity;
- there is no standard/legacy `MultiWorkspace` mode that can exist without host sessions;
- a shell without the global sidebar and host-session controller is an invalid state that must be
  prevented by constructors and types, not repaired after rendering;
- closing the sole window detaches the GUI from host servers; reopening the application restores
  one authoritative shell from the host session.

Super Zed does not expose or retain a **New Window** product concept. Production code must remove,
not merely hide:

- New Window actions and command-palette entries;
- platform keybindings for creating a window;
- File-menu, macOS Dock-menu, Windows jump-list, and title-bar/window-tab entries;
- Move Project to New Window and next/previous-window actions;
- `OpenMode::NewWindow`, `create_new_window` flags, and equivalent caller-selectable branches;
- project, recent-project, remote, WSL, dev-container, Git, and CLI helpers that construct a second
  shell;
- CLI `-n`/`--new` and `--classic` flags, plus settings that select new-window open behavior;
- native OS window tabbing for the application shell;
- separate About or utility OS windows. Such UI must be an in-window modal or popover if retained.

Low-level `cx.open_window` access is not a valid escape hatch. There must be exactly one production
construction seam for the main shell. APIs that need a UI target receive or obtain that sole shell
and operate on its current workspace or host session. Unsupported legacy requests must return a
clear error instead of creating a standard Zed window or silently selecting another workspace.

### Host

A host is one machine on which projects and long-running runtimes execute. The local machine is a
host. Every connected SSH machine is another host.

A host owns:

- its persistent server process;
- its connection and reconnection lifecycle;
- the authoritative ordered workspace collection;
- the host session database;
- project runtimes addressed by real project IDs;
- filesystem, buffer, Git, LSP, context-server, and other shareable registries;
- later, PTYs and agent runtimes.

The local host connects automatically. SSH hosts connect only through an explicit **Connect SSH
Host** action.

### Host session

A host session is the authoritative persistent state for one host. Initially there is one unnamed
default session per host.

It owns:

- stable host-session revision;
- active workspace ID;
- ordered workspace records;
- project specifications;
- saved pane and item topology;
- future server-owned terminal and agent state.

The server commits mutations before the GUI displays their result. On a revision conflict, the
server's current snapshot wins. The client reconciles it and may retry the intended operation once
against the new revision. A conflict must never leave the client permanently stale.

### Workspace

A workspace is the main multiplexer unit, equivalent to a tmux/Herdr workspace rather than a Zed
window.

A workspace owns:

- stable workspace identity;
- exactly one mutable project scope;
- an ordered pane topology and pane sizes;
- active and focused pane/item state;
- editor items and their pinned/preview state;
- later, persistent terminals and workspace-associated agents.

A workspace does not own a host connection or server process.

Creating a workspace creates one blank server-owned workspace and activates it. Switching a
workspace activates its existing GUI projection; it does not reopen its project. Closing a
workspace permanently removes that workspace's record and workspace-owned state, like closing it in
tmux or Herdr.

The application always needs one usable workspace. Closing the final workspace deletes that exact
workspace and atomically creates a new blank workspace with a new identity. This is replacement,
not restoration of the deleted workspace.

Workspace names and manual ordering are later product features. Until deliberately designed,
generic labels derived from stable IDs are correct. There must be no accidental rename-on-click,
empty names, up/down controls, or hidden reorder behavior.

### Project scope

A project is a lightweight, mutable workspace-specific view of resources on a host. It is not a
connection, host, window, or workspace identity.

A project owns only scope-specific state, including:

- its stable project ID;
- its ordered visible root-directory membership;
- settings and environment derived for that scope;
- tasks and debugger state;
- search state;
- active repository/diff presentation;
- lightweight indexes and other state that truly differs by project scope.

Every workspace has a distinct project scope, including when two workspaces show identical or
overlapping directories. Changing one project's root list must never change another workspace's
root list.

Project root lists are not unique keys. Two projects with identical roots are valid and must remain
separate. Their expensive underlying resources are shared through the host.

### Pane and item

The workspace content area uses Zed's existing n-ary `PaneGroup`, panes, flex sizes, and item views.
Super Zed does not adopt Herdr's tab layer or binary BSP layout.

Editors, terminals, and other views are pane items. The saved layout describes stable topology and
serializable item state. Runtime objects are reconstructed or reattached separately.

## Ownership boundaries

| Concern | Owner | Must not own it |
| --- | --- | --- |
| Host connection and status | Host session/client | Project or workspace |
| Persistent server lifecycle | Host | Project, pane, or GUI window |
| Ordered workspace collection | Host server | Sidebar or GUI database |
| Workspace identity and project ID | Host server | GPUI `EntityId` or path list |
| Project root membership | Workspace's project scope | Host-global singleton |
| Pane topology | Workspace record on host server | Legacy GUI `WorkspaceDb` |
| Canonical worktree/watcher | Host resource registry | Each project independently |
| Canonical buffer | Host resource registry | Each project independently |
| Git repository entity | Host resource registry | Each project independently |
| Compatible language server | Host LSP pool | Each project independently |
| MCP definitions and credentials | Host/user context-server registry | Each project independently |
| Project-specific MCP access | Project scope | Unscoped global process |
| Future PTY process | Host runtime, referenced by workspace/pane | GUI terminal view |
| Rendering | Local GPUI client | Remote rendered-frame stream |

Dropping a project projection releases only that project's references. It must never disconnect the
host client or send a host-server shutdown request.

## Host-owned resource sharing

Independent project scopes must not duplicate expensive runtime resources unnecessarily.

The host canonically owns and shares:

- worktrees and filesystem scanners by canonical root identity;
- buffers by canonical file identity;
- Git repositories by canonical Git common-directory identity;
- compatible language-server processes by worktree, adapter, toolchain, binary/settings, and
  initialization compatibility;
- context-server definitions, authentication, and compatible runtimes;
- process-level filesystem, HTTP, Node, language, and extension infrastructure.

For example, if two workspaces open the same file, both editors must observe one canonical buffer.
If two compatible project scopes need the same language server, the host should run one compatible
process and reference-count opened documents. The scopes still present their own ordered directory
lists and project-specific state.

Sharing is based on canonical runtime identity, not on collapsing projects or activating an
existing workspace.

## Context servers and MCP

Installed MCP/context-server definitions and credentials are host/user resources. They must not be
independently parsed and authenticated once for every workspace project.

The target model is:

```text
HostContextServerRegistry
├── installed server definitions
├── credentials and authentication state
├── startup/error state
└── compatible running server instances

Project scope
└── project-specific roots, permissions, environment, and access to registry services
```

A missing global credential should produce one meaningful host-level error, not one copy per
workspace. A server that requires a distinct project runtime may still receive one, but the host
registry owns that decision and lifecycle. Projects do not independently rediscover extensions or
own global credentials.

## Exact project and folder behavior

The meaning of **Open Project** in Super Zed is different from legacy Zed.

When invoked from a workspace, **Open Project** always replaces the current workspace's ordered
project roots with the selected directories.

It must not:

- search for a workspace with the same roots;
- activate another workspace;
- create a workspace;
- create a window;
- connect a host;
- close or detach the current workspace.

This remains true when another workspace already has an identical project specification. Both
workspace/project scopes continue to exist and share compatible host resources internally.

**Add Folder to Project** appends a canonical root to the current workspace's project scope and
preserves the workspace and project identities.

Opening a file that already belongs to the current project opens it in the current workspace.
Folder selection, file opening, workspace creation, and host connection are separate operations and
must not be combined by hidden heuristics.

All user entry points must obey the same semantics: menus, command palette, title bar, recent
projects, Finder/folder prompts, CLI/open events, and sidebar actions.

## Local and SSH architecture

Local and SSH hosts use the same host-session and persistent-server abstraction. Transport and
authentication differ; workspace/project semantics do not.

### Connect SSH Host

The only remote connection feature is **Connect SSH Host**:

```text
Connect SSH Host
→ collect SSH connection options and credentials
→ attach to or start one persistent server on that host
→ create one HostSessionClient
→ load and validate the authoritative host session
→ add the host and its workspaces to the global sidebar
```

Connecting a host must not select a folder, create a project, create a workspace, switch an
unrelated workspace, or open another application window.

If the host is already connected, the action reuses its existing connection rather than creating a
project-scoped connection.

Once connected, **Open Project** inside one of that host's workspaces uses a folder picker backed by
that host's filesystem and changes only that workspace's roots.

### Removed legacy remote model

The inherited **Projects: Open Remote** model is not part of Super Zed. Neither are its alternate
"from existing connection" and "open in new window" modes.

The following architecture must not remain reachable in the Super Zed product:

```text
Open Remote
→ connect
→ select directory
→ find/create remote project
→ find/create window or workspace
```

Low-level SSH transport, authentication, proxy installation, reconnection, and RPC clients remain
valuable reusable Zed components. The project/window orchestration above must be deleted or
refactored into the single host-session path. Do not leave unused public actions, modals, helpers,
or compatibility routes that imply the legacy model still exists.

## Server and client boundary

Super Zed uses a persistent headless server on every host and a local GUI client.

The server owns:

- authoritative session state and persistence;
- workspace and project registries;
- host resource registries;
- filesystem and development runtimes;
- later, PTYs and agent runtimes.

The GUI owns:

- local GPUI rendering;
- Zed workspace, pane, editor, panel, and input projections;
- user interaction and presentation of server errors;
- transient visual state that has not been declared persistent.

The GUI does not receive rendered frames from the server. It receives typed state and project RPC
data and renders locally using GPUI.

One host-session controller owns the connection, latest validated snapshot, serialized mutation
queue, projection map, and reconciliation for that host. UI components call this controller; they
do not issue their own read/modify/write RPC sequences.

There is one supported GUI client and one application window in the first usable product stage.
Simultaneous clients may be designed later. A second client must not corrupt or detach the first.

## Startup and lifecycle

Startup has one path:

1. attach to or start the local persistent host server;
2. complete the handshake;
3. load and validate the host session;
4. initialize required project scopes;
5. reconcile the initial workspace projections;
6. show the application window.

No loading window, workspace shell, connection modal, or other UI may be visible before the local
host is connected and the initial state is usable. Startup, macOS reopen, and focus events must
share one pending startup operation and one resulting window.

In production, the server remains alive when:

- the GUI exits or disconnects;
- a workspace closes;
- a project projection is dropped;
- the application window closes.

Only an explicit stop-server operation stops a production server.

Development builds keep the same persistent-server architecture but use a bounded lifecycle:

- every GUI connection verifies that the running local server has the same build identity as the
  executable being launched;
- a different development build cleanly stops and replaces the old server process without deleting
  or replacing the host-session SQLite database;
- after the last GUI disconnects, the development server remains available for 30 minutes so GUI
  detach/reattach and long-running-runtime behavior can be tested;
- reconnecting during that period cancels and restarts the disconnected idle timer;
- after 30 continuous disconnected minutes, the development server exits cleanly;
- production servers have no disconnected idle timeout.

Build compatibility and idle lifetime are separate checks. A newly built development GUI must
never attach to an older in-memory server merely because its socket and protocol version still
exist. Conversely, a compatible server must not be restarted merely because the GUI detached.

Each client attachment starts with a fresh per-client transport epoch. The server sends the
connection-ready handshake before project or workspace traffic, and it does not replay stale
per-client messages from a previous GUI attachment. The GUI registers the handlers required by the
authoritative session before project streams are allowed to publish. A handshake failure is fatal
to startup; it must not open a partial shell.

Protocol incompatibility is handled by starting/attaching to a compatible versioned server, not by
maintaining multiple behavioral fallbacks in the client. The persistent session database remains
independent of transient versioned socket/process artifacts and is migrated explicitly when its
schema changes.

Product initialization must register its actions before shipped or user keymaps are loaded. Shipped
default, Vim, and platform keymaps must load without product-caused diagnostics. A genuinely invalid
user keymap is still reported normally; errors must not be hidden to make startup appear clean.

## Persistence

Persistent host-session data lives in a dedicated server-side SQLite database, separate from the
GUI's legacy Zed workspace database.

Persisted facts include:

- session revision and active workspace;
- ordered stable workspace IDs;
- stable project IDs and ordered requested/canonical roots;
- pane topology and flex values;
- stable pane and item IDs;
- active/focused panes and items;
- file editor paths, pinned state, and preview state;
- later, terminal and agent references as their server runtimes are added.

Runtime objects are reconstructed rather than serialized: sockets, watchers, buffers, repository
objects, language-server processes, and similar entities are not database rows.

Missing roots remain represented with an error. They must not silently delete or rewrite a
workspace. Closing a workspace intentionally deletes its persisted workspace-owned data.

Super Zed must never read, migrate, or fall back to an installed Zed workspace database. There is
one Super Zed persistence model.

## Global UI

The outer sidebar is the existing global Zed sidebar that also contains chats and other global
features. It is outside every workspace. It is not the Project Panel inside a workspace, and Super
Zed must not add a separate replacement sidebar with a foreign design.

The workspace portion of the global sidebar contains:

- one section per connected host;
- host name and truthful connection status;
- one workspace-creation button per host;
- ordered workspace rows;
- active-workspace styling;
- a close button on the right of every workspace row.

Clicking a row only switches to that workspace. Clicking close only closes it. The close control
must stop row-click propagation.

Disconnecting an SSH host hides its section without deleting server state. If its workspace was
active, Super Zed activates the local host's last active workspace. Reconnecting restores the same
remote session.

The content area beside the sidebar renders only the active workspace's Zed pane layout. Project
Panel, Agent Panel, docks, editors, terminals, and other Zed panels remain inside that workspace.

Connection status is a host property. It must not be presented as a red/green per-project server in
the title bar.

### Workspace title bar

The title bar describes the active workspace; it is not a project or workspace switcher.

- switching between Super Zed workspaces happens only through the global workspace sidebar;
- selecting **Open Project** changes the active workspace's roots in place;
- the title bar may display the active workspace's root summary and Git branch information;
- branch selection may change the branch for the relevant repository;
- clicking a worktree or project label must not locate, activate, deduplicate, or create another
  workspace;
- legacy project-group switching and project/window selection UI must not remain reachable from the
  title bar;
- Git worktree creation and management may remain editor features, but their result is applied to
  the current workspace's project scope and never opens another shell.

The title bar must not expose a second interpretation of "current project." The authoritative
identity remains the active host workspace and its project scope.

## Terminal multiplexer direction

Persistent workspaces are the prerequisite for persistent terminals. A terminal belongs to a
workspace pane; terminal persistence must not be built around legacy Zed windows or projects.

The final terminal model follows tmux and Herdr semantics:

- PTYs run on the host server;
- terminal processes survive GUI detachment and relaunch;
- the client attaches to terminal state and sends input;
- workspace close destroys that workspace's terminal runtimes;
- disconnecting the GUI or host view does not destroy them;
- local and SSH terminal behavior uses the same protocol and ownership model.

Until server-owned PTYs are implemented, existing client-owned terminals may be transient. Their
loss must not corrupt saved workspace/pane topology. Temporary terminal behavior must not become a
second persistence architecture.

## Agent integration direction

Herdr's agent integration is a later major feature and should be copied behavior-for-behavior after
the workspace and terminal runtime foundations are correct.

Agent state and long-running agent processes belong on the host server, associated with stable
workspaces/terminals as defined by Herdr. Do not invent a separate client-only agent lifecycle or
adapt Zed's current agent grouping into a competing workspace model.

Before implementing agent behavior, inspect the relevant current Herdr code and record which
behavior is being adapted. The goal is one-to-one product behavior where it fits the Super Zed
model, not a loose visual imitation.

## Relationship to Herdr

Herdr is the behavioral source of truth for multiplexer ownership and later agent integration.

Copy these principles:

- the persistent server owns shared state and runtimes;
- clients attach to an existing server or start it;
- workspace creation, activation, and closing are explicit mutations of one owner;
- client detach does not destroy server state;
- serializable topology is separate from runtime objects;
- closing a workspace destroys its owned data and runtimes;
- terminal and agent lifecycles are server-owned.

Consult at least these current Herdr areas when working on their corresponding behavior:

- `src/app/state.rs`, `src/app/actions.rs`, `src/app/creation.rs`, `src/workspace.rs` for workspace
  ownership and mutation;
- `src/app/session.rs`, `src/persist/snapshot.rs`, `src/persist/io.rs`,
  `src/persist/restore.rs` for capture and restoration;
- `src/server/headless.rs`, `src/server/client_accept.rs`,
  `src/server/client_transport.rs`, `src/server/clients.rs`, `src/client/mod.rs`, and
  `src/protocol/wire.rs` for detached server/client lifecycle;
- `src/terminal/runtime_registry.rs`, `src/terminal/runtime.rs`, and
  `src/server/terminal_attach.rs` for future terminal ownership;
- the current Herdr agent modules for future agent integration.

Intentional differences from Herdr:

- GPUI renders typed state locally; Super Zed does not stream rendered terminal/application frames
  as its general UI architecture;
- Super Zed uses Zed's n-ary pane layout rather than Herdr's BSP tree;
- Super Zed does not add Herdr's separate tab layer;
- persistent structured state uses SQLite rather than copying Herdr's JSON persistence;
- Zed remains the editor, project RPC, pane, and extension foundation.

## Relationship to upstream Zed

Super Zed is an in-place Zed fork, not a from-scratch application assembled from copied fragments.
The editor and its reusable crates remain upstream-derived wherever their ownership matches this
design.

Changes should be concentrated at clear seams:

- application shell and startup;
- global workspace UI;
- host-session and server lifecycle;
- project construction and resource ownership;
- persistence and RPC routing;
- later terminal and agent runtime integration.

Avoid gratuitous divergence in the editor itself. However, do not retain a wrong shell or remote
model solely to make upstream merges easier. A clean refactor is preferred to adapters that keep
both architectures alive.

## Forbidden legacy and parallel behavior

Super Zed production code must not retain or reach:

- legacy Zed workspace restoration as a fallback;
- a local-project path that bypasses the mandatory local host;
- window-per-project or connection-per-project ownership;
- project-root matching that activates or deduplicates workspaces;
- project selection that implicitly creates a workspace or window;
- `Projects: Open Remote` and its alternate connection/window modes;
- project-owned host shutdown or connection status;
- GUI `WorkspaceDb` authority over host-session workspaces;
- multiple sidebars representing the same workspace collection;
- optional metadata tables that compete with stable server IDs;
- two mutation or reconciliation systems;
- old code kept "just in case" when no retained Super Zed feature uses it;
- compatibility flags that select between legacy and Super Zed semantics;
- silent fallback after server, persistence, or reconciliation failure.

Reusable low-level code is not legacy merely because Zed originally used it. Keep generic editor,
transport, RPC, and UI components when they serve the single Super Zed path. Delete or refactor
orchestration code whose only purpose is the rejected product model.

## Clean-code requirements

The design must remain understandable to an agent or developer unfamiliar with Rust and the Zed
codebase.

- One concept has one clear owner.
- One user operation has one public production entry point.
- Prefer existing files and crates when they already own the concept.
- Add a component only when it represents a real new ownership boundary.
- Delete superseded paths instead of layering wrappers over them.
- Do not duplicate snapshots, identity maps, connection state, or project registries in UI code.
- Do not use path equality as workspace identity.
- Do not let convenience helpers broaden an operation's semantics.
- Propagate fallible operations to a visible error boundary.
- Keep changes small enough to review and verify; do not stockpile thousands of lines before a
  complete vertical workflow works.
- Comments explain non-obvious reasons and invariants, not file organization.

Upstream compatibility is a maintenance consideration, not an architecture. The preferred merge
strategy is to keep Super Zed-specific ownership at narrow seams while retaining one coherent
product path.

## Product evolution boundaries

These are scope boundaries, not an implementation schedule.

### First usable foundation

- persistent local and SSH host sessions;
- server-owned multiple workspaces and project scopes;
- existing Zed pane/editor projection;
- project-root selection in the current workspace;
- server persistence of workspace/project/editor layout;
- shared host resources;
- one global window and one GUI client;
- transient existing terminals are acceptable.

### Terminal multiplexer foundation

- server-owned PTYs;
- terminal attachment and screen/state synchronization;
- terminal processes surviving GUI detach/relaunch;
- workspace close destroying owned terminals.

### Agent foundation

- Herdr-compatible agent discovery, ownership, attachment, and resume behavior;
- server-owned agent runtimes integrated with stable workspaces and terminals.

Workspace naming/reordering, multiple GUI clients, multiple application windows, named sessions,
and richer session management require deliberate later design. Agents must not add them creatively
while implementing an earlier foundation.

## Testing and evidence

Automated end-to-end tests are the primary acceptance mechanism. Manual testing is optional
exploration, not the recurring proof of completion.

For a regression:

1. add an automated test through the real production boundary;
2. run it against the current code and record the failure;
3. make the smallest architectural fix;
4. keep the test permanently;
5. run nearby regressions and deterministic seed sweeps where concurrency is involved.

Required test layers include:

- pure session-model and database transaction tests;
- client/server protocol and conflict-recovery tests;
- GPUI tests clicking actual sidebar, picker, and workspace controls;
- multi-workspace project isolation and host-resource-sharing tests;
- GUI detach/server survival/database restart tests;
- local and real/fake SSH lifecycle tests;
- startup/reopen single-flight tests;
- full product build, format, and targeted clippy checks.

Tests must verify outcomes, not just that actions were dispatched. Important assertions include:

- exact server workspace IDs, order, active ID, and revisions;
- exact GUI projection IDs and active entity;
- host connection remains connected during project/workspace changes;
- closed workspace data remains deleted after restart;
- identical projects remain distinct scopes while sharing canonical resources;
- **Open Project** preserves the current workspace ID;
- connecting SSH does not mutate project/workspace state;
- server PID survives GUI detach;
- no legacy window or fallback appears.

The isolated product acceptance environment must also distinguish Super Zed failures from external
profile problems. Super-Zed-owned startup, keymap, host-session, and RPC paths should produce no
error logs during successful workflows. Missing third-party credentials, broken user symlinks,
third-party theme warnings, and provider authentication errors are reported as external/profile
issues rather than silently suppressed or misclassified as workspace failures.

Do not weaken, retry, skip, or delete a failing test to claim completion. Scheduler-sensitive GPUI
failures must report reproducible seeds. A manual launch, compilation, or unit tests alone are not
sufficient.

## Review requirements

Review agents must evaluate architecture and behavior, not only whether the diff compiles.

Every relevant review should answer:

1. Is there exactly one production path for this operation?
2. Is the state owned by the host, workspace, project, or client level specified here?
3. Can connecting a host accidentally create, select, or close a project/workspace?
4. Can opening a project accidentally create or activate another workspace?
5. Can two workspaces use identical roots without being collapsed?
6. Does a project ever create, reconnect, disconnect, or shut down its host?
7. Are expensive resources shared canonically without sharing scope-specific state?
8. Is server persistence authoritative after GUI and server restart?
9. Was superseded legacy orchestration deleted rather than hidden behind a fallback?
10. Does an end-to-end test prove the full user-visible workflow and the process/database outcome?

An implementation must not be described as complete while a required workflow is untested, a
known acceptance bug remains, or a parallel legacy path can still produce different behavior.

## Compact invariant reference

```text
One machine                 = one Host
One Host                    = one persistent server + one logical connection
One Host session            = ordered authoritative Workspaces
One Workspace               = one Project scope + one PaneGroup
One Project scope           = ordered roots + lightweight scope state
Identical Project roots     = allowed in multiple Workspaces
Expensive runtime resources = canonical and shared per Host
Connect SSH Host            = connect/restore Host only
Open Project                = replace current Workspace's roots only
Add Folder                  = append to current Workspace's roots only
New Workspace               = explicit workspace creation only
Close Workspace             = permanent deletion; close-last creates a new blank identity
GUI detach                  = server and runtimes remain alive
Dev GUI detach              = server exits after 30 disconnected minutes
Dev executable rebuild      = old server replaced; persisted session retained
Legacy fallback             = forbidden
Proof of completion         = reusable automated end-to-end evidence
```
