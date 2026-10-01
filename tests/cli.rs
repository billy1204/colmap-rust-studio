use std::process::Command;
#[test]
#[ignore = "requires local COLMAP and real sample photos; writes a new workspace"]
fn real_sample_reconstruction() {
    let install = std::env::var_os("COLMAP_TEST_INSTALL").expect("COLMAP_TEST_INSTALL");
    let images = std::env::var_os("COLMAP_TEST_IMAGES").expect("COLMAP_TEST_IMAGES");
    let workspace = std::env::var_os("COLMAP_TEST_WORKSPACE").expect("COLMAP_TEST_WORKSPACE");
    let result = Command::new(env!("CARGO_BIN_EXE_colmap-launcher"))
        .arg("--colmap")
        .arg(install)
        .arg("--images")
        .arg(images)
        .arg("--workspace")
        .arg(&workspace)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("SUCCESS"));
    let work = std::path::Path::new(&workspace);
    assert!(work.join("run.log").is_file());
    assert!(!work.join(".colmap-studio.lock").exists());
    assert!(!colmap_launcher::sparse_models(work).unwrap().is_empty());
}

#[test]
fn help_describes_usage_and_unknown_options_fail() {
    let exe = env!("CARGO_BIN_EXE_colmap-launcher");
    let help = Command::new(exe).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--workspace"));
    let bad = Command::new(exe).arg("--bogus").output().unwrap();
    assert!(!bad.status.success());
    let missing = Command::new(exe).arg("--images").output().unwrap();
    assert!(!missing.status.success());
}
