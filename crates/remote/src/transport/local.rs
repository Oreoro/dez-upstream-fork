use crate::{
    RemoteArch, RemoteClientDelegate, RemoteOs, RemotePlatform,
    remote_client::{CommandTemplate, Interactive, RemoteConnection, RemoteConnectionOptions},
};
use anyhow::{Context as _, Result, anyhow, bail};
use askpass::EncryptedPassword;
use async_trait::async_trait;
use collections::HashMap;
use fs::{Fs, copy_recursive};
use futures::channel::mpsc::{Sender, UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot;
use gpui::{App, AppContext as _, AsyncApp, Task};
use release_channel::ReleaseChannel;
use rpc::proto::Envelope;
use semver::Version;
use std::{
    fmt::Write as _,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};
use util::{
    command::Stdio,
    paths::{PathStyle, RemotePathBuf},
    shell::{ShellKind, get_default_system_shell, get_system_shell},
};

pub const EMBEDDED_REMOTE_SERVER_ENV: &str = "SUPERZED_EMBEDDED_REMOTE_SERVER";
pub const EMBEDDED_REMOTE_SERVER_DATA_DIR_ENV: &str = "SUPERZED_EMBEDDED_REMOTE_SERVER_DATA_DIR";

#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct LocalConnectionOptions;

pub struct LocalRemoteClientDelegate;

impl RemoteClientDelegate for LocalRemoteClientDelegate {
    fn ask_password(
        &self,
        _prompt: String,
        _tx: oneshot::Sender<EncryptedPassword>,
        _cx: &mut AsyncApp,
    ) {
    }

    fn get_download_url(
        &self,
        _platform: RemotePlatform,
        _release_channel: ReleaseChannel,
        _version: Option<Version>,
        _cx: &mut AsyncApp,
    ) -> Task<Result<Option<String>>> {
        Task::ready(Err(anyhow!(
            "the local Super Zed host uses the current executable"
        )))
    }

    fn download_server_binary_locally(
        &self,
        _platform: RemotePlatform,
        _release_channel: ReleaseChannel,
        _version: Option<Version>,
        _cx: &mut AsyncApp,
    ) -> Task<Result<PathBuf>> {
        Task::ready(Err(anyhow!(
            "the local Super Zed host uses the current executable"
        )))
    }

    fn set_status(&self, _status: Option<&str>, _cx: &mut AsyncApp) {}
}

pub(crate) struct LocalRemoteConnection {
    killed: AtomicBool,
    shell: String,
    default_system_shell: String,
}

impl LocalRemoteConnection {
    pub(crate) fn new() -> Self {
        Self {
            killed: AtomicBool::new(false),
            shell: get_system_shell(),
            default_system_shell: get_default_system_shell(),
        }
    }
}

#[async_trait(?Send)]
impl RemoteConnection for LocalRemoteConnection {
    fn start_proxy(
        &self,
        unique_identifier: String,
        reconnect: bool,
        incoming_tx: UnboundedSender<Envelope>,
        outgoing_rx: UnboundedReceiver<Envelope>,
        connection_activity_tx: Sender<()>,
        delegate: std::sync::Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Task<Result<i32>> {
        delegate.set_status(Some("Starting local host server"), cx);
        let executable = match std::env::current_exe() {
            Ok(executable) => executable,
            Err(error) => return Task::ready(Err(error).context("locating Super Zed executable")),
        };
        let mut command = util::command::new_command(executable);
        command
            .env(EMBEDDED_REMOTE_SERVER_ENV, "1")
            .arg("proxy")
            .arg("--identifier")
            .arg(unique_identifier)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(data_dir) = paths::custom_data_dir() {
            command.env(EMBEDDED_REMOTE_SERVER_DATA_DIR_ENV, data_dir);
        }
        if reconnect {
            command.arg("--reconnect");
        }
        let process = match command.spawn() {
            Ok(process) => process,
            Err(error) => {
                return Task::ready(
                    Err(error).context("starting the embedded Super Zed host server"),
                );
            }
        };
        super::handle_rpc_messages_over_child_process_stdio(
            process,
            incoming_tx,
            outgoing_rx,
            connection_activity_tx,
            cx,
        )
    }

    fn upload_directory(
        &self,
        source: PathBuf,
        destination: RemotePathBuf,
        cx: &App,
    ) -> Task<Result<()>> {
        let fs = <dyn Fs>::global(cx);
        let destination = PathBuf::from(destination.to_string());
        cx.background_spawn(async move {
            copy_recursive(fs.as_ref(), &source, &destination, Default::default()).await
        })
    }

    async fn kill(&self) -> Result<()> {
        self.killed.store(true, Ordering::Release);
        Ok(())
    }

    fn has_been_killed(&self) -> bool {
        self.killed.load(Ordering::Acquire)
    }

    fn shares_network_interface(&self) -> bool {
        true
    }

    fn build_command(
        &self,
        program: Option<String>,
        args: &[String],
        env: &HashMap<String, String>,
        working_dir: Option<String>,
        port_forward: Option<(u16, String, u16)>,
        _interactive: Interactive,
    ) -> Result<CommandTemplate> {
        if port_forward.is_some() {
            bail!("the local host already shares Super Zed's network interface");
        }

        let shell_kind = ShellKind::new(&self.shell, cfg!(windows));
        let mut command = String::new();
        if let Some(working_dir) = working_dir {
            let working_dir = shell_kind
                .try_quote(&working_dir)
                .ok_or_else(|| anyhow!("cannot quote local working directory"))?;
            write!(command, "cd {working_dir} && ")?;
        }
        command.push_str("exec env");
        for (key, value) in env {
            let assignment = format!("{key}={value}");
            let assignment = shell_kind
                .try_quote(&assignment)
                .ok_or_else(|| anyhow!("cannot quote local environment variable"))?;
            write!(command, " {assignment}")?;
        }
        if let Some(program) = program {
            let program = shell_kind
                .try_quote_prefix_aware(&program)
                .ok_or_else(|| anyhow!("cannot quote local command"))?;
            write!(command, " {program}")?;
            for argument in args {
                let argument = shell_kind
                    .try_quote(argument)
                    .ok_or_else(|| anyhow!("cannot quote local command argument"))?;
                write!(command, " {argument}")?;
            }
        } else {
            let shell = shell_kind
                .try_quote(&self.shell)
                .ok_or_else(|| anyhow!("cannot quote local shell"))?;
            write!(command, " {shell} -l")?;
        }

        Ok(CommandTemplate {
            program: self.default_system_shell.clone(),
            args: if cfg!(windows) {
                vec!["/C".to_owned(), command]
            } else {
                vec!["-c".to_owned(), command]
            },
            env: HashMap::default(),
        })
    }

    fn build_forward_ports_command(
        &self,
        _forwards: Vec<(u16, String, u16)>,
    ) -> Result<CommandTemplate> {
        bail!("the local host already shares Super Zed's network interface")
    }

    fn connection_options(&self) -> RemoteConnectionOptions {
        RemoteConnectionOptions::Local(LocalConnectionOptions)
    }

    fn path_style(&self) -> PathStyle {
        if cfg!(windows) {
            PathStyle::Windows
        } else {
            PathStyle::Posix
        }
    }

    fn remote_platform(&self) -> RemotePlatform {
        RemotePlatform {
            os: if cfg!(target_os = "windows") {
                RemoteOs::Windows
            } else if cfg!(target_os = "macos") {
                RemoteOs::MacOs
            } else {
                RemoteOs::Linux
            },
            arch: if cfg!(target_arch = "aarch64") {
                RemoteArch::Aarch64
            } else {
                RemoteArch::X86_64
            },
        }
    }

    fn remote_os_version(&self) -> Option<String> {
        None
    }

    fn shell(&self) -> String {
        self.shell.clone()
    }

    fn default_system_shell(&self) -> String {
        self.default_system_shell.clone()
    }

    fn has_wsl_interop(&self) -> bool {
        false
    }
}
