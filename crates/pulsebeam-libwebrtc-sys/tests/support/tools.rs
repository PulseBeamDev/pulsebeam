use std::{path::PathBuf, process::Command};

pub fn command(name: &str) -> Command {
    let directory = std::env::var_os("PULSEBEAM_TEST_TOOLS").expect("declared Bazel fixture tools");
    Command::new(PathBuf::from(directory).join(name))
}
