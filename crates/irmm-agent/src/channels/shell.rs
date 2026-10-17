//! Interactive PTY. Runs as the agent's account (SYSTEM / root when installed as a service).

use std::io::{Read as _, Write as _};

use anyhow::Context as _;
use irmm_proto::{Shell, ShellFrame, read_frame, write_frame};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::mpsc;

use super::{BoxRead, BoxWrite};

fn default_command(shell: Option<Shell>) -> CommandBuilder {
    let prog = match shell {
        Some(Shell::Powershell) => if cfg!(windows) { "powershell.exe" } else { "pwsh" }.to_owned(),
        Some(Shell::Cmd) => "cmd.exe".to_owned(),
        Some(Shell::Bash) => "bash".to_owned(),
        Some(Shell::Zsh) => "zsh".to_owned(),
        Some(Shell::Sh) => "sh".to_owned(),
        Some(Shell::Python) => "python3".to_owned(),
        None if cfg!(windows) => "powershell.exe".to_owned(),
        None => std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".to_owned()),
    };
    let mut cmd = CommandBuilder::new(prog);
    if !cfg!(windows) {
        cmd.env("TERM", "xterm-256color");
    }
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        cmd.cwd(home);
    }
    cmd
}

pub async fn serve(mut r: BoxRead, mut w: BoxWrite) -> anyhow::Result<()> {
    let (shell, cols, rows) = match read_frame::<_, ShellFrame>(&mut r).await?.context("no start frame")? {
        ShellFrame::Start { shell, cols, rows } => (shell, cols, rows),
        _ => anyhow::bail!("expected start frame"),
    };
    let pty = native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
    let mut child = pty.slave.spawn_command(default_command(shell))?;
    drop(pty.slave);
    let mut reader = pty.master.try_clone_reader()?;
    let mut writer = pty.master.take_writer()?;
    let master = pty.master;

    // PTY output → client.
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let (exit_tx, mut exit_rx) = tokio::sync::oneshot::channel();
    let mut killer = child.clone_killer();
    std::thread::spawn(move || {
        let code = child.wait().ok().map(|s| s.exit_code() as i32);
        let _ = exit_tx.send(code);
    });
    // Client input → PTY.
    let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        while let Ok(b) = in_rx.recv() {
            if writer.write_all(&b).and_then(|_| writer.flush()).is_err() {
                break;
            }
        }
    });

    let result = loop {
        tokio::select! {
            out = out_rx.recv() => match out {
                Some(bytes) => write_frame(&mut w, &ShellFrame::data(&bytes)).await?,
                None => {
                    // Output closed; report the exit code once the process is reaped.
                    let code = (&mut exit_rx).await.ok().flatten();
                    write_frame(&mut w, &ShellFrame::Exit { code }).await?;
                    break Ok(());
                }
            },
            code = &mut exit_rx => {
                // Drain output that is already buffered before reporting exit.
                while let Ok(bytes) = out_rx.try_recv() {
                    write_frame(&mut w, &ShellFrame::data(&bytes)).await?;
                }
                write_frame(&mut w, &ShellFrame::Exit { code: code.ok().flatten() }).await?;
                break Ok(());
            }
            frame = read_frame::<_, ShellFrame>(&mut r) => match frame? {
                Some(ShellFrame::Data { data }) => { let _ = in_tx.send(ShellFrame::decode_data(&data)?); }
                Some(ShellFrame::Resize { cols, rows }) => {
                    master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
                }
                Some(_) => {}
                None => break Ok(()),
            },
        }
    };
    let _ = killer.kill();
    result
}
