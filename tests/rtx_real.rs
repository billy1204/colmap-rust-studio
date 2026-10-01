use colmap_launcher::{job, rtx_pipeline, validate_inputs};
use std::path::PathBuf;
use std::sync::{Arc, atomic::AtomicBool, mpsc};

#[test]
#[ignore = "requires local COLMAP CUDA build and real photos; writes a new workspace"]
fn real_rtx_dense_pipeline() {
    let exe = PathBuf::from(std::env::var_os("COLMAP_RTX_EXE").expect("COLMAP_RTX_EXE"));
    let images = PathBuf::from(std::env::var_os("COLMAP_RTX_IMAGES").expect("COLMAP_RTX_IMAGES"));
    let workspace =
        PathBuf::from(std::env::var_os("COLMAP_RTX_WORKSPACE").expect("COLMAP_RTX_WORKSPACE"));
    validate_inputs(&exe, &images, &workspace).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let (tx, _rx) = mpsc::sync_channel(512);
    let result = rtx_pipeline::run(
        &exe,
        &images,
        &workspace,
        Arc::new(AtomicBool::new(false)),
        tx,
    )
    .unwrap();
    let rtx_pipeline::PipelineOutcome::Completed(result) = result else {
        panic!("pipeline unexpectedly cancelled");
    };
    assert!(!result.models.is_empty());
    assert_eq!(result.models.len(), result.fused_clouds.len());
    for cloud in result.fused_clouds {
        assert!(std::fs::metadata(cloud).unwrap().len() > 100);
    }
    let log = std::fs::read_to_string(workspace.join("run.log")).unwrap();
    assert!(log.contains("feature_extractor"));
    assert!(log.contains("exhaustive_matcher"));
    assert!(log.contains("patch_match_stereo"));
    assert!(log.contains("stereo_fusion"));
}

#[test]
#[ignore = "requires local COLMAP and real photos; creates and removes a cancellation workspace"]
fn real_colmap_cooperative_cancellation() {
    let exe = PathBuf::from(std::env::var_os("COLMAP_RTX_EXE").expect("COLMAP_RTX_EXE"));
    let images = PathBuf::from(std::env::var_os("COLMAP_RTX_IMAGES").expect("COLMAP_RTX_IMAGES"));
    let workspace = PathBuf::from(
        std::env::var_os("COLMAP_CANCEL_WORKSPACE").expect("COLMAP_CANCEL_WORKSPACE"),
    );
    validate_inputs(&exe, &images, &workspace).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let mut stage = rtx_pipeline::sparse_stages(&exe, &images, &workspace).remove(0);
    stage.command.current_dir(&workspace);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    let (tx, rx) = mpsc::sync_channel(512);
    let log = workspace.join("run.log");
    let worker_log = log.clone();
    let worker = std::thread::spawn(move || {
        job::execute(&mut stage.command, &worker_log, worker_cancel, tx)
    });
    rx.recv_timeout(std::time::Duration::from_secs(10))
        .expect("COLMAP process start event");
    std::thread::sleep(std::time::Duration::from_secs(1));
    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(worker.join().unwrap().unwrap(), job::Outcome::Cancelled);
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("waiting up to 10s"), "{text}");
    assert!(!text.contains("force-stopping"), "{text}");
    std::fs::remove_dir_all(&workspace).unwrap();
}
