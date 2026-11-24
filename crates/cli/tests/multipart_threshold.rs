use std::process::Command;

#[test]
fn storage_class_preflight_uses_multipart_threshold_from_environment() {
    let config_dir = tempfile::tempdir().expect("create config directory");
    let source = tempfile::NamedTempFile::new().expect("create source file");

    for (size, threshold, expected_code) in [
        (8, Some("7"), 7),
        (8, Some("8"), 0),
        (8, Some("9"), 0),
        (8, None, 0),
        (8, Some("invalid"), 0),
        (64 * 1024 * 1024 + 1, None, 7),
        (64 * 1024 * 1024 + 1, Some("134217728"), 0),
    ] {
        source.as_file().set_len(size).expect("set source size");
        for planned in [false, true] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_rc"));
            command
                .arg("cp")
                .arg(source.path())
                .args([
                    "threshold-test/bucket/file.bin",
                    "--storage-class",
                    "STANDARD",
                    "--dry-run",
                ])
                .env("RC_CONFIG_DIR", config_dir.path())
                .env("RC_HOST_threshold-test", "http://key:secret@127.0.0.1:1")
                .env_remove("RC_MULTIPART_THRESHOLD");
            if planned {
                command.arg("--summary");
            }
            if let Some(threshold) = threshold {
                command.env("RC_MULTIPART_THRESHOLD", threshold);
            }
            let output = command.output().expect("run copy preflight");
            assert_eq!(
                output.status.code(),
                Some(expected_code),
                "size={size}, threshold={threshold:?}, planned={planned}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
