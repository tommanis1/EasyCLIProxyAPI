use super::support::agent_test_home;
use super::*;
use std::process::{Child, Stdio};

struct TestCore {
    child: Option<Child>,
    directory: PathBuf,
    config: PathBuf,
    executable: PathBuf,
    origin: String,
    client: reqwest::Client,
}


impl Drop for TestCore {
    fn drop(&mut self) {
        self.stop();
        if std::thread::panicking() {
            eprintln!(
                "{}",
                fs::read_to_string(self.directory.join("core.log")).unwrap_or_default()
            );
        }
        if self.directory.parent() == Some(std::env::temp_dir().as_path())
            && self
                .directory
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("cpa-gui-agent-v8-contract-")
        {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

impl TestCore {
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    async fn start(&mut self) {
        let mut command = Command::new(&self.executable);
        command
            .args(["-config"])
            .arg(&self.config)
            .arg("-local-model")
            .current_dir(&self.directory)
            .stdin(Stdio::null())
            .stdout(Stdio::from(
                File::create(self.directory.join("core.log")).unwrap(),
            ))
            .stderr(Stdio::null());
        configure_background_command(&mut command);
        self.child = Some(command.spawn().unwrap());
        for _ in 0..100 {
            assert!(
                self.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                "test kernel exited during startup"
            );
            if self
                .client
                .get(format!("{}/v8/management/config", self.origin))
                .bearer_auth("isolated-test-secret")
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("test kernel startup timed out");
    }

}








#[tokio::test]
#[ignore = "requires CPA_V8_TEST_CORE and CPA_LOCAL_TEST_PLUGIN"]
async fn local_plugin_copy_is_discovered_and_loads_in_real_kernel() {
    let executable = fs::canonicalize(env::var_os("CPA_V8_TEST_CORE").expect("set CPA_V8_TEST_CORE")).unwrap();
    let source = fs::canonicalize(env::var_os("CPA_LOCAL_TEST_PLUGIN").expect("set CPA_LOCAL_TEST_PLUGIN")).unwrap();
    let directory = agent_test_home("v8-contract-local-plugin");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut core = TestCore {
        child: None, config: directory.join("config.yaml"), directory, executable,
        origin: format!("http://127.0.0.1:{port}"),
        client: reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(3)).build().unwrap(),
    };
    let gui = GuiConfigFile {
        port, plugins_enabled: true, management_secret_key: "isolated-test-secret".into(),
        auth_dir: path_to_string(&core.directory.join("auth")),
        api_keys: vec![GuiApiKeyEntry { key: "isolated-client-key".into(), remark: String::new() }],
        ..GuiConfigFile::default()
    };
    let initial = "config-version: 8\nmanagement: {disable-control-panel: true, disable-auto-update-panel: true}\nplugins: {enabled: true, dir: plugins}\n";
    fs::write(&core.config, merge_core_config_yaml(initial, None, &gui).unwrap()).unwrap();
    core.start().await;
    let list = |client: reqwest::Client, origin: String| async move {
        client.get(format!("{origin}/v8/management/plugins")).bearer_auth("isolated-test-secret")
            .send().await.unwrap().error_for_status().unwrap().json::<serde_json::Value>().await.unwrap()
    };
    let before = list(core.client.clone(), core.origin.clone()).await;
    let returned_dir = PathBuf::from(before["plugins_dir"].as_str().unwrap());
    let root = if returned_dir.is_absolute() { returned_dir } else { core.directory.join(returned_dir) };
    let os = if cfg!(windows) { "windows" } else if cfg!(target_os="macos") { "darwin" } else { "linux" };
    let arch = if cfg!(target_arch="aarch64") { "arm64" } else { "amd64" };
    let extension = source.extension().unwrap().to_str().unwrap();
    let installed = crate::plugins::local_install::copy_plugin(&source, &root.join(os).join(arch), extension).unwrap();
    assert_eq!(fs::read(&installed).unwrap(), fs::read(&source).unwrap());
    let after = list(core.client.clone(), core.origin.clone()).await;
    let entry = after["plugins"].as_array().unwrap().iter().find(|entry| {
        Path::new(entry["path"].as_str().unwrap_or("")).file_name() == source.file_name()
    }).expect("copied plugin must be discovered");
    let id = entry["id"].as_str().unwrap().to_string();
    core.client.put(format!("{}/v8/management/config/plugins/configs/{id}/enabled", core.origin))
        .bearer_auth("isolated-test-secret").json(&true)
        .send().await.unwrap().error_for_status().unwrap();
    core.stop();
    core.start().await;
    let loaded = list(core.client.clone(), core.origin.clone()).await;
    let entry = loaded["plugins"].as_array().unwrap().iter().find(|entry| entry["id"] == id).unwrap();
    assert_eq!(entry["registered"], true, "plugin must load: {entry}");
    assert_eq!(entry["effective_enabled"], true, "plugin must be enabled: {entry}");
    println!("Verified actual kernel discovery and loading: {id}");
}
