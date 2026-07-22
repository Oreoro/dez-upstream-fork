pub mod disconnected_overlay;
mod remote_connections;
mod remote_servers;
pub mod sidebar_recent_projects;
mod ssh_config;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context as _;

use chrono::{DateTime, Utc};

use fs::Fs;

#[cfg(target_os = "windows")]
mod wsl_picker;

use remote::RemoteConnectionOptions;
pub use remote_connection::{RemoteConnectionModal, connect, connect_with_modal};
pub use remote_connections::{
    connect_ssh_host, navigate_to_positions, open_non_ssh_remote_project,
};

use disconnected_overlay::DisconnectedOverlay;
use fuzzy_nucleo::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    Action, AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Subscription, Task, TaskExt, WeakEntity, Window, actions, px,
};

use picker::{
    Picker, PickerDelegate, ScrollBehavior,
    highlighted_match_with_paths::{HighlightedMatch, HighlightedMatchWithPaths},
};
use project::{Worktree, git_store::Repository};
pub use remote_connections::RemoteSettings;
pub use remote_servers::RemoteServerProjects;
use settings::{Settings, WorktreeId};
use workspace::ProjectGroupKey;

use ui::{
    ButtonLike, ContextMenu, Divider, HighlightedLabel, KeyBinding, ListItem, ListItemSpacing,
    ListSubHeader, PopoverMenu, PopoverMenuHandle, TintColor, Tooltip, prelude::*,
};
use util::{ResultExt, paths::PathExt};
use workspace::{
    HistoryManager, ModalView, MultiWorkspace, OpenMode, OpenOptions, PathList, RecentWorkspace,
    SerializedWorkspaceLocation, Workspace, WorkspaceDb, WorkspaceId,
    notifications::DetachAndPromptErr, with_active_or_new_workspace,
};
use zed_actions::{OpenRecent, hosts::ConnectSshHost};

actions!(
    recent_projects,
    [ToggleActionsMenu, RemoveSelected, AddToWorkspace,]
);

#[derive(Clone, Debug)]
pub struct RecentProjectEntry {
    pub name: SharedString,
    pub full_path: SharedString,
    pub paths: Vec<PathBuf>,
    pub workspace_id: WorkspaceId,
    pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Debug)]
struct OpenFolderEntry {
    worktree_id: WorktreeId,
    name: SharedString,
    path: PathBuf,
    branch: Option<SharedString>,
    is_active: bool,
    connection_options: Option<RemoteConnectionOptions>,
}

#[derive(Clone, Debug)]
enum ProjectPickerEntry {
    Header(SharedString),
    /// A currently open folder from the active workspace's "Current Folders" section.
    ///
    /// `index` points into `RecentProjectsDelegate::open_folders`, and `positions` stores the
    /// fuzzy-match highlight positions for rendering the folder name.
    OpenFolder {
        index: usize,
        positions: Vec<usize>,
    },
    /// A project group from the current window's "This Window" section.
    ///
    /// These entries come from `RecentProjectsDelegate::window_project_groups`, not from the
    /// recent-project database. Empty queries list every project group known to the current
    /// window; non-empty queries list matching project groups. Confirming one activates or loads
    /// that project group in the current window.
    ProjectGroup(StringMatch),
    /// A workspace from the recent-project database's "Recent Projects" section.
    ///
    /// The match's `candidate_id` indexes into `RecentProjectsDelegate::workspaces`. Confirming
    /// one opens that recent workspace in the current Super Zed workspace.
    RecentProject(StringMatch),
}

fn is_selectable_entry(entry: &ProjectPickerEntry) -> bool {
    matches!(
        entry,
        ProjectPickerEntry::OpenFolder { .. }
            | ProjectPickerEntry::ProjectGroup(_)
            | ProjectPickerEntry::RecentProject(_)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectPickerStyle {
    Modal,
    Popover,
}

pub async fn get_recent_projects(
    current_workspace_id: Option<WorkspaceId>,
    limit: Option<usize>,
    fs: Arc<dyn fs::Fs>,
    db: &WorkspaceDb,
) -> Vec<RecentProjectEntry> {
    let workspaces = db
        .recent_project_workspaces(fs.as_ref())
        .await
        .unwrap_or_default();

    let filtered: Vec<_> = workspaces
        .into_iter()
        .filter(|workspace| Some(workspace.workspace_id) != current_workspace_id)
        .filter(|workspace| matches!(workspace.location, SerializedWorkspaceLocation::Local))
        .collect();

    let mut all_paths: Vec<PathBuf> = filtered
        .iter()
        .flat_map(|workspace| workspace.identity_paths.paths().iter().cloned())
        .collect();
    all_paths.sort_unstable();
    all_paths.dedup();
    let path_details =
        util::disambiguate::compute_disambiguation_details(&all_paths, |path, detail| {
            project::path_suffix(path, detail)
        });
    let path_detail_map: std::collections::HashMap<PathBuf, usize> =
        all_paths.into_iter().zip(path_details).collect();

    let entries: Vec<RecentProjectEntry> = filtered
        .into_iter()
        .map(|workspace| {
            let paths: Vec<PathBuf> = workspace.paths.paths().to_vec();
            let ordered_paths: Vec<&PathBuf> = workspace.identity_paths.ordered_paths().collect();

            let name = ordered_paths
                .iter()
                .map(|p| {
                    let detail = path_detail_map.get(*p).copied().unwrap_or(0);
                    project::path_suffix(p, detail)
                })
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(", ");

            let full_path = ordered_paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("\n");

            RecentProjectEntry {
                name: SharedString::from(name),
                full_path: SharedString::from(full_path),
                paths,
                workspace_id: workspace.workspace_id,
                timestamp: workspace.timestamp,
            }
        })
        .collect();

    match limit {
        Some(n) => entries.into_iter().take(n).collect(),
        None => entries,
    }
}

pub async fn delete_recent_project(workspace_id: WorkspaceId, db: &WorkspaceDb) {
    let _ = db.delete_workspace_by_id(workspace_id).await;
}

fn get_open_folders(workspace: &Workspace, cx: &App) -> Vec<OpenFolderEntry> {
    let project = workspace.project().read(cx);
    let connection_options = project.remote_connection_options(cx);
    let visible_worktrees: Vec<_> = project.visible_worktrees(cx).collect();

    if visible_worktrees.len() <= 1 {
        return Vec::new();
    }

    let active_worktree_id = if let Some(repo) = project.active_repository(cx) {
        let repo = repo.read(cx);
        let repo_path = &repo.work_directory_abs_path;
        project.visible_worktrees(cx).find_map(|worktree| {
            let worktree_path = worktree.read(cx).abs_path();
            (worktree_path == *repo_path || worktree_path.starts_with(repo_path.as_ref()))
                .then(|| worktree.read(cx).id())
        })
    } else {
        project
            .visible_worktrees(cx)
            .next()
            .map(|wt| wt.read(cx).id())
    };

    let mut all_paths: Vec<PathBuf> = visible_worktrees
        .iter()
        .map(|wt| wt.read(cx).abs_path().to_path_buf())
        .collect();
    all_paths.sort_unstable();
    all_paths.dedup();
    let path_details =
        util::disambiguate::compute_disambiguation_details(&all_paths, |path, detail| {
            project::path_suffix(path, detail)
        });
    let path_detail_map: std::collections::HashMap<PathBuf, usize> =
        all_paths.into_iter().zip(path_details).collect();

    let git_store = project.git_store().read(cx);
    let repositories: Vec<_> = git_store.repositories().values().cloned().collect();

    let mut entries: Vec<OpenFolderEntry> = visible_worktrees
        .into_iter()
        .map(|worktree| {
            let worktree_ref = worktree.read(cx);
            let worktree_id = worktree_ref.id();
            let path = worktree_ref.abs_path().to_path_buf();
            let detail = path_detail_map.get(&path).copied().unwrap_or(0);
            let name = SharedString::from(project::path_suffix(&path, detail));
            let branch = get_branch_for_worktree(worktree_ref, &repositories, cx);
            let is_active = active_worktree_id == Some(worktree_id);
            OpenFolderEntry {
                worktree_id,
                name,
                path,
                branch,
                is_active,
                connection_options: connection_options.clone(),
            }
        })
        .collect();

    entries.sort_by_key(|entry| entry.name.to_lowercase());
    entries
}

fn get_branch_for_worktree(
    worktree: &Worktree,
    repositories: &[Entity<Repository>],
    cx: &App,
) -> Option<SharedString> {
    let worktree_abs_path = worktree.abs_path();
    repositories
        .iter()
        .filter(|repo| {
            let repo_path = &repo.read(cx).work_directory_abs_path;
            *repo_path == worktree_abs_path || worktree_abs_path.starts_with(repo_path.as_ref())
        })
        .max_by_key(|repo| repo.read(cx).work_directory_abs_path.as_os_str().len())
        .and_then(|repo| {
            repo.read(cx)
                .branch
                .as_ref()
                .map(|branch| SharedString::from(branch.name().to_string()))
        })
}

pub fn init(cx: &mut App) {
    #[cfg(target_os = "windows")]
    cx.on_action(|_: &zed_actions::wsl_actions::OpenFolderInWsl, cx| {
        with_active_or_new_workspace(cx, move |workspace, window, cx| {
            use gpui::PathPromptOptions;
            use project::DirectoryLister;

            let paths = workspace.prompt_for_open_path(
                PathPromptOptions {
                    files: true,
                    directories: true,
                    multiple: false,
                    prompt: None,
                },
                DirectoryLister::Local(
                    workspace.project().clone(),
                    workspace.app_state().fs.clone(),
                ),
                window,
                cx,
            );

            let app_state = workspace.app_state().clone();
            let window_handle = window.window_handle().downcast::<MultiWorkspace>();

            cx.spawn_in(window, async move |workspace, cx| {
                use util::paths::SanitizedPath;

                let Some(paths) = paths.await.log_err().flatten() else {
                    return;
                };

                let wsl_path = paths
                    .iter()
                    .find_map(util::paths::WslPath::from_path);

                if let Some(util::paths::WslPath { distro, path }) = wsl_path {
                    use remote::WslConnectionOptions;

                    let connection_options = RemoteConnectionOptions::Wsl(WslConnectionOptions {
                        distro_name: distro.to_string(),
                        user: None,
                    });

                    let requesting_window = window_handle;

                    let open_options = workspace::OpenOptions {
                        requesting_window,
                        ..Default::default()
                    };

                    open_non_ssh_remote_project(connection_options, vec![path.into()], app_state, open_options, cx).await.log_err();
                    return;
                }

                let paths = paths
                    .into_iter()
                    .filter_map(|path| SanitizedPath::new(&path).local_to_wsl())
                    .collect::<Vec<_>>();

                if paths.is_empty() {
                    let message = indoc::indoc! { r#"
                        Invalid path specified when trying to open a folder inside WSL.

                        Please note that Zed currently does not support opening network share folders inside wsl.
                    "#};

                    let _ = cx.prompt(gpui::PromptLevel::Critical, "Invalid path", Some(&message), &["OK"]).await;
                    return;
                }

                workspace.update_in(cx, |workspace, window, cx| {
                    workspace.toggle_modal(window, cx, |window, cx| {
                        crate::wsl_picker::WslOpenModal::new(paths, window, cx)
                    });
                }).log_err();
            })
            .detach();
        });
    });

    #[cfg(target_os = "windows")]
    cx.on_action(|_: &zed_actions::wsl_actions::OpenWsl, cx| {
        with_active_or_new_workspace(cx, move |workspace, window, cx| {
            let handle = cx.entity().downgrade();
            let fs = workspace.project().read(cx).fs().clone();
            workspace.toggle_modal(window, cx, |window, cx| {
                RemoteServerProjects::wsl(fs, window, handle, cx)
            });
        });
    });

    #[cfg(target_os = "windows")]
    cx.on_action(|open_wsl: &remote::OpenWslPath, cx| {
        let open_wsl = open_wsl.clone();
        with_active_or_new_workspace(cx, move |workspace, window, cx| {
            let fs = workspace.project().read(cx).fs().clone();
            add_wsl_distro(fs, &open_wsl.distro, cx);
            let requesting_window = window.window_handle().downcast::<MultiWorkspace>();
            let open_options = OpenOptions {
                requesting_window,
                ..Default::default()
            };

            let app_state = workspace.app_state().clone();

            cx.spawn_in(window, async move |_, cx| {
                open_non_ssh_remote_project(
                    RemoteConnectionOptions::Wsl(open_wsl.distro.clone()),
                    open_wsl.paths,
                    app_state,
                    open_options,
                    cx,
                )
                .await
            })
            .detach();
        });
    });

    cx.on_action(|_: &OpenRecent, cx| {
        match cx
            .active_window()
            .and_then(|w| w.downcast::<MultiWorkspace>())
        {
            Some(multi_workspace) => {
                cx.defer(move |cx| {
                    multi_workspace
                        .update(cx, |multi_workspace, window, cx| {
                            let workspace = multi_workspace.workspace().clone();
                            workspace.update(cx, |workspace, cx| {
                                let Some(recent_projects) =
                                    workspace.active_modal::<RecentProjects>(cx)
                                else {
                                    let focus_handle = workspace.focus_handle(cx);
                                    RecentProjects::open(
                                        workspace,
                                        Vec::new(),
                                        window,
                                        focus_handle,
                                        cx,
                                    );
                                    return;
                                };

                                recent_projects.update(cx, |recent_projects, cx| {
                                    recent_projects
                                        .picker
                                        .update(cx, |picker, cx| picker.cycle_selection(window, cx))
                                });
                            });
                        })
                        .log_err();
                });
            }
            None => {
                with_active_or_new_workspace(cx, move |workspace, window, cx| {
                    let Some(recent_projects) = workspace.active_modal::<RecentProjects>(cx) else {
                        let focus_handle = workspace.focus_handle(cx);
                        RecentProjects::open(workspace, Vec::new(), window, focus_handle, cx);
                        return;
                    };

                    recent_projects.update(cx, |recent_projects, cx| {
                        recent_projects
                            .picker
                            .update(cx, |picker, cx| picker.cycle_selection(window, cx))
                    });
                });
            }
        }
    });
    cx.on_action(|_: &ConnectSshHost, cx| {
        with_active_or_new_workspace(cx, move |workspace, window, cx| {
            let handle = cx.entity().downgrade();
            let fs = workspace.project().read(cx).fs().clone();
            workspace.toggle_modal(window, cx, |window, cx| {
                RemoteServerProjects::connect_ssh_host(fs, window, handle, cx)
            })
        });
    });

    cx.observe_new(DisconnectedOverlay::register).detach();
}

#[cfg(target_os = "windows")]
pub fn add_wsl_distro(
    fs: Arc<dyn project::Fs>,
    connection_options: &remote::WslConnectionOptions,
    cx: &App,
) {
    use gpui::ReadGlobal;
    use settings::SettingsStore;

    let distro_name = connection_options.distro_name.clone();
    let user = connection_options.user.clone();
    SettingsStore::global(cx).update_settings_file(fs, move |setting, _| {
        let connections = setting
            .remote
            .wsl_connections
            .get_or_insert(Default::default());

        if !connections
            .iter()
            .any(|conn| conn.distro_name == distro_name && conn.user == user)
        {
            use std::collections::BTreeSet;

            connections.push(settings::WslConnection {
                distro_name,
                user,
                projects: BTreeSet::new(),
            })
        }
    });
}

pub struct RecentProjects {
    pub picker: Entity<Picker<RecentProjectsDelegate>>,
    _subscriptions: Vec<Subscription>,
}

impl ModalView for RecentProjects {
    fn on_before_dismiss(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> workspace::DismissDecision {
        let submenu_focused = self.picker.update(cx, |picker, cx| {
            picker.delegate.actions_menu_handle.is_focused(window, cx)
        });
        workspace::DismissDecision::Dismiss(!submenu_focused)
    }
}

impl RecentProjects {
    fn new(
        delegate: RecentProjectsDelegate,
        fs: Option<Arc<dyn Fs>>,
        rem_width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let style = delegate.style;
        let picker = cx.new(|cx| {
            Picker::list(delegate, window, cx)
                .list_measure_all()
                .initial_width(rems(rem_width))
                .show_scrollbar(true)
        });

        let picker_focus_handle = picker.focus_handle(cx);
        picker.update(cx, |picker, _| {
            picker.delegate.focus_handle = picker_focus_handle;
        });

        let mut subscriptions = vec![cx.subscribe(&picker, |_, _, _, cx| cx.emit(DismissEvent))];

        if style == ProjectPickerStyle::Popover {
            let picker_focus = picker.focus_handle(cx);
            subscriptions.push(
                cx.on_focus_out(&picker_focus, window, |this, _, window, cx| {
                    let submenu_focused = this.picker.update(cx, |picker, cx| {
                        picker.delegate.actions_menu_handle.is_focused(window, cx)
                    });
                    if !submenu_focused {
                        cx.emit(DismissEvent);
                    }
                }),
            );
        }
        // We do not want to block the UI on a potentially lengthy call to DB, so we're gonna swap
        // out workspace locations once the future runs to completion.
        let db = WorkspaceDb::global(cx);
        cx.spawn_in(window, async move |this, cx| {
            let Some(fs) = fs else { return };
            let workspaces = db
                .recent_project_workspaces(fs.as_ref())
                .await
                .log_err()
                .unwrap_or_default();
            this.update_in(cx, move |this, window, cx| {
                this.picker.update(cx, move |picker, cx| {
                    picker.delegate.set_workspaces(workspaces);
                    picker.update_matches(picker.query(cx), window, cx)
                })
            })
            .ok();
        })
        .detach();
        Self {
            picker,
            _subscriptions: subscriptions,
        }
    }

    pub fn open(
        workspace: &mut Workspace,
        window_project_groups: Vec<ProjectGroupKey>,
        window: &mut Window,
        focus_handle: FocusHandle,
        cx: &mut Context<Workspace>,
    ) {
        let weak = cx.entity().downgrade();
        let open_folders = get_open_folders(workspace, cx);
        let fs = Some(workspace.app_state().fs.clone());

        workspace.toggle_modal(window, cx, |window, cx| {
            let delegate = RecentProjectsDelegate::new(
                weak,
                focus_handle,
                open_folders,
                window_project_groups,
                ProjectPickerStyle::Modal,
            );

            Self::new(delegate, fs, 42., window, cx)
        })
    }

    pub fn popover(
        workspace: WeakEntity<Workspace>,
        window_project_groups: Vec<ProjectGroupKey>,
        focus_handle: FocusHandle,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let (open_folders, fs) = workspace
            .upgrade()
            .map(|workspace| {
                let workspace = workspace.read(cx);
                (
                    get_open_folders(workspace, cx),
                    Some(workspace.app_state().fs.clone()),
                )
            })
            .unwrap_or_else(|| (Vec::new(), None));

        cx.new(|cx| {
            let delegate = RecentProjectsDelegate::new(
                workspace,
                focus_handle,
                open_folders,
                window_project_groups,
                ProjectPickerStyle::Popover,
            );
            let list = Self::new(delegate, fs, 20., window, cx);
            list.picker.focus_handle(cx).focus(window, cx);
            list
        })
    }

    fn handle_toggle_open_menu(
        &mut self,
        _: &ToggleActionsMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.picker.update(cx, |picker, cx| {
            let menu_handle = &picker.delegate.actions_menu_handle;
            if menu_handle.is_deployed() {
                menu_handle.hide(cx);
            } else {
                menu_handle.show(window, cx);
            }
        });
    }

    fn handle_remove_selected(
        &mut self,
        _: &RemoveSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.picker.update(cx, |picker, cx| {
            let ix = picker.delegate.selected_index;

            match picker.delegate.filtered_entries.get(ix) {
                Some(ProjectPickerEntry::OpenFolder { index, .. }) => {
                    if let Some(folder) = picker.delegate.open_folders.get(*index) {
                        let worktree_id = folder.worktree_id;
                        let path = folder.path.clone();
                        let Some(workspace) = picker.delegate.workspace.upgrade() else {
                            return;
                        };
                        let remove_root = workspace.update(cx, |workspace, cx| {
                            workspace.remove_project_root(path, worktree_id, window, cx)
                        });
                        cx.spawn_in(window, async move |picker, cx| {
                            remove_root.await?;
                            picker.update_in(cx, |picker, window, cx| {
                                let Some(workspace) = picker.delegate.workspace.upgrade() else {
                                    return;
                                };
                                picker.delegate.open_folders =
                                    get_open_folders(workspace.read(cx), cx);
                                let query = picker.query(cx);
                                picker.update_matches(query, window, cx);
                            })?;
                            anyhow::Ok(())
                        })
                        .detach_and_prompt_err(
                            "Failed to remove folder from project",
                            window,
                            cx,
                            |_, _, _| None,
                        );
                    }
                }
                Some(ProjectPickerEntry::ProjectGroup(hit)) => {
                    if let Some(key) = picker
                        .delegate
                        .window_project_groups
                        .get(hit.candidate_id)
                        .cloned()
                    {
                        if picker.delegate.is_active_project_group(&key, cx) {
                            return;
                        }
                        picker.delegate.remove_project_group(key, window, cx);
                        let query = picker.query(cx);
                        picker.update_matches(query, window, cx);
                    }
                }
                Some(ProjectPickerEntry::RecentProject(_)) => {
                    picker.delegate.delete_recent_project(ix, window, cx);
                }
                _ => {}
            }
        });
    }

    fn handle_add_to_workspace(
        &mut self,
        _: &AddToWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.picker.update(cx, |picker, cx| {
            let ix = picker.delegate.selected_index;

            if let Some(ProjectPickerEntry::RecentProject(hit)) =
                picker.delegate.filtered_entries.get(ix)
            {
                if let Some(workspace) = picker.delegate.workspaces.get(hit.candidate_id) {
                    if matches!(workspace.location, SerializedWorkspaceLocation::Local) {
                        let paths_to_add = workspace.paths.paths().to_vec();
                        picker
                            .delegate
                            .add_paths_to_project(paths_to_add, window, cx);
                    }
                }
            }
        });
    }
}

impl EventEmitter<DismissEvent> for RecentProjects {}

impl Focusable for RecentProjects {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for RecentProjects {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("RecentProjects")
            .on_action(cx.listener(Self::handle_toggle_open_menu))
            .on_action(cx.listener(Self::handle_remove_selected))
            .on_action(cx.listener(Self::handle_add_to_workspace))
            .child(self.picker.clone())
    }
}

pub struct RecentProjectsDelegate {
    workspace: WeakEntity<Workspace>,
    open_folders: Vec<OpenFolderEntry>,
    window_project_groups: Vec<ProjectGroupKey>,
    workspaces: Vec<RecentWorkspace>,
    filtered_entries: Vec<ProjectPickerEntry>,
    selected_index: usize,
    render_paths: bool,
    snap_selection_to_first_non_header_match: bool,
    focus_handle: FocusHandle,
    style: ProjectPickerStyle,
    actions_menu_handle: PopoverMenuHandle<ContextMenu>,
}

impl RecentProjectsDelegate {
    fn new(
        workspace: WeakEntity<Workspace>,
        focus_handle: FocusHandle,
        open_folders: Vec<OpenFolderEntry>,
        window_project_groups: Vec<ProjectGroupKey>,
        style: ProjectPickerStyle,
    ) -> Self {
        let render_paths = style == ProjectPickerStyle::Modal;
        Self {
            workspace,
            open_folders,
            window_project_groups,
            workspaces: Vec::new(),
            filtered_entries: Vec::new(),
            selected_index: 0,
            render_paths,
            snap_selection_to_first_non_header_match: true,
            focus_handle,
            style,
            actions_menu_handle: PopoverMenuHandle::default(),
        }
    }

    pub fn set_workspaces(&mut self, workspaces: Vec<RecentWorkspace>) {
        self.workspaces = workspaces;
    }

    fn filtered_entries_include_remote_project(&self) -> bool {
        self.filtered_entries
            .iter()
            .any(|entry| self.entry_is_remote_project(entry))
    }

    fn entry_is_remote_project(&self, entry: &ProjectPickerEntry) -> bool {
        match entry {
            ProjectPickerEntry::Header(_) => false,
            ProjectPickerEntry::OpenFolder { index, .. } => self
                .open_folders
                .get(*index)
                .is_some_and(|folder| folder.connection_options.is_some()),
            ProjectPickerEntry::ProjectGroup(hit) => self
                .window_project_groups
                .get(hit.candidate_id)
                .is_some_and(|key| key.host().is_some()),
            ProjectPickerEntry::RecentProject(hit) => self
                .workspaces
                .get(hit.candidate_id)
                .is_some_and(|workspace| {
                    matches!(workspace.location, SerializedWorkspaceLocation::Remote(_))
                }),
        }
    }
}
impl EventEmitter<DismissEvent> for RecentProjectsDelegate {}
impl PickerDelegate for RecentProjectsDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "recent projects"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search projects…".into()
    }

    fn match_count(&self) -> usize {
        self.filtered_entries.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn can_select(&self, ix: usize, _window: &mut Window, _cx: &mut Context<Picker<Self>>) -> bool {
        matches!(
            self.filtered_entries.get(ix),
            Some(
                ProjectPickerEntry::OpenFolder { .. }
                    | ProjectPickerEntry::ProjectGroup(_)
                    | ProjectPickerEntry::RecentProject(_)
            )
        )
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> gpui::Task<()> {
        let query = query.trim_start();
        let case = fuzzy_nucleo::Case::smart_if_uppercase_in(query);
        let is_empty_query = query.is_empty();

        let folder_matches = if self.open_folders.is_empty() {
            Vec::new()
        } else {
            let candidates: Vec<_> = self
                .open_folders
                .iter()
                .enumerate()
                .map(|(id, folder)| StringMatchCandidate::new(id, folder.name.as_ref()))
                .collect();

            match_strings(
                &candidates,
                query,
                case,
                fuzzy_nucleo::LengthPenalty::On,
                100,
            )
        };

        let project_group_candidates: Vec<_> = self
            .window_project_groups
            .iter()
            .enumerate()
            .map(|(id, key)| {
                let combined_string = key
                    .path_list()
                    .ordered_paths()
                    .map(|path| path.compact().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .concat();
                StringMatchCandidate::new(id, &combined_string)
            })
            .collect();

        let project_group_matches = match_strings(
            &project_group_candidates,
            query,
            case,
            fuzzy_nucleo::LengthPenalty::On,
            100,
        );

        // Build candidates for recent projects (not current, not sibling, not open folder)
        let recent_candidates: Vec<_> = self
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, workspace)| self.is_valid_recent_candidate(workspace, cx))
            .map(|(id, workspace)| {
                let combined_string = workspace
                    .identity_paths
                    .ordered_paths()
                    .map(|path| path.compact().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .concat();
                StringMatchCandidate::new(id, &combined_string)
            })
            .collect();

        let recent_matches = match_strings(
            &recent_candidates,
            query,
            case,
            fuzzy_nucleo::LengthPenalty::On,
            100,
        );

        let mut entries = Vec::new();

        if !self.open_folders.is_empty() {
            let matched_folders: Vec<_> = if is_empty_query {
                (0..self.open_folders.len())
                    .map(|i| (i, Vec::new()))
                    .collect()
            } else {
                folder_matches
                    .iter()
                    .map(|m| (m.candidate_id, m.positions.clone()))
                    .collect()
            };

            if !matched_folders.is_empty() {
                entries.push(ProjectPickerEntry::Header("Current Folders".into()));
                for (index, positions) in matched_folders {
                    entries.push(ProjectPickerEntry::OpenFolder { index, positions });
                }
            }
        }

        let has_projects_to_show = if is_empty_query {
            !project_group_candidates.is_empty()
        } else {
            !project_group_matches.is_empty()
        };

        if has_projects_to_show {
            entries.push(ProjectPickerEntry::Header("This Window".into()));

            if is_empty_query {
                for id in 0..self.window_project_groups.len() {
                    entries.push(ProjectPickerEntry::ProjectGroup(StringMatch {
                        candidate_id: id,
                        score: 0.0,
                        positions: Vec::new(),
                        string: Default::default(),
                    }));
                }
            } else {
                for m in project_group_matches {
                    entries.push(ProjectPickerEntry::ProjectGroup(m));
                }
            }
        }

        let has_recent_to_show = if is_empty_query {
            !recent_candidates.is_empty()
        } else {
            !recent_matches.is_empty()
        };

        if has_recent_to_show {
            entries.push(ProjectPickerEntry::Header("Recent Projects".into()));

            if is_empty_query {
                for (id, workspace) in self.workspaces.iter().enumerate() {
                    if self.is_valid_recent_candidate(workspace, cx) {
                        entries.push(ProjectPickerEntry::RecentProject(StringMatch {
                            candidate_id: id,
                            score: 0.0,
                            positions: Vec::new(),
                            string: Default::default(),
                        }));
                    }
                }
            } else {
                for m in recent_matches {
                    entries.push(ProjectPickerEntry::RecentProject(m));
                }
            }
        }

        self.filtered_entries = entries;

        if self.snap_selection_to_first_non_header_match {
            self.selected_index = self
                .filtered_entries
                .iter()
                .position(|e| !matches!(e, ProjectPickerEntry::Header(_)))
                .unwrap_or(0);
        }
        self.snap_selection_to_first_non_header_match = true;
        Task::ready(())
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        match self.filtered_entries.get(self.selected_index) {
            Some(ProjectPickerEntry::OpenFolder { index, .. }) => {
                let Some(folder) = self.open_folders.get(*index) else {
                    return;
                };
                let worktree_id = folder.worktree_id;
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        let git_store = workspace.project().read(cx).git_store().clone();
                        git_store.update(cx, |git_store, cx| {
                            git_store.set_active_repo_for_worktree(worktree_id, cx);
                        });
                    });
                }
                cx.emit(DismissEvent);
            }
            Some(ProjectPickerEntry::ProjectGroup(selected_match)) => {
                let Some(key) = self.window_project_groups.get(selected_match.candidate_id) else {
                    return;
                };

                let key = key.clone();
                if let Some(handle) = window.window_handle().downcast::<MultiWorkspace>() {
                    cx.defer(move |cx| {
                        // Try to activate an existing workspace for this project group
                        // first, so we preserve the actual worktree paths (which may
                        // differ from the main git worktree paths stored in the key).
                        if let Some(workspace) = handle
                            .update(cx, |multi_workspace, _window, cx| {
                                multi_workspace.last_active_workspace_for_group(&key, cx)
                            })
                            .log_err()
                            .flatten()
                        {
                            handle
                                .update(cx, |multi_workspace, window, cx| {
                                    multi_workspace.activate(workspace, None, window, cx);
                                })
                                .log_err();
                        } else {
                            let path_list = key.path_list().clone();
                            let host = key.host();
                            if let Some(task) = handle
                                .update(cx, |multi_workspace, window, cx| {
                                    let modal_workspace = multi_workspace.workspace().clone();
                                    multi_workspace.find_or_create_workspace(
                                        path_list,
                                        host,
                                        Some(key.clone()),
                                        move |options, window, cx| {
                                            connect_with_modal(
                                                &modal_workspace,
                                                options,
                                                window,
                                                cx,
                                            )
                                        },
                                        &[],
                                        None,
                                        OpenMode::Activate,
                                        window,
                                        cx,
                                    )
                                })
                                .log_err()
                            {
                                task.detach_and_log_err(cx);
                            }
                        }
                    });
                }
                cx.emit(DismissEvent);
            }
            Some(ProjectPickerEntry::RecentProject(selected_match)) => {
                let candidate_id = selected_match.candidate_id;
                self.open_recent_projects(candidate_id, secondary, window, cx);
            }
            _ => {}
        }
    }

    fn dismissed(&mut self, _window: &mut Window, _: &mut Context<Picker<Self>>) {}

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        let text = if self.workspaces.is_empty() && self.open_folders.is_empty() {
            "Recently opened projects will show up here".into()
        } else {
            "No matches".into()
        };
        Some(text)
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        match self.filtered_entries.get(ix)? {
            ProjectPickerEntry::Header(title) => Some(
                v_flex()
                    .w_full()
                    .gap_1()
                    .when(ix > 0, |this| this.mt_1().child(Divider::horizontal()))
                    .child(ListSubHeader::new(title.clone()).inset(true))
                    .into_any_element(),
            ),
            ProjectPickerEntry::OpenFolder { index, positions } => {
                let folder = self.open_folders.get(*index)?;
                let name = folder.name.clone();
                let path = folder.path.compact();
                let branch = folder.branch.clone();
                let is_active = folder.is_active;
                let worktree_id = folder.worktree_id;
                let positions = positions.clone();
                let show_path = self.style == ProjectPickerStyle::Modal;

                let secondary_actions = h_flex()
                    .gap_1()
                    .child(
                        IconButton::new(("remove-folder", worktree_id.to_usize()), IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip({
                                let focus_handle = self.focus_handle.clone();
                                move |_, cx| {
                                    Tooltip::for_action_in(
                                        "Remove Folder from Project",
                                        &RemoveSelected,
                                        &focus_handle,
                                        cx,
                                    )
                                }
                            })
                            .on_click(cx.listener(move |picker, _, window, cx| {
                                let Some(workspace) = picker.delegate.workspace.upgrade() else {
                                    return;
                                };
                                workspace.update(cx, |workspace, cx| {
                                    let project = workspace.project().clone();
                                    project.update(cx, |project, cx| {
                                        project.remove_worktree(worktree_id, cx);
                                    });
                                });
                                picker.delegate.open_folders =
                                    get_open_folders(workspace.read(cx), cx);
                                let query = picker.query(cx);
                                picker.update_matches(query, window, cx);
                            })),
                    )
                    .into_any_element();

                let icon = icon_for_remote_connection(folder.connection_options.as_ref());
                let show_icon = self.filtered_entries_include_remote_project();

                let tooltip_path: SharedString = path.to_string_lossy().to_string().into();
                let tooltip_branch = branch.clone();

                Some(
                    ListItem::new(ix)
                        .toggle_state(selected)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .child(
                            h_flex()
                                .id("open_folder_item")
                                .w_full()
                                .min_w_0()
                                .gap_2p5()
                                .when(show_icon, |this| {
                                    this.child(Icon::new(icon).color(Color::Muted))
                                })
                                .child(
                                    v_flex()
                                        .min_w_0()
                                        .child(
                                            h_flex()
                                                .gap_1()
                                                .child(HighlightedLabel::new(
                                                    name.to_string(),
                                                    positions,
                                                ))
                                                .when_some(branch, |this, branch| {
                                                    this.child(
                                                        Label::new(branch)
                                                            .color(Color::Muted)
                                                            .truncate(),
                                                    )
                                                })
                                                .when(is_active, |this| {
                                                    this.child(
                                                        Icon::new(IconName::Check)
                                                            .size(IconSize::Small)
                                                            .color(Color::Accent),
                                                    )
                                                }),
                                        )
                                        .when(show_path, |this| {
                                            this.child(
                                                Label::new(path.to_string_lossy().to_string())
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted),
                                            )
                                        }),
                                )
                                .when(!show_path, |this| {
                                    this.tooltip(move |_, cx| {
                                        if let Some(branch) = tooltip_branch.clone() {
                                            Tooltip::with_meta(
                                                format!("{}/{}", name, branch),
                                                None,
                                                tooltip_path.clone(),
                                                cx,
                                            )
                                        } else {
                                            Tooltip::simple(tooltip_path.clone(), cx)
                                        }
                                    })
                                }),
                        )
                        .end_slot(secondary_actions)
                        .show_end_slot_on_hover()
                        .into_any_element(),
                )
            }
            ProjectPickerEntry::ProjectGroup(hit) => {
                let key = self.window_project_groups.get(hit.candidate_id)?;
                let is_active = self.is_active_project_group(key, cx);
                let paths = key.path_list();
                let ordered_paths: Vec<_> = paths
                    .ordered_paths()
                    .map(|p| p.compact().to_string_lossy().to_string())
                    .collect();
                let tooltip_path: SharedString = ordered_paths.join("\n").into();
                let icon = icon_for_project_group(key);
                let show_icon = self.filtered_entries_include_remote_project();

                let mut path_start_offset = 0;
                let (match_labels, path_highlights): (Vec<_>, Vec<_>) = paths
                    .ordered_paths()
                    .map(|p| p.compact())
                    .map(|path| {
                        let highlighted_text =
                            highlights_for_path(path.as_ref(), &hit.positions, path_start_offset);
                        path_start_offset += highlighted_text.1.text.len();
                        highlighted_text
                    })
                    .unzip();

                let highlighted_match = HighlightedMatchWithPaths {
                    prefix: None,
                    match_label: HighlightedMatch::join(match_labels.into_iter().flatten(), ", "),
                    paths: path_highlights,
                    active: is_active,
                };

                let project_group_key = key.clone();
                let secondary_actions = h_flex()
                    .gap_0p5()
                    .when(!is_active, |this| {
                        this.child(
                            IconButton::new("remove_open_project", IconName::Close)
                                .icon_size(IconSize::Small)
                                .tooltip({
                                    let focus_handle = self.focus_handle.clone();
                                    move |_, cx| {
                                        Tooltip::for_action_in(
                                            "Remove Project from Window",
                                            &RemoveSelected,
                                            &focus_handle,
                                            cx,
                                        )
                                    }
                                })
                                .on_click({
                                    let project_group_key = project_group_key.clone();
                                    cx.listener(move |picker, _, window, cx| {
                                        cx.stop_propagation();
                                        window.prevent_default();
                                        picker.delegate.remove_project_group(
                                            project_group_key.clone(),
                                            window,
                                            cx,
                                        );
                                        let query = picker.query(cx);
                                        picker.update_matches(query, window, cx);
                                    })
                                }),
                        )
                    })
                    .into_any_element();

                Some(
                    ListItem::new(ix)
                        .inset(true)
                        .toggle_state(selected)
                        .spacing(ListItemSpacing::Sparse)
                        .child(
                            h_flex()
                                .id("open_project_info_container")
                                .w_full()
                                .min_w_0()
                                .gap_2p5()
                                .when(show_icon, |this| {
                                    this.child(Icon::new(icon).color(Color::Muted))
                                })
                                .child({
                                    let mut highlighted = highlighted_match;
                                    if !self.render_paths {
                                        highlighted.paths.clear();
                                    }
                                    highlighted.render(window, cx)
                                })
                                .tooltip(Tooltip::text(tooltip_path)),
                        )
                        .end_slot(secondary_actions)
                        .show_end_slot_on_hover()
                        .into_any_element(),
                )
            }
            ProjectPickerEntry::RecentProject(hit) => {
                let workspace = self.workspaces.get(hit.candidate_id)?;
                let location = &workspace.location;
                let raw_paths = &workspace.paths;
                let identity_paths = &workspace.identity_paths;
                let is_local = matches!(location, SerializedWorkspaceLocation::Local);
                let paths_to_add = raw_paths.paths().to_vec();
                let ordered_paths: Vec<_> = identity_paths
                    .ordered_paths()
                    .map(|p| p.compact().to_string_lossy().to_string())
                    .collect();
                let tooltip_path: SharedString = match &location {
                    SerializedWorkspaceLocation::Remote(options) => {
                        let host = options.display_name();
                        if ordered_paths.len() == 1 {
                            format!("{} ({})", ordered_paths[0], host).into()
                        } else {
                            format!("{}\n({})", ordered_paths.join("\n"), host).into()
                        }
                    }
                    _ => ordered_paths.join("\n").into(),
                };

                let mut path_start_offset = 0;
                let (match_labels, paths): (Vec<_>, Vec<_>) = identity_paths
                    .ordered_paths()
                    .map(|p| p.compact())
                    .map(|path| {
                        let highlighted_text =
                            highlights_for_path(path.as_ref(), &hit.positions, path_start_offset);
                        path_start_offset += highlighted_text.1.text.len();
                        highlighted_text
                    })
                    .unzip();

                let tooltip_title = if paths.len() > 1 {
                    "Add Folders to this Project"
                } else {
                    "Add Folder to this Project"
                };

                let prefix = match &location {
                    SerializedWorkspaceLocation::Remote(options) => {
                        Some(SharedString::from(options.display_name()))
                    }
                    _ => None,
                };

                let highlighted_match = HighlightedMatchWithPaths {
                    prefix,
                    match_label: HighlightedMatch::join(match_labels.into_iter().flatten(), ", "),
                    paths,
                    active: false,
                };

                let primary_confirm_tooltip = "Open Project in This Workspace";

                let secondary_actions = h_flex()
                    .gap_px()
                    .when(is_local, |this| {
                        this.child(
                            IconButton::new("add_to_workspace", IconName::FolderInclude)
                                .icon_size(IconSize::Small)
                                .tooltip({
                                    let focus_handle = self.focus_handle.clone();
                                    move |_, cx| {
                                        Tooltip::with_meta_in(
                                            tooltip_title,
                                            Some(&AddToWorkspace),
                                            "As a multi-root folder",
                                            &focus_handle,
                                            cx,
                                        )
                                    }
                                })
                                .on_click({
                                    let paths_to_add = paths_to_add.clone();
                                    cx.listener(move |picker, _event, window, cx| {
                                        cx.stop_propagation();
                                        window.prevent_default();
                                        picker.delegate.add_paths_to_project(
                                            paths_to_add.clone(),
                                            window,
                                            cx,
                                        );
                                    })
                                }),
                        )
                    })
                    .child(
                        IconButton::new("delete", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip({
                                let focus_handle = self.focus_handle.clone();
                                move |_, cx| {
                                    Tooltip::for_action_in(
                                        "Remove from Recent Projects",
                                        &RemoveSelected,
                                        &focus_handle,
                                        cx,
                                    )
                                }
                            })
                            .on_click(cx.listener(move |this, _event, window, cx| {
                                cx.stop_propagation();
                                window.prevent_default();
                                this.delegate.delete_recent_project(ix, window, cx)
                            })),
                    )
                    .into_any_element();

                let icon = icon_for_remote_connection(match location {
                    SerializedWorkspaceLocation::Local => None,
                    SerializedWorkspaceLocation::Remote(options) => Some(options),
                });
                let show_icon = self.filtered_entries_include_remote_project();

                Some(
                    ListItem::new(ix)
                        .toggle_state(selected)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .child(
                            h_flex()
                                .id("project_info_container")
                                .w_full()
                                .min_w_0()
                                .gap_2p5()
                                .flex_grow_1()
                                .when(show_icon, |this| {
                                    this.child(Icon::new(icon).color(Color::Muted))
                                })
                                .child({
                                    let mut highlighted = highlighted_match;
                                    if !self.render_paths {
                                        highlighted.paths.clear();
                                    }
                                    highlighted.render(window, cx)
                                })
                                .tooltip(move |_, cx| {
                                    Tooltip::with_meta(
                                        primary_confirm_tooltip,
                                        None,
                                        tooltip_path.clone(),
                                        cx,
                                    )
                                }),
                        )
                        .end_slot(secondary_actions)
                        .show_end_slot_on_hover()
                        .into_any_element(),
                )
            }
        }
    }

    fn render_footer(&self, _: &mut Window, cx: &mut Context<Picker<Self>>) -> Option<AnyElement> {
        let focus_handle = self.focus_handle.clone();
        let popover_style = matches!(self.style, ProjectPickerStyle::Popover);

        let is_already_open_entry = matches!(
            self.filtered_entries.get(self.selected_index),
            Some(ProjectPickerEntry::OpenFolder { .. } | ProjectPickerEntry::ProjectGroup(_))
        );

        if popover_style {
            return Some(
                v_flex()
                    .flex_1()
                    .p_1p5()
                    .gap_1()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child({
                        ButtonLike::new("open_local_folder")
                            .debug_selector(|| "SUPERZED_OPEN_LOCAL_FOLDER".to_string())
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_1()
                                    .justify_between()
                                    .child(Label::new("Open Local Folders"))
                                    .child(KeyBinding::for_action_in(
                                        &workspace::Open,
                                        &focus_handle,
                                        cx,
                                    )),
                            )
                            .on_click({
                                let workspace = self.workspace.clone();
                                move |_, window, cx| {
                                    open_local_project(workspace.clone(), window, cx);
                                }
                            })
                    })
                    .child(
                        ButtonLike::new("connect_ssh_host")
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_1()
                                    .justify_between()
                                    .child(Label::new("Connect SSH Host"))
                                    .child(KeyBinding::for_action(&ConnectSshHost, cx)),
                            )
                            .on_click(move |_, window, cx| {
                                window.dispatch_action(ConnectSshHost.boxed_clone(), cx)
                            }),
                    )
                    .into_any(),
            );
        }

        let selected_entry = self.filtered_entries.get(self.selected_index);

        let is_current_workspace_entry =
            if let Some(ProjectPickerEntry::ProjectGroup(hit)) = selected_entry {
                self.window_project_groups
                    .get(hit.candidate_id)
                    .is_some_and(|key| self.is_active_project_group(key, cx))
            } else {
                false
            };

        let secondary_footer_actions: Option<AnyElement> = match selected_entry {
            Some(ProjectPickerEntry::OpenFolder { .. }) => Some(
                Button::new("remove_selected", "Remove Folder")
                    .key_binding(KeyBinding::for_action_in(
                        &RemoveSelected,
                        &focus_handle,
                        cx,
                    ))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(RemoveSelected.boxed_clone(), cx)
                    })
                    .into_any_element(),
            ),
            Some(ProjectPickerEntry::ProjectGroup(_)) if !is_current_workspace_entry => Some(
                Button::new("remove_selected", "Remove from Window")
                    .key_binding(KeyBinding::for_action_in(
                        &RemoveSelected,
                        &focus_handle,
                        cx,
                    ))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(RemoveSelected.boxed_clone(), cx)
                    })
                    .into_any_element(),
            ),
            Some(ProjectPickerEntry::RecentProject(_)) => Some(
                Button::new("delete_recent", "Remove")
                    .key_binding(KeyBinding::for_action_in(
                        &RemoveSelected,
                        &focus_handle,
                        cx,
                    ))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(RemoveSelected.boxed_clone(), cx)
                    })
                    .into_any_element(),
            ),
            _ => None,
        };

        Some(
            h_flex()
                .flex_1()
                .p_1p5()
                .gap_1()
                .justify_end()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .when_some(secondary_footer_actions, |this, actions| {
                    this.child(actions)
                })
                .map(|this| {
                    if is_already_open_entry {
                        this.child(
                            Button::new("activate", "Activate")
                                .key_binding(KeyBinding::for_action_in(
                                    &menu::Confirm,
                                    &focus_handle,
                                    cx,
                                ))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(menu::Confirm.boxed_clone(), cx)
                                }),
                        )
                    } else {
                        this.child(
                            Button::new("open_here", "Open")
                                .debug_selector(|| "SUPERZED_OPEN_RECENT".to_string())
                                .key_binding(KeyBinding::for_action_in(
                                    &menu::Confirm,
                                    &focus_handle,
                                    cx,
                                ))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(menu::Confirm.boxed_clone(), cx)
                                }),
                        )
                    }
                })
                .child(Divider::vertical())
                .child(
                    PopoverMenu::new("actions-menu-popover")
                        .with_handle(self.actions_menu_handle.clone())
                        .anchor(gpui::Anchor::BottomRight)
                        .offset(gpui::Point {
                            x: px(0.0),
                            y: px(-2.0),
                        })
                        .trigger(
                            Button::new("actions-trigger", "Actions")
                                .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                                .key_binding(KeyBinding::for_action_in(
                                    &ToggleActionsMenu,
                                    &focus_handle,
                                    cx,
                                )),
                        )
                        .menu({
                            let focus_handle = focus_handle.clone();
                            let workspace_handle = self.workspace.clone();
                            let open_action = workspace::Open;
                            let show_add_to_workspace = match selected_entry {
                                Some(ProjectPickerEntry::RecentProject(hit)) => self
                                    .workspaces
                                    .get(hit.candidate_id)
                                    .map(|workspace| {
                                        matches!(
                                            workspace.location,
                                            SerializedWorkspaceLocation::Local
                                        )
                                    })
                                    .unwrap_or(false),
                                _ => false,
                            };

                            move |window, cx| {
                                Some(ContextMenu::build(window, cx, {
                                    let focus_handle = focus_handle.clone();
                                    let workspace_handle = workspace_handle.clone();
                                    let open_action = open_action.clone();
                                    move |menu, _, _| {
                                        menu.context(focus_handle)
                                            .when(show_add_to_workspace, |menu| {
                                                menu.action(
                                                    "Add Folder to this Project",
                                                    AddToWorkspace.boxed_clone(),
                                                )
                                                .separator()
                                            })
                                            .entry(
                                                "Open Local Folders",
                                                Some(open_action.boxed_clone()),
                                                {
                                                    let workspace_handle = workspace_handle.clone();
                                                    move |window, cx| {
                                                        open_local_project(
                                                            workspace_handle.clone(),
                                                            window,
                                                            cx,
                                                        );
                                                    }
                                                },
                                            )
                                            .action(
                                                "Connect SSH Host",
                                                ConnectSshHost.boxed_clone(),
                                            )
                                    }
                                }))
                            }
                        }),
                )
                .into_any(),
        )
    }
}

fn icon_for_project_group(key: &ProjectGroupKey) -> IconName {
    let host = key.host();
    icon_for_remote_connection(host.as_ref())
}

pub(crate) fn icon_for_remote_connection(options: Option<&RemoteConnectionOptions>) -> IconName {
    match options {
        None => IconName::Screen,
        Some(options) => match options {
            RemoteConnectionOptions::Local(_) => IconName::Screen,
            RemoteConnectionOptions::Ssh(_) => IconName::Server,
            RemoteConnectionOptions::Wsl(_) => IconName::Linux,
            RemoteConnectionOptions::Docker(_) => IconName::Box,
            #[cfg(any(test, feature = "test-support"))]
            RemoteConnectionOptions::Mock(_) => IconName::Server,
        },
    }
}

// Compute the highlighted text for the name and path
pub(crate) fn highlights_for_path(
    path: &Path,
    match_positions: &Vec<usize>,
    path_start_offset: usize,
) -> (Option<HighlightedMatch>, HighlightedMatch) {
    let path_string = path.to_string_lossy();
    let path_text = path_string.to_string();
    let path_byte_len = path_text.len();
    // Get the subset of match highlight positions that line up with the given path.
    // Also adjusts them to start at the path start
    let path_positions = match_positions
        .iter()
        .copied()
        .skip_while(|position| *position < path_start_offset)
        .take_while(|position| *position < path_start_offset + path_byte_len)
        .map(|position| position - path_start_offset)
        .collect::<Vec<_>>();

    // Again subset the highlight positions to just those that line up with the file_name
    // again adjusted to the start of the file_name
    let file_name_text_and_positions = path.file_name().map(|file_name| {
        let file_name_text = file_name.to_string_lossy().into_owned();
        let file_name_start_byte = path_byte_len - file_name_text.len();
        let highlight_positions = path_positions
            .iter()
            .copied()
            .skip_while(|position| *position < file_name_start_byte)
            .take_while(|position| *position < file_name_start_byte + file_name_text.len())
            .map(|position| position - file_name_start_byte)
            .collect::<Vec<_>>();
        HighlightedMatch {
            text: file_name_text,
            highlight_positions,
            color: Color::Default,
        }
    });

    (
        file_name_text_and_positions,
        HighlightedMatch {
            text: path_text,
            highlight_positions: path_positions,
            color: Color::Default,
        },
    )
}

fn open_local_project(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
    use gpui::PathPromptOptions;
    use project::DirectoryLister;

    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let target = workspace.read(cx).host_workspace_identity().cloned();

    let paths = workspace.update(cx, |workspace, cx| {
        workspace.prompt_for_open_path(
            PathPromptOptions {
                files: true,
                directories: true,
                multiple: true,
                prompt: None,
            },
            DirectoryLister::Local(
                workspace.project().clone(),
                workspace.app_state().fs.clone(),
            ),
            window,
            cx,
        )
    });
    workspace.update(cx, |workspace, cx| workspace.hide_modal(window, cx));

    let multi_workspace_handle = window.window_handle().downcast::<MultiWorkspace>();
    window
        .spawn(cx, async move |cx| {
            let Some(paths) = paths.await? else {
                return anyhow::Ok(());
            };
            let is_host_session = multi_workspace_handle
                .and_then(|handle| {
                    handle
                        .read_with(cx, |multi_workspace, _| {
                            multi_workspace.host_session().is_some()
                        })
                        .ok()
                })
                .unwrap_or(false);
            if is_host_session {
                let target = target.context("requesting workspace has no host identity")?;
                let open = cx.update(|_, cx| {
                    workspace::open_superzed_paths(
                        target,
                        &paths,
                        &paths,
                        workspace::OpenOptions {
                            requesting_window: multi_workspace_handle,
                            open_mode: OpenMode::Activate,
                            ..Default::default()
                        },
                        cx,
                    )
                })?;
                open.await?;
                return anyhow::Ok(());
            }
            if let Some(handle) = multi_workspace_handle {
                let task = handle.update(cx, |multi_workspace, window, cx| {
                    multi_workspace.open_project(paths, OpenMode::Activate, window, cx)
                })?;
                task.await?;
                return anyhow::Ok(());
            }
            anyhow::bail!("opening a project requires the authoritative Super Zed host shell")
        })
        .detach_and_prompt_err("Failed to open project", window, cx, |_, _, _| None);
}

impl RecentProjectsDelegate {
    fn open_recent_projects(
        &mut self,
        candidate_id: usize,
        _secondary: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(candidate_workspace) = self.workspaces.get(candidate_id) else {
            return;
        };

        let candidate_workspace_id = candidate_workspace.workspace_id;
        let candidate_workspace_location = candidate_workspace.location.clone();
        let candidate_workspace_paths = candidate_workspace.paths.clone();

        workspace.update(cx, |workspace, cx| {
            if workspace.database_id() == Some(candidate_workspace_id) {
                return;
            }
            match candidate_workspace_location {
                SerializedWorkspaceLocation::Local => {
                    let paths = candidate_workspace_paths.paths().to_vec();
                    if let Some(target) = workspace.host_workspace_identity().cloned() {
                        let Some(requesting_window) =
                            window.window_handle().downcast::<MultiWorkspace>()
                        else {
                            return;
                        };
                        cx.defer(move |cx| {
                            requesting_window
                                .update(cx, |_, window, cx| {
                                    workspace::open_superzed_paths(
                                        target,
                                        &paths,
                                        &paths,
                                        workspace::OpenOptions {
                                            requesting_window: Some(requesting_window),
                                            open_mode: OpenMode::Activate,
                                            ..Default::default()
                                        },
                                        cx,
                                    )
                                    .detach_and_prompt_err(
                                        "Failed to open project",
                                        window,
                                        cx,
                                        |_, _, _| None,
                                    );
                                })
                                .log_err();
                        });
                        return;
                    }
                    if let Some(handle) = window.window_handle().downcast::<MultiWorkspace>() {
                        cx.defer(move |cx| {
                            if let Some(task) = handle
                                .update(cx, |multi_workspace, window, cx| {
                                    multi_workspace.open_project(
                                        paths,
                                        OpenMode::Activate,
                                        window,
                                        cx,
                                    )
                                })
                                .log_err()
                            {
                                task.detach_and_log_err(cx);
                            }
                        });
                    }
                    return;
                }
                SerializedWorkspaceLocation::Remote(mut connection) => {
                    let app_state = workspace.app_state().clone();
                    let host_window = window.window_handle().downcast::<MultiWorkspace>();
                    let open_options = OpenOptions {
                        requesting_window: host_window,
                        ..Default::default()
                    };
                    if let RemoteConnectionOptions::Ssh(connection) = &mut connection {
                        RemoteSettings::get_global(cx)
                            .fill_connection_options_from_settings(connection);
                    };
                    let paths = candidate_workspace_paths.paths().to_vec();
                    cx.spawn_in(window, async move |_, cx| {
                        if matches!(connection, RemoteConnectionOptions::Ssh(_)) {
                            let host_window = host_window
                                .context("Super Zed requires its existing host window")?;
                            connect_ssh_host(connection, app_state, host_window, cx).await
                        } else {
                            open_non_ssh_remote_project(
                                connection,
                                paths,
                                app_state,
                                open_options,
                                cx,
                            )
                            .await
                        }
                    })
                    .detach_and_prompt_err(
                        "Failed to open project",
                        window,
                        cx,
                        |_, _, _| None,
                    );
                }
            }
        });
        cx.emit(DismissEvent);
    }

    fn add_paths_to_project(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let add_roots = workspace.update(cx, |workspace, cx| {
            workspace.add_project_roots(paths, window, cx)
        });
        cx.spawn_in(window, async move |picker, cx| {
            add_roots.await?;
            picker
                .update_in(cx, |picker, window, cx| {
                    let Some(workspace) = picker.delegate.workspace.upgrade() else {
                        return;
                    };
                    picker.delegate.open_folders = get_open_folders(workspace.read(cx), cx);
                    let query = picker.query(cx);
                    picker.update_matches(query, window, cx);
                })
                .ok();
            anyhow::Ok(())
        })
        .detach_and_prompt_err(
            "Failed to add folder to project",
            window,
            cx,
            |_, _, _| None,
        );
    }

    /// Returns the new selection index after the entry at `deleted_index`
    /// is removed.
    ///
    /// - Prefers the nearest entry matching `prefer_section` so the user
    ///   stays in the same section they were navigating.
    /// - Falls back to any other selectable entry so the picker doesn't
    ///   land on a header.
    fn replacement_index_after_deletion(
        &self,
        deleted_index: usize,
        prefer_previous: bool,
        prefer_section: fn(&ProjectPickerEntry) -> bool,
    ) -> Option<usize> {
        let replacement_index = |matches_entry: fn(&ProjectPickerEntry) -> bool| {
            let next_index = self
                .filtered_entries
                .iter()
                .enumerate()
                .skip(deleted_index)
                .find_map(|(index, entry)| matches_entry(entry).then_some(index));
            let previous_index = self
                .filtered_entries
                .iter()
                .enumerate()
                .take(deleted_index.min(self.filtered_entries.len()))
                .rev()
                .find_map(|(index, entry)| matches_entry(entry).then_some(index));

            if prefer_previous {
                previous_index.or(next_index)
            } else {
                next_index.or(previous_index)
            }
        };

        replacement_index(prefer_section).or_else(|| replacement_index(is_selectable_entry))
    }

    fn update_picker_after_recent_project_deletion(
        picker: &mut Picker<Self>,
        deleted_index: usize,
        workspaces: Vec<RecentWorkspace>,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let prefer_previous = picker.is_scrolled_to_end() == Some(true);
        picker.delegate.set_workspaces(workspaces);
        picker.delegate.snap_selection_to_first_non_header_match = false;
        picker.update_matches_with_options(
            picker.query(cx),
            ScrollBehavior::PreserveOffset,
            window,
            cx,
        );
        if let Some(replacement_index) = picker.delegate.replacement_index_after_deletion(
            deleted_index,
            prefer_previous,
            |entry| matches!(entry, ProjectPickerEntry::RecentProject(_)),
        ) {
            picker.set_selected_index(replacement_index, None, false, window, cx);
        }
    }

    fn delete_recent_project(
        &self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        if let Some(ProjectPickerEntry::RecentProject(selected_match)) =
            self.filtered_entries.get(ix)
        {
            let Some(recent_workspace) = self.workspaces.get(selected_match.candidate_id).cloned()
            else {
                return;
            };
            let fs = self
                .workspace
                .upgrade()
                .map(|ws| ws.read(cx).app_state().fs.clone());
            let db = WorkspaceDb::global(cx);
            cx.spawn_in(window, async move |this, cx| {
                let Some(fs) = fs else { return };
                let deleted_workspace_ids = db
                    .delete_recent_workspace_group(&recent_workspace)
                    .await
                    .log_err()
                    .unwrap_or_default();
                let workspaces = db
                    .recent_project_workspaces(fs.as_ref())
                    .await
                    .unwrap_or_default();
                this.update_in(cx, move |picker, window, cx| {
                    Self::update_picker_after_recent_project_deletion(
                        picker, ix, workspaces, window, cx,
                    );
                    // After deleting a project, we want to update the history manager to reflect the change.
                    // But we do not emit a update event when user opens a project, because it's handled in `workspace::load_workspace`.
                    if let Some(history_manager) = HistoryManager::global(cx) {
                        history_manager.update(cx, |this, cx| {
                            for workspace_id in &deleted_workspace_ids {
                                this.delete_history(*workspace_id, cx);
                            }
                        });
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    fn remove_project_group(
        &mut self,
        key: ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        if let Some(handle) = window.window_handle().downcast::<MultiWorkspace>() {
            let key_for_remove = key.clone();
            cx.defer(move |cx| {
                handle
                    .update(cx, |multi_workspace, window, cx| {
                        multi_workspace
                            .remove_project_group(&key_for_remove, window, cx)
                            .detach_and_log_err(cx);
                    })
                    .log_err();
            });
        }

        self.window_project_groups.retain(|k| k != &key);
    }

    fn is_current_workspace(
        &self,
        workspace_id: WorkspaceId,
        cx: &mut Context<Picker<Self>>,
    ) -> bool {
        if let Some(workspace) = self.workspace.upgrade() {
            let workspace = workspace.read(cx);
            if Some(workspace_id) == workspace.database_id() {
                return true;
            }
        }

        false
    }

    fn is_active_project_group(&self, key: &ProjectGroupKey, cx: &App) -> bool {
        if let Some(workspace) = self.workspace.upgrade() {
            return workspace.read(cx).project_group_key(cx) == *key;
        }
        false
    }

    fn is_in_current_window_groups(&self, workspace: &RecentWorkspace) -> bool {
        self.window_project_groups
            .iter()
            .any(|key| key.matches(&workspace.project_group_key()))
    }

    fn is_open_folder(&self, paths: &PathList) -> bool {
        if self.open_folders.is_empty() {
            return false;
        }

        for workspace_path in paths.paths() {
            for open_folder in &self.open_folders {
                if workspace_path == &open_folder.path {
                    return true;
                }
            }
        }

        false
    }

    fn is_valid_recent_candidate(
        &self,
        workspace: &RecentWorkspace,
        cx: &mut Context<Picker<Self>>,
    ) -> bool {
        !self.is_current_workspace(workspace.workspace_id, cx)
            && !self.is_in_current_window_groups(workspace)
            && !self.is_open_folder(&workspace.paths)
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, UpdateGlobal, VisualTestContext};

    use serde_json::json;
    use settings::SettingsStore;
    use workspace::AppState;

    use super::*;

    // Test picker for the empty query:
    //
    //   [0] Header("Current Folders")
    //   [1] OpenFolder(0)
    //   [2] OpenFolder(1)
    //   [3] Header("This Window")
    //   [4] ProjectGroup(0)
    //   [5] ProjectGroup(1)
    //   [6] Header("Recent Projects")
    //   [7..=26] RecentProject(0..=19)
    //
    const RECENT_PROJECT_COUNT: usize = 20;
    const FIRST_RECENT_PROJECT: usize = 7;
    const LAST_RECENT_PROJECT: usize = FIRST_RECENT_PROJECT + RECENT_PROJECT_COUNT - 1;

    fn open_folder(index: usize) -> OpenFolderEntry {
        OpenFolderEntry {
            worktree_id: WorktreeId::from_usize(index),
            name: format!("project-folder-{index}").into(),
            path: PathBuf::from(format!("/current/project-folder-{index}")),
            branch: None,
            is_active: false,
            connection_options: None,
        }
    }

    fn project_group(index: usize) -> ProjectGroupKey {
        ProjectGroupKey::new(
            None,
            PathList::new(&[PathBuf::from(format!("/this-window/project-{index}"))]),
        )
    }

    fn remote_project_group(index: usize) -> ProjectGroupKey {
        ProjectGroupKey::new(
            Some(RemoteConnectionOptions::Mock(
                remote::MockConnectionOptions { id: index as u64 },
            )),
            PathList::new(&[PathBuf::from(format!(
                "/this-window/remote-project-{index}"
            ))]),
        )
    }

    fn recent_workspace(index: usize) -> RecentWorkspace {
        let paths = PathList::new(&[PathBuf::from(format!("/recent/project-{index:02}"))]);
        RecentWorkspace {
            workspace_id: WorkspaceId::from_i64(index as i64),
            location: SerializedWorkspaceLocation::Local,
            paths: paths.clone(),
            identity_paths: paths,
            timestamp: Utc::now(),
        }
    }

    fn recent_workspaces() -> Vec<RecentWorkspace> {
        (0..RECENT_PROJECT_COUNT).map(recent_workspace).collect()
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.draw(cx).clear());
    }

    fn build_picker(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Picker<RecentProjectsDelegate>>,
        &mut VisualTestContext,
    ) {
        init_test(cx);
        let (picker, cx) = cx.add_window_view(|window, cx| {
            let mut delegate = RecentProjectsDelegate::new(
                WeakEntity::new_invalid(),
                cx.focus_handle(),
                vec![open_folder(0), open_folder(1)],
                vec![project_group(0), project_group(1)],
                ProjectPickerStyle::Modal,
            );
            delegate.set_workspaces(recent_workspaces());
            Picker::list(delegate, window, cx)
                .list_measure_all()
                .show_scrollbar(true)
                .max_height(Rems::from_pixels(px(240.0), window))
        });
        draw(cx);
        (picker, cx)
    }

    fn scroll_to_and_select(
        picker: &Entity<Picker<RecentProjectsDelegate>>,
        cx: &mut VisualTestContext,
        index: usize,
    ) -> usize {
        picker.update_in(cx, |picker, window, cx| {
            picker.set_selected_index(index, None, true, window, cx);
        });
        draw(cx);
        picker.update(cx, |picker, _| picker.logical_scroll_top_index())
    }

    fn delete_recent_project_in_picker(
        picker: &Entity<Picker<RecentProjectsDelegate>>,
        cx: &mut VisualTestContext,
        index: usize,
    ) {
        picker.update_in(cx, |picker, window, cx| {
            let Some(ProjectPickerEntry::RecentProject(hit)) =
                picker.delegate.filtered_entries.get(index)
            else {
                panic!("expected entry at {index} to be a recent project");
            };
            let mut workspaces = picker.delegate.workspaces.clone();
            workspaces.remove(hit.candidate_id);
            RecentProjectsDelegate::update_picker_after_recent_project_deletion(
                picker, index, workspaces, window, cx,
            );
        });
    }

    #[track_caller]
    fn assert_scroll_top_is(
        picker: &Entity<Picker<RecentProjectsDelegate>>,
        cx: &mut VisualTestContext,
        expected: usize,
        phase: &str,
    ) {
        picker.update(cx, |picker, _| {
            assert_eq!(
                picker.logical_scroll_top_index(),
                expected,
                "scroll top should remain at {expected} ({phase})"
            );
            assert_selected_entry_is_recent_project(picker);
        });
    }

    #[track_caller]
    fn assert_pinned_to_bottom(
        picker: &Entity<Picker<RecentProjectsDelegate>>,
        cx: &mut VisualTestContext,
        phase: &str,
    ) {
        picker.update(cx, |picker, _| {
            assert_eq!(
                picker.is_scrolled_to_end(),
                Some(true),
                "picker should remain pinned to the bottom ({phase})"
            );
            assert!(
                picker.logical_scroll_top_index() > 0,
                "picker should not jump to the top while pinned to the bottom ({phase})"
            );
            assert_selected_entry_is_recent_project(picker);
        });
    }

    #[track_caller]
    fn assert_selected_entry_is_recent_project(picker: &Picker<RecentProjectsDelegate>) {
        assert!(matches!(
            picker
                .delegate
                .filtered_entries
                .get(picker.delegate.selected_index),
            Some(ProjectPickerEntry::RecentProject(_))
        ));
    }

    #[gpui::test]
    fn this_window_project_icons_use_each_project_group_host(cx: &mut TestAppContext) {
        init_test(cx);

        let mut delegate = RecentProjectsDelegate::new(
            WeakEntity::new_invalid(),
            cx.update(|cx| cx.focus_handle()),
            Vec::new(),
            vec![project_group(0), remote_project_group(1)],
            ProjectPickerStyle::Modal,
        );
        delegate.filtered_entries = vec![
            ProjectPickerEntry::ProjectGroup(StringMatch {
                candidate_id: 0,
                score: 0.0,
                positions: Vec::new(),
                string: Default::default(),
            }),
            ProjectPickerEntry::ProjectGroup(StringMatch {
                candidate_id: 1,
                score: 0.0,
                positions: Vec::new(),
                string: Default::default(),
            }),
        ];

        assert!(!delegate.entry_is_remote_project(&delegate.filtered_entries[0]));
        assert!(delegate.entry_is_remote_project(&delegate.filtered_entries[1]));
        assert!(delegate.filtered_entries_include_remote_project());
        assert_eq!(
            icon_for_project_group(&delegate.window_project_groups[0]),
            IconName::Screen
        );
        assert_eq!(
            icon_for_project_group(&delegate.window_project_groups[1]),
            IconName::Server
        );
    }

    #[gpui::test]
    fn deleting_top_recent_project_preserves_scroll_position(cx: &mut TestAppContext) {
        let target = FIRST_RECENT_PROJECT;
        let (picker, cx) = build_picker(cx);
        let scroll_top = scroll_to_and_select(&picker, cx, target);
        assert!(
            scroll_top > 0,
            "test should start scrolled away from the top"
        );

        delete_recent_project_in_picker(&picker, cx, target);
        assert_scroll_top_is(&picker, cx, scroll_top, "after delete");

        // The picker re-runs layout on the next frame; the scroll position
        // must still be preserved after that redraw.
        draw(cx);
        assert_scroll_top_is(&picker, cx, scroll_top, "after redraw");
    }

    #[gpui::test]
    fn deleting_middle_recent_project_preserves_scroll_position(cx: &mut TestAppContext) {
        let target = FIRST_RECENT_PROJECT + RECENT_PROJECT_COUNT / 2;
        let (picker, cx) = build_picker(cx);
        let scroll_top = scroll_to_and_select(&picker, cx, target);
        assert!(
            scroll_top > 0,
            "test should start scrolled away from the top"
        );

        delete_recent_project_in_picker(&picker, cx, target);
        assert_scroll_top_is(&picker, cx, scroll_top, "after delete");

        draw(cx);
        assert_scroll_top_is(&picker, cx, scroll_top, "after redraw");
    }

    #[gpui::test]
    fn deleting_last_recent_project_preserves_scroll_position(cx: &mut TestAppContext) {
        let target = LAST_RECENT_PROJECT;
        let (picker, cx) = build_picker(cx);
        scroll_to_and_select(&picker, cx, target);

        picker.update(cx, |picker, _| {
            assert_eq!(
                picker.is_scrolled_to_end(),
                Some(true),
                "selecting the last entry should leave the picker pinned to the bottom"
            );
        });

        delete_recent_project_in_picker(&picker, cx, target);
        assert_pinned_to_bottom(&picker, cx, "after delete");

        draw(cx);
        assert_pinned_to_bottom(&picker, cx, "after redraw");
    }

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
            let state = AppState::test(cx);
            crate::init(cx);
            editor::init(cx);
            state
        })
    }

    fn selected_recent_workspace(path: &str) -> RecentWorkspace {
        let paths = PathList::new(&[PathBuf::from(path)]);
        RecentWorkspace {
            workspace_id: WorkspaceId::from_i64(1),
            location: SerializedWorkspaceLocation::Local,
            paths: paths.clone(),
            identity_paths: paths,
            timestamp: Utc::now(),
        }
    }

    fn select_only_recent_project(
        recent_projects: &Entity<RecentProjects>,
        path: &str,
        cx: &mut VisualTestContext,
    ) {
        recent_projects.update(cx, |recent_projects, cx| {
            recent_projects.picker.update(cx, |picker, cx| {
                picker.delegate.workspaces = vec![selected_recent_workspace(path)];
                picker.delegate.filtered_entries = vec![
                    ProjectPickerEntry::Header("Recent Projects".into()),
                    ProjectPickerEntry::RecentProject(StringMatch {
                        candidate_id: 0,
                        score: 0.0,
                        positions: Vec::new(),
                        string: Default::default(),
                    }),
                ];
                picker.delegate.selected_index = 1;
                cx.notify();
            });
        });
    }

    fn host_workspace_state(
        multi_workspace: &Entity<MultiWorkspace>,
        cx: &VisualTestContext,
    ) -> (
        superzed_session::WorkspaceId,
        Vec<(superzed_session::WorkspaceId, Vec<PathBuf>)>,
    ) {
        multi_workspace.read_with(cx, |multi_workspace, cx| {
            let host_session = multi_workspace
                .host_session()
                .expect("test window should remain attached to its host");
            let snapshot = host_session.read(cx).snapshot();
            (
                snapshot.active_workspace_id,
                snapshot
                    .workspaces
                    .iter()
                    .map(|workspace| {
                        (
                            workspace.id,
                            workspace
                                .project_spec
                                .roots
                                .iter()
                                .map(|root| root.canonical_path.clone())
                                .collect(),
                        )
                    })
                    .collect(),
            )
        })
    }

    fn attach_open_folder_popover(
        multi_workspace: &Entity<MultiWorkspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<RecentProjects> {
        multi_workspace.update_in(cx, |multi_workspace, window, cx| {
            let workspace = multi_workspace.workspace().clone();
            let focus_handle = workspace.read(cx).focus_handle(cx);
            let workspace_handle = workspace.downgrade();
            let project_groups = multi_workspace.project_group_keys();
            workspace.update(cx, |workspace, cx| {
                workspace.hide_modal(window, cx);
                let open_folders = get_open_folders(workspace, cx);
                let fs = Some(workspace.app_state().fs.clone());
                let delegate = RecentProjectsDelegate::new(
                    workspace_handle,
                    focus_handle,
                    open_folders,
                    project_groups,
                    ProjectPickerStyle::Popover,
                );
                workspace.toggle_modal(window, cx, |window, cx| {
                    RecentProjects::new(delegate, fs, 20., window, cx)
                });
                workspace
                    .active_modal::<RecentProjects>(cx)
                    .expect("production recent-projects popover should attach to the workspace")
            })
        })
    }

    #[gpui::test]
    async fn test_superzed_folder_and_open_recent_actions_use_host_workspace_semantics(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            workspace::init(app_state.clone(), cx);
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.workspace.use_system_path_prompts = Some(false);
                });
            });
        });
        server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));

        for path in ["/one", "/two", "/shared"] {
            app_state
                .fs
                .as_fake()
                .insert_tree(path, json!({ "src": { "main.rs": "" } }))
                .await;
        }
        let server_fs = fs::FakeFs::new(server_cx.executor());
        for path in ["/one", "/two", "/shared"] {
            server_fs
                .insert_tree(path, json!({ "src": { "main.rs": "" } }))
                .await;
        }

        let database_directory =
            tempfile::tempdir().expect("temporary host database directory should open");
        let (database, snapshot) = remote_server::SuperzedHost::load(
            database_directory.path().join("host-session.sqlite"),
        )
        .expect("blank host session should load");
        let (connection_options, server_session, connect_guard) =
            remote::RemoteClient::fake_server(cx, server_cx);
        server_cx.update(remote_server::HeadlessProject::init);
        let server_executor = server_cx.executor();
        let _host = server_cx.new(|cx| {
            remote_server::SuperzedHost::new(
                remote_server::HeadlessAppState {
                    session: server_session,
                    fs: server_fs,
                    http_client: Arc::new(http_client::BlockedHttpClient),
                    node_runtime: node_runtime::NodeRuntime::unavailable(),
                    languages: Arc::new(language::LanguageRegistry::new(server_executor)),
                    extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                    startup_time: std::time::Instant::now(),
                },
                database,
                snapshot,
                cx,
            )
        });
        let mut async_cx = cx.to_async();
        let attach = workspace::open_superzed_host(
            connection_options,
            Arc::new(remote::MockDelegate),
            app_state,
            &mut async_cx,
        );
        drop(connect_guard);
        let window = attach.await.expect("blank local host should attach");
        let multi_workspace = window.root(cx).expect("host window should have a root");
        let host_session = multi_workspace.read_with(cx, |multi_workspace, _| {
            multi_workspace
                .host_session()
                .expect("host session should be attached")
                .clone()
        });
        let source_workspace = host_session.read_with(cx, |host_session, _| {
            host_session.snapshot().workspaces[0].clone()
        });
        let project_spec = |path: &str| superzed_session::ProjectSpec {
            roots: vec![superzed_session::ProjectRoot {
                requested_path: PathBuf::from(path),
                canonical_path: PathBuf::from(path),
            }],
        };
        let set_source_roots = cx.update(|cx| {
            workspace::HostSessionClient::set_project_roots(
                &host_session,
                window,
                source_workspace.id,
                project_spec("/one"),
                cx,
            )
        });
        set_source_roots
            .await
            .expect("source workspace roots should be initialized");
        let create_target = cx.update(|cx| {
            workspace::HostSessionClient::create_workspace(
                &host_session,
                window,
                project_spec("/two"),
                cx,
            )
        });
        create_target
            .await
            .expect("target workspace should be initialized");
        let target_workspace = host_session.read_with(cx, |host_session, _| {
            host_session
                .snapshot()
                .workspaces
                .iter()
                .find(|workspace| workspace.id == host_session.snapshot().active_workspace_id)
                .expect("created target workspace should be active")
                .clone()
        });
        assert_ne!(source_workspace.id, target_workspace.id);
        assert_ne!(source_workspace.project_id, target_workspace.project_id);

        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let active_workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        active_workspace.update(cx, |workspace, _| {
            workspace.set_prompt_for_open_path(Box::new(|_, _, _, _| {
                let (sender, receiver) = futures::channel::oneshot::channel();
                sender.send(Some(vec![PathBuf::from("/one")])).ok();
                receiver
            }));
        });
        attach_open_folder_popover(&multi_workspace, cx);
        draw(cx);
        let open_folder = cx
            .debug_bounds("SUPERZED_OPEN_LOCAL_FOLDER")
            .expect("Open Local Folders button should render");
        cx.simulate_click(open_folder.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        let (active_workspace_id, workspaces) = host_workspace_state(&multi_workspace, cx);
        assert_eq!(
            active_workspace_id, target_workspace.id,
            "Open Project must not activate the workspace that already has identical roots"
        );
        assert_eq!(
            workspaces.len(),
            2,
            "Open Project must not create a workspace"
        );
        assert_eq!(
            workspaces,
            vec![
                (source_workspace.id, vec![PathBuf::from("/one")]),
                (target_workspace.id, vec![PathBuf::from("/one")]),
            ]
        );
        host_session.read_with(cx, |host_session, _| {
            let source_after_open = host_session
                .snapshot()
                .workspaces
                .iter()
                .find(|workspace| workspace.id == source_workspace.id)
                .expect("source workspace should remain present");
            let target_after_open = host_session
                .snapshot()
                .workspaces
                .iter()
                .find(|workspace| workspace.id == target_workspace.id)
                .expect("target workspace should remain present");
            assert_eq!(source_after_open.project_id, source_workspace.project_id);
            assert_eq!(target_after_open.project_id, target_workspace.project_id);
        });

        let active_workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        active_workspace.update(cx, |workspace, _| {
            workspace.set_prompt_for_open_path(Box::new(|_, _, _, _| {
                let (sender, receiver) = futures::channel::oneshot::channel();
                sender.send(Some(vec![PathBuf::from("/shared")])).ok();
                receiver
            }));
        });
        cx.dispatch_action(workspace::AddFolderToProject);
        cx.run_until_parked();

        let (active_after_add, workspaces_after_add) = host_workspace_state(&multi_workspace, cx);
        assert_eq!(active_after_add, target_workspace.id);
        assert_eq!(
            workspaces_after_add,
            vec![
                (source_workspace.id, vec![PathBuf::from("/one")]),
                (
                    target_workspace.id,
                    vec![PathBuf::from("/one"), PathBuf::from("/shared")]
                ),
            ],
            "Add Folder must append only to the current workspace"
        );

        cx.dispatch_action(OpenRecent);
        cx.run_until_parked();
        let recent_projects = multi_workspace.read_with(cx, |multi_workspace, cx| {
            multi_workspace
                .workspace()
                .read(cx)
                .active_modal::<RecentProjects>(cx)
                .expect("Open Recent action should display the production modal")
        });
        recent_projects.read_with(cx, |recent_projects, cx| {
            assert!(
                recent_projects
                    .picker
                    .read(cx)
                    .delegate
                    .filtered_entries
                    .iter()
                    .all(|entry| !matches!(entry, ProjectPickerEntry::ProjectGroup(_))),
                "Open Recent must not expose legacy project-group workspace switching"
            );
        });
        select_only_recent_project(&recent_projects, "/two", cx);
        draw(cx);
        let open_recent = cx
            .debug_bounds("SUPERZED_OPEN_RECENT")
            .expect("Open button should render for the selected recent project");
        cx.simulate_click(open_recent.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        let (active_after_recent, workspaces_after_recent) =
            host_workspace_state(&multi_workspace, cx);
        assert_eq!(active_after_recent, target_workspace.id);
        assert_eq!(
            workspaces_after_recent,
            vec![
                (source_workspace.id, vec![PathBuf::from("/one")]),
                (target_workspace.id, vec![PathBuf::from("/two")]),
            ],
            "Open Recent must replace only the current workspace roots"
        );
        host_session.read_with(cx, |host_session, _| {
            assert_eq!(host_session.snapshot().workspaces.len(), 2);
            assert_eq!(
                host_session.snapshot().workspaces[0].project_id,
                source_workspace.project_id
            );
            assert_eq!(
                host_session.snapshot().workspaces[1].project_id,
                target_workspace.project_id
            );
        });
    }
}
