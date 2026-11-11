//! Start at logon through a Task Scheduler task.
//!
//! The app needs administrator rights, so a `Run` registry entry would show
//! a UAC prompt at every logon. A task with "highest privileges" starts
//! elevated without one.

use anyhow::{Context, Result, bail};
use std::os::windows::process::CommandExt;
use std::process::Command;

const TASK: &str = "SwiftFind";
/// Command-line flag: start hidden in the tray.
pub const BACKGROUND_FLAG: &str = "--background";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn schtasks(args: &[&str]) -> Result<std::process::Output> {
    Command::new("schtasks.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("run schtasks")
}

fn check(out: std::process::Output) -> Result<()> {
    if out.status.success() {
        return Ok(());
    }
    let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
    bail!("schtasks failed: {msg}")
}

/// Whether the task exists and points at this executable.
pub fn is_enabled() -> bool {
    let Ok(out) = schtasks(&["/Query", "/TN", TASK, "/XML"]) else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    // The XML comes back in the console code page; only ASCII paths are
    // compared reliably, so accept any existing task if the path isn't ASCII.
    let xml = decode(&out.stdout).to_lowercase();
    match std::env::current_exe() {
        Ok(exe) if exe.to_string_lossy().is_ascii() => {
            xml.contains(&exe.to_string_lossy().to_lowercase())
        }
        _ => true,
    }
}

/// schtasks prints UTF-16 or the console code page depending on the version.
fn decode(b: &[u8]) -> String {
    let utf16 = b.starts_with(&[0xFF, 0xFE]) || (b.len() > 1 && b[1] == 0);
    if utf16 {
        let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(b).into_owned()
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn set(enabled: bool) -> Result<()> {
    if !enabled {
        let out = schtasks(&["/Delete", "/TN", TASK, "/F"])?;
        // Deleting a task that doesn't exist is fine.
        return if out.status.success() || !is_enabled() {
            Ok(())
        } else {
            check(out)
        };
    }
    let exe = std::env::current_exe().context("locate executable")?;
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(d), Ok(u)) => format!("{d}\\{u}"),
        (_, Ok(u)) => u,
        _ => bail!("unknown user name"),
    };
    let dir = exe
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    // Defaults of `schtasks /Create` would stop the app after 72 hours and
    // on battery power, and run it at below-normal priority.
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Start SwiftFind in the tray at logon.</Description></RegistrationInfo>
  <Triggers>
    <LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>"{exe}"</Command>
      <Arguments>{BACKGROUND_FLAG}</Arguments>
      <WorkingDirectory>{dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        user = escape(&user),
        exe = escape(&exe.display().to_string()),
        dir = escape(&dir),
    );
    let path = std::env::temp_dir().join(format!("swiftfind-task-{}.xml", std::process::id()));
    // UTF-16 LE with BOM, as the declaration says; schtasks reads it reliably.
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(&path, bytes).context("write task XML")?;
    let result = schtasks(&[
        "/Create",
        "/TN",
        TASK,
        "/XML",
        &path.to_string_lossy(),
        "/F",
    ]);
    std::fs::remove_file(&path).ok();
    check(result?)
}
