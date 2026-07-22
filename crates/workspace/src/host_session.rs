use std::sync::Arc;

use anyhow::{Context as _, Result};
use collections::HashMap;
use futures::lock::Mutex;
use gpui::{App, AppContext as _, AsyncApp, Entity, Task, WindowHandle};
use project::{Project, context_server_store::HostContextServerRegistry};
use remote::{ConnectionState, RemoteClient, RemoteConnectionOptions};
use superzed_session::{
    HostSessionSnapshot, LayoutNode, ProjectId, ProjectSpec, SessionMutation, WorkspaceId,
    WorkspaceSnapshot,
};

use crate::{AppState, MultiWorkspace, Workspace};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum HostId {
    Local,
    Ssh(Arc<str>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostWorkspaceIdentity {
    pub host_id: HostId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
}

#[derive(Clone)]
pub(crate) struct WorkspaceProjection {
    pub identity: HostWorkspaceIdentity,
    pub project: Entity<Project>,
    pub workspace: Entity<Workspace>,
    pub project_spec: ProjectSpec,
    pub layout_revision: u64,
}

pub struct HostSessionClient {
    host_id: HostId,
    display_name: Arc<str>,
    remote_client: Entity<RemoteClient>,
    app_state: Arc<AppState>,
    snapshot: HostSessionSnapshot,
    projects: HashMap<ProjectId, Entity<Project>>,
    projections: HashMap<WorkspaceId, WorkspaceProjection>,
    context_servers: HostContextServerRegistry,
    mutation_queue: Arc<Mutex<()>>,
    reconciliation_queue: Arc<Mutex<()>>,
}

impl HostSessionClient {
    pub fn local(
        remote_client: Entity<RemoteClient>,
        app_state: Arc<AppState>,
        snapshot: HostSessionSnapshot,
        context_servers: HostContextServerRegistry,
    ) -> Self {
        Self {
            host_id: HostId::Local,
            display_name: "Local".into(),
            remote_client,
            app_state,
            snapshot,
            projects: HashMap::default(),
            projections: HashMap::default(),
            context_servers,
            mutation_queue: Arc::new(Mutex::new(())),
            reconciliation_queue: Arc::new(Mutex::new(())),
        }
    }

    pub fn remote(
        remote_client: Entity<RemoteClient>,
        app_state: Arc<AppState>,
        snapshot: HostSessionSnapshot,
        connection_options: &RemoteConnectionOptions,
    ) -> Result<Self> {
        let host_id = Self::host_id_for_connection(connection_options)?;
        anyhow::ensure!(
            host_id != HostId::Local,
            "remote host cannot use the local host ID"
        );
        Ok(Self {
            host_id,
            display_name: connection_options.display_name().into(),
            remote_client,
            app_state,
            snapshot,
            projects: HashMap::default(),
            projections: HashMap::default(),
            context_servers: HostContextServerRegistry::default(),
            mutation_queue: Arc::new(Mutex::new(())),
            reconciliation_queue: Arc::new(Mutex::new(())),
        })
    }

    pub fn host_id_for_connection(connection_options: &RemoteConnectionOptions) -> Result<HostId> {
        match connection_options {
            RemoteConnectionOptions::Local(_) => Ok(HostId::Local),
            RemoteConnectionOptions::Ssh(options) => {
                let username = options
                    .username
                    .as_deref()
                    .map(|username| format!("{username}@"))
                    .unwrap_or_default();
                let port = options
                    .port
                    .map(|port| format!(":{port}"))
                    .unwrap_or_default();
                Ok(HostId::Ssh(
                    format!(
                        "ssh://{username}{}{port}",
                        options.host.to_bracketed_string()
                    )
                    .into(),
                ))
            }
            #[cfg(any(test, feature = "test-support"))]
            RemoteConnectionOptions::Mock(options) => {
                Ok(HostId::Ssh(format!("mock://{}", options.id).into()))
            }
            RemoteConnectionOptions::Wsl(_) | RemoteConnectionOptions::Docker(_) => {
                anyhow::bail!("Milestone 1 supports only local and SSH host sessions")
            }
        }
    }

    pub fn host_id(&self) -> &HostId {
        &self.host_id
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn remote_client(&self) -> &Entity<RemoteClient> {
        &self.remote_client
    }

    pub fn connection_state(&self, cx: &App) -> ConnectionState {
        self.remote_client.read(cx).connection_state()
    }

    pub fn snapshot(&self) -> &HostSessionSnapshot {
        &self.snapshot
    }

    pub(crate) fn projection(&self, workspace_id: WorkspaceId) -> Option<&WorkspaceProjection> {
        self.projections.get(&workspace_id)
    }

    pub fn replace_workspace_project_roots(
        this: &Entity<Self>,
        window: WindowHandle<MultiWorkspace>,
        target: HostWorkspaceIdentity,
        requested_project_spec: ProjectSpec,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let this = this.clone();
        cx.spawn(async move |cx| {
            let client = this.read_with(cx, |this, cx| {
                anyhow::ensure!(
                    this.host_id == target.host_id,
                    "target workspace belongs to another host"
                );
                let workspace = this
                    .snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == target.workspace_id)
                    .context("target workspace is absent from its host session")?;
                anyhow::ensure!(
                    workspace.project_id == target.project_id,
                    "target workspace project identity changed"
                );
                Ok::<_, anyhow::Error>(this.remote_client.read(cx).proto_client())
            })?;
            let resolved = client
                .request(client::proto::ResolveSuperzedProjectSpec {
                    project_spec_json: serde_json::to_string(&requested_project_spec)?,
                })
                .await?;
            let project_spec: ProjectSpec = serde_json::from_str(&resolved.project_spec_json)
                .context("deserializing canonical Super Zed project roots")?;
            let replace = cx.update(|cx| {
                Self::set_project_roots(&this, window, target.workspace_id, project_spec, cx)
            });
            replace.await
        })
    }

    pub(crate) fn attach_initial_projection(
        &mut self,
        snapshot: &WorkspaceSnapshot,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        cx: &App,
    ) -> Result<()> {
        anyhow::ensure!(
            self.projections.is_empty(),
            "initial host projection may only be attached once"
        );
        let identity = HostWorkspaceIdentity {
            host_id: self.host_id.clone(),
            workspace_id: snapshot.id,
            project_id: snapshot.project_id,
        };
        anyhow::ensure!(
            workspace.read(cx).host_workspace_identity() == Some(&identity),
            "initial workspace identity does not match its server snapshot"
        );
        anyhow::ensure!(
            !self.projects.contains_key(&snapshot.project_id),
            "initial host project may only be attached once"
        );
        self.projects.insert(snapshot.project_id, project.clone());
        self.projections.insert(
            snapshot.id,
            WorkspaceProjection {
                identity,
                project,
                workspace,
                project_spec: snapshot.project_spec.clone(),
                layout_revision: 0,
            },
        );
        Ok(())
    }

    pub fn create_workspace(
        this: &Entity<Self>,
        window: WindowHandle<MultiWorkspace>,
        project_spec: ProjectSpec,
        cx: &mut App,
    ) -> Task<Result<()>> {
        Self::mutate(
            this,
            window,
            move |snapshot| SessionMutation::CreateWorkspace {
                after: Some(snapshot.active_workspace_id),
                project_spec: project_spec.clone(),
            },
            true,
            cx,
        )
    }

    pub fn activate_workspace(
        this: &Entity<Self>,
        window: WindowHandle<MultiWorkspace>,
        workspace_id: WorkspaceId,
        cx: &mut App,
    ) -> Task<Result<()>> {
        Self::mutate(
            this,
            window,
            move |_| SessionMutation::ActivateWorkspace { workspace_id },
            true,
            cx,
        )
    }

    pub fn close_workspace(
        this: &Entity<Self>,
        window: WindowHandle<MultiWorkspace>,
        workspace_id: WorkspaceId,
        cx: &mut App,
    ) -> Task<Result<()>> {
        Self::mutate(
            this,
            window,
            move |_| SessionMutation::CloseWorkspace { workspace_id },
            false,
            cx,
        )
    }

    pub fn set_project_roots(
        this: &Entity<Self>,
        window: WindowHandle<MultiWorkspace>,
        workspace_id: WorkspaceId,
        project_spec: ProjectSpec,
        cx: &mut App,
    ) -> Task<Result<()>> {
        Self::mutate(
            this,
            window,
            move |_| SessionMutation::SetWorkspaceProjectRoots {
                workspace_id,
                project_spec: project_spec.clone(),
            },
            false,
            cx,
        )
    }

    pub fn add_project_root(
        this: &Entity<Self>,
        window: WindowHandle<MultiWorkspace>,
        workspace_id: WorkspaceId,
        root: std::path::PathBuf,
        cx: &mut App,
    ) -> Task<Result<()>> {
        Self::add_project_roots(this, window, workspace_id, vec![root], cx)
    }

    pub fn add_project_roots(
        this: &Entity<Self>,
        window: WindowHandle<MultiWorkspace>,
        workspace_id: WorkspaceId,
        roots: Vec<std::path::PathBuf>,
        cx: &mut App,
    ) -> Task<Result<()>> {
        Self::mutate_result(
            this,
            window,
            move |snapshot| {
                let workspace = snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == workspace_id)
                    .context("workspace for added project root is absent")?;
                let mut project_spec = workspace.project_spec.clone();
                project_spec.roots.extend(roots.iter().cloned().map(|path| {
                    superzed_session::ProjectRoot {
                        requested_path: path.clone(),
                        canonical_path: path,
                    }
                }));
                Ok(SessionMutation::SetWorkspaceProjectRoots {
                    workspace_id,
                    project_spec,
                })
            },
            None,
            false,
            cx,
        )
    }

    pub fn remove_project_root(
        this: &Entity<Self>,
        window: WindowHandle<MultiWorkspace>,
        workspace_id: WorkspaceId,
        root: std::path::PathBuf,
        cx: &mut App,
    ) -> Task<Result<()>> {
        Self::mutate_result(
            this,
            window,
            move |snapshot| {
                let workspace = snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == workspace_id)
                    .context("workspace for removed project root is absent")?;
                let mut project_spec = workspace.project_spec.clone();
                project_spec.roots.retain(|candidate| {
                    candidate.canonical_path != root && candidate.requested_path != root
                });
                Ok(SessionMutation::SetWorkspaceProjectRoots {
                    workspace_id,
                    project_spec,
                })
            },
            None,
            false,
            cx,
        )
    }

    pub fn replace_layout(
        this: &Entity<Self>,
        window: WindowHandle<MultiWorkspace>,
        workspace_id: WorkspaceId,
        layout: LayoutNode,
        cx: &mut App,
    ) -> Task<Result<()>> {
        Self::mutate_result(
            this,
            window,
            move |snapshot| {
                let workspace = snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == workspace_id)
                    .context("serialized workspace is absent from host session")?;
                Ok(SessionMutation::ReplaceWorkspaceLayout {
                    workspace_id,
                    expected_layout_revision: workspace.layout_revision,
                    layout: layout.clone(),
                })
            },
            Some(workspace_id),
            false,
            cx,
        )
    }

    fn mutate(
        this: &Entity<Self>,
        window: WindowHandle<MultiWorkspace>,
        mutation: impl Fn(&HostSessionSnapshot) -> SessionMutation + 'static,
        activate_host: bool,
        cx: &mut App,
    ) -> Task<Result<()>> {
        Self::mutate_result(
            this,
            window,
            move |snapshot| Ok(mutation(snapshot)),
            None,
            activate_host,
            cx,
        )
    }

    fn mutate_result(
        this: &Entity<Self>,
        window: WindowHandle<MultiWorkspace>,
        mutation: impl Fn(&HostSessionSnapshot) -> Result<SessionMutation> + 'static,
        local_layout_workspace_id: Option<WorkspaceId>,
        activate_host: bool,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let this = this.clone();
        cx.spawn(async move |cx| {
            let mutation_queue = this.read_with(cx, |this, _| this.mutation_queue.clone());
            let _mutation_guard = mutation_queue.lock().await;
            for attempt in 0..=1 {
                let (client, request) = this.read_with(cx, |this, cx| {
                    let mutation = mutation(&this.snapshot)?;
                    Ok::<_, anyhow::Error>((
                        this.remote_client.read(cx).proto_client(),
                        superzed_session::MutationRequest {
                            expected_revision: this.snapshot.revision,
                            mutation,
                        },
                    ))
                })?;
                let response = client
                    .request(client::proto::MutateSuperzedSession {
                        mutation_json: serde_json::to_string(&request)?,
                    })
                    .await?;
                let snapshot: HostSessionSnapshot =
                    serde_json::from_str(&response.snapshot_json)
                        .context("deserializing mutated Super Zed host session")?;
                snapshot
                    .validate()
                    .context("validating mutated Super Zed host session")?;
                let applied = response.applied;
                let reconciled_layout_workspace_id = if applied {
                    local_layout_workspace_id
                } else {
                    None
                };
                Self::reconcile_snapshot(
                    this.clone(),
                    window,
                    snapshot,
                    reconciled_layout_workspace_id,
                    activate_host,
                    cx,
                )
                .await?;
                if applied {
                    return Ok(());
                }
                if attempt == 1 {
                    anyhow::bail!("host session remained stale after retrying mutation");
                }
            }
            Err(anyhow::anyhow!(
                "host mutation retry loop ended without a result"
            ))
        })
    }

    pub async fn reconcile(
        this: Entity<Self>,
        window: WindowHandle<MultiWorkspace>,
        snapshot: HostSessionSnapshot,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        Self::reconcile_snapshot(this, window, snapshot, None, false, cx).await
    }

    async fn reconcile_snapshot(
        this: Entity<Self>,
        window: WindowHandle<MultiWorkspace>,
        snapshot: HostSessionSnapshot,
        local_layout_workspace_id: Option<WorkspaceId>,
        activate_host: bool,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let reconciliation_queue = this.read_with(cx, |this, _| this.reconciliation_queue.clone());
        let _reconciliation_guard = reconciliation_queue.lock().await;
        snapshot.validate()?;
        let (host_id, remote_client, app_state, context_servers, existing) =
            this.read_with(cx, |this, _| {
                (
                    this.host_id.clone(),
                    this.remote_client.clone(),
                    this.app_state.clone(),
                    this.context_servers.clone(),
                    this.projections.clone(),
                )
            });

        let mut desired = HashMap::default();
        let mut layouts_to_restore = Vec::new();
        for workspace_snapshot in &snapshot.workspaces {
            let identity = HostWorkspaceIdentity {
                host_id: host_id.clone(),
                workspace_id: workspace_snapshot.id,
                project_id: workspace_snapshot.project_id,
            };
            let mut projection = if let Some(projection) = existing
                .get(&workspace_snapshot.id)
                .filter(|projection| projection.identity == identity)
            {
                projection.clone()
            } else {
                let project = if let Some(project) = this.read_with(cx, |this, _| {
                    this.projects.get(&workspace_snapshot.project_id).cloned()
                }) {
                    project
                } else {
                    let project = cx.update(|cx| {
                        Project::remote_for_project(
                            remote_client.clone(),
                            app_state.client.clone(),
                            app_state.node_runtime.clone(),
                            app_state.user_store.clone(),
                            app_state.languages.clone(),
                            app_state.fs.clone(),
                            workspace_snapshot.project_id.get(),
                            true,
                            context_servers.clone(),
                            cx,
                        )
                    });
                    this.update(cx, |this, _| {
                        this.projects
                            .insert(workspace_snapshot.project_id, project.clone());
                    });
                    project
                };
                Project::initialize_superzed_project(project.clone(), cx)
                    .await
                    .with_context(|| {
                        format!(
                            "initializing host project {}",
                            workspace_snapshot.project_id.get()
                        )
                    })?;
                let workspace = window.update(cx, |_, window, cx| {
                    cx.new(|cx| {
                        Workspace::new_for_host(
                            None,
                            project.clone(),
                            app_state.clone(),
                            identity.clone(),
                            window,
                            cx,
                        )
                    })
                })?;
                WorkspaceProjection {
                    identity,
                    project,
                    workspace,
                    project_spec: workspace_snapshot.project_spec.clone(),
                    layout_revision: 0,
                }
            };

            if projection.project_spec != workspace_snapshot.project_spec {
                Project::initialize_superzed_project(projection.project.clone(), cx).await?;
                projection.project_spec = workspace_snapshot.project_spec.clone();
            }
            if local_layout_workspace_id == Some(workspace_snapshot.id) {
                projection.layout_revision = workspace_snapshot.layout_revision;
            } else if projection.layout_revision != workspace_snapshot.layout_revision {
                layouts_to_restore.push((
                    projection.workspace.clone(),
                    workspace_snapshot.layout.clone(),
                ));
                projection.layout_revision = workspace_snapshot.layout_revision;
            }
            desired.insert(workspace_snapshot.id, projection);
        }

        for (workspace, layout) in layouts_to_restore {
            window
                .update(cx, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        workspace.restore_superzed_layout(layout, window, cx)
                    })
                })?
                .await?;
        }

        let ordered_workspaces = snapshot
            .workspaces
            .iter()
            .map(|workspace| {
                desired
                    .get(&workspace.id)
                    .map(|projection| projection.workspace.clone())
                    .context("reconciled workspace projection is missing")
            })
            .collect::<Result<Vec<_>>>()?;
        let active_workspace = desired
            .get(&snapshot.active_workspace_id)
            .map(|projection| projection.workspace.clone())
            .context("reconciled active workspace projection is missing")?;

        this.update(cx, |this, cx| {
            let active_project_ids = snapshot
                .workspaces
                .iter()
                .map(|workspace| workspace.project_id)
                .collect::<std::collections::HashSet<_>>();
            let removed_project_ids = this
                .projects
                .keys()
                .filter(|project_id| !active_project_ids.contains(project_id))
                .copied()
                .collect::<Vec<_>>();
            let proto_client = this.remote_client.read(cx).proto_client();
            for project_id in removed_project_ids {
                proto_client.unsubscribe_from_remote_id(project_id.get());
                this.projects.remove(&project_id);
            }
            this.snapshot = snapshot;
            this.projections = desired;
            cx.notify();
        });
        window.update(cx, |multi_workspace, window, cx| {
            multi_workspace.reconcile_host_projections(
                &host_id,
                ordered_workspaces,
                active_workspace,
                activate_host,
                window,
                cx,
            );
        })?;
        Ok(())
    }

    pub(crate) fn initial_workspace_snapshot(
        snapshot: &HostSessionSnapshot,
    ) -> Result<&WorkspaceSnapshot> {
        snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == snapshot.active_workspace_id)
            .context("Super Zed host session has no active workspace")
    }
}
