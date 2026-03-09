//! Contract tests for Dockerfile runtime image tooling guarantees.

#[test]
fn runtime_image_contains_jq_and_yq() {
    let dockerfile = include_str!("../../../Dockerfile");
    let runtime_section = dockerfile
        .split("FROM alpine:3.23")
        .nth(1)
        .expect("Dockerfile must contain runtime stage based on alpine:3.23");
    let runtime_apk_line = runtime_section
        .lines()
        .find(|line| line.contains("apk add --no-cache"))
        .expect("runtime stage must install packages via `apk add --no-cache`");

    let mut has_jq = false;
    let mut has_yq_go = false;

    for token in runtime_apk_line.split_whitespace() {
        if token == "jq" {
            has_jq = true;
        } else if token == "yq-go" {
            has_yq_go = true;
        }
    }

    assert!(
        has_jq && has_yq_go,
        "runtime image must install jq and yq-go"
    );
}
