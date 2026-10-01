use colmap_launcher::{diagnostics, ply, point_cloud, sparse_scene};
use std::path::PathBuf;

#[test]
#[ignore = "requires a real fused.ply and local COLMAP/NVIDIA installation"]
fn real_dense_preview_and_diagnostics() {
    let ply_path = PathBuf::from(std::env::var_os("COLMAP_REAL_PLY").expect("COLMAP_REAL_PLY"));
    let install =
        PathBuf::from(std::env::var_os("COLMAP_REAL_INSTALL").expect("COLMAP_REAL_INSTALL"));
    let workspace = ply_path
        .ancestors()
        .nth(3)
        .expect("fused.ply under workspace/dense/model")
        .to_path_buf();

    let count = ply::vertex_count(&ply_path).expect("read dense PLY header");
    let cloud = ply::load(&ply_path).expect("parse and sample dense PLY");
    assert_eq!(count, cloud.total_points);
    assert!(cloud.total_points > 1_000_000);
    assert_eq!(cloud.points.len(), 100_000);

    let report = diagnostics::run(&install, &workspace);
    assert!(
        report.quick_ready(),
        "COLMAP diagnostics failed: {report:?}"
    );
    assert!(report.gpu.is_ok(), "GPU detection failed: {report:?}");
    assert!(report.free_bytes.expect("disk space") > 0);

    let model = workspace.join("sparse").join("0");
    let sparse = point_cloud::load(&model.join("points3D.bin")).expect("load sparse points");
    let scene = sparse_scene::load(&model).expect("load sparse cameras, images, rigs, and frames");
    assert!(!scene.cameras.is_empty());
    assert_eq!(
        scene.overlays(sparse.normalization, 0.035).unwrap().len(),
        scene.cameras.len()
    );
}
