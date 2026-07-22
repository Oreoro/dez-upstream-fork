use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
#[cfg(any(test, feature = "test-support"))]
use askpass::EncryptedPassword;
use editor::Editor;
use extension_host::ExtensionStore;
#[cfg(any(test, feature = "test-support"))]
use futures::{FutureExt as _, channel::oneshot, select};
#[cfg(any(test, feature = "test-support"))]
use gpui::PromptLevel;
#[cfg(any(test, feature = "test-support"))]
use gpui::Task;
use gpui::{AsyncApp, WindowHandle};

#[cfg(any(test, feature = "test-support"))]
use project::trusted_worktrees;
use remote::{
    DockerConnectionOptions, Interactive, RemoteConnection, RemoteConnectionOptions,
    SshConnectionOptions, remote_client::ConnectionIdentifier,
};
pub use settings::SshConnection;
use settings::{DevContainerConnection, ExtendingVec, RegisterSetting, Settings, WslConnection};
use util::paths::PathWithPosition;
use workspace::{AppState, MultiWorkspace};
#[cfg(any(test, feature = "test-support"))]
use workspace::{OpenOptions, SerializedWorkspaceLocation, find_existing_workspace};

#[cfg(any(test, feature = "test-support"))]
use remote_connection::RemoteClientDelegate;
pub use remote_connection::{
    RemoteConnectionModal, RemoteConnectionPrompt, SshConnectionHeader, connect,
};

#[derive(RegisterSetting)]
pub struct RemoteSettings {
    pub ssh_connections: ExtendingVec<SshConnection>,
    pub wsl_connections: ExtendingVec<WslConnection>,
    /// Whether to read ~/.ssh/config for ssh connection sources.
    pub read_ssh_config: bool,
}

impl RemoteSettings {
    pub fn ssh_connections(&self) -> impl Iterator<Item = SshConnection> + use<> {
        self.ssh_connections.clone().0.into_iter()
    }

    pub fn wsl_connections(&self) -> impl Iterator<Item = WslConnection> + use<> {
        self.wsl_connections.clone().0.into_iter()
    }

    pub fn fill_connection_options_from_settings(&self, options: &mut SshConnectionOptions) {
        for conn in self.ssh_connections() {
            if conn.host == options.host.to_string()
                && conn.username == options.username
                && conn.port == options.port
            {
                options.nickname = conn.nickname;
                options.upload_binary_over_ssh = conn.upload_binary_over_ssh.unwrap_or_default();
                options.args = Some(conn.args);
                options.port_forwards = conn.port_forwards;
                break;
            }
        }
    }

    pub fn connection_options_for(
        &self,
        host: String,
        port: Option<u16>,
        username: Option<String>,
    ) -> SshConnectionOptions {
        let mut options = SshConnectionOptions {
            host: host.into(),
            port,
            username,
            ..Default::default()
        };
        self.fill_connection_options_from_settings(&mut options);
        options
    }
}

#[derive(Clone, PartialEq)]
pub enum Connection {
    Ssh(SshConnection),
    Wsl(WslConnection),
    DevContainer(DevContainerConnection),
}

impl From<Connection> for RemoteConnectionOptions {
    fn from(val: Connection) -> Self {
        match val {
            Connection::Ssh(conn) => RemoteConnectionOptions::Ssh(conn.into()),
            Connection::Wsl(conn) => RemoteConnectionOptions::Wsl(conn.into()),
            Connection::DevContainer(conn) => {
                RemoteConnectionOptions::Docker(DockerConnectionOptions {
                    name: conn.name,
                    remote_user: conn.remote_user,
                    container_id: conn.container_id,
                    upload_binary_over_docker_exec: false,
                    use_podman: conn.use_podman,
                    remote_env: conn.remote_env,
                })
            }
        }
    }
}

impl From<SshConnection> for Connection {
    fn from(val: SshConnection) -> Self {
        Connection::Ssh(val)
    }
}

impl From<WslConnection> for Connection {
    fn from(val: WslConnection) -> Self {
        Connection::Wsl(val)
    }
}

impl Settings for RemoteSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let remote = &content.remote;
        Self {
            ssh_connections: remote.ssh_connections.clone().unwrap_or_default().into(),
            wsl_connections: remote.wsl_connections.clone().unwrap_or_default().into(),
            read_ssh_config: remote.read_ssh_config.unwrap(),
        }
    }
}

#[cfg(not(any(test, feature = "test-support")))]
pub async fn open_non_ssh_remote_project(
    _connection_options: RemoteConnectionOptions,
    _paths: Vec<PathBuf>,
    _app_state: Arc<AppState>,
    _open_options: workspace::OpenOptions,
    _cx: &mut AsyncApp,
) -> Result<WindowHandle<MultiWorkspace>> {
    anyhow::bail!("WSL and Dev Container project windows are not supported by Super Zed")
}

#[cfg(any(test, feature = "test-support"))]
pub async fn open_non_ssh_remote_project(
    connection_options: RemoteConnectionOptions,
    paths: Vec<PathBuf>,
    app_state: Arc<AppState>,
    mut open_options: workspace::OpenOptions,
    cx: &mut AsyncApp,
) -> Result<WindowHandle<MultiWorkspace>> {
    anyhow::ensure!(
        matches!(
            connection_options,
            RemoteConnectionOptions::Wsl(_) | RemoteConnectionOptions::Docker(_)
        ),
        "remote project orchestration is retained only for WSL and Dev Containers"
    );
    let host_window = open_options
        .requesting_window
        .or_else(|| {
            cx.update(|cx| {
                cx.windows().into_iter().find_map(|window| {
                    let window = window.downcast::<MultiWorkspace>()?;
                    window
                        .read(cx)
                        .ok()?
                        .host_session()
                        .is_some()
                        .then_some(window)
                })
            })
        })
        .context("Super Zed must connect its local host before adding an SSH host")?;
    open_options.requesting_window = Some(host_window);
    let created_new_window = false;

    let host_id = workspace::HostSessionClient::host_id_for_connection(&connection_options)?;
    let existing_host_session = host_window.update(cx, |multi_workspace, _, cx| {
        multi_workspace.host_session_for_id(&host_id, cx)
    })?;
    if let Some(host_session) = existing_host_session {
        let target = host_session.read_with(cx, |host_session, _| {
            let workspace = host_session
                .snapshot()
                .workspaces
                .iter()
                .find(|workspace| workspace.id == host_session.snapshot().active_workspace_id)
                .context("connected host has no active workspace")?;
            Ok::<_, anyhow::Error>(workspace::HostWorkspaceIdentity {
                host_id: host_session.host_id().clone(),
                workspace_id: workspace.id,
                project_id: workspace.project_id,
            })
        })?;
        let opened = cx
            .update(|cx| {
                workspace::open_paths_in_host_workspace(
                    host_session,
                    target,
                    host_window,
                    paths.clone(),
                    cx,
                )
            })
            .await?;
        let remote_connection = host_window.update(cx, |multi_workspace, _, cx| {
            multi_workspace
                .workspace()
                .read(cx)
                .project()
                .read(cx)
                .remote_client()
                .and_then(|client| client.read(cx).remote_connection())
        })?;
        if let Some(remote_connection) = remote_connection {
            let (_, positions) = determine_paths_with_positions(&remote_connection, paths).await;
            navigate_to_positions(&host_window, opened, &positions, cx);
        }
        return Ok(host_window);
    }

    let (existing, open_visible) = find_existing_workspace(
        &paths,
        &open_options,
        &SerializedWorkspaceLocation::Remote(connection_options.clone()),
        cx,
    )
    .await;

    if let Some((existing_window, existing_workspace)) = existing {
        let remote_connection = cx.update(|cx| {
            existing_workspace
                .read(cx)
                .project()
                .read(cx)
                .remote_client()
                .and_then(|client| client.read(cx).remote_connection())
        });

        if let Some(remote_connection) = remote_connection {
            let (resolved_paths, paths_with_positions) =
                determine_paths_with_positions(&remote_connection, paths).await;

            let open_results = existing_window
                .update(cx, |multi_workspace, window, cx| {
                    window.activate_window();
                    multi_workspace.activate(existing_workspace.clone(), None, window, cx);
                    existing_workspace.update(cx, |workspace, cx| {
                        workspace.open_paths(
                            resolved_paths,
                            OpenOptions {
                                visible: Some(open_visible),
                                ..Default::default()
                            },
                            None,
                            window,
                            cx,
                        )
                    })
                })?
                .await;

            _ = existing_window.update(cx, |multi_workspace, _, cx| {
                let workspace = multi_workspace.workspace().clone();
                workspace.update(cx, |workspace, cx| {
                    for item in open_results.iter().flatten() {
                        if let Err(e) = item {
                            workspace.show_error(format!("{e}"), cx);
                        }
                    }
                });
            });

            let items = open_results
                .into_iter()
                .map(|r| r.and_then(|r| r.ok()))
                .collect::<Vec<_>>();
            navigate_to_positions(&existing_window, items, &paths_with_positions, cx);

            return Ok(existing_window);
        }
        // If the remote connection is dead (e.g. server not running after failed reconnect),
        // fall through to establish a fresh connection instead of showing an error.
        log::info!(
            "existing remote workspace found but connection is dead, starting fresh connection"
        );
    }

    let window = open_options
        .requesting_window
        .context("remote projects require the sole Super Zed shell")?;
    let initial_workspace = window.update(cx, |multi_workspace, _, _| {
        multi_workspace.workspace().clone()
    })?;

    loop {
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        let delegate = window.update(cx, {
            let paths = paths.clone();
            let connection_options = connection_options.clone();
            let initial_workspace = initial_workspace.clone();
            move |_multi_workspace: &mut MultiWorkspace, window, cx| {
                window.activate_window();
                initial_workspace.update(cx, |workspace, cx| {
                    workspace.hide_modal(window, cx);
                    workspace.toggle_modal(window, cx, |window, cx| {
                        RemoteConnectionModal::new(&connection_options, paths, window, cx)
                    });

                    let ui = workspace
                        .active_modal::<RemoteConnectionModal>(cx)?
                        .read(cx)
                        .prompt
                        .clone();

                    ui.update(cx, |ui, _cx| {
                        ui.set_cancellation_tx(cancel_tx);
                    });

                    Some(Arc::new(RemoteClientDelegate::new(
                        window.window_handle(),
                        ui.downgrade(),
                        if let RemoteConnectionOptions::Ssh(options) = &connection_options {
                            options
                                .password
                                .as_deref()
                                .and_then(|pw| EncryptedPassword::try_from(pw).ok())
                        } else {
                            None
                        },
                    )))
                })
            }
        })?;

        let Some(delegate) = delegate else { break };

        let connection = remote::connect(connection_options.clone(), delegate.clone(), cx);
        let connection = select! {
            _ = cancel_rx => {
                initial_workspace.update(cx, |workspace, cx| {
                    if let Some(ui) = workspace.active_modal::<RemoteConnectionModal>(cx) {
                        ui.update(cx, |modal, cx| modal.finished(cx))
                    }
                });

                break;
            },
            result = connection.fuse() => result,
        };
        let remote_connection = match connection {
            Ok(connection) => connection,
            Err(e) => {
                initial_workspace.update(cx, |workspace, cx| {
                    if let Some(ui) = workspace.active_modal::<RemoteConnectionModal>(cx) {
                        ui.update(cx, |modal, cx| modal.finished(cx))
                    }
                });
                log::error!("Failed to open project: {e:#}");
                let response = window
                    .update(cx, |_, window, cx| {
                        window.prompt(
                            PromptLevel::Critical,
                            match connection_options {
                                RemoteConnectionOptions::Local(_) => {
                                    "Failed to connect to local Super Zed host"
                                }
                                RemoteConnectionOptions::Ssh(_) => "Failed to connect over SSH",
                                RemoteConnectionOptions::Wsl(_) => "Failed to connect to WSL",
                                RemoteConnectionOptions::Docker(_) => {
                                    "Failed to connect to Dev Container"
                                }
                                #[cfg(any(test, feature = "test-support"))]
                                RemoteConnectionOptions::Mock(_) => {
                                    "Failed to connect to mock server"
                                }
                            },
                            Some(&format!("{e:#}")),
                            &["Retry", "Cancel"],
                            cx,
                        )
                    })?
                    .await;

                if response == Ok(0) {
                    continue;
                }

                if created_new_window {
                    window
                        .update(cx, |_, window, _| window.remove_window())
                        .ok();
                }
                return Ok(window);
            }
        };

        let (_, paths_with_positions) =
            determine_paths_with_positions(&remote_connection, paths.clone()).await;

        let opened_items = cx
            .update(|cx| {
                workspace::connect_superzed_host_with_new_connection(
                    window,
                    remote_connection,
                    cancel_rx,
                    delegate.clone(),
                    app_state.clone(),
                    cx,
                )
            })
            .await
            .map(|_| Vec::new());

        initial_workspace.update(cx, |workspace, cx| {
            if let Some(ui) = workspace.active_modal::<RemoteConnectionModal>(cx) {
                ui.update(cx, |modal, cx| modal.finished(cx))
            }
        });

        match opened_items {
            Err(e) => {
                log::error!("Failed to open project: {e:#}");
                let response = window
                    .update(cx, |_, window, cx| {
                        window.prompt(
                            PromptLevel::Critical,
                            match connection_options {
                                RemoteConnectionOptions::Local(_) => {
                                    "Failed to connect to local Super Zed host"
                                }
                                RemoteConnectionOptions::Ssh(_) => "Failed to connect over SSH",
                                RemoteConnectionOptions::Wsl(_) => "Failed to connect to WSL",
                                RemoteConnectionOptions::Docker(_) => {
                                    "Failed to connect to Dev Container"
                                }
                                #[cfg(any(test, feature = "test-support"))]
                                RemoteConnectionOptions::Mock(_) => {
                                    "Failed to connect to mock server"
                                }
                            },
                            Some(&format!("{e:#}")),
                            &["Retry", "Cancel"],
                            cx,
                        )
                    })?
                    .await;
                if response == Ok(0) {
                    continue;
                }

                if created_new_window {
                    window
                        .update(cx, |_, window, _| window.remove_window())
                        .ok();
                }
                initial_workspace.update(cx, |workspace, cx| {
                    trusted_worktrees::track_worktree_trust(
                        workspace.project().read(cx).worktree_store(),
                        None,
                        None,
                        None,
                        cx,
                    );
                });
            }

            Ok(items) => {
                navigate_to_positions(&window, items, &paths_with_positions, cx);
                if created_new_window {
                    let active_workspace = window.update(cx, |multi_workspace, _, _| {
                        multi_workspace.workspace().clone()
                    })?;
                    if initial_workspace != active_workspace {
                        window
                            .update(cx, |multi_workspace, window, cx| {
                                multi_workspace.remove(
                                    [initial_workspace.clone()],
                                    move |_, _, _| Task::ready(Ok(active_workspace)),
                                    window,
                                    cx,
                                )
                            })?
                            .await?;
                    }
                }
            }
        }

        break;
    }

    // The non-SSH project flow activates the projected WSL or Dev Container workspace.
    window
        .update(cx, |multi_workspace: &mut MultiWorkspace, _, cx| {
            let workspace = multi_workspace.workspace().clone();
            workspace.update(cx, |workspace, cx| {
                if let Some(client) = workspace.project().read(cx).remote_client() {
                    if let Some(extension_store) = ExtensionStore::try_global(cx) {
                        extension_store
                            .update(cx, |store, cx| store.register_remote_client(client, cx));
                    }
                }
            });
        })
        .ok();
    Ok(window)
}

pub async fn connect_ssh_host(
    connection_options: RemoteConnectionOptions,
    app_state: Arc<AppState>,
    host_window: WindowHandle<MultiWorkspace>,
    cx: &mut AsyncApp,
) -> Result<WindowHandle<MultiWorkspace>> {
    let is_ssh_host = match &connection_options {
        RemoteConnectionOptions::Ssh(_) => true,
        #[cfg(any(test, feature = "test-support"))]
        RemoteConnectionOptions::Mock(_) => true,
        _ => false,
    };
    anyhow::ensure!(
        is_ssh_host,
        "Connect SSH Host only accepts SSH connection options"
    );
    let host_id = workspace::HostSessionClient::host_id_for_connection(&connection_options)?;
    if host_window.update(cx, |multi_workspace, _, cx| {
        multi_workspace.host_session_for_id(&host_id, cx).is_some()
    })? {
        return Ok(host_window);
    }

    let modal_workspace = host_window.update(cx, |multi_workspace, _, _| {
        multi_workspace.workspace().clone()
    })?;
    let connect = host_window.update(cx, {
        let connection_options = connection_options.clone();
        let modal_workspace = modal_workspace.clone();
        move |_, window, cx| {
            modal_workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    RemoteConnectionModal::new(&connection_options, Vec::new(), window, cx)
                });
                let modal = workspace
                    .active_modal::<RemoteConnectionModal>(cx)
                    .context("opening the SSH host connection dialog")?;
                Ok::<_, anyhow::Error>(connect(
                    ConnectionIdentifier::Host,
                    connection_options,
                    modal.read(cx).prompt.clone(),
                    window,
                    cx,
                ))
            })
        }
    })??;
    let session = connect.await?;
    modal_workspace.update(cx, |workspace, cx| {
        if let Some(modal) = workspace.active_modal::<RemoteConnectionModal>(cx) {
            modal.update(cx, |modal, cx| modal.finished(cx));
        }
    });
    let Some(session) = session else {
        return Ok(host_window);
    };

    workspace::attach_connected_superzed_host(
        host_window,
        session.clone(),
        connection_options,
        app_state,
        cx,
    )
    .await?;
    cx.update(|cx| {
        if let Some(extension_store) = ExtensionStore::try_global(cx) {
            extension_store.update(cx, |store, cx| {
                store.register_remote_client(session.clone(), cx)
            });
        }
    });
    Ok(host_window)
}

pub fn navigate_to_positions(
    window: &WindowHandle<MultiWorkspace>,
    items: impl IntoIterator<Item = Option<Box<dyn workspace::item::ItemHandle>>>,
    positions: &[PathWithPosition],
    cx: &mut AsyncApp,
) {
    for (item, path) in items.into_iter().zip(positions) {
        let Some(item) = item else {
            continue;
        };
        let Some(row) = path.row else {
            continue;
        };
        if let Some(active_editor) = item.downcast::<Editor>() {
            window
                .update(cx, |_, window, cx| {
                    active_editor.update(cx, |editor, cx| {
                        let row = row.saturating_sub(1);
                        let col = path.column.unwrap_or(0).saturating_sub(1);
                        let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
                            return;
                        };
                        let buffer_snapshot = buffer.read(cx).snapshot();
                        let point = buffer_snapshot.point_from_external_input(row, col);
                        editor.go_to_singleton_buffer_point(point, window, cx);
                    });
                })
                .ok();
        }
    }
}

pub(crate) async fn determine_paths_with_positions(
    remote_connection: &Arc<dyn RemoteConnection>,
    mut paths: Vec<PathBuf>,
) -> (Vec<PathBuf>, Vec<PathWithPosition>) {
    let mut paths_with_positions = Vec::<PathWithPosition>::new();
    for path in &mut paths {
        if let Some(path_str) = path.to_str() {
            let path_with_position = PathWithPosition::parse_str(&path_str);
            if path_with_position.row.is_some() {
                if !path_exists(&remote_connection, &path).await {
                    *path = path_with_position.path.clone();
                    paths_with_positions.push(path_with_position);
                    continue;
                }
            }
        }
        paths_with_positions.push(PathWithPosition::from_path(path.clone()))
    }
    (paths, paths_with_positions)
}

async fn path_exists(connection: &Arc<dyn RemoteConnection>, path: &Path) -> bool {
    let Ok(command) = connection.build_command(
        Some("test".to_string()),
        &["-e".to_owned(), path.to_string_lossy().to_string()],
        &Default::default(),
        None,
        None,
        Interactive::No,
    ) else {
        return false;
    };
    let Ok(mut child) = util::command::new_command(command.program)
        .args(command.args)
        .envs(command.env)
        .spawn()
    else {
        return false;
    };
    child.status().await.is_ok_and(|status| status.success())
}
