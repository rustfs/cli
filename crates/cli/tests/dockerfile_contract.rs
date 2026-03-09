#[test]
fn runtime_image_contains_jq_and_yq() {
    let dockerfile = include_str!("../../../Dockerfile");

    assert!(
        dockerfile.contains("apk add --no-cache ca-certificates jq yq-go"),
        "runtime image must install jq and yq-go"
    );
}
