use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "cortex-cli-package-release-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn make_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn host_target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        other => panic!("unsupported packager fixture host: {other:?}"),
    }
}

fn real_mv() -> String {
    let output = Command::new("sh")
        .args(["-c", "command -v mv"])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn run_with_manifest_move_failure(
    root: &Path,
    binary: &Path,
    output_dir: &Path,
    mv_path: &Path,
) -> Output {
    let path = format!(
        "{}:{}",
        mv_path.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new("bash")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/package-release.sh"
        ))
        .args(["--version", "1.2.3", "--target", host_target(), "--binary"])
        .arg(binary)
        .arg("--output-dir")
        .arg(output_dir)
        .env("PATH", path)
        .env("CORTEX_TEST_REAL_MV", real_mv())
        .current_dir(root)
        .output()
        .unwrap()
}

#[test]
fn failed_manifest_publish_removes_only_new_archive_and_preserves_existing_assets() {
    let root = TestDir::new();
    let output_dir = root.path().join("release-assets");
    fs::create_dir_all(&output_dir).unwrap();
    let old_manifest = b"0123456789abcdef  previous-asset.tar.gz\n";
    fs::write(output_dir.join("SHA256SUMS"), old_manifest).unwrap();
    let previous_asset = output_dir.join("previous-asset.tar.gz");
    fs::write(&previous_asset, b"preserve this published asset").unwrap();
    let previous_asset_contents = fs::read(&previous_asset).unwrap();

    let binary = root.path().join("cortex");
    make_executable(&binary, "#!/bin/sh\nprintf fixture\\n\n");
    let mv_path = root.path().join("bin");
    fs::create_dir_all(&mv_path).unwrap();
    let mv = mv_path.join("mv");
    make_executable(
        &mv,
        "#!/bin/sh\ncase \"$1\" in */SHA256SUMS) exit 73 ;; esac\nexec \"$CORTEX_TEST_REAL_MV\" \"$@\"\n",
    );

    let archive_name = format!("cortex-v1.2.3-{}.tar.gz", host_target());
    let result = run_with_manifest_move_failure(root.path(), &binary, &output_dir, &mv_path);

    assert!(!result.status.success(), "manifest publication must fail");
    assert!(!output_dir.join(archive_name).exists());
    assert_eq!(
        fs::read(output_dir.join("SHA256SUMS")).unwrap(),
        old_manifest
    );
    assert_eq!(fs::read(previous_asset).unwrap(), previous_asset_contents);
    assert!(fs::read_dir(&output_dir).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".cortex-package.")
    }));
}
