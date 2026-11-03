use super::*;

pub(super) fn agent_test_home(name: &str) -> PathBuf {
    let path = crate::unique_test_temp_path(&format!("cpa-gui-agent-{name}"));
    fs::create_dir_all(&path).unwrap();
    path
}




#[test]
fn unique_test_temp_paths_do_not_collide_across_threads() {
    let threads = (0..8)
        .map(|_| {
            std::thread::spawn(|| {
                (0..500)
                    .map(|_| crate::unique_test_temp_path("cpa-unique-path"))
                    .collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();
    let mut seen = std::collections::HashSet::new();
    for thread in threads {
        for path in thread.join().unwrap() {
            assert!(
                seen.insert(path.clone()),
                "duplicate test path {}",
                path.display()
            );
        }
    }
}
