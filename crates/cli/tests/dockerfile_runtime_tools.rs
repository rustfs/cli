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

fn runtime_stage_apk_packages(contents: &str) -> Vec<String> {
    let lines: Vec<&str> = contents.lines().collect();
    let mut from_indices = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| line.trim_start().starts_with("FROM ").then_some(index));

    let _builder_from = from_indices
        .next()
        .expect("Dockerfile should contain a builder stage");
    let runtime_from = from_indices
        .next()
        .expect("Dockerfile should contain a runtime stage");

    let runtime_end = lines
        .iter()
        .enumerate()
        .skip(runtime_from + 1)
        .find_map(|(index, line)| line.trim_start().starts_with("FROM ").then_some(index))
        .unwrap_or(lines.len());

    let apk_start = (runtime_from..runtime_end)
        .find(|&index| lines[index].contains("apk add --no-cache"))
        .expect("runtime stage should include an apk add command");

    let mut command = String::new();
    let mut index = apk_start;
    while index < runtime_end {
        let line = lines[index].trim_end();
        let (without_backslash, has_backslash) = match line.strip_suffix('\\') {
            Some(stripped) => (stripped, true),
            None => (line, false),
        };

        if !command.is_empty() {
            command.push(' ');
        }
        command.push_str(without_backslash.trim());

        if !has_backslash {
            break;
        }
        index += 1;
    }

    let tokens: Vec<&str> = command.split_whitespace().collect();
    let apk_index = tokens
        .iter()
        .position(|token| *token == "apk")
        .expect("runtime command should include `apk`");
    assert!(
        tokens.len() > apk_index + 1 && tokens[apk_index + 1] == "add",
        "runtime apk command should start with `apk add`; found: {command}"
    );

    tokens
        .iter()
        .skip(apk_index + 2)
        .copied()
        .filter(|token| !token.starts_with('-'))
        .map(ToOwned::to_owned)
        .collect()
}

#[test]
fn runtime_image_installs_jq_and_yq() {
    let contents = dockerfile_contents();
    let packages = runtime_stage_apk_packages(&contents);

    assert!(
        packages.iter().any(|package| package == "jq"),
        "runtime apk install command should include jq as a package; found: {packages:?}"
    );
    assert!(
        packages.iter().any(|package| package == "yq"),
        "runtime apk install command should include yq as a package; found: {packages:?}"
    );
}
