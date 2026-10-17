//! One-shot script execution with a timeout.

use std::process::Stdio;
use std::time::Duration;

use irmm_proto::{AgentMsg, Shell};
use tokio::io::AsyncWriteExt as _;
use uuid::Uuid;

/// Interpreter and arguments; the script body is passed on stdin or as a temp file.
fn command(shell: Shell, script: &std::path::Path) -> tokio::process::Command {
    let p = script.display().to_string();
    let (prog, args): (&str, Vec<String>) = match shell {
        Shell::Powershell => (
            if cfg!(windows) { "powershell.exe" } else { "pwsh" },
            vec!["-NoProfile".into(), "-NonInteractive".into(), "-ExecutionPolicy".into(), "Bypass".into(), "-File".into(), p],
        ),
        Shell::Cmd => ("cmd.exe", vec!["/C".into(), p]),
        Shell::Bash => ("bash", vec![p]),
        Shell::Sh => ("sh", vec![p]),
        Shell::Zsh => ("zsh", vec![p]),
        Shell::Python => (if cfg!(windows) { "python" } else { "python3" }, vec![p]),
    };
    let mut c = tokio::process::Command::new(prog);
    c.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

fn extension(shell: Shell) -> &'static str {
    match shell {
        Shell::Powershell => "ps1",
        Shell::Cmd => "cmd",
        Shell::Python => "py",
        _ => "sh",
    }
}

pub async fn run(run_id: Uuid, shell: Shell, body: &str, timeout: Duration) -> AgentMsg {
    let path = std::env::temp_dir().join(format!("irmm-{run_id}.{}", extension(shell)));
    let result = async {
        let mut f = tokio::fs::File::create(&path).await?;
        f.write_all(body.as_bytes()).await?;
        f.flush().await?;
        drop(f);
        let child = command(shell, &path).spawn()?;
        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(out) => anyhow::Ok(out?),
            Err(_) => anyhow::bail!("timed out after {}s", timeout.as_secs()),
        }
    }
    .await;
    let _ = tokio::fs::remove_file(&path).await;
    match result {
        Ok(out) => AgentMsg::ScriptResult {
            run_id,
            exit_code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            error: None,
        },
        Err(e) => AgentMsg::ScriptResult {
            run_id,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(format!("{e:#}")),
        },
    }
}
