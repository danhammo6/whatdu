use std::{fs, process::Command};

fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_whatdu"))
}

#[test]
fn json_export_and_worker_counts_agree() {
    let temp = tempfile::tempdir().unwrap();
    for i in 0..16 {
        let dir = temp.path().join(format!("dir{i:02}"));
        fs::create_dir(&dir).unwrap();
        for j in 0..16 {
            fs::write(dir.join(format!("file{j:02}")), vec![b'x'; j + 1]).unwrap();
        }
    }
    let single = command()
        .args(["--json", "-t", "1"])
        .arg(temp.path())
        .output()
        .unwrap();
    let parallel = command()
        .args(["--json", "-t", "8"])
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(single.status.success());
    assert!(parallel.status.success());
    assert_eq!(single.stdout, parallel.stdout);
    let tree: serde_json::Value = serde_json::from_slice(&parallel.stdout).unwrap();
    assert_eq!(tree["items"], 272);
    assert_eq!(tree["errors"], 0);
}

#[cfg(unix)]
#[test]
fn piped_output_is_plain_text_and_escapes_controls() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("line\nbreak"), b"abc").unwrap();
    let output = command().arg(temp.path()).output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("line\\nbreak"));
    assert!(text.contains("total:"));
    assert!(!text.contains('\x1b'));
}

#[test]
fn empty_directory_and_file_roots_work() {
    let temp = tempfile::tempdir().unwrap();
    let empty = command().arg("--json").arg(temp.path()).output().unwrap();
    assert!(empty.status.success());
    let tree: serde_json::Value = serde_json::from_slice(&empty.stdout).unwrap();
    assert_eq!(tree["items"], 0);
    let file = temp.path().join("file");
    fs::write(&file, b"abc").unwrap();
    let output = command()
        .args(["--no-ui", "-a"])
        .arg(file)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout).unwrap().contains("3 B"));
}

#[test]
fn invalid_arguments_and_missing_root_fail() {
    let temp = tempfile::tempdir().unwrap();
    assert_eq!(
        command().args(["-t", "0"]).output().unwrap().status.code(),
        Some(2)
    );
    assert_eq!(
        command()
            .arg(temp.path().join("missing"))
            .output()
            .unwrap()
            .status
            .code(),
        Some(1)
    );
}

#[cfg(unix)]
#[test]
fn logical_size_can_exceed_disk_capacity_without_claiming_disk_usage() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("Docker.raw");
    // A one-TiB sparse image requires no one-TiB allocation.
    fs::File::create(&path).unwrap().set_len(1 << 40).unwrap();
    let json = command().arg("--json").arg(&path).output().unwrap();
    assert!(json.status.success());
    let tree: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(tree["apparent"], 1u64 << 40);
    assert!(tree["allocated"].as_u64().unwrap() < 1 << 40);
    for args in [vec!["--no-ui"], vec!["--no-ui", "-a"]] {
        let output = command().args(args).arg(&path).output().unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("Disk usage"));
        assert!(text.contains("Logical"));
        assert!(text.contains("1.0 TiB"));
        assert!(text.contains("sparse"));
    }
}

#[test]
fn piped_output_handles_spaces_and_unicode() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("space and café.txt"), b"abc").unwrap();
    let output = command().arg(temp.path()).output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("space and café.txt"));
    assert!(text.contains("total:"));
    assert!(!text.contains('\x1b'));
}
