# Superzed Fork Notes

This file documents the permanent product and architecture decisions that make
Superzed different from upstream Zed.

Use this as the first source of truth when resolving upstream merge conflicts.
It is not an implementation plan. It records what this fork is trying to
preserve, why the fork diverges, and how new upstream behavior should be mapped
into Superzed.

## Product Direction

Superzed is a fork of Zed, not a separate editor rewritten from scratch. The
goal is to keep as much of Zed's working editor, UI, language, git, and agent
infrastructure as possible while changing the product model.

Upstream Zed is project/window oriented. Superzed is terminal-first and
workspace-oriented:

- Users work in persistent workspaces, not opened projects.
- A workspace is a tmux-like set of panes and tabs.
- Files, terminals, chats, search views, git views, settings views, and future
  agent views are tabs/items inside workspaces.
- Worktree/project context is derived from open path-bearing tabs.
- Opening a directory is not the primary user action. Users open terminals or
  files. Directories become useful IDE context because tabs point at them.
- The app should feel like a terminal multiplexer that has IDE features, not an
  IDE that happens to include terminals.

Mergeability with upstream matters. Prefer solutions that keep upstream API
shapes and call sites when they do not conflict with the product model. Do not
keep incorrect upstream ownership semantics just to reduce conflicts.

## Fork Identity

The app is branded as Superzed.

Locked decisions:

- Binary name: `superzed`.
- Stable app name: `Superzed`.
- Dev app name: `Superzed Dev`.
- Config and data must be isolated from official Zed.
- Superzed must not self-update into upstream Zed.
- Updates are manual: rebuild, bundle, or install this fork.

Important consequences:

- Keep bundle metadata, identifiers, URL schemes, channel display names, binary
  names, config paths, and app support paths separate from upstream Zed.
- Do not reintroduce upstream update polling or a manual update action that can
  install upstream Zed over Superzed.
- If upstream changes release/bundle/update code, preserve Superzed isolation
  first.

## Merge Conflict Principles

When upstream conflicts with fork code, resolve by these rules:

1. Keep upstream architecture where it is compatible with Superzed.
2. Preserve Superzed product behavior where upstream assumes projects or
   multiple independent windows.
3. Prefer upstream names and APIs for mergeability when the semantics remain
   correct.
4. Do not introduce temporary aliases as final architecture.
5. Do not duplicate source-of-truth state and then sync it later.
6. Keep user-facing behavior scoped to the current workspace.
7. Keep backend data shared globally when multiple workspaces use the same
   worktree, repository, buffers, LSP state, or search substrate.
8. If upstream adds a new project-scoped UI feature, expose it through the
   workspace-scoped `Project` proxy rather than reading global stores directly.
9. If upstream adds a new window-opening path, route it into the single app
   session and current singleton window unless future multi-window viewport
   support is explicitly being implemented.
10. If upstream adds an "open project/folder" affordance, do not expose it as a
    primary ownership primitive. Map useful behavior to opening a terminal or
    file in a workspace.

## Current Ownership Model

Target data ownership:

```text
AppSession
+-- AppState
|   +-- shared backend stores
|   |   +-- WorktreeStore
|   |   +-- GitStore
|   |   +-- BufferStore
|   |   +-- LspStore
|   |   +-- DapStore
|   |   +-- TaskStore
|   |   +-- EnvironmentStore
|   |   +-- ToolchainStore
|   |   +-- thread/agent stores
|   |   +-- terminal backend data
|   |   +-- search/history services
|   |   +-- other upstream project backend stores
|   +-- durable app-session persistence
|
+-- MultiWorkspace
    +-- workspaces
    +-- active workspace
    +-- sidebar/app viewport state

Workspace
+-- project: Entity<Project>
|   +-- workspace-visible worktree ids
|   +-- workspace active repository id
|   +-- workspace active context
|   +-- references shared backend stores
+-- panes/tabs/items
|   +-- terminal tabs -> cwd path evidence
|   +-- file tabs -> file path evidence
|   +-- tool tabs -> no path evidence
+-- layout/focus state
```

The important rule is:

```text
Workspace owns tabs/layout.
Tabs provide path evidence.
Project owns workspace-scoped IDE context.
Shared stores own reusable backend data.
```

## `Project` Means Workspace Proxy

Keep the public `project::Project` type name for upstream compatibility. Do not
replace it with a broad `services` abstraction.

Superzed semantics:

- Each workspace has its own `Entity<Project>`.
- That project is a workspace-scoped proxy.
- Multiple workspace projects can reference the same shared backend stores.
- Normal UI-facing `Project` methods must be scoped to that workspace.
- Explicit raw/global store access is allowed only when the caller is truly
  global or backend-internal.

Examples of workspace-scoped behavior:

- `Project::worktrees()` should mean worktrees visible in that workspace.
- `Project::repositories()` should mean repositories visible in that workspace.
- `Project::active_repository()` must not return a repository from another
  workspace.
- Diagnostics, language-server status, file tree, git panel, search, tasks, and
  debugger UI should use scoped `Project` methods where possible.

Do not restore `Workspace::services()` or a `services` field as final
architecture. Upstream-style `workspace.project()` and `project.*` calls are
preferred when they remain semantically correct.

## Workspaces

Workspaces are the main user-facing container.

Locked decisions:

- Users can create many workspaces.
- A workspace is independent from any directory or project.
- A workspace can be empty.
- Empty workspaces must persist.
- An empty workspace has no attached worktrees.
- Workspaces contain panes/tabs/items.
- Every new workspace should start with a normal terminal by default where that
  behavior is appropriate.
- A workspace can contain terminals/files from multiple unrelated directories.
- Multiple workspaces may use the same worktree/repository at the same time.

Do not introduce a model where a workspace permanently owns a project root.
Roots are derived from open tabs.

## Path Evidence And Worktrees

Worktree visibility is derived from open path-bearing tabs in the current
workspace.

Path-bearing tabs:

- Terminal tabs contribute their current working directory.
- File-backed editor/view tabs contribute their file path.

Non-path tabs:

- Search views.
- Git views.
- Diagnostics views.
- Settings views.
- Chat/thread views.
- Tool views.
- Any other tab that does not directly represent a file or current working
  directory.

Non-path tabs must not contribute path evidence.

Root derivation:

```text
workspace tabs
-> current path evidence
-> resolve roots/worktrees
-> update workspace Project visible worktrees
-> shared stores provide metadata/cache
```

Important decisions:

- Roots must not be attached by CLI/open-project state unless an actual tab
  produces that path.
- Moving a terminal from `~` to `~/dev/site` removes `~` from that workspace
  unless another open path-bearing tab still evidences `~`.
- Nested path handling is scoped to the current workspace's evidence.
- Do not deduplicate against global worktrees or other workspaces.
- Keep underlying workspace-visible worktrees available for tool selectors even
  when the file tree visually flattens nested roots.

Example:

````text
Terminal A cwd: ~/dev/paykit
Terminal B cwd: ~/dev/paykit/apps/web

File tree can show one root: ~/dev/paykit
Tooling must still reason from this workspace's evidence, not global state. ```

If a workspace has:

```text
Terminal A cwd: ~
Terminal B cwd: ~/dev/maxkatz.me
````

the workspace evidence includes both paths. File tree rendering may avoid
duplicated nested display, but search/git/repo/worktree selectors must not lose
the child context just because a parent root exists.

## Shared Backend Stores

Backend metadata is shared globally.

Shared data includes:

- Worktree metadata and filesystem caches.
- Git repositories, status, branches, diffs, and repository cache.
- Buffers.
- LSP state and language server infrastructure.
- Search substrate/history where appropriate.
- Tasks, debug, environment, snippets, toolchains, and context servers.
- Thread/agent metadata stores.

The reason is reuse:

- Workspace A and Workspace B can use the same repo without duplicate indexing.
- Parent/child roots can reuse filesystem cache where supported.
- The same git repository should not be represented as separate repos just
  because multiple workspaces can see it.

The workspace `Project` decides what subset of shared data is visible to the
current workspace.

## Active Context

There is no global project/worktree/repository selector.

Each workspace has an active context:

- active path,
- active worktree,
- active repository when the active path is inside a repository.

The active context follows focus:

- Focusing a path-bearing terminal/file updates the workspace active context.
- Changing the cwd of the focused terminal updates the active context.
- Focusing a non-path tab keeps the previous active context for that workspace.
- A workspace can only select a repository visible in that workspace.

Tools use the active context as a default, then expose local selectors when
scope matters.

Examples:

- Git panel defaults to the workspace active repository.
- Branch picker defaults to the workspace active repository.
- Worktree picker defaults from the workspace context.
- Project/worktree settings use the active worktree by default.
- Generic commands use active context; scoped command variants may target a
  specific visible worktree.

Do not add a globally visible "current project" selector.

## Tool-Local Scope Selectors

Selectors belong near the tool that needs scope.

Allowed selector examples:

- Git panel repository/worktree selector.
- Branch picker repository/worktree selector.
- Worktree picker repository selector.
- Search scope selector.
- Settings worktree selector.
- Commands with dynamic per-worktree variants.

Rules:

- Reuse selector components.
- Do not implement separate picker UI for each surface.
- Choices must come from the current workspace's visible worktrees/repos.
- Non-repository worktrees may appear disabled in repo pickers when hiding them
  would confuse users.
- Tool-local selector state must not become global workspace ownership state.

Search defaults to the whole current workspace. A specific worktree filter is an
explicit local override.

## Search

Project search in code may keep upstream names such as `ProjectSearchView`, but
the product behavior is workspace search.

Decisions:

- Search must use the current workspace's scoped `Project`.
- Search must not use all globally known worktrees.
- Search tabs do not contribute path evidence.
- Default search scope is the full current workspace.
- Specific worktree search is a local filter.
- If a selected worktree filter is no longer visible, the UI should make that
  state understandable rather than silently hiding the selected filter.

## Git

Git repository data is globally cached. Active repository selection is
workspace-scoped.

Rules:

- Each workspace has its own active repository.
- Selecting a repo in Workspace A must not change Workspace B.
- Git panel, branch picker, worktree picker, project diff, branch diff, and
  uncommitted diff must operate on repositories visible to the current
  workspace.
- If a workspace has multiple worktrees and only some contain git repositories,
  selectors can show all worktrees and disable non-repository entries with a
  clear reason.
- Branch and stash controls should act on the selected/current workspace
  repository only.

## App Session And Persistence

Superzed has one durable app session.

Locked decisions:

- There is one app universe.
- There is one OS window for now.
- The OS window is a viewport over the app session.
- Workspaces are durable app-session data.
- Startup loads the app session; it does not "restore the last window" as an
  optional mode.
- `restore_on_startup`-style settings should not decide what survives.
- `cli_default_open_behavior` must not decide whether workspaces survive.
- Passing CLI paths must not replace the session.

Startup model:

```text
launch app
-> load durable app-session state
-> hydrate all persisted workspaces
-> if no workspace exists, create one default workspace
-> apply any CLI/open request into the loaded app session
-> show one OS window
```

CLI folder model:

```text
superzed ~/dev/paykit
-> load all persisted workspaces first
-> find a workspace whose evidence/root already matches ~/dev/paykit
-> activate it if found
-> otherwise create a new durable workspace and open a terminal there
-> never drop other workspaces
```

CLI file model:

```text
superzed ~/dev/paykit/src/main.rs
-> load all persisted workspaces first
-> open the file in a matching workspace if one exists
-> otherwise create a durable workspace and open the file there
```

Persistence should store:

- workspace ids/order,
- active workspace id,
- unresolved workspace ids,
- sidebar open/closed state,
- sidebar width/state,
- workspace pane/tab/layout state.

Persistence should not store attached worktree roots as workspace ownership.
Roots are re-derived from restored tabs.

If a workspace id fails to load, preserve it as unresolved. Do not silently drop
it during a later save.

## Windows

Superzed supports one OS window for now.

Decisions:

- Opening a new window should not create another independent state universe.
- "New Window" compatibility actions should create or activate a workspace in
  the singleton window, or no-op where appropriate.
- macOS Dock "New Workspace" should create a new workspace in the existing app
  session.
- Future multi-window support may exist only as multiple viewports over the
  same app session.

Do not implement multi-window state ownership unless explicitly working on that
future viewport model.

## Sidebar

The sidebar is the main navigation surface.

It should contain workspace navigation and active chat/thread navigation. It is
not a project opener.

Decisions:

- Workspaces are shown in the main sidebar.
- Project/open-folder buttons should not be primary sidebar controls.
- "Open Project", "Open Local Folder", "Open Remote Folder", and similar
  project-first affordances should be hidden or removed from primary UI.
- Project panel empty states should not push users toward Finder-based project
  opening.
- Sidebar open/closed state and width must persist.
- Since there is one window today, sidebar viewport state can be app-session
  state. Future multi-window support can move it to per-window viewport state.

## Chats And Threads

Agent/chat threads are tabs.

Decisions:

- Active/open threads should correspond to open tabs.
- A non-archived active thread must not exist only as sidebar metadata.
- Closing the thread tab archives/closes the thread.
- Opening a thread from history/archive creates or activates a tab.
- Sidebar chat/thread lists should not be grouped by project group or worktree.
- Thread metadata/history can remain global.
- The active/open state should be derived from tabs, not a fragile sync engine
  between metadata and UI rows.

The word "archive" exists in upstream code and UI today, but the product model
is active threads plus history. Future cleanup may rename this.

## Terminal Threads

Terminal threads are not part of the target model.

Decisions:

- Normal terminals remain normal terminal tabs.
- Terminals contribute cwd path evidence.
- Terminal thread rows should not appear in the sidebar.
- Terminal-thread project grouping/archive behavior should not be used.
- Legacy terminal-thread metadata may be ignored if removing it is risky.

Future agent-in-terminal behavior should be detected from normal terminal
sessions. If a terminal is running an agent, that state belongs to that terminal
session, not to a separate terminal-thread model.

Terminal process/session persistence is not implemented yet. Do not reintroduce
terminal threads as a shortcut to terminal persistence.

## Panes, Tabs, And Panels

Superzed intentionally moves more UI into panes/tabs.

Fork changes include:

- Terminal and debugger surfaces moved into tabs.
- Side panels moved into pane items where appropriate.
- Agent threads moved into tabs.
- Project pane toggle moved into the sidebar.
- Title bar chrome moved toward sidebar/sidebar chrome.
- Pane layout, card-style surfaces, split behavior, drag/drop, traffic-light
  spacing, tab spacing, and panel sizing were customized.

Merge guidance:

- Preserve the pane/tab-centric model.
- When upstream adds a dock/panel-only feature, consider whether it should be a
  tab/pane item in Superzed.
- Keep layout behavior stable for split, drag/drop, zoom, and project pane
  sizing.

## Project Panel And File Tree

The project panel is still useful, but it displays workspace-derived context.

Rules:

- File tree roots come from the active workspace's path evidence.
- The file tree must not show roots from other workspaces.
- Empty workspace should show no worktree/repository detected states, not open
  project calls to action.
- File tree loading should be lazy and bounded.
- Expanding directories should drive deeper filesystem loading.
- Large directories must not trigger whole-home recursive scanning just because
  a terminal cwd is `~`.
- Git status in file tree should be demand-driven and scoped to visible/current
  workspace roots.

Visual flattening of nested roots is acceptable for the tree display, but it
must not delete the child worktree/repository from the workspace's actual tool
scope.

## Lazy Filesystem, Git, Search, And Language Work

Superzed should not eagerly turn every terminal cwd into expensive IDE work.

Decisions:

- Worktree existence should be lightweight.
- File tree demand drives filesystem scanning.
- Search scans on demand for the current query/scope.
- Git status is demand-driven and scoped.
- Nested repository status should load when a visible/expanded nested repo or a
  git tool actually needs it.
- LSP/language tooling is driven by open editor buffers, not worktree existence.

Opening or deriving a worktree must not start language servers, whole-project
diagnostics, flycheck, `cargo check`, or equivalent checkers.

Language tooling model:

- Opening a language file may start the relevant language server.
- Closing the last editor buffer using a language server removes demand.
- Heavy disk diagnostics should be cancelled when demand disappears.
- Language server processes may remain briefly under an idle timeout, then stop
  if no demand returns.
- This policy should be generic across languages.
- Rust-specific flycheck cancellation must stay behind adapter-specific hooks,
  not generic rust hard-coding.
- Do not persist LSP processes, diagnostics results, or cargo-check state across
  app restarts.

## User-Facing Project UI

Project-first language should be reduced when it changes the mental model.

Keep:

- Internal crate/type names such as `Project` when useful for upstream
  mergeability.
- Existing settings/tasks/rules concepts when they operate on the active or
  selected worktree.

Avoid:

- Visible "Open Project" as a primary workflow.
- Global project selector.
- Footer project/worktree controls that imply the workspace owns a chosen
  project.
- Project grouping as sidebar/chat ownership.

Use clearer product language where practical:

- workspace search instead of project search,
- worktree settings instead of project settings,
- no worktree detected,
- no repository detected.

Do not mass-rename crates, files, and upstream action identifiers only for
cosmetic reasons. Preserve upstream-compatible internals where semantics are
correct.

## Defaults And Visual Direction

Superzed intentionally ships different defaults and visual choices from
upstream.

Committed changes include:

- Compact UI and editor defaults.
- Different font sizes and line heights.
- Project panel on the right by default.
- Hidden root when appropriate.
- Auto-fold directories disabled.
- Smaller project panel indentation.
- Project panel scrollbars and indent guides reduced/hidden.
- Editor scrollbar size setting and tuned scrollbar behavior.
- Overlay/autohiding scrollbars.
- Terminal resize anchoring improvements.
- Shell-owned terminal titles.
- Compact breadcrumbs and breadcrumb symbol settings.
- File icons enabled in tabs.
- Reduced gutter/fold/runnable visual noise.
- Collaboration/status/project controls reduced or moved.

Merge guidance:

- Preserve Superzed's quieter, terminal-first defaults unless explicitly
  changing the product direction.
- Do not accept upstream defaults blindly if they reintroduce project-first or
  dock-heavy UI.

## Icons And Naming Polish

Fork-specific icon and label changes include:

- Superzed config recognized as JSONC.
- Robot icon normalized.
- Diff icon used for project diff tabs.
- Project diff tabs renamed for clearer tab presentation.
- Project/file tree and sidebar icons adjusted.

These are lower priority than architecture during conflicts, but should be
preserved when easy.

## Remote And Project Opening

Remote/project-opening UI is not a primary Superzed workflow.

Guidance:

- Do not expose "Open Remote Folder" or "Open Local Folder" as primary sidebar
  buttons.
- If upstream remote functionality remains in backend code, keep it only where
  it does not conflict with terminal-first workspace behavior.
- Local paths from CLI or file opens must route into the durable app session.
- A folder path may create a terminal rooted at that folder if no matching
  workspace exists.

## Build And Verification Expectations

For normal dev verification, use:

```sh
cargo build -p zed --bin superzed
```

For fast production-like local runs, use:

```sh
cargo run --profile release-fast -p zed --bin superzed
```

For macOS bundles, use:

```sh
script/bundle-mac
```

Do not use debug `cargo run` performance as a production signal.

## Updating This File

Update this file when adding a permanent fork deviation from upstream.

Good additions:

- Product decisions.
- Ownership decisions.
- Merge-conflict resolution rules.
- User-facing behavior that must differ from upstream.
- Why a divergence exists.

Avoid adding:

- Temporary implementation steps.
- Long code walkthroughs.
- TODO lists that belong in `kb/plans`.
- Exhaustive commit history.
