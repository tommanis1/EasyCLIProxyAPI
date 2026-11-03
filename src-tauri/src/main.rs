#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_settings;
mod configuration_watcher;
mod core_config;
mod core_runtime;
mod core_recovery;
mod desktop_theme;
mod instance_lock;
#[cfg(any(target_os = "linux", test))]
mod linux_graphics;
mod management_api;
mod network_proxy;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows"
))]
mod native_i18n;
mod oauth_browser;
mod plugin_webview;
mod plugins;
mod privacy_mode;
mod provider_health;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows"
))]
mod tray;
mod usage;
#[cfg(target_os = "windows")]
mod windows_frame;
mod window_material;
mod window_activity;

#[cfg(test)]
use configuration_watcher::nearest_existing_watch_directory;
use management_api::{
    format_management_request_error, management_authorization, management_endpoint,
    management_http_client, management_plaintext_key, read_management_value,
};
use oauth_browser::*;
#[cfg(test)]
use provider_health::{
    provider_health_content_type_is_streaming, provider_health_stream_has_terminal_success,
    provider_health_stream_has_text,
};

use app_settings::*;
use core_config::*;
use core_runtime::*;
use instance_lock::*;
#[cfg(target_os = "macos")]
use objc2::MainThreadMarker;
#[cfg(target_os = "macos")]
use objc2_app_kit::NSEvent;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "macos")]
use std::sync::Arc;
use std::{
    collections::HashSet,
    env, fs,
    fs::File,
    io::{self, Seek, SeekFrom, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket},
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{LazyLock, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
#[cfg(any(target_os = "linux", target_os = "windows"))]
use tauri::menu::PredefinedMenuItem;
#[cfg(target_os = "macos")]
use tauri::tray::{MouseButtonState, TrayIcon};
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows"
))]
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, TrayIconBuilder, TrayIconEvent},
};
use tauri::{Emitter, LogicalSize, Manager};
use tauri_plugin_opener::OpenerExt;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows"
))]
use tray::*;

const CORE_STATUS_EVENT: &str = "core-status-changed";
const CONFIG_FILES_CHANGED_EVENT: &str = "config-files-changed";
#[cfg(any(target_os = "linux", target_os = "windows"))]
const WINDOWS_CLOSE_REQUEST_EVENT: &str = "windows-close-requested";
const CORE_METADATA_FILE: &str = "cpa-gui-meta.json";
const CORE_CONFIG_FILE: &str = "config.yaml";
const CORE_EXAMPLE_CONFIG_FILE: &str = "config.example.yaml";
const GUI_CONFIG_FILE: &str = "config.toml";
const LEGACY_GUI_CONFIG_FILE: &str = "cpa-gui.yaml";
const MIN_MAIN_WINDOW_WIDTH: u32 = 640;
const MIN_MAIN_WINDOW_HEIGHT: u32 = 600;
const MAX_SAVED_WINDOW_DIMENSION: u32 = 16_384;
const DEFAULT_MAIN_WINDOW_WIDTH: u32 = 1280;
const DEFAULT_MAIN_WINDOW_HEIGHT: u32 = 800;
const LEGACY_DEFAULT_MAIN_WINDOW_WIDTH: u32 = 1531;
const LEGACY_DEFAULT_MAIN_WINDOW_HEIGHT: u32 = 891;
const OAUTH_DIR_NAME: &str = "oauth";
const DEFAULT_AUTH_DIR: &str = "../oauth";
const DEFAULT_API_KEY: &str = "123456";
const DEFAULT_API_KEY_INITIAL_REMARK: &str = "Default key";
const DEFAULT_REQUEST_RETRY: u32 = 3;
const DEFAULT_MAX_RETRY_CREDENTIALS: u32 = 0;
const DEFAULT_MAX_RETRY_INTERVAL: i64 = 30;
const DEFAULT_STREAMING_BOOTSTRAP_RETRIES: u32 = 0;
const DEFAULT_DISABLE_COOLING: bool = true;
const DEFAULT_LOGS_MAX_TOTAL_SIZE_MB: u32 = 0;
const DEFAULT_ERROR_LOGS_MAX_FILES: u32 = 10;
const DEFAULT_REDIS_USAGE_QUEUE_RETENTION_SECONDS: u32 = 60;
const LEGACY_DEFAULT_MANAGEMENT_SECRET_KEY: &str = "123456";
const USER_AGENT: &str = concat!(
    "CPA-GUI/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/router-for-me/CLIProxyAPI)"
);
const APP_USER_AGENT: &str = concat!(
    "EasyCLIProxyAPI/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/router-for-me/EasyCLIProxyAPI)"
);
static CORE_CONFIG_FILE_LOCK: Mutex<()> = Mutex::new(());
static CONFIG_WRITE_HASHES: LazyLock<Mutex<std::collections::HashMap<PathBuf, String>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

fn normalize_app_locale(locale: &str) -> &'static str {
    let normalized = locale.trim().to_ascii_lowercase();
    if normalized.starts_with("ja") {
        "ja"
    } else if normalized == "zh-tw"
        || normalized == "zh-hk"
        || normalized == "zh-mo"
        || normalized.starts_with("zh-hant")
    {
        "zh-TW"
    } else if normalized.starts_with("zh") {
        "zh-CN"
    } else {
        "en"
    }
}

#[derive(Default)]
struct CoreProcessState {
    recovery: core_recovery::CoreRecovery,
    child: Mutex<Option<CoreChild>>,
    adopted_processes: Mutex<Vec<AdoptedCoreProcess>>,
    starting: AtomicBool,
    shutting_down: AtomicBool,
    shutdown_complete: AtomicBool,
}

#[derive(Clone)]
struct AdoptedCoreProcess {
    process_id: u32,
    binary_path: PathBuf,
}

struct GuiConfigState {
    inner: Mutex<GuiConfigFile>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SavedWindowSize {
    width: u32,
    height: u32,
}

struct MainWindowState {
    inner: Mutex<Option<SavedWindowSize>>,
    position: Mutex<Option<tauri::PhysicalPosition<i32>>>,
    position_tracking: AtomicBool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigFilesChangedPayload {
    paths: Vec<String>,
    errors: Vec<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CorePlatform {
    os: String,
    arch: String,
    asset_os: String,
    asset_arch: String,
    archive_kind: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CoreStatus {
    installed: bool,
    running: bool,
    ready: bool,
    starting: bool,
    managed: bool,
    process_id: Option<u32>,
    current_version: Option<String>,
    install_dir: String,
    binary_path: Option<String>,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery: Option<core_recovery::RecoveryStatus>,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MacosUpdateDescriptor {
    parent_pid: u32,
    current_app: PathBuf,
    staged_app: PathBuf,
    backup_app: PathBuf,
    executable_relative_path: PathBuf,
    ack_path: PathBuf,
    work_dir: PathBuf,
    target_version: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct GuiConfigFile {
    locale: String,
    port: u16,
    allow_lan: bool,
    host: String,
    run_on_startup: bool,
    start_core_on_launch: bool,
    silent_start: bool,
    close_behavior: WindowsCloseBehavior,
    window_width: Option<u32>,
    window_height: Option<u32>,
    window_x: Option<i32>,
    window_y: Option<i32>,
    auth_dir: String,
    auth_dir_user_selected: bool,
    image_generation_user_selected: bool,
    #[serde(deserialize_with = "deserialize_gui_api_keys")]
    api_keys: Vec<GuiApiKeyEntry>,
    api_access_remarks: Vec<GuiApiAccessRemark>,
    management_secret_key: String,
    debug: bool,
    commercial_mode: bool,
    logging_to_file: bool,
    logs_max_total_size_mb: u32,
    error_logs_max_files: u32,
    usage_statistics_enabled: bool,
    #[serde(rename = "usage_statistics_disabled", skip_serializing_if = "Option::is_none")]
    usage_statistics_disabled: Option<bool>,
    redis_usage_queue_retention_seconds: u32,
    request_log: bool,
    plugins_enabled: bool,
    routing_strategy: String,
    proxy_url: String,
    proxy_override: bool,
    download_source: VersionDownloadSource,
    custom_download_mirrors: Vec<String>,
    active_custom_download_mirror: String,
    prefer_gitcode_downloads: bool,
    routing_session_affinity: bool,
    routing_session_affinity_ttl: String,
    disable_cooling: bool,
    request_retry: u32,
    max_retry_credentials: u32,
    max_retry_interval: i64,
    streaming_bootstrap_retries: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum VersionDownloadSource {
    #[default]
    Github,
    Gitcode,
    GhProxy,
    GhFast,
    Custom,
}

impl VersionDownloadSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitcode => "gitcode",
            Self::GhProxy => "gh-proxy",
            Self::GhFast => "gh-fast",
            Self::Custom => "custom",
        }
    }

}

fn normalize_custom_download_mirror_url(value: &str) -> Result<String, String> {
    let mut url =
        reqwest::Url::parse(value.trim()).map_err(|error| format!("Invalid mirror URL: {error}"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Mirror URL must be an HTTPS URL without authentication, port, query parameters, or fragment".to_string());
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url.to_string())
}


#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum WindowsCloseAction {
    Exit,
    MinimizeToTray,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum WindowsCloseBehavior {
    #[default]
    Ask,
    Exit,
    MinimizeToTray,
}

impl WindowsCloseBehavior {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Exit => "exit",
            Self::MinimizeToTray => "minimize-to-tray",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct GuiApiKeyEntry {
    key: String,
    #[serde(default)]
    remark: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct GuiApiAccessRemark {
    provider_section: String,
    api_key_hash: String,
    #[serde(default)]
    record_hash: String,
    remark: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiAccessRemarkLocator {
    provider_name: String,
    base_url: String,
    api_keys: Vec<String>,
    #[serde(default)]
    config_identity: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiAccessRemarkQuery {
    provider_section: String,
    #[serde(flatten)]
    locator: ApiAccessRemarkLocator,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiAccessRemarkUpdate {
    provider_section: String,
    previous_records: Vec<ApiAccessRemarkLocator>,
    records: Vec<ApiAccessRemarkLocator>,
    all_records: Vec<ApiAccessRemarkLocator>,
    remark: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GuiApiKeyInput {
    Legacy(String),
    Entry(GuiApiKeyEntry),
}

fn deserialize_gui_api_keys<'de, D>(deserializer: D) -> Result<Vec<GuiApiKeyEntry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let entries = Vec::<GuiApiKeyInput>::deserialize(deserializer)?;
    Ok(entries
        .into_iter()
        .map(|entry| match entry {
            GuiApiKeyInput::Legacy(key) => GuiApiKeyEntry {
                remark: String::new(),
                key,
            },
            GuiApiKeyInput::Entry(entry) => entry,
        })
        .collect())
}

impl Default for GuiConfigFile {
    fn default() -> Self {
        Self {
            locale: "en".to_string(),
            port: 8317,
            allow_lan: false,
            host: "127.0.0.1".to_string(),
            run_on_startup: false,
            start_core_on_launch: true,
            silent_start: false,
            close_behavior: WindowsCloseBehavior::Ask,
            window_width: Some(DEFAULT_MAIN_WINDOW_WIDTH),
            window_height: Some(DEFAULT_MAIN_WINDOW_HEIGHT),
            window_x: None,
            window_y: None,
            auth_dir: DEFAULT_AUTH_DIR.to_string(),
            auth_dir_user_selected: false,
            image_generation_user_selected: false,
            api_keys: vec![default_api_key_entry()],
            api_access_remarks: Vec::new(),
            management_secret_key: LEGACY_DEFAULT_MANAGEMENT_SECRET_KEY.to_string(),
            debug: false,
            commercial_mode: false,
            logging_to_file: false,
            logs_max_total_size_mb: DEFAULT_LOGS_MAX_TOTAL_SIZE_MB,
            error_logs_max_files: DEFAULT_ERROR_LOGS_MAX_FILES,
            usage_statistics_enabled: true,
            usage_statistics_disabled: None,
            redis_usage_queue_retention_seconds: DEFAULT_REDIS_USAGE_QUEUE_RETENTION_SECONDS,
            request_log: false,
            plugins_enabled: false,
            routing_strategy: "round-robin".to_string(),
            proxy_url: String::new(),
            proxy_override: false,
            download_source: VersionDownloadSource::Github,
            custom_download_mirrors: Vec::new(),
            active_custom_download_mirror: String::new(),
            prefer_gitcode_downloads: false,
            routing_session_affinity: false,
            routing_session_affinity_ttl: String::new(),
            disable_cooling: DEFAULT_DISABLE_COOLING,
            request_retry: DEFAULT_REQUEST_RETRY,
            max_retry_credentials: DEFAULT_MAX_RETRY_CREDENTIALS,
            max_retry_interval: DEFAULT_MAX_RETRY_INTERVAL,
            streaming_bootstrap_retries: DEFAULT_STREAMING_BOOTSTRAP_RETRIES,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct GuiConfigPresence {
    locale: Option<String>,
    port: Option<u16>,
    allow_lan: Option<bool>,
    host: Option<String>,
    auth_dir: Option<String>,
    api_keys: Option<Vec<GuiApiKeyInput>>,
    api_access_remarks: Option<Vec<GuiApiAccessRemark>>,
    management_secret_key: Option<String>,
    close_behavior: Option<WindowsCloseBehavior>,
    start_core_on_launch: Option<bool>,
    silent_start: Option<bool>,
    debug: Option<bool>,
    commercial_mode: Option<bool>,
    logging_to_file: Option<bool>,
    logs_max_total_size_mb: Option<u32>,
    error_logs_max_files: Option<u32>,
    usage_statistics_enabled: Option<bool>,
    redis_usage_queue_retention_seconds: Option<u32>,
    request_log: Option<bool>,
    plugins_enabled: Option<bool>,
    routing_strategy: Option<String>,
    proxy_url: Option<String>,
    proxy_override: Option<bool>,
    download_source: Option<VersionDownloadSource>,
    custom_download_mirrors: Option<Vec<String>>,
    active_custom_download_mirror: Option<String>,
    prefer_gitcode_downloads: Option<bool>,
    routing_session_affinity: Option<bool>,
    routing_session_affinity_ttl: Option<String>,
    disable_cooling: Option<bool>,
    request_retry: Option<u32>,
    max_retry_credentials: Option<u32>,
    max_retry_interval: Option<i64>,
    streaming_bootstrap_retries: Option<u32>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GuiSettings {
    host: String,
    port: u16,
    allow_lan: bool,
    run_on_startup: bool,
    close_behavior: WindowsCloseBehavior,
}


// Coordinates in the configuration arrays; never persisted into CPA configuration.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GuiNetworkSettings {
    port: u16,
    allow_lan: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GuiNetworkRoutingSettings {
    port: u16,
    allow_lan: bool,
    routing_session_affinity: bool,
    routing_session_affinity_ttl: String,
    #[serde(default)]
    disable_cooling: bool,
    request_retry: u32,
    max_retry_credentials: u32,
    max_retry_interval: i64,
    streaming_bootstrap_retries: u32,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CoreTlsSettings {
    enabled: bool,
    cert: String,
    key: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CoreConfigSettings {
    #[serde(skip_serializing)]
    host: String,
    #[serde(skip_serializing)]
    port: u16,
    #[serde(skip_serializing)]
    auth_dir: String,
    api_keys: Vec<String>,
    management_secret_configured: bool,
    debug: bool,
    commercial_mode: bool,
    logging_to_file: bool,
    logs_max_total_size_mb: u32,
    error_logs_max_files: u32,
    usage_statistics_enabled: bool,
    redis_usage_queue_retention_seconds: u32,
    request_log: bool,
    plugins_enabled: bool,
    routing_strategy: String,
    proxy_url: String,
    routing_session_affinity: bool,
    routing_session_affinity_ttl: String,
    disable_cooling: bool,
    request_retry: u32,
    max_retry_credentials: u32,
    max_retry_interval: i64,
    streaming_bootstrap_retries: u32,
    #[allow(dead_code)]
    #[serde(skip_serializing)]
    management_secret_key: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CoreApiKeyView {
    api_key: String,
    remark: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CoreConfigView {
    host: String,
    api_keys: Vec<CoreApiKeyView>,
    management_secret_configured: bool,
    port: u16,
    allow_lan: bool,
    debug: bool,
    commercial_mode: bool,
    logging_to_file: bool,
    logs_max_total_size_mb: u32,
    error_logs_max_files: u32,
    usage_statistics_enabled: bool,
    redis_usage_queue_retention_seconds: u32,
    request_log: bool,
    plugins_enabled: bool,
    routing_strategy: String,
    proxy_url: String,
    proxy_override: bool,
    routing_session_affinity: bool,
    routing_session_affinity_ttl: String,
    disable_cooling: bool,
    request_retry: u32,
    max_retry_credentials: u32,
    max_retry_interval: i64,
    streaming_bootstrap_retries: u32,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CoreMetadata {
    version: String,
    asset_name: String,
    installed_at_unix: u64,
}

impl CoreProcessState {
    fn new(starting: bool) -> Self {
        Self {
            recovery: core_recovery::CoreRecovery::default(),
            child: Mutex::new(None),
            adopted_processes: Mutex::new(Vec::new()),
            starting: AtomicBool::new(starting),
            shutting_down: AtomicBool::new(false),
            shutdown_complete: AtomicBool::new(false),
        }
    }

    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    fn ensure_active(&self) -> Result<(), String> {
        if self.is_shutting_down() {
            Err("Application is exiting; kernel operation canceled".to_string())
        } else {
            Ok(())
        }
    }

    fn is_starting(&self) -> bool {
        self.starting.load(Ordering::Acquire)
    }

    fn set_starting(&self, starting: bool) {
        self.starting.store(starting, Ordering::Release);
    }

    fn managed_pid(&self) -> Option<u32> {
        if let Ok(mut child) = self.child.lock() {
            if let Some(process) = child.as_mut() {
                match process.try_wait() {
                    Ok(None) => return Some(process.id()),
                    Ok(Some(exit)) => {
                        self.recovery.exited(format!("PID {}: {exit}", process.id()));
                        *child = None;
                    }
                    // A status-check error is not proof of exit. Dropping a live
                    // CoreChild would terminate its Windows job and create an outage.
                    Err(_) => return Some(process.id()),
                }
            }
        }

        self.adopted_processes
            .lock()
            .ok()
            .and_then(|mut processes| {
                let had_processes = !processes.is_empty();
                processes.retain(|process| {
                    process_executable_path(process.process_id)
                        .as_deref()
                        .is_some_and(|path| executable_paths_match(&process.binary_path, path))
                });
                if had_processes && processes.is_empty() {
                    self.recovery.exited("Adopted kernel process exited".into());
                }
                processes.first().map(|process| process.process_id)
            })
    }

    fn take_child(&self) -> Option<CoreChild> {
        self.child.lock().ok().and_then(|mut child| child.take())
    }

    fn adopt_process_ids(&self, binary_path: &Path, process_ids: Vec<u32>) -> Result<(), String> {
        let mut adopted = self
            .adopted_processes
            .lock()
            .map_err(|_| "Adopted kernel process status lock is poisoned".to_string())?;
        *adopted = process_ids
            .into_iter()
            .filter(|process_id| is_process_alive(*process_id))
            .map(|process_id| AdoptedCoreProcess {
                process_id,
                binary_path: binary_path.to_path_buf(),
            })
            .collect();
        adopted.sort_unstable_by_key(|process| process.process_id);
        adopted.dedup_by_key(|process| process.process_id);
        if !adopted.is_empty() {
            self.recovery.running();
        }
        Ok(())
    }

    fn clear_adopted_processes(&self) -> Result<(), String> {
        self.adopted_processes
            .lock()
            .map(|mut processes| processes.clear())
            .map_err(|_| "Adopted kernel process status lock is poisoned".to_string())
    }

    fn take_adopted_processes(&self) -> Vec<AdoptedCoreProcess> {
        self.adopted_processes
            .lock()
            .map(|mut processes| std::mem::take(&mut *processes))
            .unwrap_or_default()
    }

    fn store_child(&self, child: CoreChild) -> Result<u32, String> {
        self.ensure_active()?;
        let pid = child.id();
        self.clear_adopted_processes()?;
        let mut managed_child = self
            .child
            .lock()
            .map_err(|_| "Kernel process status lock is poisoned".to_string())?;
        *managed_child = Some(child);
        self.recovery.running();
        Ok(pid)
    }
}

impl MainWindowState {
    fn new(size: Option<SavedWindowSize>, position: Option<tauri::PhysicalPosition<i32>>) -> Self {
        Self {
            inner: Mutex::new(size),
            position: Mutex::new(position),
            position_tracking: AtomicBool::new(false),
        }
    }

    fn position(&self) -> Result<Option<tauri::PhysicalPosition<i32>>, String> {
        self.position
            .lock()
            .map(|position| *position)
            .map_err(|_| "Main window position status lock is poisoned".to_string())
    }

    fn replace_position(&self, position: tauri::PhysicalPosition<i32>) -> Result<(), String> {
        *self
            .position
            .lock()
            .map_err(|_| "Main window position status lock is poisoned".to_string())? =
            Some(position);
        Ok(())
    }

    fn snapshot(&self) -> Result<Option<SavedWindowSize>, String> {
        self.inner
            .lock()
            .map(|size| *size)
            .map_err(|_| "Main window size status lock is poisoned".to_string())
    }

    fn replace(&self, size: SavedWindowSize) -> Result<(), String> {
        let mut current = self
            .inner
            .lock()
            .map_err(|_| "Main window size status lock is poisoned".to_string())?;
        *current = Some(size);
        Ok(())
    }
}

impl GuiConfigState {
    fn new(config: GuiConfigFile) -> Self {
        Self {
            inner: Mutex::new(config),
        }
    }

    fn snapshot(&self) -> Result<GuiConfigFile, String> {
        self.inner
            .lock()
            .map(|config| config.clone())
            .map_err(|_| "GUI configuration status lock is poisoned".to_string())
    }

    fn replace_external(&self, config: GuiConfigFile) -> Result<(), String> {
        let mut current = self
            .inner
            .lock()
            .map_err(|_| "GUI configuration status lock is poisoned".to_string())?;
        *current = config;
        Ok(())
    }

    fn replace_core_settings_external(
        &self,
        settings: &CoreConfigSettings,
    ) -> Result<GuiConfigFile, String> {
        let mut current = self
            .inner
            .lock()
            .map_err(|_| "GUI configuration status lock is poisoned".to_string())?;
        let mut config = current.clone();
        apply_core_settings_to_gui_config(&mut config, settings);
        apply_external_core_proxy_override(&mut config, settings)?;
        sanitize_gui_config(&mut config)?;
        validate_gui_config(&config)?;
        *current = config.clone();
        Ok(config)
    }

    fn update_network(&self, port: u16, allow_lan: bool) -> Result<GuiConfigFile, String> {
        self.update(|config| {
            config.port = port;
            config.allow_lan = allow_lan;
            config.host = if allow_lan { "0.0.0.0" } else { "127.0.0.1" }.to_string();
            Ok(())
        })
    }

    fn update_network_routing(&self, settings: &GuiConfigFile) -> Result<GuiConfigFile, String> {
        self.update(|config| {
            config.port = settings.port;
            config.allow_lan = settings.allow_lan;
            config.host = settings.host.clone();
            config.proxy_url = settings.proxy_url.clone();
            config.routing_session_affinity = settings.routing_session_affinity;
            config.routing_session_affinity_ttl = settings.routing_session_affinity_ttl.clone();
            config.disable_cooling = settings.disable_cooling;
            config.request_retry = settings.request_retry;
            config.max_retry_credentials = settings.max_retry_credentials;
            config.max_retry_interval = settings.max_retry_interval;
            config.streaming_bootstrap_retries = settings.streaming_bootstrap_retries;
            Ok(())
        })
    }

    fn set_run_on_startup(&self, run_on_startup: bool) -> Result<GuiConfigFile, String> {
        self.update(|config| {
            config.run_on_startup = run_on_startup;
            Ok(())
        })
    }

    fn set_close_behavior(
        &self,
        close_behavior: WindowsCloseBehavior,
    ) -> Result<GuiConfigFile, String> {
        self.update(|config| {
            config.close_behavior = close_behavior;
            Ok(())
        })
    }

    fn set_locale(&self, locale: String) -> Result<GuiConfigFile, String> {
        self.update(|config| {
            config.locale = normalize_app_locale(&locale).to_string();
            Ok(())
        })
    }

    fn set_window_geometry(
        &self,
        size: SavedWindowSize,
        position: Option<tauri::PhysicalPosition<i32>>,
    ) -> Result<GuiConfigFile, String> {
        self.update(|config| {
            config.window_width = Some(size.width);
            config.window_height = Some(size.height);
            config.window_x = position.map(|position| position.x);
            config.window_y = position.map(|position| position.y);
            Ok(())
        })
    }

    fn sync_core_settings(&self, settings: &CoreConfigSettings) -> Result<GuiConfigFile, String> {
        self.sync_core_settings_internal(settings, None, false, false)
    }

    fn sync_core_settings_external(
        &self,
        settings: &CoreConfigSettings,
    ) -> Result<GuiConfigFile, String> {
        self.sync_core_settings_internal(settings, None, true, false)
    }

    fn sync_core_settings_with_api_key(
        &self,
        settings: &CoreConfigSettings,
        added_api_key: Option<GuiApiKeyEntry>,
    ) -> Result<GuiConfigFile, String> {
        self.sync_core_settings_internal(settings, added_api_key, false, false)
    }

    fn sync_core_settings_internal(
        &self,
        settings: &CoreConfigSettings,
        added_api_key: Option<GuiApiKeyEntry>,
        apply_external_proxy: bool,
        save_usage_preference: bool,
    ) -> Result<GuiConfigFile, String> {
        self.update(|config| {
            config.api_keys = merge_core_api_keys_with_gui_metadata(
                &config.api_keys,
                &settings.api_keys,
                added_api_key.as_ref(),
            );
            config.host = settings.host.clone();
            config.port = settings.port;
            config.allow_lan = !is_loopback_host(&settings.host);
            config.auth_dir = settings.auth_dir.clone();
            config.debug = settings.debug;
            config.commercial_mode = settings.commercial_mode;
            config.logging_to_file = settings.logging_to_file;
            config.logs_max_total_size_mb = settings.logs_max_total_size_mb;
            config.error_logs_max_files = settings.error_logs_max_files;
            config.usage_statistics_enabled = settings.usage_statistics_enabled;
            if save_usage_preference {
                config.usage_statistics_disabled = (!settings.usage_statistics_enabled).then_some(true);
            }
            config.redis_usage_queue_retention_seconds =
                settings.redis_usage_queue_retention_seconds;
            config.request_log = settings.request_log;
            if let Some(secret_key) = settings
                .management_secret_key
                .as_deref()
                .filter(|secret_key| !is_hashed_management_secret_key(secret_key))
            {
                config.management_secret_key = secret_key.to_string();
            }
            config.plugins_enabled = settings.plugins_enabled;
            config.routing_strategy = settings.routing_strategy.clone();
            config.routing_session_affinity = settings.routing_session_affinity;
            config.routing_session_affinity_ttl = settings.routing_session_affinity_ttl.clone();
            config.disable_cooling = settings.disable_cooling;
            config.request_retry = settings.request_retry;
            config.max_retry_credentials = settings.max_retry_credentials;
            config.max_retry_interval = settings.max_retry_interval;
            config.streaming_bootstrap_retries = settings.streaming_bootstrap_retries;
            if apply_external_proxy {
                apply_external_core_proxy_override(config, settings)?;
            }
            Ok(())
        })
    }

    fn update<F>(&self, update: F) -> Result<GuiConfigFile, String>
    where
        F: FnOnce(&mut GuiConfigFile) -> Result<(), String>,
    {
        let mut current = self
            .inner
            .lock()
            .map_err(|_| "GUI configuration status lock is poisoned".to_string())?;
        let mut config = current.clone();
        update(&mut config)?;
        sanitize_gui_config(&mut config)?;
        write_gui_config(&config)?;
        *current = config.clone();
        Ok(config)
    }
}

impl From<&GuiConfigFile> for GuiSettings {
    fn from(config: &GuiConfigFile) -> Self {
        Self {
            host: config.host.clone(),
            port: config.port,
            allow_lan: config.allow_lan,
            run_on_startup: config.run_on_startup,
            close_behavior: config.close_behavior,
        }
    }
}

impl From<&GuiConfigFile> for CoreConfigSettings {
    fn from(config: &GuiConfigFile) -> Self {
        Self {
            host: config.host.clone(),
            port: config.port,
            auth_dir: config.auth_dir.clone(),
            api_keys: gui_api_key_values(&config.api_keys),
            management_secret_configured: !config.management_secret_key.is_empty(),
            debug: config.debug,
            commercial_mode: config.commercial_mode,
            logging_to_file: config.logging_to_file,
            logs_max_total_size_mb: config.logs_max_total_size_mb,
            error_logs_max_files: config.error_logs_max_files,
            usage_statistics_enabled: config.usage_statistics_enabled,
            redis_usage_queue_retention_seconds: config.redis_usage_queue_retention_seconds,
            request_log: config.request_log,
            plugins_enabled: config.plugins_enabled,
            routing_strategy: config.routing_strategy.clone(),
            proxy_url: config.proxy_url.clone(),
            routing_session_affinity: config.routing_session_affinity,
            routing_session_affinity_ttl: config.routing_session_affinity_ttl.clone(),
            disable_cooling: config.disable_cooling,
            request_retry: config.request_retry,
            max_retry_credentials: config.max_retry_credentials,
            max_retry_interval: config.max_retry_interval,
            streaming_bootstrap_retries: config.streaming_bootstrap_retries,
            management_secret_key: Some(config.management_secret_key.clone()),
        }
    }
}

impl From<&GuiConfigFile> for CoreConfigView {
    fn from(config: &GuiConfigFile) -> Self {
        Self {
            host: config.host.clone(),
            api_keys: config
                .api_keys
                .iter()
                .map(|entry| CoreApiKeyView {
                    api_key: entry.key.clone(),
                    remark: entry.remark.clone(),
                })
                .collect(),
            management_secret_configured: !config.management_secret_key.is_empty(),
            port: config.port,
            allow_lan: config.allow_lan,
            debug: config.debug,
            commercial_mode: config.commercial_mode,
            logging_to_file: config.logging_to_file,
            logs_max_total_size_mb: config.logs_max_total_size_mb,
            error_logs_max_files: config.error_logs_max_files,
            usage_statistics_enabled: config.usage_statistics_enabled,
            redis_usage_queue_retention_seconds: config.redis_usage_queue_retention_seconds,
            request_log: config.request_log,
            plugins_enabled: config.plugins_enabled,
            routing_strategy: config.routing_strategy.clone(),
            proxy_url: config.proxy_url.clone(),
            proxy_override: config.proxy_override,
            routing_session_affinity: config.routing_session_affinity,
            routing_session_affinity_ttl: config.routing_session_affinity_ttl.clone(),
            disable_cooling: config.disable_cooling,
            request_retry: config.request_retry,
            max_retry_credentials: config.max_retry_credentials,
            max_retry_interval: config.max_retry_interval,
            streaming_bootstrap_retries: config.streaming_bootstrap_retries,
        }
    }
}

fn main() {
    // SAFETY: no worker threads or GTK/WebKit runtime have started yet.
    #[cfg(target_os = "linux")]
    unsafe {
        linux_graphics::configure();
    }

    let _instance_guard = match acquire_app_instance_guard() {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("{error}");
            return;
        }
    };

    let gui_config = match load_or_create_gui_config() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            let mut config = GuiConfigFile::default();
            // If the GUI configuration cannot be read before the kernel starts,
            // use the shared recovery key so requests and the generated kernel
            // configuration stay synchronized.
            config.management_secret_key = LEGACY_DEFAULT_MANAGEMENT_SECRET_KEY.to_string();
            if let Err(sanitize_error) = sanitize_gui_config(&mut config) {
                eprintln!("Failed to initialize fixed credentials directory: {sanitize_error}");
            }
            config
        }
    };
    let initial_window_size = configured_window_size(&gui_config);
    let initial_window_position = configured_window_position(&gui_config);
    let start_hidden = should_start_hidden(&gui_config);

    let context = tauri::generate_context!();
    #[cfg(target_os = "linux")]
    let context = {
        let mut context = context;
        // Prevent #491's partial blank rendering on Linux (AMD and NVIDIA): disable native
        // transparency before build creates the window, not later in setup.
        linux_graphics::configure_main_window(context.config_mut());
        context
    };

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        .manage(window_activity::WindowActivity::default())
        .manage(CoreProcessState::new(gui_config.start_core_on_launch))
        .manage(usage::UsageCollectorState::default())
        .manage(GuiConfigState::new(gui_config))
        .manage(plugin_webview::PluginWebviewState::default())
        .manage(MainWindowState::new(
            initial_window_size,
            initial_window_position,
        ));

    let app = app.on_window_event(|window, event| {
        if window.label() != "main" {
            return;
        }

        if matches!(event, tauri::WindowEvent::Resized(_) | tauri::WindowEvent::Focused(_)) {
            window_activity::sync(window.app_handle());
        }

        if let tauri::WindowEvent::Moved(position) = event {
            let state = window.state::<MainWindowState>();
            // Ignore startup placement and the OS's minimized/maximized coordinates.
            if state.position_tracking.load(Ordering::Acquire)
                && !window.is_minimized().unwrap_or(true)
                && !window.is_maximized().unwrap_or(true)
            {
                if let Err(error) = state.replace_position(*position) {
                    eprintln!("Failed to record main window position: {error}");
                }
            }
        }

        let observed_size = match event {
            tauri::WindowEvent::Resized(physical_size) => {
                window.scale_factor().ok().and_then(|scale_factor| {
                    logical_window_size_from_physical(physical_size, scale_factor)
                })
            }
            tauri::WindowEvent::ScaleFactorChanged {
                scale_factor,
                new_inner_size,
                ..
            } => logical_window_size_from_physical(new_inner_size, *scale_factor),
            _ => None,
        };

        if let Some(observed_size) = observed_size {
            let window_size_state = window.state::<MainWindowState>();
            if let Err(error) = window_size_state.replace(observed_size) {
                eprintln!("Failed to record main window size: {error}");
            }
        }
    });

    #[cfg(target_os = "macos")]
    let app = app.on_window_event(|window, event| {
        if window.label() == "main" {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                set_macos_dock_visible(window.app_handle(), false);
                if let Err(error) = window_activity::hide(window) {
                    eprintln!("Failed to hide main window: {error}");
                    set_macos_dock_visible(window.app_handle(), true);
                }
            }
        }
    });

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let app = app.on_window_event(|window, event| {
        if window.label() == "main" {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                if let Err(error) = window.emit(WINDOWS_CLOSE_REQUEST_EVENT, ()) {
                    eprintln!("Failed to show desktop close confirmation: {error}");
                }
            }
        }
    });

    let app = app
        .setup(move |app| {
            // Match WindowFrame's Windows-only shell before showing the main window.
            #[cfg(target_os = "windows")]
            if let Some(window) = app.get_webview_window("main") {
                windows_frame::install(&window)?;
                window.set_decorations(false)?;
            }
            if let Some(window) = app.get_webview_window("main") {
                window.set_background_color(Some(tauri::window::Color(255, 255, 255, 255)))?;
            }
            if let Err(error) = network_proxy::refresh(app.state::<GuiConfigState>().inner()) {
                eprintln!("Failed to read startup proxy settings: {error}");
            }
            if let Err(error) = restore_main_window_geometry(app.handle()) {
                eprintln!("{error}");
            }
            app.state::<MainWindowState>()
                .position_tracking
                .store(true, Ordering::Release);

            #[cfg(target_os = "macos")]
            setup_macos_tray(app)?;
            #[cfg(target_os = "linux")]
            if let Err(error) = setup_windows_tray(app) {
                eprintln!(
                    "Failed to set up Linux tray; keeping the main window available: {error}"
                );
            }
            #[cfg(target_os = "windows")]
            setup_windows_tray(app)?;

            if let Err(error) = configure_initial_main_window(app.handle(), start_hidden) {
                eprintln!("Failed to configure startup window state: {error}");
            }

            if let Err(error) =
                configuration_watcher::start_configuration_file_watcher(app.handle().clone())
            {
                eprintln!("Failed to start configuration file watcher: {error}");
            }

            network_proxy::start_monitor(app.handle().clone());
            core_recovery::start(app.handle().clone());

            let usage_app = app.handle().clone();
            tauri::async_runtime::spawn_blocking(move || {
                if let Err(error) = usage::initialize_usage_storage() {
                    eprintln!("Failed to initialize usage records directory: {error}");
                }
                usage::start_usage_collector(usage_app);
            });


            let core_app = app.handle().clone();
            tauri::async_runtime::spawn_blocking(move || {
                let Ok(_guard) = CORE_OPERATION_LOCK.lock() else {
                    return;
                };
                let gui_config_state = core_app.state::<GuiConfigState>();
                let process_state = core_app.state::<CoreProcessState>();
                if process_state.ensure_active().is_err() {
                    process_state.set_starting(false);
                    return;
                }
                let Ok(config) = gui_config_state.snapshot() else {
                    process_state.set_starting(false);
                    return;
                };

                if config.start_core_on_launch {
                    if let Ok(status) =
                        current_core_status(Some(process_state.inner()), Some(config.port))
                    {
                        emit_core_status(&core_app, &status);
                    }
                }

                let adopted_process_ids = match adopt_existing_core_processes(process_state.inner())
                {
                    Ok(process_ids) => process_ids,
                    Err(error) => {
                        eprintln!("Failed to scan for and adopt the CPA kernel in the current directory: {error}");
                        Vec::new()
                    }
                };
                if !adopted_process_ids.is_empty() {
                    eprintln!(
                        "Adopted running CPA kernel in the current directory: PID {}",
                        adopted_process_ids
                            .iter()
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                } else if should_start_core_on_launch(&config) {
                    if let Err(error) = start_core_process_inner(process_state.inner(), &config) {
                        eprintln!("Failed to automatically start CPA kernel: {error}");
                    }
                }

                process_state.set_starting(false);

                if let Ok(status) =
                    current_core_status(Some(process_state.inner()), Some(config.port))
                {
                    emit_core_status(&core_app, &status);
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            window_activity::main_window_active,
            desktop_theme::get_linux_system_theme,
            desktop_theme::list_custom_themes,
            desktop_theme::open_custom_themes_directory,
            desktop_theme::get_theme_selection,
            desktop_theme::save_theme_selection,
            window_material::detect_window_material,
            window_material::set_window_material,
            privacy_mode::get_privacy_mode,
            privacy_mode::save_privacy_mode,
            health_check,
            detect_core_platform,
            get_core_status,
            get_gui_settings,
            get_usage_view_preferences,
            save_usage_view_preferences,
            get_routing_profiles,
            save_routing_profiles,
            resolve_api_access_remarks,
            save_api_access_remark,
            set_app_locale,
            resolve_windows_close_request,
            get_lan_ipv4,
            save_gui_settings,
            save_network_routing_settings,
            get_core_tls_settings,
            get_core_config_settings,
            set_core_request_log,
            add_core_api_key,
            update_core_api_key,
            delete_core_api_key,
            management_api::management_request,
            management_api::prepare_management_request,
            management_api::cancel_management_request,
            plugins::get_plugin_support,
            plugins::local_install::install_local_plugin,
            plugin_webview::show_plugin_webview,
            plugin_webview::set_plugin_webview_bounds,
            plugin_webview::hide_plugin_webview,
            plugin_webview::close_plugin_webview,
            plugins::get_plugin_latest_releases,
            provider_health::provider_health_probe,
            provider_health::get_core_models,
            management_api::upload_auth_file,
            management_api::import_vertex_credential,
            management_api::open_auth_files_directory,
            management_api::open_core_logs_directory,
            set_core_plugins_enabled,
            set_core_proxy_url,
            set_core_session_affinity,
            set_core_session_affinity_ttl,
            management_api::start_oauth_login,
            management_api::get_oauth_status,
            management_api::submit_oauth_callback,
            list_oauth_browsers,
            open_oauth_url,
            open_external_url,
            usage::get_usage_collector_status,
            usage::get_usage_overview,
            usage::get_usage_analysis,
            usage::get_usage_events,
            usage::save_usage_events_export,
            usage::get_usage_pricing,
            usage::get_usage_storage_settings,
            usage::repair_usage_cache_records,
            usage::save_usage_storage_settings,
            usage::shrink_usage_database,
            usage::save_usage_model_price,
            usage::delete_usage_model_price,
            usage::preview_usage_model_prices,
            usage::apply_usage_model_prices,
            start_core_process,
            stop_core_process,
            restart_core_process,
        ])
        .build(context)
        .expect("failed to build app");

    app.run(|app_handle, event| match event {
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen {
            has_visible_windows: false,
            ..
        } => show_main_window(app_handle),
        tauri::RunEvent::ExitRequested { api, code, .. } => {
            let process_state = app_handle.state::<CoreProcessState>();
            if process_state.shutdown_complete.load(Ordering::Acquire) {
                return;
            }
            api.prevent_exit();
            if process_state.shutting_down.swap(true, Ordering::AcqRel) {
                return;
            }
            process_state.recovery.cancel();
            if let Err(error) = persist_main_window_geometry(app_handle) {
                eprintln!("Failed to save main window geometry: {error}");
            }
            let app_handle = app_handle.clone();
            tauri::async_runtime::spawn_blocking(move || {
                let _guard = CORE_OPERATION_LOCK
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let process_state = app_handle.state::<CoreProcessState>();
                let gui_config_state = app_handle.state::<GuiConfigState>();
                shutdown_managed_core(process_state.inner(), gui_config_state.inner());
                process_state
                    .shutdown_complete
                    .store(true, Ordering::Release);
                app_handle.exit(code.unwrap_or(0));
            });
        }
        tauri::RunEvent::Exit => {
            usage::stop_usage_collector(app_handle);
            let gui_config_state = app_handle.state::<GuiConfigState>();
            let process_state = app_handle.state::<CoreProcessState>();
            if !process_state.shutdown_complete.load(Ordering::Acquire) {
                process_state.shutting_down.store(true, Ordering::Release);
                shutdown_managed_core(process_state.inner(), gui_config_state.inner());
            }
        }
        _ => {}
    });
}

/// Returns a path under the system temporary directory that is unique for the
/// lifetime of the test process. The wall clock alone is not enough: parallel
/// tests that start within the clock's resolution would otherwise share a
/// directory and delete or overwrite each other's files.
#[cfg(test)]
pub(crate) fn unique_test_temp_path(prefix: &str) -> PathBuf {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

#[cfg(test)]
mod tests;

pub(crate) fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

