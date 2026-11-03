use super::support::*;
use super::*;







#[test]
fn core_process_starting_state_tracks_automatic_launch() {
    let state = CoreProcessState::new(true);
    assert!(state.is_starting());
    state.set_starting(false);
    assert!(!state.is_starting());
}

#[test]
fn executable_path_matching_keeps_core_instances_directory_scoped() {
    let root = agent_test_home("core-process-path-scope");
    let first_dir = root.join("first").join("cpa-core");
    let second_dir = root.join("second").join("cpa-core");
    fs::create_dir_all(&first_dir).unwrap();
    fs::create_dir_all(&second_dir).unwrap();
    let first_binary = first_dir.join(core_binary_name());
    let second_binary = second_dir.join(core_binary_name());
    fs::write(&first_binary, b"first").unwrap();
    fs::write(&second_binary, b"second").unwrap();

    assert!(executable_paths_match(&first_binary, &first_binary));
    assert!(!executable_paths_match(&first_binary, &second_binary));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn core_process_discovery_sleep_helper() {
    if env::var_os("EASYCLIPROXYAPI_PROCESS_DISCOVERY_TEST_HELPER").is_some() {
        thread::sleep(Duration::from_secs(10));
    }
}

fn core_child_sleep_command() -> Command {
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "tests::core_runtime::core_process_discovery_sleep_helper",
            "--nocapture",
        ])
        .env("EASYCLIPROXYAPI_PROCESS_DISCOVERY_TEST_HELPER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_background_command(&mut command);
    command
}

#[test]
fn observed_child_exit_schedules_recovery_once_without_a_new_monitor() {
    let state = CoreProcessState::new(false);
    let pid = state.store_child(spawn_core_child(core_child_sleep_command()).unwrap()).unwrap();
    assert_eq!(state.managed_pid(), Some(pid));
    assert!(state.recovery.status().is_none());
    {
        let mut child = state.child.lock().unwrap();
        let child = child.as_mut().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }
    assert_eq!(state.managed_pid(), None);
    let status = state.recovery.status().unwrap();
    assert_eq!(status.phase, "waiting");
    assert_eq!(status.attempt, 1);
    assert!(status.detail.contains(&pid.to_string()));
    assert_eq!(state.managed_pid(), None);
    assert_eq!(state.recovery.status().unwrap().attempt, 1);
    state.recovery.cancel();
}


#[test]
fn adopted_process_exit_also_requests_recovery() {
    let state = CoreProcessState::new(false);
    let mut child = spawn_core_child(core_child_sleep_command()).unwrap();
    state.adopt_process_ids(&env::current_exe().unwrap(), vec![child.id()]).unwrap();
    assert_eq!(state.managed_pid(), Some(child.id()));
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(state.managed_pid(), None);
    assert_eq!(state.recovery.status().unwrap().phase, "waiting");
    state.recovery.cancel();
}

fn assert_core_child_survives(child: &mut Child) {
    thread::sleep(Duration::from_millis(200));
    let status = child.try_wait();
    let _ = child.kill();
    let _ = child.wait();
    assert!(status.unwrap().is_none(), "core exited with its launcher");
}

#[test]
fn core_child_survives_launcher_thread_exit() {
    let mut child = thread::spawn(|| spawn_core_child(core_child_sleep_command()).unwrap())
        .join()
        .unwrap();
    assert_core_child_survives(&mut child);
}

#[test]
fn core_child_survives_blocking_runtime_shutdown() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut child = runtime
        .block_on(runtime.spawn_blocking(|| spawn_core_child(core_child_sleep_command()).unwrap()))
        .unwrap();
    drop(runtime);
    assert_core_child_survives(&mut child);
}

#[test]
fn core_child_spawner_remains_available_after_spawn_failure() {
    let missing_binary = agent_test_home("missing-core-spawner-binary").join(core_binary_name());
    assert!(spawn_core_child_on_lifetime_thread(Command::new(missing_binary)).is_err());
    let mut child =
        thread::spawn(|| spawn_core_child_on_lifetime_thread(core_child_sleep_command()).unwrap())
            .join()
            .unwrap();
    assert_core_child_survives(&mut child);
}

#[test]
fn exiting_during_core_startup_cancels_the_port_wait() {
    let state = std::sync::Arc::new(CoreProcessState::new(true));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut child = spawn_core_child(core_child_sleep_command()).unwrap();
    let child_id = child.id();
    let exiting_state = state.clone();
    let exit = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        exiting_state.shutting_down.store(true, Ordering::Release);
    });

    let started = Instant::now();
    let result = wait_for_core_management_port(&mut child, address, &state);
    drop(child);
    exit.join().unwrap();
    assert!(matches!(result, Err(CoreStartupFailure::ShuttingDown)));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!is_process_alive(child_id));
}

#[test]
fn exiting_rejects_new_core_operations_and_cleans_up_an_in_flight_child() {
    let state = CoreProcessState::new(false);
    let child = spawn_core_child(core_child_sleep_command()).unwrap();
    let child_id = child.id();
    state.shutting_down.store(true, Ordering::Release);

    assert!(lock_core_operation(&state)
        .unwrap_err()
        .contains("Application is exiting"));
    assert!(state
        .store_child(child)
        .unwrap_err()
        .contains("Application is exiting"));
    assert!(!is_process_alive(child_id));
    assert_eq!(state.managed_pid(), None);
}

#[cfg(windows)]
#[test]
fn core_config_file_lock_helper() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    let Some(path) = env::var_os("EASYCLIPROXYAPI_CORE_FILE_LOCK_TEST_HELPER") else {
        return;
    };
    let _file = File::options()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
        .unwrap();
    println!("CORE_FILE_LOCKED");
    io::stdout().flush().unwrap();
    thread::sleep(Duration::from_secs(10));
}

#[cfg(windows)]
#[test]
fn updating_stops_an_adopted_core_without_a_port_before_replacing_its_config() {
    use std::io::BufRead;

    let root = agent_test_home("core-stop-before-replace");
    let config_path = root.join(CORE_CONFIG_FILE);
    let replacement = root.join("new-config.yaml");
    fs::write(&config_path, b"old config").unwrap();
    fs::write(&replacement, b"new config").unwrap();
    let binary_path = env::current_exe().unwrap();
    let mut command = Command::new(&binary_path);
    command
        .args([
            "--exact",
            "tests::core_runtime::core_config_file_lock_helper",
            "--nocapture",
        ])
        .env("EASYCLIPROXYAPI_CORE_FILE_LOCK_TEST_HELPER", &config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    configure_background_command(&mut command);
    let mut child = command.spawn().unwrap();
    let output = io::BufReader::new(child.stdout.take().unwrap());
    let ready = output
        .lines()
        .any(|line| line.unwrap() == "CORE_FILE_LOCKED");
    let state = CoreProcessState::new(false);
    state
        .adopt_process_ids(&binary_path, vec![child.id()])
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let status = current_core_status(Some(&state), Some(port)).unwrap();
    let locked_result = copy_core_file_replace(&replacement, &config_path);
    let stopped = pause_core_process_for_install(&state);
    let exited = child.try_wait().unwrap().is_some();
    let replace_result = copy_core_file_replace(&replacement, &config_path);
    let _ = child.kill();
    let _ = child.wait();

    assert!(ready);
    assert!(
        status.running,
        "the tracked orphan remains a running process without a listening management port"
    );
    assert!(!status.ready, "the orphan is not management-ready");
    assert!(
        locked_result.is_err(),
        "the orphan must hold a real Windows file lock"
    );
    assert!(
        stopped.unwrap(),
        "the update must stop the orphan despite the closed port"
    );
    assert!(
        exited,
        "stopping must wait until the process has actually exited"
    );
    replace_result.unwrap();
    assert_eq!(fs::read(&config_path).unwrap(), b"new config");
    fs::remove_dir_all(root).unwrap();
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn core_child_owner_process_helper() {
    if env::var_os("EASYCLIPROXYAPI_CORE_OWNER_TEST_HELPER").is_none() {
        return;
    }
    let mut command = core_child_sleep_command();
    command.stdout(Stdio::inherit());
    let _child = spawn_core_child(command).unwrap();
    println!("CORE_CHILD_READY");
    io::stdout().flush().unwrap();
    thread::sleep(Duration::from_secs(10));
}


#[test]
fn running_core_process_discovery_ignores_the_same_binary_name_in_another_directory() {
    let root = agent_test_home("running-core-process-scope");
    let first_dir = root.join("first").join("cpa-core");
    let second_dir = root.join("second").join("cpa-core");
    fs::create_dir_all(&first_dir).unwrap();
    fs::create_dir_all(&second_dir).unwrap();
    let first_binary = first_dir.join(core_binary_name());
    let second_binary = second_dir.join(core_binary_name());

    let source_binary = env::current_exe().unwrap();
    fs::copy(&source_binary, &first_binary).unwrap();
    fs::copy(&source_binary, &second_binary).unwrap();
    let arguments = [
        "--exact",
        "tests::core_runtime::core_process_discovery_sleep_helper",
        "--nocapture",
    ];
    let mut first = Command::new(&first_binary)
        .args(&arguments)
        .env("EASYCLIPROXYAPI_PROCESS_DISCOVERY_TEST_HELPER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut second = Command::new(&second_binary)
        .args(&arguments)
        .env("EASYCLIPROXYAPI_PROCESS_DISCOVERY_TEST_HELPER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(200));

    let first_running = first.try_wait().unwrap().is_none();
    let second_running = second.try_wait().unwrap().is_none();
    let candidate_process_ids = find_candidate_core_process_ids();
    let first_actual_path = process_executable_path(first.id());
    let second_actual_path = process_executable_path(second.id());
    let first_matches = find_core_process_ids(&first_binary);
    let second_matches = find_core_process_ids(&second_binary);

    let _ = first.kill();
    let _ = second.kill();
    let _ = first.wait();
    let _ = second.wait();
    assert_eq!(
        first_matches,
        vec![first.id()],
        "running={first_running}, candidate={}, actual={first_actual_path:?}, expected={first_binary:?}",
        candidate_process_ids.contains(&first.id())
    );
    assert_eq!(
        second_matches,
        vec![second.id()],
        "running={second_running}, candidate={}, actual={second_actual_path:?}, expected={second_binary:?}",
        candidate_process_ids.contains(&second.id())
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn current_process_executable_path_can_be_resolved() {
    let expected = env::current_exe().unwrap();
    let actual = process_executable_path(std::process::id()).unwrap();
    assert!(executable_paths_match(&expected, &actual));
}

#[test]
fn core_process_state_tracks_and_releases_adopted_processes() {
    let state = CoreProcessState::new(false);
    let binary_path = env::current_exe().unwrap();
    state
        .adopt_process_ids(&binary_path, vec![std::process::id(), std::process::id()])
        .unwrap();

    assert_eq!(state.managed_pid(), Some(std::process::id()));
    assert_eq!(
        state
            .take_adopted_processes()
            .into_iter()
            .map(|process| process.process_id)
            .collect::<Vec<_>>(),
        vec![std::process::id()]
    );
    assert_eq!(state.managed_pid(), None);
}

#[test]
fn tracked_core_stays_running_when_a_management_port_probe_misses() {
    let state = CoreProcessState::new(false);
    let binary_path = env::current_exe().unwrap();
    let process_id = std::process::id();
    state
        .adopt_process_ids(&binary_path, vec![process_id])
        .unwrap();

    // Port zero cannot be the configured management endpoint. This models a
    // transient failed health probe while the tracked process is still alive.
    let status = current_core_status(Some(&state), Some(0)).unwrap();
    state.clear_adopted_processes().unwrap();

    assert!(status.running);
    assert!(!status.ready);
    assert_eq!(status.process_id, Some(process_id));
}

#[test]
fn tracked_core_is_ready_when_its_management_port_accepts_connections() {
    let state = CoreProcessState::new(false);
    let binary_path = env::current_exe().unwrap();
    let process_id = std::process::id();
    state
        .adopt_process_ids(&binary_path, vec![process_id])
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let status = current_core_status(Some(&state), Some(port)).unwrap();
    state.clear_adopted_processes().unwrap();

    assert!(status.running);
    assert!(status.ready);
    assert_eq!(status.process_id, Some(process_id));
}











#[test]
fn v7_config_startup_with_v8_template_writes_v8_version() {
    for marker in ["", "config-version: 8\n"] {
        let template = format!("{marker}server: {{port: 8317}}\naccess: {{api-keys: []}}\n");
        let current = "config-version: 7\ncodex-api-key: [{api-key: upstream-test, models: [{name: example}]}]\n";
        let rendered = merge_core_config_yaml(&template, Some(current), &GuiConfigFile::default()).unwrap();
        let document: serde_norway::Value = serde_norway::from_str(&rendered).unwrap();
        assert_eq!(document["config-version"].as_u64(), Some(8));
        assert_eq!(document["api-keys"]["codex"][0]["keys"][0]["api-key"], "upstream-test");
        assert!(document.get("codex-api-key").is_none());
    }
}

#[test]
fn v8_migration_preserves_each_legacy_provider_family_independently() {
    let template = "config-version: 8\napi-keys: {codex: [], claude: []}\n";
    for marker in ["", "config-version: 7\n"] {
        let old = format!("{marker}api-keys: {{codex: []}}\ncodex-api-key: [{{api-key: ignored}}]\nclaude-api-key: [{{api-key: fixture, models: [{{name: upstream, alias: claude-alias}}]}}]\n");
        let migrated = migrate_legacy_core_config_to_v8(template, &old).unwrap();
        let value: serde_norway::Value = serde_norway::from_str(&migrated).unwrap();
        assert_eq!(value["api-keys"]["codex"].as_sequence().unwrap().len(), 0);
        assert_eq!(value["api-keys"]["claude"][0]["keys"][0]["api-key"], "fixture");
        assert_eq!(value["api-keys"]["claude"][0]["models"][0]["alias"], "claude-alias");
        assert!(value.get("claude-api-key").is_none());
        assert!(value.get("codex-api-key").is_none());
    }
}

#[test]
fn v8_migration_preserves_narrow_fields_and_canonical_struct_leaves() {
    let template = "config-version: 8\nmanagement: {allow-remote: false}\noauth: {providers: {claude: {model-level-cooling: false}, codex: {response-steering: false}}}\n";
    let legacy = r#"
remote-management: {allow-remote: true, secret-key: old-secret}
management: {secret-key: canonical-secret}
claude-header-defaults: {user-agent: custom-agent, timezone: custom-zone}
claude-code: {disable-cloaking-model-list: true}
disable-claude-cloak-mode: true
claude: {model-level-cooling: true}
codex-header-defaults: {user-agent: codex-agent}
codex: {response-steering: true, optimize-multi-agent-v2: true}
oauth:
  providers:
    claude:
      model-level-cooling: false
      header-defaults: {timezone: canonical-zone}
    codex: {response-steering: false}
"#;
    let migrated = migrate_legacy_core_config_to_v8(template, legacy).unwrap();
    let value: serde_norway::Value = serde_norway::from_str(&migrated).unwrap();
    let claude = &value["oauth"]["providers"]["claude"];
    assert_eq!(claude["header-defaults"]["user-agent"], "custom-agent");
    assert_eq!(claude["header-defaults"]["timezone"], "canonical-zone");
    assert_eq!(claude["model-level-cooling"].as_bool(), Some(false));
    assert_eq!(claude["claude-code"]["disable-cloaking-model-list"].as_bool(), Some(true));
    assert_eq!(claude["disable-claude-cloak-mode"].as_bool(), Some(true));
    assert_eq!(value["management"]["allow-remote"].as_bool(), Some(true));
    assert_eq!(value["management"]["secret-key"], "canonical-secret");
    assert_eq!(value["oauth"]["providers"]["codex"]["header-defaults"]["user-agent"], "codex-agent");
    assert_eq!(value["oauth"]["providers"]["codex"]["response-steering"].as_bool(), Some(false));
    assert!(value["oauth"]["providers"]["codex"].get("optimize-multi-agent-v2").is_none());
    assert_eq!(value["client"]["codex"]["optimize-multi-agent-v2"].as_bool(), Some(true));
}

#[test]
fn v8_migration_preserves_network_settings_over_template_defaults() {
    let template = "config-version: 8\nserver: {trusted-proxies: [], discovery: {enabled: false}, tls: {enable: false, cert: '', key: ''}}\n";
    let legacy = "trusted-proxies: [127.0.0.1]\ndiscovery: {enabled: true, service-name: office}\ntls: {enable: true, cert: old-cert, key: old-key}\nserver: {tls: {cert: canonical-cert}}\n";
    let migrated = migrate_legacy_core_config_to_v8(template, legacy).unwrap();
    let value: serde_norway::Value = serde_norway::from_str(&migrated).unwrap();
    assert_eq!(value["server"]["trusted-proxies"][0], "127.0.0.1");
    assert_eq!(value["server"]["discovery"]["enabled"].as_bool(), Some(true));
    assert_eq!(value["server"]["discovery"]["service-name"], "office");
    assert_eq!(value["server"]["tls"]["enable"].as_bool(), Some(true));
    assert_eq!(value["server"]["tls"]["cert"], "canonical-cert");
    assert_eq!(value["server"]["tls"]["key"], "old-key");
    assert!(value.get("trusted-proxies").is_none());
    assert!(value.get("discovery").is_none());
}

#[test]
fn v8_migration_preserves_github_token() {
    let template = "config-version: 8\nserver: {github-token: ''}\n";
    let migrated = migrate_legacy_core_config_to_v8(template, "github-token: legacy-token\n").unwrap();
    let value: serde_norway::Value = serde_norway::from_str(&migrated).unwrap();
    assert_eq!(value["server"]["github-token"], "legacy-token");
    assert!(value.get("github-token").is_none());
    let current = "config-version: 8\nserver: {github-token: current-token}\n";
    assert_eq!(migrate_legacy_core_config_to_v8(template, current).unwrap(), current);
}

#[test]
fn v8_migration_preserves_explicit_canonical_clears_and_existing_v8_files() {
    let template = "config-version: 8\nserver: {trusted-proxies: [192.0.2.1]}\n";
    let old = "trusted-proxies: [127.0.0.1]\nserver: {trusted-proxies: []}\nclaude-header-defaults: {user-agent: legacy}\nclaude: {model-level-cooling: true}\noauth: {providers: {claude: null}, model-alias: {}}\noauth-model-alias: {claude: [{name: original, alias: old-alias}]}\n";
    let migrated = migrate_legacy_core_config_to_v8(template, old).unwrap();
    let value: serde_norway::Value = serde_norway::from_str(&migrated).unwrap();
    assert!(value["server"]["trusted-proxies"].as_sequence().unwrap().is_empty());
    assert!(value["oauth"]["providers"]["claude"].is_null());
    assert!(value["oauth"]["model-alias"].as_mapping().unwrap().is_empty());
    let already_v8 = format!("config-version: 8\n{old}");
    assert_eq!(migrate_legacy_core_config_to_v8(template, &already_v8).unwrap(), already_v8);
}


#[cfg(windows)]
#[test]
fn replacing_a_core_rejects_locked_config_without_using_defaults() {
    use std::os::windows::fs::OpenOptionsExt;
    let root = agent_test_home("core-config-locked");
    let source = root.join("source");
    let target = root.join("target");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&target).unwrap();
    let original = "codex-api-key:\n  - api-key: test-key\n    base-url: https://example.test\n";
    let source_path = source.join(CORE_CONFIG_FILE);
    fs::write(&source_path, original).unwrap();
    fs::write(target.join(CORE_EXAMPLE_CONFIG_FILE), "port: 8317\n").unwrap();
    fs::write(target.join(CORE_CONFIG_FILE), "staged: untouched\n").unwrap();
    let locked = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&source_path)
        .unwrap();
    let result = migrate_core_config_for_update(&source, &target);
    drop(locked);
    assert!(
        result.is_err(),
        "unreadable configuration must not be replaced by defaults"
    );
    assert_eq!(fs::read_to_string(source_path).unwrap(), original);
    assert_eq!(
        fs::read_to_string(target.join(CORE_CONFIG_FILE)).unwrap(),
        "staged: untouched\n"
    );
    fs::remove_dir_all(root).unwrap();
}











#[test]
fn core_start_log_path_follows_the_managed_logs_directory() {
    let base_dir = PathBuf::from("test-base");
    let install_dir = base_dir.join("cpa-core");

    assert_eq!(
        core_start_log_path(&install_dir, DEFAULT_AUTH_DIR).unwrap(),
        base_dir
            .join("oauth")
            .join("logs")
            .join("core-start-output.log")
    );
    assert_eq!(
        core_start_log_path(&install_dir, "custom-auth").unwrap(),
        install_dir
            .join("custom-auth")
            .join("logs")
            .join("core-start-output.log")
    );
}

#[test]
fn core_start_output_helper() {
    if env::var_os("EASYCLIPROXYAPI_CORE_OUTPUT_TEST_HELPER").is_none() {
        return;
    }
    println!("core stdout marker");
    eprintln!("core stderr marker");
}

#[test]
fn core_start_log_captures_stdout_and_stderr() {
    let root = agent_test_home("core-start-output");
    let log_path = root.join("logs").join("core-start-output.log");
    fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    fs::write(&log_path, "stale startup output").unwrap();
    let (stdout, stderr) = core_start_stdio(&log_path).unwrap();
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "tests::core_runtime::core_start_output_helper",
            "--nocapture",
        ])
        .env("EASYCLIPROXYAPI_CORE_OUTPUT_TEST_HELPER", "1")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    configure_background_command(&mut command);

    assert!(command.status().unwrap().success());

    let output = fs::read_to_string(&log_path).unwrap();
    assert!(output.contains("===== CPA kernel startup"));
    assert!(output.contains("core stdout marker"));
    assert!(output.contains("core stderr marker"));
    assert!(output.starts_with("stale startup output\n"));
    drop(core_start_stdio(&log_path).unwrap());
    let restarted_output = fs::read_to_string(&log_path).unwrap();
    assert!(restarted_output.starts_with(&output));
    assert_eq!(restarted_output.matches("===== CPA kernel startup").count(), 2);
    fs::remove_dir_all(root).unwrap();
}



