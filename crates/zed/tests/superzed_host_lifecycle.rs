#![cfg(unix)]

use std::{
    io::Read as _,
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use client::proto::{Envelope, envelope};
use prost::Message as _;

const IDENTIFIER: &str = "dev-superzed-test-current";
const PREVIOUS_IDENTIFIER: &str = "dev-superzed-test-previous";
const READY_TIMEOUT: Duration = Duration::from_secs(15);

// This process lifecycle test runs outside GPUI and deliberately controls real child processes.
#[allow(clippy::disallowed_methods)]
fn spawn_proxy(data_dir: &Path) -> Child {
    spawn_proxy_for_identifier(data_dir, IDENTIFIER, None)
}

#[allow(clippy::disallowed_methods)]
fn spawn_proxy_with_idle_timeout(data_dir: &Path, idle_timeout: Option<Duration>) -> Child {
    spawn_proxy_for_identifier(data_dir, IDENTIFIER, idle_timeout)
}

#[allow(clippy::disallowed_methods)]
fn spawn_proxy_for_identifier(
    data_dir: &Path,
    identifier: &str,
    idle_timeout: Option<Duration>,
) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_superzed"));
    command
        .env("SUPERZED_EMBEDDED_REMOTE_SERVER", "1")
        .env("SUPERZED_EMBEDDED_REMOTE_SERVER_DATA_DIR", data_dir)
        .args(["proxy", "--identifier", identifier])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(idle_timeout) = idle_timeout {
        command.env(
            "SUPERZED_TEST_DEV_SERVER_IDLE_TIMEOUT_MS",
            idle_timeout.as_millis().to_string(),
        );
    }
    command.spawn().expect("spawn Super Zed host proxy")
}

fn remote_started_result(child: &mut Child) -> Result<bool, String> {
    let mut stdout = child.stdout.take().expect("proxy stdout");
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = (|| {
            let mut length = [0; size_of::<u32>()];
            stdout.read_exact(&mut length)?;
            let length = u32::from_le_bytes(length) as usize;
            let mut message = vec![0; length];
            stdout.read_exact(&mut message)?;
            let envelope = Envelope::decode(message.as_slice()).map_err(std::io::Error::other)?;
            match envelope.payload {
                Some(envelope::Payload::RemoteStarted(_)) => Ok(true),
                Some(envelope::Payload::Error(error)) => Err(std::io::Error::other(error.message)),
                _ => Ok(false),
            }
        })();
        let connected = result.as_ref().is_ok_and(|started| *started);
        sender.send(result).ok();
        if connected {
            std::io::copy(&mut stdout, &mut std::io::sink()).ok();
        }
    });

    receiver
        .recv_timeout(READY_TIMEOUT)
        .map_err(|error| format!("host handshake timed out: {error}"))?
        .map_err(|error| format!("read host handshake: {error}"))
}

fn read_remote_started(child: &mut Child) {
    assert_eq!(
        remote_started_result(child),
        Ok(true),
        "first host frame was not RemoteStarted"
    );
}

fn wait_for_pid(path: &Path) -> u32 {
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(contents) = std::fs::read_to_string(path)
            && let Ok(pid) = contents.parse()
        {
            return pid;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("host PID file was not ready: {path:?}");
}

#[allow(clippy::disallowed_methods)]
fn process_is_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn stop_child(child: &mut Child) {
    child.kill().expect("stop host proxy");
    child.wait().expect("wait for host proxy");
}

fn detach_client(child: &mut Child) {
    drop(child.stdin.take());
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("inspect detached host proxy") {
            assert!(status.success(), "detached host proxy exited with {status}");
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    stop_child(child);
    panic!("detached host proxy did not exit after its input closed");
}

#[allow(clippy::disallowed_methods)]
fn stop_daemon(pid: u32) {
    assert!(process_is_alive(pid), "host daemon {pid} is not alive");
    let status = Command::new("kill")
        .arg(pid.to_string())
        .status()
        .expect("stop host daemon");
    assert!(status.success(), "failed to stop host daemon {pid}");
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if !process_is_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("host daemon {pid} did not stop");
}

fn wait_for_process_exit(pid: u32) {
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if !process_is_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("host daemon {pid} did not exit");
}

#[test]
fn detached_host_spawns_recovers_stale_state_and_rejects_a_second_simultaneous_client() {
    let unique_suffix = uuid::Uuid::new_v4()
        .as_simple()
        .to_string()
        .chars()
        .take(8)
        .collect::<String>();
    let data_dir =
        PathBuf::from("/tmp").join(format!("sz-{}-{unique_suffix}", std::process::id(),));
    let server_state_dir = data_dir.join("server_state");
    let host_dir = server_state_dir.join(IDENTIFIER);
    std::fs::create_dir_all(&host_dir).expect("create host test directory");
    let pid_file = host_dir.join("server.pid");
    let build_identity_file = host_dir.join("server-build-id");
    let socket_paths = [
        host_dir.join("stdin.sock"),
        host_dir.join("stdout.sock"),
        host_dir.join("stderr.sock"),
    ];

    for socket_path in &socket_paths {
        drop(UnixListener::bind(socket_path).expect("create stale host socket"));
    }
    std::fs::write(&pid_file, std::process::id().to_string()).expect("write stale live PID");

    let database_path = server_state_dir.join("superzed-session.sqlite3");
    let database =
        superzed_session::HostSessionDb::open(&database_path).expect("open host session database");
    let mut expected_snapshot = superzed_session::HostSessionSnapshot::default();
    let busy_project = data_dir.join("busy-project");
    std::fs::create_dir_all(&busy_project).expect("create busy project fixture");
    for index in 0..256 {
        std::fs::write(busy_project.join(format!("file-{index}.txt")), "fixture")
            .expect("write busy project fixture");
    }
    expected_snapshot
        .apply(superzed_session::MutationRequest {
            expected_revision: expected_snapshot.revision,
            mutation: superzed_session::SessionMutation::SetWorkspaceProjectRoots {
                workspace_id: expected_snapshot.active_workspace_id,
                project_spec: superzed_session::ProjectSpec {
                    roots: vec![superzed_session::ProjectRoot {
                        requested_path: busy_project.clone(),
                        canonical_path: busy_project,
                    }],
                },
            },
        })
        .expect("set busy persisted project");
    for _ in 0..2 {
        expected_snapshot
            .apply(superzed_session::MutationRequest {
                expected_revision: expected_snapshot.revision,
                mutation: superzed_session::SessionMutation::CreateWorkspace {
                    after: Some(expected_snapshot.active_workspace_id),
                    project_spec: superzed_session::ProjectSpec::default(),
                },
            })
            .expect("add persisted workspace");
    }
    database
        .save(&expected_snapshot)
        .expect("save host session fixture");
    drop(database);

    let mut first_client = spawn_proxy(&data_dir);
    read_remote_started(&mut first_client);
    let first_host_pid = wait_for_pid(&pid_file);
    assert_ne!(first_host_pid, std::process::id());
    assert!(
        process_is_alive(std::process::id()),
        "stale PID recovery killed the unrelated test process"
    );

    let mut rejected_client = spawn_proxy(&data_dir);
    let rejection = remote_started_result(&mut rejected_client);
    assert!(
        rejection
            .as_ref()
            .is_err_and(|error| error.contains("another Super Zed GUI is already attached")),
        "a second simultaneous GUI client was not rejected clearly: {rejection:?}"
    );
    rejected_client
        .wait()
        .expect("wait for rejected second host proxy");
    assert!(
        first_client
            .try_wait()
            .expect("inspect first proxy")
            .is_none(),
        "rejecting the second GUI disturbed the first client"
    );
    assert!(
        process_is_alive(first_host_pid),
        "rejecting the second GUI stopped the persistent host"
    );

    detach_client(&mut first_client);
    assert!(
        process_is_alive(first_host_pid),
        "detached host died with its first client"
    );

    let mut second_client = spawn_proxy(&data_dir);
    read_remote_started(&mut second_client);
    assert_eq!(
        wait_for_pid(&pid_file),
        first_host_pid,
        "second client spawned another host instead of attaching"
    );
    detach_client(&mut second_client);

    std::fs::write(&build_identity_file, "an-incompatible-development-build")
        .expect("replace host build identity fixture");
    let mut replacement_client = spawn_proxy(&data_dir);
    read_remote_started(&mut replacement_client);
    let replacement_host_pid = wait_for_pid(&pid_file);
    assert_ne!(
        replacement_host_pid, first_host_pid,
        "a new development build reused the old in-memory host"
    );
    wait_for_process_exit(first_host_pid);
    detach_client(&mut replacement_client);

    let previous_pid_file = server_state_dir
        .join(PREVIOUS_IDENTIFIER)
        .join("server.pid");
    let mut previous_client = spawn_proxy_for_identifier(&data_dir, PREVIOUS_IDENTIFIER, None);
    read_remote_started(&mut previous_client);
    let previous_host_pid = wait_for_pid(&previous_pid_file);
    wait_for_process_exit(replacement_host_pid);
    detach_client(&mut previous_client);

    let mut current_client = spawn_proxy(&data_dir);
    read_remote_started(&mut current_client);
    let current_host_pid = wait_for_pid(&pid_file);
    wait_for_process_exit(previous_host_pid);
    detach_client(&mut current_client);

    stop_daemon(current_host_pid);
    let mut restarted_client = spawn_proxy(&data_dir);
    read_remote_started(&mut restarted_client);
    let restarted_host_pid = wait_for_pid(&pid_file);
    assert_ne!(restarted_host_pid, first_host_pid);
    stop_child(&mut restarted_client);
    stop_daemon(restarted_host_pid);

    let restored_snapshot = superzed_session::HostSessionDb::open(&database_path)
        .expect("reopen host session database")
        .load()
        .expect("load host session after restart")
        .expect("persisted host session exists");
    assert_eq!(restored_snapshot, expected_snapshot);

    let accepting_log_count = std::fs::read_dir(data_dir.join("logs"))
        .expect("read host lifecycle logs")
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .map(|log| log.matches("accepting new connections").count())
        .sum::<usize>();
    assert!(
        accepting_log_count <= 20,
        "a detached host logged one accept cycle per queued RPC message: {accepting_log_count}"
    );

    std::fs::remove_dir_all(data_dir).expect("remove host lifecycle test directory");
}

#[test]
fn disconnected_development_host_exits_after_its_idle_timeout() {
    let unique_suffix = uuid::Uuid::new_v4()
        .as_simple()
        .to_string()
        .chars()
        .take(8)
        .collect::<String>();
    let data_dir =
        PathBuf::from("/tmp").join(format!("sz-idle-{}-{unique_suffix}", std::process::id()));
    let pid_file = data_dir
        .join("server_state")
        .join(IDENTIFIER)
        .join("server.pid");

    let mut client = spawn_proxy_with_idle_timeout(&data_dir, Some(Duration::from_millis(200)));
    read_remote_started(&mut client);
    let host_pid = wait_for_pid(&pid_file);
    detach_client(&mut client);
    wait_for_process_exit(host_pid);

    std::fs::remove_dir_all(data_dir).expect("remove idle-timeout test directory");
}
