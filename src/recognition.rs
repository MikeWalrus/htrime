use std::process::{Child, Command, Stdio};

pub fn run() -> Child {
    let mut command = Command::new("python");
    command
        .arg("adaptor.py")
        .current_dir("/home/mike/repos/third-party/SimpleHTR/src")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());

    command.spawn().expect("failed to execute child")
}
