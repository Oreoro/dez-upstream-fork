use std::{collections::HashSet, path::PathBuf};

use anyhow::{Context as _, Result};
use collections::HashMap;
use extension_host::headless_host::HeadlessExtensionStore;
use gpui::{AppContext as _, AsyncApp, Context, Entity};
use project::host_resource_registry::HostResourceRegistry;
use rpc::{TypedEnvelope, proto};
use superzed_session::{
    HostSessionDb, HostSessionSnapshot, MutationRequest, ProjectId, ProjectRoot, ProjectSpec,
    SessionError, SessionMutation,
};

use crate::{HeadlessAppState, HeadlessProject};

pub struct SuperzedHost {
    app_state: HeadlessAppState,
    database: HostSessionDb,
    snapshot: HostSessionSnapshot,
    host_resources: HostResourceRegistry,
    projects: HashMap<ProjectId, Entity<HeadlessProject>>,
    protocol_handlers_registered: bool,
    extensions: Entity<HeadlessExtensionStore>,
}

impl SuperzedHost {
    pub fn load(database_path: PathBuf) -> Result<(HostSessionDb, HostSessionSnapshot)> {
        let database = HostSessionDb::open(&database_path)
            .with_context(|| format!("opening host session database {database_path:?}"))?;
        let snapshot = database.load()?.unwrap_or_default();
        database.save(&snapshot)?;
        Ok((database, snapshot))
    }

    pub fn new(
        app_state: HeadlessAppState,
        database: HostSessionDb,
        snapshot: HostSessionSnapshot,
        cx: &mut Context<Self>,
    ) -> Self {
        let session = app_state.session.clone();
        let extensions = HeadlessExtensionStore::new(
            app_state.fs.clone(),
            app_state.http_client.clone(),
            paths::remote_extensions_dir().to_path_buf(),
            app_state.extension_host_proxy.clone(),
            app_state.node_runtime.clone(),
            cx,
        );
        let mut host = Self {
            app_state,
            database,
            snapshot,
            host_resources: HostResourceRegistry::default(),
            projects: HashMap::default(),
            protocol_handlers_registered: false,
            extensions: extensions.clone(),
        };
        host.reconcile_projects(cx);

        session.add_request_handler(cx.weak_entity(), Self::handle_get_session);
        session.add_request_handler(cx.weak_entity(), Self::handle_resolve_project_spec);
        session.add_request_handler(cx.weak_entity(), Self::handle_mutate_session);
        session.add_request_handler(cx.weak_entity(), Self::handle_list_remote_directory);
        session.add_request_handler(cx.weak_entity(), Self::handle_shutdown_remote_server);
        session.add_request_handler(cx.weak_entity(), Self::handle_ping);
        session.add_request_handler(
            extensions.downgrade(),
            HeadlessExtensionStore::handle_sync_extensions,
        );
        session.add_request_handler(
            extensions.downgrade(),
            HeadlessExtensionStore::handle_install_extension,
        );
        host
    }

    pub fn first_project(&self) -> Option<Entity<HeadlessProject>> {
        self.snapshot
            .workspaces
            .iter()
            .find_map(|workspace| self.projects.get(&workspace.project_id).cloned())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn project_for_test(&self, project_id: ProjectId) -> Option<Entity<HeadlessProject>> {
        self.projects.get(&project_id).cloned()
    }

    #[cfg(test)]
    pub fn project_count_for_test(&self) -> usize {
        self.projects.len()
    }

    #[cfg(test)]
    pub fn host_resource_counts_for_test(&self) -> (usize, usize, usize) {
        self.host_resources.resource_user_counts_for_test()
    }

    async fn handle_list_remote_directory(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::ListRemoteDirectory>,
        cx: AsyncApp,
    ) -> Result<proto::ListRemoteDirectoryResponse> {
        use smol::stream::StreamExt;

        let fs = this.read_with(&cx, |this, _| this.app_state.fs.clone());
        let path = PathBuf::from(shellexpand::tilde(&envelope.payload.path).to_string());
        let check_info = envelope
            .payload
            .config
            .as_ref()
            .is_some_and(|config| config.is_dir);
        let mut entries = Vec::new();
        let mut entry_info = Vec::new();
        let mut directory = fs.read_dir(&path).await?;
        while let Some(path) = directory.next().await {
            let path = path?;
            if let Some(file_name) = path.file_name() {
                entries.push(file_name.to_string_lossy().into_owned());
                if check_info {
                    entry_info.push(proto::EntryInfo {
                        is_dir: fs.is_dir(&path).await,
                    });
                }
            }
        }
        Ok(proto::ListRemoteDirectoryResponse {
            entries,
            entry_info,
        })
    }

    async fn handle_shutdown_remote_server(
        _this: Entity<Self>,
        _envelope: TypedEnvelope<proto::ShutdownRemoteServer>,
        cx: AsyncApp,
    ) -> Result<proto::Ack> {
        cx.spawn(async move |cx| {
            cx.update(|cx| {
                cx.shutdown();
                cx.quit();
            })
        })
        .detach();
        Ok(proto::Ack {})
    }

    async fn handle_ping(
        _this: Entity<Self>,
        _envelope: TypedEnvelope<proto::Ping>,
        _cx: AsyncApp,
    ) -> Result<proto::Ack> {
        log::debug!("Received ping from client");
        Ok(proto::Ack {})
    }

    async fn handle_get_session(
        this: Entity<Self>,
        _envelope: TypedEnvelope<proto::GetSuperzedSession>,
        cx: AsyncApp,
    ) -> Result<proto::GetSuperzedSessionResponse> {
        let snapshot_json = this.read_with(&cx, |this, _| serde_json::to_string(&this.snapshot))?;
        Ok(proto::GetSuperzedSessionResponse { snapshot_json })
    }

    async fn handle_resolve_project_spec(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::ResolveSuperzedProjectSpec>,
        cx: AsyncApp,
    ) -> Result<proto::ResolveSuperzedProjectSpecResponse> {
        let project_spec: ProjectSpec = serde_json::from_str(&envelope.payload.project_spec_json)
            .context("deserializing Super Zed project spec")?;
        let fs = this.read_with(&cx, |this, _| this.app_state.fs.clone());
        let project_spec = canonicalize_project_spec(fs.as_ref(), &project_spec).await?;
        let snapshot_json = this.read_with(&cx, |this, _| serde_json::to_string(&this.snapshot))?;
        Ok(proto::ResolveSuperzedProjectSpecResponse {
            project_spec_json: serde_json::to_string(&project_spec)?,
            snapshot_json,
        })
    }

    async fn handle_mutate_session(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::MutateSuperzedSession>,
        mut cx: AsyncApp,
    ) -> Result<proto::MutateSuperzedSessionResponse> {
        let mut request: MutationRequest = serde_json::from_str(&envelope.payload.mutation_json)
            .context("deserializing Super Zed session mutation")?;
        let fs = this.read_with(&cx, |this, _| this.app_state.fs.clone());
        match &mut request.mutation {
            SessionMutation::CreateWorkspace { project_spec, .. }
            | SessionMutation::SetWorkspaceProjectRoots { project_spec, .. } => {
                *project_spec = canonicalize_project_spec(fs.as_ref(), project_spec).await?;
            }
            _ => {}
        }
        let (snapshot, applied) = this.update(&mut cx, |this, cx| {
            match this.database.commit_mutation(&mut this.snapshot, request) {
                Ok(()) => {
                    this.reconcile_projects(cx);
                    anyhow::Ok((this.snapshot.clone(), true))
                }
                Err(error)
                    if error.downcast_ref::<SessionError>().is_some_and(|error| {
                        matches!(
                            error,
                            SessionError::StaleRevision { .. }
                                | SessionError::StaleLayoutRevision { .. }
                        )
                    }) =>
                {
                    anyhow::Ok((this.snapshot.clone(), false))
                }
                Err(error) => Err(error),
            }
        })?;
        Ok(proto::MutateSuperzedSessionResponse {
            snapshot_json: serde_json::to_string(&snapshot)?,
            applied,
        })
    }

    fn reconcile_projects(&mut self, cx: &mut Context<Self>) {
        let active_project_ids = self
            .snapshot
            .workspaces
            .iter()
            .map(|workspace| workspace.project_id)
            .collect::<HashSet<_>>();
        let removed_project_ids = self
            .projects
            .keys()
            .filter(|project_id| !active_project_ids.contains(project_id))
            .copied()
            .collect::<Vec<_>>();
        for project_id in removed_project_ids {
            if let Some(project) = self.projects.remove(&project_id) {
                project.update(cx, |project, cx| project.close_superzed_runtime(cx));
            }
        }

        for workspace in self.snapshot.workspaces.clone() {
            let project = if let Some(project) = self.projects.get(&workspace.project_id) {
                project.clone()
            } else {
                let register_protocol_handlers = !self.protocol_handlers_registered;
                let project = cx.new(|cx| {
                    HeadlessProject::new_for_project(
                        self.app_state.clone(),
                        workspace.project_id.get(),
                        self.host_resources.clone(),
                        true,
                        register_protocol_handlers,
                        Some(self.extensions.clone()),
                        cx,
                    )
                });
                if register_protocol_handlers {
                    self.protocol_handlers_registered = true;
                }
                self.projects.insert(workspace.project_id, project.clone());
                project
            };
            let desired_paths = workspace
                .project_spec
                .roots
                .into_iter()
                .map(|root| root.canonical_path)
                .collect();
            project.update(cx, |project, cx| {
                project.reconcile_superzed_roots(desired_paths, cx)
            });
        }
    }
}

async fn canonicalize_project_spec(fs: &dyn fs::Fs, spec: &ProjectSpec) -> Result<ProjectSpec> {
    let mut canonical_paths = HashSet::new();
    let mut roots = Vec::with_capacity(spec.roots.len());
    for root in &spec.roots {
        let canonical_path = canonicalize_allow_missing(fs, &root.requested_path).await?;
        let canonical_path = if fs.is_file(&canonical_path).await {
            canonical_path
                .parent()
                .context("project file has no parent directory")?
                .to_path_buf()
        } else {
            canonical_path
        };
        if canonical_paths.insert(canonical_path.clone()) {
            roots.push(ProjectRoot {
                requested_path: root.requested_path.clone(),
                canonical_path,
            });
        }
    }
    Ok(ProjectSpec { roots })
}

async fn canonicalize_allow_missing(fs: &dyn fs::Fs, path: &std::path::Path) -> Result<PathBuf> {
    let mut existing_ancestor = path;
    let mut missing_components = Vec::new();
    let canonical_ancestor = loop {
        match fs.canonicalize(existing_ancestor).await {
            Ok(path) => break path,
            Err(error) => {
                let Some(file_name) = existing_ancestor.file_name() else {
                    return Err(error)
                        .with_context(|| format!("canonicalizing project root {path:?}"));
                };
                missing_components.push(file_name.to_os_string());
                existing_ancestor = existing_ancestor
                    .parent()
                    .with_context(|| format!("project root {path:?} has no existing ancestor"))?;
            }
        }
    };
    Ok(missing_components
        .into_iter()
        .rev()
        .fold(canonical_ancestor, |path, component| path.join(component)))
}
