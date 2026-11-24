use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cortex"))
        .args(args)
        .output()
        .expect("CLI must start")
}

#[test]
fn help_is_plain_and_does_not_need_credentials_or_a_terminal() {
    for args in [vec![], vec!["--help"], vec!["-h"]] {
        let output = run(&args);
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("Usage: cortex"));
        assert!(!text.contains('\x1b'));
    }
}

#[test]
fn version_matches_the_package() {
    let output = run(&["--version"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("Cortex {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn invalid_arguments_fail_without_echoing_potential_credentials() {
    for args in [
        vec!["private-argument-value"],
        vec!["--help", "private-argument-value"],
        vec!["--version", "private-argument-value"],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("use --help"));
        assert!(!error.contains("private-argument-value"));
    }
}

#[test]
fn help_documents_voice_model_removal_and_space_dictation() {
    let output = run(&["--help"]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("voice --remove-model"));
    assert!(text.contains("hold Space to dictate"));
}

#[test]
fn voice_model_removal_command_is_parsed_without_exposing_arguments() {
    use cortex_cli::presentation::cli::commands::{self, Command};
    let args = ["voice", "--remove-model"]
        .into_iter()
        .map(std::ffi::OsString::from);
    assert!(matches!(
        commands::parse(args),
        Ok(Command::RemoveVoiceModel)
    ));
}

#[cfg(unix)]
#[test]
fn invalid_utf8_argument_fails() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let output = Command::new(env!("CARGO_BIN_EXE_cortex"))
        .arg(OsString::from_vec(vec![0xff]))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}
