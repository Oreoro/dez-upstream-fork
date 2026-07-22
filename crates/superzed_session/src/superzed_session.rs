mod persistence;

pub use persistence::HostSessionDb;

use std::{collections::HashSet, num::NonZeroU64, path::PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            pub fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

uuid_id!(WorkspaceId);
uuid_id!(PaneId);
uuid_id!(SessionItemId);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(transparent)]
pub struct ProjectId(NonZeroU64);

impl ProjectId {
    pub fn new(value: u64) -> Option<Self> {
        NonZeroU64::new(value).map(Self)
    }

    pub fn get(self) -> u64 {
        self.0.get()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProjectRoot {
    pub requested_path: PathBuf,
    pub canonical_path: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProjectSpec {
    pub roots: Vec<ProjectRoot>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileEditorItem {
    pub id: SessionItemId,
    pub absolute_path: PathBuf,
    pub pinned: bool,
    pub preview: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaneSnapshot {
    pub id: PaneId,
    pub items: Vec<FileEditorItem>,
    pub active_item_id: Option<SessionItemId>,
    pub focused: bool,
}

impl PaneSnapshot {
    pub fn empty() -> Self {
        Self {
            id: PaneId::new(),
            items: Vec::new(),
            active_item_id: None,
            focused: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LayoutNode {
    Axis {
        axis: LayoutAxis,
        flexes: Vec<f32>,
        children: Vec<LayoutNode>,
    },
    Pane(PaneSnapshot),
}

impl LayoutNode {
    pub fn empty() -> Self {
        Self::Pane(PaneSnapshot::empty())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub id: WorkspaceId,
    pub project_id: ProjectId,
    pub project_spec: ProjectSpec,
    pub layout_revision: u64,
    pub layout: LayoutNode,
}

impl WorkspaceSnapshot {
    pub fn blank(project_id: ProjectId) -> Self {
        Self {
            id: WorkspaceId::new(),
            project_id,
            project_spec: ProjectSpec::default(),
            layout_revision: 0,
            layout: LayoutNode::empty(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostSessionSnapshot {
    pub revision: u64,
    pub next_project_id: u64,
    pub active_workspace_id: WorkspaceId,
    pub workspaces: Vec<WorkspaceSnapshot>,
}

impl Default for HostSessionSnapshot {
    fn default() -> Self {
        let project_id = ProjectId(NonZeroU64::MIN);
        let workspace = WorkspaceSnapshot::blank(project_id);
        Self {
            revision: 0,
            next_project_id: 2,
            active_workspace_id: workspace.id,
            workspaces: vec![workspace],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MutationRequest {
    pub expected_revision: u64,
    pub mutation: SessionMutation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SessionMutation {
    CreateWorkspace {
        after: Option<WorkspaceId>,
        project_spec: ProjectSpec,
    },
    CloseWorkspace {
        workspace_id: WorkspaceId,
    },
    ActivateWorkspace {
        workspace_id: WorkspaceId,
    },
    SetWorkspaceProjectRoots {
        workspace_id: WorkspaceId,
        project_spec: ProjectSpec,
    },
    ReplaceWorkspaceLayout {
        workspace_id: WorkspaceId,
        expected_layout_revision: u64,
        layout: LayoutNode,
    },
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session revision is stale: expected {expected}, current {current}")]
    StaleRevision { expected: u64, current: u64 },
    #[error("workspace {0} does not exist")]
    WorkspaceNotFound(WorkspaceId),
    #[error("layout revision is stale: expected {expected}, current {current}")]
    StaleLayoutRevision { expected: u64, current: u64 },
    #[error("invalid session snapshot: {0}")]
    Invalid(String),
    #[error("project id space is exhausted")]
    ProjectIdExhausted,
    #[error("session revision space is exhausted")]
    SessionRevisionExhausted,
}

impl HostSessionSnapshot {
    pub fn validate(&self) -> Result<(), SessionError> {
        if self.workspaces.is_empty() {
            return Err(SessionError::Invalid(
                "a host session must contain at least one workspace".into(),
            ));
        }

        if self.next_project_id == 0
            || self
                .workspaces
                .iter()
                .any(|workspace| workspace.project_id.get() >= self.next_project_id)
        {
            return Err(SessionError::Invalid(
                "next project id must be greater than every active project id".into(),
            ));
        }

        let mut workspace_ids = HashSet::new();
        let mut project_ids = HashSet::new();
        for workspace in &self.workspaces {
            if !workspace_ids.insert(workspace.id) {
                return Err(SessionError::Invalid(format!(
                    "duplicate workspace id {}",
                    workspace.id
                )));
            }
            if !project_ids.insert(workspace.project_id) {
                return Err(SessionError::Invalid(format!(
                    "project id {} is assigned to multiple workspace scopes",
                    workspace.project_id.get()
                )));
            }
            validate_project_spec(&workspace.project_spec)?;
            validate_layout(&workspace.layout)?;
        }

        if !workspace_ids.contains(&self.active_workspace_id) {
            return Err(SessionError::Invalid(
                "active workspace is absent from the session".into(),
            ));
        }

        Ok(())
    }

    pub fn apply(&mut self, request: MutationRequest) -> Result<(), SessionError> {
        let mut next = self.clone();
        next.apply_inner(request)?;
        next.validate()?;
        *self = next;
        Ok(())
    }

    fn apply_inner(&mut self, request: MutationRequest) -> Result<(), SessionError> {
        if request.expected_revision != self.revision {
            return Err(SessionError::StaleRevision {
                expected: request.expected_revision,
                current: self.revision,
            });
        }

        match request.mutation {
            SessionMutation::CreateWorkspace {
                after,
                project_spec,
            } => {
                validate_project_spec(&project_spec)?;
                let insertion_index = match after {
                    Some(after) => self
                        .workspace_index(after)
                        .ok_or(SessionError::WorkspaceNotFound(after))?
                        .saturating_add(1),
                    None => self.workspaces.len(),
                };
                let project_id = self.allocate_project_id()?;
                let workspace = WorkspaceSnapshot {
                    project_spec,
                    ..WorkspaceSnapshot::blank(project_id)
                };
                self.active_workspace_id = workspace.id;
                self.workspaces.insert(insertion_index, workspace);
            }
            SessionMutation::CloseWorkspace { workspace_id } => {
                let index = self
                    .workspace_index(workspace_id)
                    .ok_or(SessionError::WorkspaceNotFound(workspace_id))?;
                self.workspaces.remove(index);
                if self.workspaces.is_empty() {
                    let replacement = WorkspaceSnapshot::blank(self.allocate_project_id()?);
                    self.active_workspace_id = replacement.id;
                    self.workspaces.push(replacement);
                } else if self.active_workspace_id == workspace_id {
                    let fallback_index = index.min(self.workspaces.len().saturating_sub(1));
                    let fallback = self.workspaces.get(fallback_index).ok_or_else(|| {
                        SessionError::Invalid("failed to select a fallback workspace".into())
                    })?;
                    self.active_workspace_id = fallback.id;
                }
            }
            SessionMutation::ActivateWorkspace { workspace_id } => {
                if self.workspace_index(workspace_id).is_none() {
                    return Err(SessionError::WorkspaceNotFound(workspace_id));
                }
                self.active_workspace_id = workspace_id;
            }
            SessionMutation::SetWorkspaceProjectRoots {
                workspace_id,
                project_spec,
            } => {
                validate_project_spec(&project_spec)?;
                self.workspace_mut(workspace_id)?.project_spec = project_spec;
            }
            SessionMutation::ReplaceWorkspaceLayout {
                workspace_id,
                expected_layout_revision,
                layout,
            } => {
                validate_layout(&layout)?;
                let workspace = self.workspace_mut(workspace_id)?;
                if workspace.layout_revision != expected_layout_revision {
                    return Err(SessionError::StaleLayoutRevision {
                        expected: expected_layout_revision,
                        current: workspace.layout_revision,
                    });
                }
                workspace.layout = layout;
                workspace.layout_revision = workspace.layout_revision.saturating_add(1);
            }
        }

        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(SessionError::SessionRevisionExhausted)?;
        Ok(())
    }

    fn workspace_index(&self, id: WorkspaceId) -> Option<usize> {
        self.workspaces
            .iter()
            .position(|workspace| workspace.id == id)
    }

    fn workspace_mut(&mut self, id: WorkspaceId) -> Result<&mut WorkspaceSnapshot, SessionError> {
        self.workspaces
            .iter_mut()
            .find(|workspace| workspace.id == id)
            .ok_or(SessionError::WorkspaceNotFound(id))
    }

    fn allocate_project_id(&mut self) -> Result<ProjectId, SessionError> {
        let project_id =
            ProjectId::new(self.next_project_id).ok_or(SessionError::ProjectIdExhausted)?;
        self.next_project_id = self
            .next_project_id
            .checked_add(1)
            .ok_or(SessionError::ProjectIdExhausted)?;
        Ok(project_id)
    }
}

fn validate_project_spec(spec: &ProjectSpec) -> Result<(), SessionError> {
    let mut canonical_paths = HashSet::new();
    for root in &spec.roots {
        if !root.requested_path.is_absolute() || !root.canonical_path.is_absolute() {
            return Err(SessionError::Invalid(
                "project roots must use absolute requested and canonical paths".into(),
            ));
        }
        if !canonical_paths.insert(root.canonical_path.clone()) {
            return Err(SessionError::Invalid(format!(
                "duplicate canonical project root {}",
                root.canonical_path.display()
            )));
        }
    }
    Ok(())
}

fn validate_layout(layout: &LayoutNode) -> Result<(), SessionError> {
    fn visit(
        node: &LayoutNode,
        pane_ids: &mut HashSet<PaneId>,
        item_ids: &mut HashSet<SessionItemId>,
        focused_panes: &mut usize,
    ) -> Result<(), SessionError> {
        match node {
            LayoutNode::Axis {
                flexes, children, ..
            } => {
                if children.len() < 2 {
                    return Err(SessionError::Invalid(
                        "a layout axis must contain at least two children".into(),
                    ));
                }
                if flexes.len() != children.len() {
                    return Err(SessionError::Invalid(
                        "layout flex count must match child count".into(),
                    ));
                }
                if flexes.iter().any(|flex| !flex.is_finite() || *flex <= 0.0) {
                    return Err(SessionError::Invalid(
                        "layout flex values must be finite and positive".into(),
                    ));
                }
                for child in children {
                    visit(child, pane_ids, item_ids, focused_panes)?;
                }
            }
            LayoutNode::Pane(pane) => {
                if !pane_ids.insert(pane.id) {
                    return Err(SessionError::Invalid(format!(
                        "duplicate pane id {}",
                        pane.id
                    )));
                }
                if pane.focused {
                    *focused_panes = focused_panes.saturating_add(1);
                }
                let mut pane_item_ids = HashSet::new();
                for item in &pane.items {
                    if !item.absolute_path.is_absolute() {
                        return Err(SessionError::Invalid(
                            "file editor paths must be absolute".into(),
                        ));
                    }
                    if !pane_item_ids.insert(item.id) || !item_ids.insert(item.id) {
                        return Err(SessionError::Invalid(format!(
                            "duplicate session item id {}",
                            item.id
                        )));
                    }
                }
                if let Some(active_item_id) = pane.active_item_id
                    && !pane_item_ids.contains(&active_item_id)
                {
                    return Err(SessionError::Invalid(format!(
                        "pane {} has an absent active item",
                        pane.id
                    )));
                }
            }
        }
        Ok(())
    }

    let mut pane_ids = HashSet::new();
    let mut item_ids = HashSet::new();
    let mut focused_panes = 0;
    visit(layout, &mut pane_ids, &mut item_ids, &mut focused_panes)?;
    if focused_panes > 1 {
        return Err(SessionError::Invalid(
            "a workspace layout cannot contain multiple focused panes".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absolute_project_spec(paths: &[&str]) -> ProjectSpec {
        ProjectSpec {
            roots: paths
                .iter()
                .map(|path| ProjectRoot {
                    requested_path: PathBuf::from(path),
                    canonical_path: PathBuf::from(path),
                })
                .collect(),
        }
    }

    fn apply(session: &mut HostSessionSnapshot, mutation: SessionMutation) {
        session
            .apply(MutationRequest {
                expected_revision: session.revision,
                mutation,
            })
            .expect("session mutation should succeed");
    }

    #[test]
    fn closing_last_workspace_creates_blank_replacement() {
        let mut session = HostSessionSnapshot::default();
        let original_id = session.active_workspace_id;
        session
            .apply(MutationRequest {
                expected_revision: 0,
                mutation: SessionMutation::CloseWorkspace {
                    workspace_id: original_id,
                },
            })
            .expect("closing the last workspace should create a replacement");

        assert_eq!(session.workspaces.len(), 1);
        assert_ne!(session.active_workspace_id, original_id);
        assert_eq!(session.workspaces[0].project_id.get(), 2);
        assert_eq!(session.next_project_id, 3);
        assert!(session.workspaces[0].project_spec.roots.is_empty());
    }

    #[test]
    fn create_activate_and_close_preserve_server_order() {
        let mut session = HostSessionSnapshot::default();
        let first_workspace_id = session.active_workspace_id;
        apply(
            &mut session,
            SessionMutation::CreateWorkspace {
                after: Some(first_workspace_id),
                project_spec: absolute_project_spec(&["/code/second"]),
            },
        );
        let second_workspace_id = session.active_workspace_id;
        apply(
            &mut session,
            SessionMutation::CreateWorkspace {
                after: Some(first_workspace_id),
                project_spec: absolute_project_spec(&["/code/third"]),
            },
        );
        let third_workspace_id = session.active_workspace_id;

        assert_eq!(
            session
                .workspaces
                .iter()
                .map(|workspace| workspace.id)
                .collect::<Vec<_>>(),
            vec![first_workspace_id, third_workspace_id, second_workspace_id]
        );
        assert_eq!(
            session
                .workspaces
                .iter()
                .map(|workspace| workspace.project_id.get())
                .collect::<Vec<_>>(),
            vec![1, 3, 2]
        );

        apply(
            &mut session,
            SessionMutation::ActivateWorkspace {
                workspace_id: second_workspace_id,
            },
        );
        apply(
            &mut session,
            SessionMutation::CloseWorkspace {
                workspace_id: third_workspace_id,
            },
        );
        assert_eq!(session.active_workspace_id, second_workspace_id);
        assert_eq!(
            session
                .workspaces
                .iter()
                .map(|workspace| workspace.id)
                .collect::<Vec<_>>(),
            vec![first_workspace_id, second_workspace_id]
        );

        apply(
            &mut session,
            SessionMutation::CloseWorkspace {
                workspace_id: second_workspace_id,
            },
        );
        assert_eq!(session.active_workspace_id, first_workspace_id);
    }

    #[test]
    fn project_roots_and_layout_replace_only_the_target_workspace() {
        let mut session = HostSessionSnapshot::default();
        let first_workspace_id = session.active_workspace_id;
        apply(
            &mut session,
            SessionMutation::CreateWorkspace {
                after: Some(first_workspace_id),
                project_spec: absolute_project_spec(&["/code/second"]),
            },
        );
        let second_workspace_id = session.active_workspace_id;
        let first_project_spec = absolute_project_spec(&["/code/first", "/code/shared"]);
        apply(
            &mut session,
            SessionMutation::SetWorkspaceProjectRoots {
                workspace_id: first_workspace_id,
                project_spec: first_project_spec.clone(),
            },
        );

        let item = FileEditorItem {
            id: SessionItemId::new(),
            absolute_path: PathBuf::from("/code/first/src/main.rs"),
            pinned: true,
            preview: false,
        };
        let layout = LayoutNode::Pane(PaneSnapshot {
            id: PaneId::new(),
            items: vec![item.clone()],
            active_item_id: Some(item.id),
            focused: true,
        });
        apply(
            &mut session,
            SessionMutation::ReplaceWorkspaceLayout {
                workspace_id: first_workspace_id,
                expected_layout_revision: 0,
                layout: layout.clone(),
            },
        );

        let first = session
            .workspaces
            .iter()
            .find(|workspace| workspace.id == first_workspace_id)
            .expect("first workspace should remain present");
        let second = session
            .workspaces
            .iter()
            .find(|workspace| workspace.id == second_workspace_id)
            .expect("second workspace should remain present");
        assert_eq!(first.project_spec, first_project_spec);
        assert_eq!(first.layout, layout);
        assert_eq!(first.layout_revision, 1);
        assert_eq!(
            second.project_spec,
            absolute_project_spec(&["/code/second"])
        );
        assert_eq!(second.layout_revision, 0);
    }

    #[test]
    fn rejects_stale_session_and_layout_revisions() {
        let mut session = HostSessionSnapshot::default();
        let workspace = session.workspaces[0].clone();
        let stale_session = session.apply(MutationRequest {
            expected_revision: 1,
            mutation: SessionMutation::ActivateWorkspace {
                workspace_id: workspace.id,
            },
        });
        assert!(matches!(
            stale_session,
            Err(SessionError::StaleRevision { .. })
        ));

        let stale_layout = session.apply(MutationRequest {
            expected_revision: 0,
            mutation: SessionMutation::ReplaceWorkspaceLayout {
                workspace_id: workspace.id,
                expected_layout_revision: 1,
                layout: LayoutNode::empty(),
            },
        });
        assert!(matches!(
            stale_layout,
            Err(SessionError::StaleLayoutRevision { .. })
        ));
    }

    #[test]
    fn rejects_duplicate_panes_and_invalid_flexes() {
        let pane = PaneSnapshot::empty();
        let layout = LayoutNode::Axis {
            axis: LayoutAxis::Horizontal,
            flexes: vec![1.0, 0.0],
            children: vec![LayoutNode::Pane(pane.clone()), LayoutNode::Pane(pane)],
        };
        assert!(validate_layout(&layout).is_err());
    }

    #[test]
    fn validates_snapshot_identity_roots_and_layout_references() {
        let session = HostSessionSnapshot::default();

        let mut duplicate_workspace = session.clone();
        duplicate_workspace
            .workspaces
            .push(duplicate_workspace.workspaces[0].clone());
        assert!(duplicate_workspace.validate().is_err());

        let mut missing_active = session.clone();
        missing_active.active_workspace_id = WorkspaceId::new();
        assert!(missing_active.validate().is_err());

        let mut relative_root = session.clone();
        relative_root.workspaces[0].project_spec = ProjectSpec {
            roots: vec![ProjectRoot {
                requested_path: PathBuf::from("relative"),
                canonical_path: PathBuf::from("/code/relative"),
            }],
        };
        assert!(relative_root.validate().is_err());

        let mut duplicate_root = session.clone();
        duplicate_root.workspaces[0].project_spec = ProjectSpec {
            roots: vec![
                ProjectRoot {
                    requested_path: PathBuf::from("/code/one"),
                    canonical_path: PathBuf::from("/code/shared"),
                },
                ProjectRoot {
                    requested_path: PathBuf::from("/code/two"),
                    canonical_path: PathBuf::from("/code/shared"),
                },
            ],
        };
        assert!(duplicate_root.validate().is_err());

        let pane = PaneSnapshot::empty();
        let mut invalid_axis = session.clone();
        invalid_axis.workspaces[0].layout = LayoutNode::Axis {
            axis: LayoutAxis::Horizontal,
            flexes: vec![1.0],
            children: vec![LayoutNode::Pane(pane.clone())],
        };
        assert!(invalid_axis.validate().is_err());

        let mut duplicate_pane = session.clone();
        duplicate_pane.workspaces[0].layout = LayoutNode::Axis {
            axis: LayoutAxis::Horizontal,
            flexes: vec![1.0, 1.0],
            children: vec![LayoutNode::Pane(pane.clone()), LayoutNode::Pane(pane)],
        };
        assert!(duplicate_pane.validate().is_err());

        let mut absent_active_item = session.clone();
        absent_active_item.workspaces[0].layout = LayoutNode::Pane(PaneSnapshot {
            id: PaneId::new(),
            items: Vec::new(),
            active_item_id: Some(SessionItemId::new()),
            focused: true,
        });
        assert!(absent_active_item.validate().is_err());

        let mut multiple_focused = session;
        multiple_focused.workspaces[0].layout = LayoutNode::Axis {
            axis: LayoutAxis::Horizontal,
            flexes: vec![1.0, 1.0],
            children: vec![
                LayoutNode::Pane(PaneSnapshot::empty()),
                LayoutNode::Pane(PaneSnapshot::empty()),
            ],
        };
        assert!(multiple_focused.validate().is_err());
    }

    #[test]
    fn failed_mutations_leave_the_snapshot_unchanged() {
        let mut session = HostSessionSnapshot::default();
        let before_missing_workspace = session.clone();
        let missing_workspace = WorkspaceId::new();
        let error = session.apply(MutationRequest {
            expected_revision: session.revision,
            mutation: SessionMutation::CreateWorkspace {
                after: Some(missing_workspace),
                project_spec: ProjectSpec::default(),
            },
        });
        assert!(
            matches!(error, Err(SessionError::WorkspaceNotFound(id)) if id == missing_workspace)
        );
        assert_eq!(session, before_missing_workspace);

        session.revision = u64::MAX;
        let before_revision_overflow = session.clone();
        let error = session.apply(MutationRequest {
            expected_revision: u64::MAX,
            mutation: SessionMutation::ActivateWorkspace {
                workspace_id: session.active_workspace_id,
            },
        });
        assert!(matches!(error, Err(SessionError::SessionRevisionExhausted)));
        assert_eq!(session, before_revision_overflow);
    }

    #[test]
    fn project_ids_are_not_reused_after_workspace_close() {
        let mut session = HostSessionSnapshot::default();
        let first_workspace_id = session.active_workspace_id;
        session
            .apply(MutationRequest {
                expected_revision: 0,
                mutation: SessionMutation::CloseWorkspace {
                    workspace_id: first_workspace_id,
                },
            })
            .expect("replacement workspace should be created");
        let replacement_project_id = session.workspaces[0].project_id;
        session
            .apply(MutationRequest {
                expected_revision: 1,
                mutation: SessionMutation::CreateWorkspace {
                    after: Some(session.active_workspace_id),
                    project_spec: ProjectSpec::default(),
                },
            })
            .expect("second workspace should be created");

        assert_eq!(replacement_project_id.get(), 2);
        assert_eq!(session.workspaces[1].project_id.get(), 3);
        assert_eq!(session.next_project_id, 4);
    }
}
