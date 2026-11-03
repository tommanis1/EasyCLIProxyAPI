use super::*;

pub(crate) static CORE_OPERATION_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn lock_core_operation(
    process_state: &CoreProcessState,
) -> Result<std::sync::MutexGuard<'static, ()>, String> {
    let guard = CORE_OPERATION_LOCK
        .try_lock()
        .map_err(|_| "The kernel is performing another operation. Try again later".to_string())?;
    process_state.ensure_active()?;
    Ok(guard)
}

pub(crate) struct CoreChild {
    child: Child,
    #[cfg(windows)]
    job: isize,
}

impl std::ops::Deref for CoreChild {
    type Target = Child;

    fn deref(&self) -> &Child {
        &self.child
    }
}

impl std::ops::DerefMut for CoreChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

impl Drop for CoreChild {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            close_windows_handle(self.job);
            let _ = self.child.wait();
        }
        #[cfg(not(windows))]
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = terminate_child(&mut self.child);
        }
    }
}

#[cfg(any(target_os = "linux", test))]
struct CoreSpawnRequest {
    command: Command,
    reply: std::sync::mpsc::SyncSender<io::Result<Child>>,
}

#[cfg(any(target_os = "linux", test))]
static CORE_PROCESS_SPAWNER: LazyLock<Result<std::sync::mpsc::Sender<CoreSpawnRequest>, String>> =
    LazyLock::new(|| {
        let (sender, receiver) = std::sync::mpsc::channel::<CoreSpawnRequest>();
        thread::Builder::new()
            .name("cpa-core-spawner".to_string())
            .spawn(move || {
                for mut request in receiver {
                    configure_child_lifetime(&mut request.command);
                    let result = request.command.spawn();
                    if let Err(std::sync::mpsc::SendError(Ok(mut child))) =
                        request.reply.send(result)
                    {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                }
            })
            .map_err(|error| format!("Failed to create CPA kernel startup thread: {error}"))?;
        Ok(sender)
    });

pub(crate) fn spawn_core_child(command: Command) -> Result<CoreChild, String> {
    #[cfg(target_os = "linux")]
    let child = spawn_core_child_on_lifetime_thread(command)?;

    #[cfg(not(target_os = "linux"))]
    let child = {
        let mut command = command;
        command
            .spawn()
            .map_err(|error| format!("Failed to start CPA kernel: {error}"))?
    };

    #[cfg(windows)]
    let job = match attach_child_to_windows_job(&child) {
        Ok(job) => job,
        Err(error) => {
            let mut child = child;
            return match terminate_child(&mut child) {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(format!(
                    "{error}; failed to clean up unmanaged kernel process: {cleanup_error}"
                )),
            };
        }
    };
    Ok(CoreChild {
        child,
        #[cfg(windows)]
        job,
    })
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn spawn_core_child_on_lifetime_thread(command: Command) -> Result<Child, String> {
    let sender = CORE_PROCESS_SPAWNER.as_ref().map_err(Clone::clone)?;
    let (reply, result) = std::sync::mpsc::sync_channel(1);
    sender
        .send(CoreSpawnRequest { command, reply })
        .map_err(|_| "CPA kernel startup thread exited".to_string())?;
    result
        .recv()
        .map_err(|_| "CPA kernel startup thread did not return a startup result".to_string())?
        .map_err(|error| format!("Failed to start CPA kernel: {error}"))
}

async fn run_core_command(
    app: tauri::AppHandle,
    operation: fn(&CoreProcessState, &GuiConfigState) -> Result<CoreStatus, String>,
) -> Result<CoreStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = lock_core_operation(app.state::<CoreProcessState>().inner())?;
        let status = operation(
            app.state::<CoreProcessState>().inner(),
            app.state::<GuiConfigState>().inner(),
        )?;
        emit_core_status(&app, &status);
        Ok(status)
    })
    .await
    .map_err(|error| format!("Kernel background task failed: {error}"))?
}

#[tauri::command]
pub(crate) async fn start_core_process(app: tauri::AppHandle) -> Result<CoreStatus, String> {
    run_core_command(app, start_core_process_with_state).await
}

pub(crate) fn start_core_process_with_state(
    process_state: &CoreProcessState,
    gui_config_state: &GuiConfigState,
) -> Result<CoreStatus, String> {
    network_proxy::refresh(gui_config_state)?;
    let config = gui_config_state.snapshot()?;
    process_state.recovery.cancel();
    start_core_process_inner(process_state, &config)?;
    if let Err(error) = gui_config_state.set_run_on_startup(true) {
        let _ = stop_core_process_inner(process_state);
        return Err(error);
    }
    current_core_status(Some(process_state), Some(config.port))
}

#[tauri::command]
pub(crate) async fn stop_core_process(app: tauri::AppHandle) -> Result<CoreStatus, String> {
    run_core_command(app, stop_core_process_with_state).await
}

pub(crate) fn stop_core_process_with_state(
    process_state: &CoreProcessState,
    gui_config_state: &GuiConfigState,
) -> Result<CoreStatus, String> {
    stop_core_process_inner(process_state)?;
    let config = gui_config_state.set_run_on_startup(false)?;
    current_core_status(Some(process_state), Some(config.port))
}

#[tauri::command]
pub(crate) async fn restart_core_process(app: tauri::AppHandle) -> Result<CoreStatus, String> {
    run_core_command(app, restart_core_process_with_state).await
}

pub(crate) fn restart_core_process_with_state(
    process_state: &CoreProcessState,
    gui_config_state: &GuiConfigState,
) -> Result<CoreStatus, String> {
    network_proxy::refresh(gui_config_state)?;
    let config = gui_config_state.snapshot()?;
    process_state.recovery.cancel();
    if current_core_status(Some(process_state), None)?.running {
        stop_core_process_inner(process_state)?;
    }
    start_core_process_inner(process_state, &config)?;
    if let Err(error) = gui_config_state.set_run_on_startup(true) {
        let _ = stop_core_process_inner(process_state);
        return Err(error);
    }
    current_core_status(Some(process_state), Some(config.port))
}

pub(crate) fn apply_configured_proxy(
    builder: reqwest::ClientBuilder,
    proxy_url: &str,
) -> Result<reqwest::ClientBuilder, String> {
    let proxy_url = proxy_url.trim();
    let builder = builder.no_proxy();
    if proxy_url.is_empty() {
        return Ok(builder);
    }
    let proxy = reqwest::Proxy::all(proxy_url)
        .map_err(|_| "Invalid proxy URL".to_string())?
        .no_proxy(reqwest::NoProxy::from_string("localhost,127.0.0.1,::1"));
    Ok(builder.proxy(proxy))
}

pub(crate) fn build_http_client_with_proxy(
    builder: reqwest::ClientBuilder,
    proxy_url: &str,
    error_prefix: &str,
) -> Result<reqwest::Client, String> {
    apply_configured_proxy(builder, proxy_url)
        .map_err(|error| format!("{error_prefix}: {error}"))?
        .build()
        .map_err(|error| format!("{error_prefix}: {error}"))
}

pub(crate) fn current_core_platform() -> Result<CorePlatform, String> {
    let os = env::consts::OS;
    let arch = env::consts::ARCH;

    let (asset_os, archive_kind) = match os {
        "linux" => ("linux", "tar.gz"),
        "macos" => ("darwin", "tar.gz"),
        "windows" => ("windows", "zip"),
        other => return Err(format!("Unsupported operating system: {other}")),
    };

    let asset_arch = match arch {
        "x86_64" => "amd64",
        "aarch64" => "aarch64",
        other => return Err(format!("Unsupported CPU architecture: {other}")),
    };

    Ok(CorePlatform {
        os: os.to_string(),
        arch: arch.to_string(),
        asset_os: asset_os.to_string(),
        asset_arch: asset_arch.to_string(),
        archive_kind: archive_kind.to_string(),
    })
}

pub(crate) fn current_core_status(
    process_state: Option<&CoreProcessState>,
    management_port: Option<u16>,
) -> Result<CoreStatus, String> {
    let install_dir = core_install_dir()?;
    let binary_path = find_core_binary(&install_dir);
    let installed = binary_path.is_some();
    let starting = process_state.is_some_and(CoreProcessState::is_starting);
    let managed_pid = process_state.and_then(|state| state.managed_pid());
    let management_port_open = management_port.map(is_management_port_open);
    let process_id = match managed_pid {
        Some(process_id) => Some(process_id),
        None if management_port_open.unwrap_or(true) => binary_path
            .as_ref()
            .and_then(|path| find_core_process_ids(path).first().copied()),
        None => None,
    };
    // A tracked process remains running even if a single management-port probe
    // times out. The port probe is still required before discovering an
    // untracked process, but using it to override a known live PID makes the UI
    // oscillate between running and stopped while continuing to show that PID.
    let running = process_id.is_some();
    let ready = running && management_port_open.unwrap_or(true);
    let current_version = read_core_metadata(&install_dir).map(|metadata| metadata.version);

    let message = if starting {
        "CPA kernel is starting".to_string()
    } else if !installed {
        "CPA kernel is not installed. Install the latest version first".to_string()
    } else if running {
        "CPA kernel is running".to_string()
    } else {
        "CPA kernel is installed but not running".to_string()
    };

    Ok(CoreStatus {
        installed,
        running,
        ready,
        starting,
        recovery: process_state.and_then(|state| state.recovery.status()),
        managed: managed_pid.is_some(),
        process_id,
        current_version,
        install_dir: path_to_string(&install_dir),
        binary_path: binary_path.map(|path| path_to_string(&path)),
        message,
    })
}

pub(crate) fn is_management_port_open(port: u16) -> bool {
    let listen_host = read_installed_core_config_settings()
        .map(|settings| settings.host)
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    let Ok(address) = core_management_address(&listen_host, port) else {
        return false;
    };
    for attempt in 0..3 {
        if TcpStream::connect_timeout(&address, Duration::from_millis(150)).is_ok() {
            return true;
        }
        if attempt < 2 {
            thread::sleep(Duration::from_millis(50));
        }
    }
    false
}

fn core_management_address(listen_host: &str, port: u16) -> Result<SocketAddr, String> {
    let host = core_connect_host(listen_host);
    let ip = host
        .parse::<IpAddr>()
        .map_err(|_| format!("Invalid core listen IP: {listen_host}"))?;
    Ok(SocketAddr::new(ip, port))
}

pub(crate) enum CoreStartupFailure {
    ShuttingDown,
    Exited(std::process::ExitStatus),
    Spawn(String),
    StatusCheck(io::Error),
    TimedOut(u16),
}

impl CoreStartupFailure {
    fn child_has_exited(&self) -> bool {
        matches!(self, Self::Exited(_))
    }

    #[cfg(target_os = "macos")]
    fn was_killed_by_sigkill(&self) -> bool {
        use std::os::unix::process::ExitStatusExt;

        matches!(
            self,
            Self::Exited(status)
                if status.code().is_none() && status.signal() == Some(libc::SIGKILL)
        )
    }
}

impl std::fmt::Display for CoreStartupFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ShuttingDown => formatter.write_str("Application is exiting; kernel startup canceled"),
            Self::Exited(status) => write!(formatter, "CPA kernel exited immediately after startup: {status}"),
            Self::Spawn(error) => formatter.write_str(error),
            Self::StatusCheck(error) => write!(formatter, "Failed to check CPA kernel startup status: {error}"),
            Self::TimedOut(port) => {
                write!(formatter, "CPA kernel startup timed out: management port {port} was not listening within 10 seconds")
            }
        }
    }
}

pub(crate) fn core_start_log_path(install_dir: &Path, auth_dir: &str) -> Result<PathBuf, String> {
    Ok(core_logs_dir_path(auth_dir, install_dir)?.join("core-start-output.log"))
}

pub(crate) fn core_start_stdio(log_path: &Path) -> io::Result<(Stdio, Stdio)> {
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut header_file = File::options()
        .append(true)
        .create(true)
        .open(log_path)?;
    writeln!(header_file, "\n===== CPA kernel startup {} =====", unix_now())?;
    drop(header_file);

    let stdout_file = File::options().append(true).open(log_path)?;
    let stderr_file = File::options().append(true).create(true).open(log_path)?;

    Ok((Stdio::from(stdout_file), Stdio::from(stderr_file)))
}

fn core_start_error_with_log(error: &str, log_path: &Path, log_error: Option<&str>) -> String {
    match log_error {
        Some(log_error) => format!(
            "{error}; failed to write startup log {}: {log_error}",
            path_to_string(log_path)
        ),
        None => format!("{error}; startup log: {}", path_to_string(log_path)),
    }
}

struct CoreStartAttemptFailure {
    failure: CoreStartupFailure,
    log_error: Option<String>,
}

impl CoreStartAttemptFailure {
    #[cfg(target_os = "macos")]
    fn was_killed_by_sigkill(&self) -> bool {
        self.failure.was_killed_by_sigkill()
    }

    fn message(&self, log_path: &Path) -> String {
        self.message_with_detail(log_path, None)
    }

    fn message_with_detail(&self, log_path: &Path, detail: Option<&str>) -> String {
        let error = match detail {
            Some(detail) => format!("{}；{detail}", self.failure),
            None => self.failure.to_string(),
        };
        core_start_error_with_log(&error, log_path, self.log_error.as_deref())
    }
}

fn start_core_process_once(
    process_state: &CoreProcessState,
    binary_path: &Path,
    config_path: &str,
    install_dir: &Path,
    log_path: &Path,
    management_address: SocketAddr,
) -> Result<CoreChild, CoreStartAttemptFailure> {
    let mut command = Command::new(binary_path);
    command
        .args(["-config", config_path])
        .current_dir(install_dir)
        .stdin(Stdio::null());
    let log_error = match core_start_stdio(log_path) {
        Ok((stdout, stderr)) => {
            command.stdout(stdout).stderr(stderr);
            None
        }
        Err(error) => {
            command.stdout(Stdio::null()).stderr(Stdio::null());
            Some(error.to_string())
        }
    };
    configure_background_command(&mut command);
    for variable in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env_remove(variable);
    }

    let mut child = match spawn_core_child(command) {
        Ok(child) => child,
        Err(error) => {
            return Err(CoreStartAttemptFailure {
                failure: CoreStartupFailure::Spawn(error),
                log_error,
            });
        }
    };
    match wait_for_core_management_port(&mut child, management_address, process_state) {
        Ok(()) => Ok(child),
        Err(failure) => {
            if !failure.child_has_exited() {
                let _ = terminate_child(&mut child);
            }
            Err(CoreStartAttemptFailure { failure, log_error })
        }
    }
}

pub(crate) fn start_core_process_inner(
    process_state: &CoreProcessState,
    gui_config: &GuiConfigFile,
) -> Result<(), String> {
    process_state.ensure_active()?;
    let mut resolved_config = gui_config.clone();
    resolved_config.proxy_url = network_proxy::resolve(gui_config);
    let gui_config = &resolved_config;
    let install_dir = core_install_dir()?;
    if !gui_config.auth_dir.trim().is_empty() {
        let auth_dir = auth_dir_path_for_core(&gui_config.auth_dir, &install_dir)?;
        fs::create_dir_all(&auth_dir)
            .map_err(|error| format!("Failed to create credentials directory {}: {error}", path_to_string(&auth_dir)))?;
    }
    let binary_path = find_core_binary(&install_dir)
        .ok_or_else(|| "CPA kernel is not installed. Install the latest version first".to_string())?;

    let existing_process_ids = find_core_process_ids(&binary_path);
    if process_state.managed_pid().is_some() || !existing_process_ids.is_empty() {
        if !existing_process_ids.is_empty() {
            process_state.adopt_process_ids(&binary_path, existing_process_ids)?;
        }
        return Err("CPA kernel is already running".to_string());
    }
    let management_address = core_management_address(&gui_config.host, gui_config.port)?;
    if TcpStream::connect_timeout(&management_address, Duration::from_millis(250)).is_ok() {
        return Err(format!(
            "Port {} is in use by another program. Change the port and try again",
            gui_config.port
        ));
    }

    let config_path = merge_core_config_for_start(&install_dir, gui_config)?;
    let config_path = path_to_string(&config_path);
    let log_path = core_start_log_path(&install_dir, &gui_config.auth_dir)?;
    let start_once = || {
        start_core_process_once(
            process_state,
            &binary_path,
            &config_path,
            &install_dir,
            &log_path,
            management_address,
        )
    };

    #[cfg(target_os = "macos")]
    let child = match start_once() {
        Ok(child) => child,
        Err(failure) if failure.was_killed_by_sigkill() => {
            if let Err(heal_error) = rematerialize_core_binary(&binary_path) {
                return Err(failure.message_with_detail(
                    &log_path,
                    Some(&format!("Failed to automatically repair CPA kernel files: {heal_error}")),
                ));
            }
            match start_once() {
                Ok(child) => child,
                Err(failure) if failure.was_killed_by_sigkill() => {
                    return Err(failure.message_with_detail(
                        &log_path,
                        Some("The system terminated the CPA kernel again. Reinstall the kernel and try again"),
                    ));
                }
                Err(failure) => return Err(failure.message(&log_path)),
            }
        }
        Err(failure) => return Err(failure.message(&log_path)),
    };
    #[cfg(not(target_os = "macos"))]
    let child = start_once().map_err(|failure| failure.message(&log_path))?;

    process_state.store_child(child)?;
    Ok(())
}

pub(crate) fn wait_for_core_management_port(
    child: &mut Child,
    address: SocketAddr,
    process_state: &CoreProcessState,
) -> Result<(), CoreStartupFailure> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if process_state.is_shutting_down() {
            return Err(CoreStartupFailure::ShuttingDown);
        }
        if let Some(status) = child.try_wait().map_err(CoreStartupFailure::StatusCheck)? {
            return Err(CoreStartupFailure::Exited(status));
        }
        if TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(CoreStartupFailure::TimedOut(address.port()));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(any(target_os = "linux", test))]
fn configure_child_lifetime(command: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;

        let parent_process_id = unsafe { libc::getpid() };
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(io::Error::last_os_error());
                }

                if libc::getppid() != parent_process_id {
                    return Err(io::Error::other(
                        "EasyCLIProxyAPI exited before the core process started",
                    ));
                }

                Ok(())
            });
        }
    }

    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

pub(crate) fn configure_background_command(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    #[cfg(not(windows))]
    let _ = command;
}

pub(crate) fn stop_core_process_inner(process_state: &CoreProcessState) -> Result<(), String> {
    process_state.recovery.cancel();
    let mut stopped_any = false;
    let mut errors = Vec::new();

    if let Some(mut child) = process_state.take_child() {
        stopped_any = true;
        if let Err(error) = terminate_child(&mut child) {
            errors.push(error);
        }
    }

    let mut process_ids = process_state
        .take_adopted_processes()
        .into_iter()
        .filter(|process| {
            process_executable_path(process.process_id)
                .as_deref()
                .is_some_and(|path| executable_paths_match(&process.binary_path, path))
        })
        .map(|process| process.process_id)
        .collect::<Vec<_>>();
    if let Ok(install_dir) = core_install_dir() {
        if let Some(binary_path) = find_core_binary(&install_dir) {
            process_ids.extend(find_core_process_ids(&binary_path));
        }
    }
    process_ids.sort_unstable();
    process_ids.dedup();

    for process_id in process_ids {
        if !is_process_alive(process_id) {
            continue;
        }
        stopped_any = true;
        if let Err(error) = terminate_process_id(process_id) {
            errors.push(error);
        }
    }

    if !errors.is_empty() {
        Err(errors.join("；"))
    } else if stopped_any {
        Ok(())
    } else {
        Err("CPA kernel is not currently running".to_string())
    }
}

pub(crate) fn core_install_dir() -> Result<PathBuf, String> {
    Ok(core_base_dir()?.join("cpa-core"))
}

pub(crate) fn executable_dir() -> Result<PathBuf, String> {
    let exe_path = env::current_exe().map_err(|err| format!("Failed to read current executable path: {err}"))?;
    exe_path
        .parent()
        .map(|path| path.to_path_buf())
        .ok_or_else(|| format!("Current executable path has no parent directory: {}", path_to_string(&exe_path)))
}

pub(crate) fn core_base_dir() -> Result<PathBuf, String> {
    let executable_dir = executable_dir()?;
    #[cfg(target_os = "macos")]
    if macos_app_resources_dir(&executable_dir).is_some() {
        let home_dir = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "Unable to determine macOS user directory".to_string())?;
        return Ok(home_dir
            .join("Library")
            .join("Application Support")
            .join("com.cpa.gui"));
    }
    Ok(executable_dir)
}

pub(crate) fn read_core_metadata(install_dir: &Path) -> Option<CoreMetadata> {
    let metadata_path = install_dir.join(CORE_METADATA_FILE);
    let content = fs::read_to_string(metadata_path).ok()?;
    serde_json::from_str(&content).ok()
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub(crate) fn is_core_running(binary_path: &Path) -> bool {
    !find_core_process_ids(binary_path).is_empty()
}

pub(crate) fn find_core_process_ids(binary_path: &Path) -> Vec<u32> {
    find_candidate_core_process_ids()
        .into_iter()
        .filter(|process_id| {
            process_executable_path(*process_id)
                .as_deref()
                .is_some_and(|path| executable_paths_match(binary_path, path))
        })
        .collect()
}

pub(crate) fn executable_paths_match(expected: &Path, actual: &Path) -> bool {
    let expected = fs::canonicalize(expected).unwrap_or_else(|_| expected.to_path_buf());
    let actual = fs::canonicalize(actual).unwrap_or_else(|_| actual.to_path_buf());

    #[cfg(windows)]
    {
        path_to_string(&expected).eq_ignore_ascii_case(&path_to_string(&actual))
    }

    #[cfg(not(windows))]
    {
        expected == actual
    }
}

#[cfg(all(not(target_os = "linux"), target_os = "windows"))]
pub(crate) fn find_candidate_core_process_ids() -> Vec<u32> {
    use std::{ffi::OsString, mem, os::windows::ffi::OsStringExt};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }

    let mut entry = unsafe { mem::zeroed::<PROCESSENTRY32W>() };
    entry.dwSize = mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut process_ids = Vec::new();
    if unsafe { Process32FirstW(snapshot, &mut entry) } != 0 {
        loop {
            let name_length = entry
                .szExeFile
                .iter()
                .position(|character| *character == 0)
                .unwrap_or(entry.szExeFile.len());
            let image_name = OsString::from_wide(&entry.szExeFile[..name_length]);
            if entry.th32ProcessID != 0
                && image_name
                    .to_string_lossy()
                    .eq_ignore_ascii_case(core_binary_name())
            {
                process_ids.push(entry.th32ProcessID);
            }
            if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
                break;
            }
        }
    }
    unsafe { CloseHandle(snapshot) };
    process_ids
}

#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
pub(crate) fn find_candidate_core_process_ids() -> Vec<u32> {
    Command::new("pgrep")
        .args(["-x", core_binary_name()])
        .output()
        .ok()
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.trim().parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
pub(crate) fn find_candidate_core_process_ids() -> Vec<u32> {
    Command::new("pgrep")
        .args(["-x", core_binary_name()])
        .output()
        .ok()
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.trim().parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
pub(crate) fn process_executable_path(process_id: u32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{process_id}/exe")).ok()
}

#[cfg(target_os = "macos")]
pub(crate) fn process_executable_path(process_id: u32) -> Option<PathBuf> {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    extern "C" {
        fn proc_pidpath(pid: i32, buffer: *mut std::ffi::c_void, buffer_size: u32) -> i32;
    }

    let mut buffer = vec![0_u8; 4096];
    let length = unsafe {
        proc_pidpath(
            process_id.try_into().ok()?,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
        )
    };
    if length <= 0 {
        return None;
    }
    buffer.truncate(length as usize);
    Some(PathBuf::from(OsString::from_vec(buffer)))
}

#[cfg(windows)]
pub(crate) fn process_executable_path(process_id: u32) -> Option<PathBuf> {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if handle.is_null() {
        return None;
    }
    let mut buffer = vec![0_u16; 32_768];
    let mut length = buffer.len() as u32;
    let success =
        unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length) };
    unsafe { CloseHandle(handle as HANDLE) };
    if success == 0 {
        return None;
    }
    buffer.truncate(length as usize);
    Some(PathBuf::from(OsString::from_wide(&buffer)))
}

pub(crate) fn adopt_existing_core_processes(
    process_state: &CoreProcessState,
) -> Result<Vec<u32>, String> {
    let install_dir = core_install_dir()?;
    let Some(binary_path) = find_core_binary(&install_dir) else {
        process_state.clear_adopted_processes()?;
        return Ok(Vec::new());
    };
    let process_ids = find_core_process_ids(&binary_path);
    process_state.adopt_process_ids(&binary_path, process_ids.clone())?;
    Ok(process_ids)
}

pub(crate) fn shutdown_managed_core(
    process_state: &CoreProcessState,
    gui_config_state: &GuiConfigState,
) {
    let started_here = process_state.managed_pid().is_some();
    let was_running = started_here
        || core_install_dir()
            .ok()
            .and_then(|install_dir| find_core_binary(&install_dir))
            .is_some_and(|binary_path| is_core_running(&binary_path));
    // Stop only the kernel this GUI started. A kernel run by a service manager (adopted at start-up) keeps
    // serving, so the GUI can be closed or replaced without cutting off requests in flight.
    if started_here {
        if let Err(error) = stop_core_process_inner(process_state) {
            eprintln!("Failed to stop CPA kernel on exit: {error}");
        }
    }
    if let Err(error) = gui_config_state.set_run_on_startup(was_running) {
        eprintln!("Failed to save kernel state before exit: {error}");
    }
}

#[cfg(windows)]
pub(crate) fn attach_child_to_windows_job(child: &Child) -> Result<isize, String> {
    use std::{mem, os::windows::io::AsRawHandle, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
    };

    unsafe {
        let job = CreateJobObjectW(ptr::null(), ptr::null());
        if job.is_null() {
            return Err(format!(
                "Failed to create CPA kernel process job: {}",
                io::Error::last_os_error()
            ));
        }

        let mut information: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
        information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &information as *const _ as *const _,
            mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if configured == 0 {
            let error = io::Error::last_os_error();
            CloseHandle(job);
            return Err(format!("Failed to configure CPA kernel process job: {error}"));
        }

        let process_handle = child.as_raw_handle() as HANDLE;
        if AssignProcessToJobObject(job, process_handle) == 0 {
            let error = io::Error::last_os_error();
            CloseHandle(job);
            return Err(format!("Failed to manage CPA kernel child process: {error}"));
        }

        Ok(job as isize)
    }
}

#[cfg(windows)]
pub(crate) fn close_windows_handle(handle: isize) {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};

    unsafe {
        CloseHandle(handle as HANDLE);
    }
}

pub(crate) fn terminate_child(child: &mut Child) -> Result<(), String> {
    if child
        .try_wait()
        .map_err(|error| format!("Failed to check CPA kernel process status: {error}"))?
        .is_some()
    {
        return Ok(());
    }
    #[cfg(windows)]
    {
        child
            .kill()
            .map_err(|err| format!("Failed to stop CPA kernel process: {err}"))?;
        child
            .wait()
            .map_err(|err| format!("Failed while waiting for CPA kernel process to exit: {err}"))?;
        Ok(())
    }

    #[cfg(not(windows))]
    {
        let process_id = child.id();
        send_process_signal(process_id, "TERM")?;

        for _ in 0..20 {
            match child.try_wait() {
                Ok(Some(_)) => return Ok(()),
                Ok(None) => thread::sleep(Duration::from_millis(100)),
                Err(err) => return Err(format!("Failed to check CPA kernel process status: {err}")),
            }
        }

        child
            .kill()
            .map_err(|err| format!("Failed to force-stop CPA kernel process: {err}"))?;
        child
            .wait()
            .map_err(|err| format!("Failed while waiting for CPA kernel process to exit: {err}"))?;

        Ok(())
    }
}

pub(crate) fn terminate_process_id(process_id: u32) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::{
            Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
        };

        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, process_id) };
        if handle.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                return Ok(());
            }
            return Err(format!("Failed to open CPA kernel process: PID {process_id}: {error}"));
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let raw_handle = handle.as_raw_handle();
        if unsafe { WaitForSingleObject(raw_handle, 0) } == WAIT_OBJECT_0 {
            return Ok(());
        }
        let mut command = Command::new("taskkill");
        command
            .args(["/PID", &process_id.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_background_command(&mut command);
        let status = command
            .status()
            .map_err(|err| format!("Failed to stop CPA kernel process: {err}"))?;
        let wait =
            unsafe { WaitForSingleObject(raw_handle, if status.success() { 10_000 } else { 0 }) };
        match wait {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => Err(format!("CPA kernel process did not exit: PID {process_id}")),
            _ => Err(format!(
                "Failed while waiting for CPA kernel process to exit: PID {process_id}: {}",
                io::Error::last_os_error()
            )),
        }
    }

    #[cfg(not(windows))]
    {
        send_process_signal(process_id, "TERM")?;

        for _ in 0..20 {
            if !is_process_alive(process_id) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
        }

        send_process_signal(process_id, "KILL")
    }
}

#[cfg(not(windows))]
pub(crate) fn send_process_signal(process_id: u32, signal: &str) -> Result<(), String> {
    let status = Command::new("kill")
        .args([format!("-{signal}"), process_id.to_string()])
        .status()
        .map_err(|err| format!("Failed to send process signal: {err}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("Failed to send process signal: PID {process_id}"))
    }
}

#[cfg(windows)]
pub(crate) fn is_process_alive(process_id: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_TIMEOUT},
        System::Threading::{OpenProcess, WaitForSingleObject},
    };

    const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
    let handle = unsafe { OpenProcess(SYNCHRONIZE_ACCESS, 0, process_id) };
    if handle.is_null() {
        return false;
    }
    let result = unsafe { WaitForSingleObject(handle, 0) };
    unsafe { CloseHandle(handle) };
    result == WAIT_TIMEOUT
}

#[cfg(not(windows))]
pub(crate) fn is_process_alive(process_id: u32) -> bool {
    let process_id = process_id.to_string();
    Command::new("kill")
        .args(["-0", &process_id])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}


pub(crate) fn find_core_binary(install_dir: &Path) -> Option<PathBuf> {
    let binary_path = install_dir.join(core_binary_name());
    if binary_path.is_file() {
        return Some(binary_path);
    }

    let mut dirs = vec![install_dir.to_path_buf()];

    while let Some(dir) = dirs.pop() {
        let entries = fs::read_dir(dir).ok()?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path
                .file_name()
                .and_then(|file_name| file_name.to_str())
                .map(|file_name| file_name == core_binary_name())
                .unwrap_or(false)
            {
                return Some(path);
            }
        }
    }

    None
}

pub(crate) fn core_binary_name() -> &'static str {
    if env::consts::OS == "windows" {
        "cli-proxy-api.exe"
    } else {
        "cli-proxy-api"
    }
}

pub(crate) fn should_start_hidden(config: &GuiConfigFile) -> bool {
    config.silent_start
        && cfg!(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "windows"
        ))
}

pub(crate) fn should_start_core_on_launch(config: &GuiConfigFile) -> bool {
    config.start_core_on_launch
}

pub(crate) fn configure_initial_main_window(
    app_handle: &tauri::AppHandle,
    start_hidden: bool,
) -> Result<(), String> {
    let window = main_window(app_handle).ok_or_else(|| "Main window does not exist".to_string())?;

    #[cfg(target_os = "macos")]
    set_macos_dock_visible(app_handle, !start_hidden);

    #[cfg(target_os = "linux")]
    let start_hidden = if start_hidden && !linux_tray_available(app_handle) {
        eprintln!(
            "System tray is unavailable; showing the main window instead of starting hidden"
        );
        false
    } else {
        start_hidden
    };

    if start_hidden {
        return crate::window_activity::hide(&window)
            .map_err(|error| format!("Failed to hide main window during silent startup: {error}"));
    }

    crate::window_activity::show(&window)
        .map_err(|error| format!("Failed to show main window: {error}"))?;
    if window.is_minimized().unwrap_or(false) {
        window
            .unminimize()
            .map_err(|error| format!("Failed to restore main window: {error}"))?;
    }
    if let Err(error) = sync_main_webview_bounds(app_handle) {
        eprintln!("Failed to synchronize the main webview bounds: {error}");
    }
    window
        .set_focus()
        .map_err(|error| format!("Failed to focus main window: {error}"))
}

pub(crate) fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
