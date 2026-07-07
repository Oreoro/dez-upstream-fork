use crate::git_status_icon;
use git::status::{FileStatus, StatusCode, TrackedStatus, UnmergedStatus, UnmergedStatusCode};
use gpui::{
    Action, Anchor, App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, Task, WeakEntity,
};
use picker::{Picker, PickerDelegate, PickerEditorPosition};
use project::{Project, Worktree, git_store::Repository};
use std::sync::Arc;
use ui::{ButtonLike, ContextMenu, ListItem, ListItemSpacing, PopoverMenu, Tooltip, prelude::*};
use workspace::{ModalView, Workspace};

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(open);
}

pub fn repository_selector_button(
    id: impl Into<ElementId>,
    project: &Entity<Project>,
    cx: &mut App,
) -> Option<AnyElement> {
    let project_ref = project.read(cx);
    if project_ref.visible_repositories(cx).len() <= 1 {
        return None;
    }

    let active_repository = project_ref.active_repository(cx)?;
    let label = SharedString::from(Arc::from(
        active_repository
            .read(cx)
            .display_name()
            .trim_end_matches('/'),
    ));

    Some(
        repository_selector_trigger(id, label, LabelSize::Small)
            .on_click(|_, window, cx| {
                window.dispatch_action(zed_actions::git::SelectRepo.boxed_clone(), cx);
            })
            .into_any_element(),
    )
}

pub fn repository_selector_menu(
    id: impl Into<ElementId>,
    project: &Entity<Project>,
    cx: &mut App,
) -> Option<AnyElement> {
    repository_selector_menu_with_label_size(id, project, LabelSize::Small, cx)
}

pub fn repository_selector_menu_default(
    id: impl Into<ElementId>,
    project: &Entity<Project>,
    cx: &mut App,
) -> Option<AnyElement> {
    repository_selector_menu_with_label_size(id, project, LabelSize::Default, cx)
}

fn repository_selector_menu_with_label_size(
    id: impl Into<ElementId>,
    project: &Entity<Project>,
    label_size: LabelSize,
    cx: &mut App,
) -> Option<AnyElement> {
    let id = id.into();
    let project_ref = project.read(cx);
    let entries = worktree_repository_entries(project_ref, cx);
    if entries.is_empty() {
        return None;
    }

    let active_repository = project_ref.active_repository(cx);
    let label = SharedString::from(Arc::from(
        active_repository
            .as_ref()
            .map(|repository| repository.read(cx).display_name())
            .unwrap_or_else(|| entries[0].label.clone())
            .trim_end_matches('/'),
    ));

    if entries.len() <= 1 {
        return Some(repository_selector_label(label, label_size, false).into_any_element());
    }

    let trigger = repository_selector_trigger("repository-selector-trigger", label, label_size);
    let project = project.clone();

    Some(
        PopoverMenu::new(id)
            .trigger(trigger)
            .menu(move |window, cx| {
                let active_repository = project.read(cx).active_repository(cx);
                let active_repository_id = active_repository
                    .as_ref()
                    .map(|repository| repository.read(cx).id);
                let entries = worktree_repository_entries(project.read(cx), cx);
                let project_for_context_menu = project.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, cx| {
                    for entry in entries {
                        let selected = entry.repository.as_ref().is_some_and(|repository| {
                            Some(repository.read(cx).id) == active_repository_id
                        });
                        if let Some(repository) = entry.repository {
                            let repository_id = repository.read(cx).id;
                            let label = entry.label.clone();
                            let project = project_for_context_menu.clone();
                            menu = menu.custom_entry(
                                move |_window, _cx| {
                                    worktree_repository_menu_row(label.clone(), selected, false)
                                },
                                move |_window, cx| {
                                    project.update(cx, |project, cx| {
                                        project.set_active_repository_id(Some(repository_id), cx);
                                    });
                                },
                            );
                        } else {
                            let label = entry.label.clone();
                            menu = menu
                                .custom_row(move |_window, _cx| {
                                    worktree_repository_menu_row(label.clone(), false, true)
                                })
                                .selectable(false);
                        }
                    }
                    menu
                }))
            })
            .anchor(Anchor::TopLeft)
            .into_any_element(),
    )
}

struct WorktreeRepositoryEntry {
    label: SharedString,
    repository: Option<Entity<Repository>>,
}

fn worktree_repository_entries(project: &Project, cx: &App) -> Vec<WorktreeRepositoryEntry> {
    let repositories = project.visible_repositories(cx);
    project
        .visible_worktrees(cx)
        .map(|worktree| WorktreeRepositoryEntry {
            label: SharedString::from(worktree.read(cx).root_name().as_unix_str().to_string()),
            repository: repository_for_worktree(&worktree, &repositories, cx),
        })
        .collect()
}

fn repository_for_worktree(
    worktree: &Entity<Worktree>,
    repositories: &[Entity<Repository>],
    cx: &App,
) -> Option<Entity<Repository>> {
    let worktree_abs_path = worktree.read(cx).abs_path();
    repositories
        .iter()
        .filter(|repository| {
            worktree_abs_path.starts_with(repository.read(cx).work_directory_abs_path.as_ref())
        })
        .max_by_key(|repository| {
            repository
                .read(cx)
                .work_directory_abs_path
                .as_os_str()
                .len()
        })
        .cloned()
}

fn worktree_repository_menu_row(label: SharedString, selected: bool, disabled: bool) -> AnyElement {
    h_flex()
        .id(SharedString::from(format!(
            "worktree-repository-menu-row-{}",
            label
        )))
        .w_full()
        .justify_between()
        .gap_2()
        .child(
            h_flex()
                .gap_1()
                .child(
                    Icon::new(IconName::Folder)
                        .size(IconSize::Small)
                        .color(if disabled {
                            Color::Disabled
                        } else {
                            Color::Muted
                        }),
                )
                .child(Label::new(label).when(disabled, |this| this.color(Color::Disabled))),
        )
        .when(selected, |this| {
            this.child(
                Icon::new(IconName::Check)
                    .size(IconSize::Small)
                    .color(Color::Accent),
            )
        })
        .when(disabled, |this| {
            this.tooltip(Tooltip::text("Not a Git repository"))
        })
        .into_any_element()
}

fn repository_selector_trigger(
    id: impl Into<ElementId>,
    label: SharedString,
    label_size: LabelSize,
) -> ButtonLike {
    ButtonLike::new(id).child(repository_selector_label(label, label_size, true))
}

fn repository_selector_label(
    label: SharedString,
    label_size: LabelSize,
    show_chevron: bool,
) -> impl IntoElement {
    h_flex()
        .gap_0p5()
        .child(
            Icon::new(IconName::Folder)
                .size(IconSize::Small)
                .color(Color::Muted),
        )
        .child(Label::new(label).size(label_size))
        .when(show_chevron, |this| {
            this.child(
                Icon::new(IconName::ChevronDown)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
        })
}

pub fn open(
    workspace: &mut Workspace,
    _: &zed_actions::git::SelectRepo,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    workspace.toggle_modal(window, cx, |window, cx| {
        RepositorySelector::new(project, rems(34.), window, cx)
    })
}

pub struct RepositorySelector {
    picker: Entity<Picker<RepositorySelectorDelegate>>,
}

impl RepositorySelector {
    pub fn new(
        project_handle: Entity<Project>,
        width: Rems,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let repository_entries = project_handle.update(cx, |project, cx| {
            let mut repos = project.visible_repositories(cx);

            repos.sort_by(|a, b| {
                a.read(cx)
                    .display_name()
                    .to_lowercase()
                    .cmp(&b.read(cx).display_name().to_lowercase())
            });

            repos
        });
        let filtered_repositories = repository_entries.clone();

        let active_repository = project_handle.read(cx).active_repository(cx);
        let selected_index = active_repository
            .as_ref()
            .and_then(|active| filtered_repositories.iter().position(|repo| repo == active))
            .unwrap_or(0);
        let delegate = RepositorySelectorDelegate {
            repository_selector: cx.entity().downgrade(),
            project: project_handle,
            repository_entries,
            filtered_repositories,
            active_repository,
            selected_index,
        };

        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .initial_width(width)
                .show_scrollbar(true)
        });

        RepositorySelector { picker }
    }
}

//pub(crate) fn filtered_repository_entries(
//    git_store: &GitStore,
//    cx: &App,
//) -> Vec<Entity<Repository>> {
//    let repositories = git_store
//        .repositories()
//        .values()
//        .sorted_by_key(|repo| {
//            let repo = repo.read(cx);
//            (
//                repo.dot_git_abs_path.clone(),
//                repo.worktree_abs_path.clone(),
//            )
//        })
//        .collect::<Vec<&Entity<Repository>>>();
//
//    repositories
//        .chunk_by(|a, b| a.read(cx).dot_git_abs_path == b.read(cx).dot_git_abs_path)
//        .flat_map(|chunk| {
//            let has_non_single_file_worktree = chunk
//                .iter()
//                .any(|repo| !repo.read(cx).is_from_single_file_worktree);
//            chunk.iter().filter(move |repo| {
//                // Remove any entry that comes from a single file worktree and represents a repository that is also represented by a non-single-file worktree.
//                !repo.read(cx).is_from_single_file_worktree || !has_non_single_file_worktree
//            })
//        })
//        .map(|&repo| repo.clone())
//        .collect()
//}

impl EventEmitter<DismissEvent> for RepositorySelector {}

impl Focusable for RepositorySelector {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for RepositorySelector {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("GitRepositorySelector")
            .child(self.picker.clone())
    }
}

impl ModalView for RepositorySelector {}

pub struct RepositorySelectorDelegate {
    repository_selector: WeakEntity<RepositorySelector>,
    project: Entity<Project>,
    repository_entries: Vec<Entity<Repository>>,
    filtered_repositories: Vec<Entity<Repository>>,
    active_repository: Option<Entity<Repository>>,
    selected_index: usize,
}

impl RepositorySelectorDelegate {
    pub fn update_repository_entries(&mut self, all_repositories: Vec<Entity<Repository>>) {
        self.repository_entries = all_repositories.clone();
        self.filtered_repositories = all_repositories;
        self.selected_index = self
            .active_repository
            .as_ref()
            .and_then(|active| {
                self.filtered_repositories
                    .iter()
                    .position(|repo| repo == active)
            })
            .unwrap_or(0);
    }
}

impl PickerDelegate for RepositorySelectorDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "repository selector"
    }

    fn match_count(&self) -> usize {
        self.filtered_repositories.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix.min(self.filtered_repositories.len().saturating_sub(1));
        cx.notify();
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select a repository...".into()
    }

    fn editor_position(&self) -> PickerEditorPosition {
        PickerEditorPosition::End
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let all_repositories = self.repository_entries.clone();

        let repo_names: Vec<(Entity<Repository>, String)> = all_repositories
            .iter()
            .map(|repo| (repo.clone(), repo.read(cx).display_name().to_lowercase()))
            .collect();

        cx.spawn_in(window, async move |this, cx| {
            let filtered_repositories = cx
                .background_spawn(async move {
                    if query.is_empty() {
                        all_repositories
                    } else {
                        let query_lower = query.to_lowercase();
                        repo_names
                            .into_iter()
                            .filter(|(_, display_name)| display_name.contains(&query_lower))
                            .map(|(repo, _)| repo)
                            .collect()
                    }
                })
                .await;

            this.update_in(cx, |this, window, cx| {
                let mut sorted_repositories = filtered_repositories;
                sorted_repositories.sort_by(|a, b| {
                    a.read(cx)
                        .display_name()
                        .to_lowercase()
                        .cmp(&b.read(cx).display_name().to_lowercase())
                });
                let selected_index = this
                    .delegate
                    .active_repository
                    .as_ref()
                    .and_then(|active| sorted_repositories.iter().position(|repo| repo == active))
                    .unwrap_or(0);
                this.delegate.filtered_repositories = sorted_repositories;
                this.delegate.set_selected_index(selected_index, window, cx);
                cx.notify();
            })
            .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(selected_repo) = self.filtered_repositories.get(self.selected_index) else {
            return;
        };
        let repository_id = selected_repo.read(cx).id;
        self.project.update(cx, |project, cx| {
            project.set_active_repository_id(Some(repository_id), cx);
        });
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.repository_selector
            .update(cx, |_this, cx| cx.emit(DismissEvent))
            .ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let repo_info = self.filtered_repositories.get(ix)?;
        let repo = repo_info.read(cx);
        let display_name = repo.display_name();
        let summary = repo.status_summary();
        let is_active = self
            .active_repository
            .as_ref()
            .is_some_and(|active| active == repo_info);

        let mut item = ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected)
            .child(
                h_flex()
                    .gap_1()
                    .child(Label::new(display_name))
                    .when(is_active, |this| {
                        this.child(
                            Icon::new(IconName::Check)
                                .size(IconSize::Small)
                                .color(Color::Accent),
                        )
                    }),
            );

        if summary.count > 0 {
            let status = if summary.conflict > 0 {
                FileStatus::Unmerged(UnmergedStatus {
                    first_head: UnmergedStatusCode::Updated,
                    second_head: UnmergedStatusCode::Updated,
                })
            } else if summary.worktree.deleted > 0 || summary.index.deleted > 0 {
                FileStatus::Tracked(TrackedStatus {
                    index_status: StatusCode::Deleted,
                    worktree_status: StatusCode::Unmodified,
                })
            } else if summary.worktree.modified > 0 || summary.index.modified > 0 {
                FileStatus::Tracked(TrackedStatus {
                    index_status: StatusCode::Modified,
                    worktree_status: StatusCode::Unmodified,
                })
            } else {
                FileStatus::Tracked(TrackedStatus {
                    index_status: StatusCode::Added,
                    worktree_status: StatusCode::Unmodified,
                })
            };
            item = item.end_slot(div().pr_2().child(git_status_icon(status)));
        }

        Some(item)
    }
}
