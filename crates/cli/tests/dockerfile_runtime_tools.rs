//! Dockerfile runtime tooling contract tests.
//!
//! These tests prevent regressions where container utility tools expected by
//! users (for example in Kubernetes jobs) are removed from the runtime image.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("cli crate has parent directory")
        .parent()
        .expect("workspace root exists")
        .to_path_buf()
}

fn dockerfile_contents() -> String {
    let dockerfile_path = workspace_root().join("Dockerfile");
    std::fs::read_to_string(&dockerfile_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", dockerfile_path.display()))
}

#[test]
fn runtime_image_installs_jq_and_yq() {
    let contents = dockerfile_contents();
    let runtime_apk_line = contents
        .lines()
        .rev()
        .find(|line| line.contains("apk add --no-cache"))
        .expect("Dockerfile should install runtime packages with apk");

    assert!(
        runtime_apk_line.contains("jq"),
        "runtime apk install line should include jq; found: {runtime_apk_line}"
    );
    assert!(
        runtime_apk_line.contains("yq"),
        "runtime apk install line should include yq; found: {runtime_apk_line}"
    );
}
