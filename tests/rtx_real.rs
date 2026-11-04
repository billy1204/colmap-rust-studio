use colmap_launcher::{rtx_pipeline, validate_inputs};
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
