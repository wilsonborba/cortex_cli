use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "cortex-cli-installer-{label}-{}-{nonce}",
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

fn make_archive(root: &Path, entry: EntryKind, extra_member: bool) -> PathBuf {
    let contents = root.join("archive-contents");
    fs::create_dir_all(&contents).unwrap();
    let executable = contents.join("cortex");
    match entry {
        EntryKind::Executable => make_executable(&executable, "#!/bin/sh\nprintf fixture\n"),
        EntryKind::NonExecutable => fs::write(&executable, b"not executable").unwrap(),
        EntryKind::Symlink => {
            let external = root.join("external-target");
            make_executable(&external, "#!/bin/sh\nprintf external\n");
            std::os::unix::fs::symlink(&external, &executable).unwrap();
        }
    }
    if extra_member {
        fs::write(contents.join("extra"), b"extra").unwrap();
    }
    let archive = root.join("release.tar.gz");
    let status = Command::new("tar")
        .args(["-czf"])
        .arg(&archive)
        .args(["-C"])
        .arg(&contents)
        .arg("cortex")
        .args(extra_member.then_some("extra"))
        .status()
        .unwrap();
    assert!(status.success());
    archive
}

#[derive(Clone, Copy)]
enum EntryKind {
    Executable,
    NonExecutable,
    Symlink,
}

fn manifest(root: &Path, archive: &Path, digest: Option<&str>, duplicate: bool) -> PathBuf {
    let bytes = fs::read(archive).unwrap();
    let actual = format!("{:x}", Sha256::digest(bytes));
    let digest = digest.unwrap_or(&actual);
    let name = "cortex-v0.1.0-x86_64-unknown-linux-gnu.tar.gz";
    let line = format!("{digest}  {name}\n");
    let path = root.join("SHA256SUMS");
    fs::write(
        &path,
        if duplicate {
            format!("{line}{line}")
        } else {
            line
        },
    )
    .unwrap();
    path
}

fn installer_dir(root: &Path, fail_install: bool, fail_move: bool) -> PathBuf {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let curl = bin.join("curl");
    fs::write(
        &curl,
        r##"#!/bin/sh
url=""
output=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output) shift; output="$1" ;;
    http*) url="$1" ;;
  esac
  shift
done
case "$url" in
  */SHA256SUMS) cp "$CORTEX_FIXTURE_CHECKSUMS" "$output" ;;
  *) cp "$CORTEX_FIXTURE_ARCHIVE" "$output" ;;
esac
"##,
    )
    .unwrap();
    let mut permissions = fs::metadata(&curl).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&curl, permissions).unwrap();
    if fail_install {
        let install = bin.join("install");
        fs::write(
            &install,
            "#!/bin/sh\nfor dest do :; done\nprintf partial > \"$dest\"\nexit 1\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&install).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&install, permissions).unwrap();
    }
    if fail_move {
        let mv = bin.join("mv");
        fs::write(&mv, "#!/bin/sh\nexit 1\n").unwrap();
        let mut permissions = fs::metadata(&mv).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&mv, permissions).unwrap();
    }
    bin
}

fn run_installer(
    root: &Path,
    archive: &Path,
    checksums: &Path,
    install_dir: &Path,
    fail_install: bool,
    fail_move: bool,
) -> Output {
    let bin = installer_dir(root, fail_install, fail_move);
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new("bash")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install.sh"))
        .args(["--version", "0.1.0", "--install-dir"])
        .arg(install_dir)
        .env("PATH", path)
        .env("CORTEX_FIXTURE_ARCHIVE", archive)
        .env("CORTEX_FIXTURE_CHECKSUMS", checksums)
        .output()
        .unwrap()
}

fn assert_no_staging_files(path: &Path) {
    if !path.exists() {
        return;
    }
    assert!(
        fs::read_dir(path).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".cortex.new.")),
        "failed install must clean its owned staging executable"
    );
}

#[test]
fn installer_accepts_only_checksum_verified_executable_regular_archive() {
    let root = TestDir::new("regular");
    let archive = make_archive(root.path(), EntryKind::Executable, false);
    let checksums = manifest(root.path(), &archive, None, false);
    let install_dir = root.path().join("install");
    let result = run_installer(
        root.path(),
        &archive,
        &checksums,
        &install_dir,
        false,
        false,
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let installed = install_dir.join("cortex");
    assert!(installed.is_file());
    assert!(fs::metadata(installed).unwrap().permissions().mode() & 0o111 != 0);
    assert_no_staging_files(&install_dir);
}

#[test]
fn installer_rejects_wrong_duplicate_and_extra_archive_checksums() {
    for (label, wrong_digest, duplicate, extra) in [
        ("wrong", true, false, false),
        ("duplicate", false, true, false),
        ("extra", false, false, true),
    ] {
        let root = TestDir::new(label);
        let archive = make_archive(root.path(), EntryKind::Executable, extra);
        let wrong = "0".repeat(64);
        let checksums = manifest(
            root.path(),
            &archive,
            wrong_digest.then_some(wrong.as_str()),
            duplicate,
        );
        let install_dir = root.path().join("install");
        let result = run_installer(
            root.path(),
            &archive,
            &checksums,
            &install_dir,
            false,
            false,
        );
        assert!(!result.status.success(), "fixture {label} must be rejected");
        assert!(!install_dir.join("cortex").exists());
        assert_no_staging_files(&install_dir);
    }
}

#[test]
fn installer_rejects_symlink_and_non_executable_archive_members() {
    for (label, entry) in [
        ("symlink", EntryKind::Symlink),
        ("non-executable", EntryKind::NonExecutable),
    ] {
        let root = TestDir::new(label);
        let archive = make_archive(root.path(), entry, false);
        let checksums = manifest(root.path(), &archive, None, false);
        let install_dir = root.path().join("install");
        let result = run_installer(
            root.path(),
            &archive,
            &checksums,
            &install_dir,
            false,
            false,
        );
        assert!(!result.status.success(), "fixture {label} must be rejected");
        assert!(!install_dir.join("cortex").exists());
        assert_no_staging_files(&install_dir);
    }
}

#[test]
fn installer_failures_clean_unique_staging_and_preserve_previous_executable() {
    for (label, fail_install, fail_move) in
        [("install-fails", true, false), ("move-fails", false, true)]
    {
        let root = TestDir::new(label);
        let archive = make_archive(root.path(), EntryKind::Executable, false);
        let checksums = manifest(root.path(), &archive, None, false);
        let install_dir = root.path().join("install");
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(install_dir.join("cortex"), b"previous executable").unwrap();
        let result = run_installer(
            root.path(),
            &archive,
            &checksums,
            &install_dir,
            fail_install,
            fail_move,
        );
        assert!(!result.status.success(), "fixture {label} must fail");
        assert_eq!(
            fs::read(install_dir.join("cortex")).unwrap(),
            b"previous executable"
        );
        assert_no_staging_files(&install_dir);
    }
}
