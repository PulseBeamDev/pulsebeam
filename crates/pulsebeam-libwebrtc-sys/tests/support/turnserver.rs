//! External fixture compatibility, not an engine or application executor.
use std::{process::Command, sync::OnceLock};

pub fn without_dtls() -> Command {
    static HAS_DISABLE_FLAG: OnceLock<bool> = OnceLock::new();
    let has_disable_flag = *HAS_DISABLE_FLAG.get_or_init(|| {
        let help = crate::tools::command("turnserver")
            .arg("-h")
            .output()
            .expect("coturn is required by the Linux runtime test image");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&help.stdout),
            String::from_utf8_lossy(&help.stderr)
        );
        if text.contains("--no-dtls") {
            true
        } else {
            assert!(
                text.contains("DTLS is not started by default"),
                "coturn provides neither --no-dtls nor a documented disabled DTLS default"
            );
            false
        }
    });
    let mut command = crate::tools::command("turnserver");
    if has_disable_flag {
        command.arg("--no-dtls");
    }
    command
}
