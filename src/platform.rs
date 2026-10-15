use crate::{
    format,
    model::{Conversation, Kind, Message},
};
use anyhow::{Result, bail};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug)]
pub enum PlatformEvent {
    Open { id: String, generation: u64 },
    Warning { message: String, generation: u64 },
}

pub fn open_url(url: &str) -> Result<()> {
    if !format::safe_url(url) {
        bail!("Only http and https links can be opened");
    }
    webbrowser::open(url)
        .map_err(|_| anyhow::anyhow!("Could not open your browser. URL: {url}"))?;
    Ok(())
}

pub fn notify(
    conversation: &Conversation,
    message: &Message,
    generation: u64,
    tx: UnboundedSender<PlatformEvent>,
) {
    let id = conversation.id.clone();
    let title = format::preview(&message.from, 100);
    let title = if title.is_empty() {
        "Intercom".into()
    } else {
        title
    };
    let body = format::preview(&message.plain(), 120);
    tokio::spawn(async move {
        if let Err(error) = native_notify(&id, &title, &body, generation, tx.clone()).await {
            let _ = tx.send(PlatformEvent::Warning {
                message: format!("Desktop notification unavailable: {error}"),
                generation,
            });
        }
    });
}

/// Detect follow-ups even when Intercom does not change waiting_since.
pub fn new_customer_messages(previous: &Conversation, current: &Conversation) -> Vec<Message> {
    if current.state != crate::model::State::Open {
        return vec![];
    }
    let newest = previous
        .messages
        .iter()
        .map(|m| m.created_at)
        .max()
        .unwrap_or(previous.updated_at);
    current
        .messages
        .iter()
        .filter(|m| {
            m.kind == Kind::Customer
                && m.created_at >= newest
                && !previous.messages.iter().any(|old| old.id == m.id)
        })
        .cloned()
        .collect()
}

#[cfg(target_os = "linux")]
async fn native_notify(
    id: &str,
    title: &str,
    body: &str,
    generation: u64,
    tx: UnboundedSender<PlatformEvent>,
) -> Result<()> {
    use notify_rust::{Notification, NotificationResponse};
    let handle = Notification::new()
        .appname("Intercom")
        .summary(&format::escape(title))
        .body(&format::escape(body))
        .icon("mail-unread")
        .action("default", "Open conversation")
        .timeout(8000)
        .show_async()
        .await?;
    // Keep the D-Bus handle alive without blocking polling or terminal input.
    let _ = tokio::time::timeout(
        Duration::from_secs(3600),
        handle.wait_for_action_async(|response| {
            if matches!(response, NotificationResponse::Default)
                || matches!(response, NotificationResponse::Action(action) if action == "default")
            {
                let _ = tx.send(PlatformEvent::Open {
                    id: id.into(),
                    generation,
                });
            }
        }),
    )
    .await;
    Ok(())
}

#[cfg(target_os = "macos")]
async fn native_notify(
    id: &str,
    title: &str,
    body: &str,
    generation: u64,
    tx: UnboundedSender<PlatformEvent>,
) -> Result<()> {
    use mac_usernotifications::{Action, Notification};
    if mac_usernotifications::check_bundle().is_err() {
        // A bare cargo-installed executable can still show legacy native toasts.
        // The bundled build is required for modern permissions and click-through.
        let title = title.to_owned();
        let body = body.to_owned();
        tokio::task::spawn_blocking(move || {
            mac_notification_sys::send_notification(&title, Some("Intercom"), &body, None)
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!("{e}"))
        })
        .await??;
        return Ok(());
    }
    if !mac_usernotifications::request_auth().await? {
        bail!("Allow Intercom TUI notifications in macOS System Settings");
    }
    let handle = Notification::new()
        .title(title)
        .subtitle("Intercom")
        .message(body)
        .thread_id(id)
        .action(Action::button("open", "Open conversation"))
        .timeout(Duration::from_secs(3600))
        .send()
        .await?;
    let response = handle.response().await?;
    if response.close_reason.is_none() {
        let _ = tx.send(PlatformEvent::Open {
            id: id.into(),
            generation,
        });
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn native_notify(
    _: &str,
    _: &str,
    _: &str,
    _: u64,
    _: UnboundedSender<PlatformEvent>,
) -> Result<()> {
    bail!("Native notifications are currently supported on Linux and macOS")
}

pub fn focus_terminal() {
    // Best effort only: notification callbacks must never steal shell input or
    // create a second TUI. Match the window containing this process on Hyprland.
    #[cfg(target_os = "linux")]
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        tokio::spawn(async {
            if let Ok(output) = tokio::process::Command::new("hyprctl")
                .args(["clients", "-j"])
                .output()
                .await
                && let Ok(rows) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
            {
                let mut pid = std::process::id();
                for _ in 0..20 {
                    if let Some(window) = rows
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|row| row["pid"].as_u64() == Some(pid as u64))
                    {
                        if let Some(address) = window["address"].as_str().filter(|address| {
                            address.starts_with("0x")
                                && address[2..].chars().all(|c| c.is_ascii_hexdigit())
                        }) {
                            let _ = tokio::process::Command::new("hyprctl")
                                .args(["dispatch", "focuswindow", &format!("address:{address}")])
                                .output()
                                .await;
                        }
                        break;
                    }
                    let Ok(stat) = tokio::fs::read_to_string(format!("/proc/{pid}/stat")).await
                    else {
                        break;
                    };
                    let Some(parent) = stat
                        .rsplit_once(')')
                        .and_then(|(_, tail)| tail.split_whitespace().nth(1))
                        .and_then(|p| p.parse::<u32>().ok())
                    else {
                        break;
                    };
                    if parent <= 1 || parent == pid {
                        break;
                    }
                    pid = parent;
                }
            }
        });
    }
    #[cfg(target_os = "macos")]
    {
        let bundle = std::env::var("__CFBundleIdentifier")
            .ok()
            .filter(|s| s != "com.wildlands.intercom-tui")
            .or_else(
                || match std::env::var("TERM_PROGRAM").unwrap_or_default().as_str() {
                    "iTerm.app" => Some("com.googlecode.iterm2".into()),
                    "Apple_Terminal" => Some("com.apple.Terminal".into()),
                    "WezTerm" => Some("com.github.wez.wezterm".into()),
                    "ghostty" => Some("com.mitchellh.ghostty".into()),
                    _ => None,
                },
            );
        if let Some(bundle) = bundle {
            tokio::spawn(async move {
                let _ = tokio::process::Command::new("open")
                    .args(["-b", &bundle])
                    .output()
                    .await;
            });
        }
    }
}

pub fn copy(text: &str) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let candidates: Vec<(&str, Vec<&str>)> = if cfg!(target_os = "macos") {
        vec![("pbcopy", vec![])]
    } else {
        vec![
            ("wl-copy", vec![]),
            ("xclip", vec!["-selection", "clipboard"]),
        ]
    };
    for (program, args) in candidates {
        if let Ok(mut child) = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(text.as_bytes())?;
            }
            if child.wait()?.success() {
                return Ok(());
            }
        }
    }
    bail!("Clipboard unavailable. Install wl-clipboard/xclip, or use the terminal's copy shortcut.")
}

pub fn paste() -> Result<String> {
    use std::process::{Command, Stdio};
    let candidates: Vec<(&str, Vec<&str>)> = if cfg!(target_os = "macos") {
        vec![("pbpaste", vec![])]
    } else {
        vec![
            ("wl-paste", vec!["--no-newline"]),
            ("xclip", vec!["-selection", "clipboard", "-o"]),
        ]
    };
    for (program, args) in candidates {
        if let Ok(output) = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            && output.status.success()
        {
            return Ok(format::safe_text(&String::from_utf8_lossy(&output.stdout)));
        }
    }
    bail!("Clipboard unavailable. Paste using your terminal's paste shortcut.")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn followup_with_same_waiting_timestamp_notifies_but_not_historical_or_own_messages() {
        let mut previous = Conversation {
            id: "1".into(),
            waiting_since: 10,
            messages: vec![Message {
                id: "old".into(),
                created_at: 10,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut current = previous.clone();
        current.messages.push(Message {
            id: "followup".into(),
            created_at: 11,
            ..Default::default()
        });
        current.messages.push(Message {
            id: "older-history".into(),
            created_at: 1,
            ..Default::default()
        });
        current.messages.push(Message {
            id: "mine".into(),
            kind: Kind::Teammate,
            created_at: 12,
            ..Default::default()
        });
        assert_eq!(new_customer_messages(&previous, &current).len(), 1);
        previous = current.clone();
        assert!(new_customer_messages(&previous, &current).is_empty());
    }
}
