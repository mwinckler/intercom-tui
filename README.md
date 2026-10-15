# Intercom TUI

A portable Rust Intercom inbox for **Linux (including Omarchy) and macOS**. Ported
from the Omarchy Intercom notifier, with a terminal interface instead of a bar/tray
widget. No Python, Qt, Quickshell, or Omarchy installation is required.

- Open, Waiting, Closed, Snoozed, and All inboxes, with complete pagination.
- Split conversation/thread reader, formatted messages, links, and local unread dots.
- Markdown replies and private notes, live preview, clipboard editing, and saved drafts.
- Scheduled replies/notes with typed dates or a keyboard/mouse calendar.
- Local full-thread search, optional Intercom API search, and a downloadable archive.
- Native desktop notifications with sender/preview and conversation click-through.
- Personal OAuth sign-in, teammate profile, and automatic US/EU/AU region detection.

**Foreground only:** close the TUI and polling, notifications, archive downloads,
and scheduled sends stop. Saved pending schedules resume when you next launch it.
There is no daemon, tray icon, autostart service, or background scheduler.

## Build and run

Building requires Rust **1.90+** and a C compiler (GCC/Clang, or Xcode command-line
tools on macOS). Use a UTF-8 terminal; **80×24 or larger** is recommended.

```sh
cargo build --release --locked
./target/release/intercom-tui

# Or install onto PATH:
cargo install --path . --locked

# Try the UI without signing in or making network requests:
cargo run --locked -- --demo --data-dir /tmp/intercom-demo
```

Linux desktop notifications use the session's standard D-Bus notification service
(the same system notification service used by Omarchy). Clipboard editing uses
`wl-copy`/`wl-paste` on Wayland, or `xclip` on X11. Those commands are optional:
terminal bracketed paste and the terminal's own copy/paste shortcuts still work.
Notifications are optional; a missing notification service does not stop the TUI.

### macOS notifications

The terminal interface runs directly from `cargo install`. For modern Notification
Center permissions and reliable notification click-through, create the bundled build:

```sh
bash scripts/bundle-macos.sh

# Run the bundled binary from your terminal:
"dist/Intercom TUI.app/Contents/MacOS/intercom-tui"
```

You can move `Intercom TUI.app` to `~/Applications/` and symlink its executable onto
your PATH. Run that executable **from a terminal**, not by double-clicking the app.
The script ad-hoc signs the bundle; it does not install a service or change your
desktop settings. Allow notifications when macOS requests permission.

The main macOS run loop is kept alive for notification callbacks. A bare, unbundled
binary falls back to legacy native toasts; modern permissions and click-through
require the bundle. Terminal activation is best effort: Hyprland focuses the
ancestor terminal window; macOS activates the detected terminal app. Other Linux
desktops still select the requested conversation, but may require you to focus the
terminal yourself. No second TUI is launched by a notification.

## Sign in

One teammate creates a private Intercom app in the
[Developer Hub](https://app.intercom.com/a/developers). Enable **Use OAuth**, register
the exact redirect URL **`http://localhost:18741/callback`**, and grant:

- **Read conversations**
- **Write conversations**
- **Read admins** (required to identify you and send as yourself)

Run `intercom-tui`; **F1 in Settings** shows the setup guide. Copy the shared app
Client ID and Client secret from the app's **Basic Information** page into Settings,
then press **F3**. Authorize in your browser; the TUI obtains and saves your token.
F6 copies the callback URL; F7 copies the authorization URL if the browser did not
open. Sign-in times out after 150 seconds. The loopback port must be available.

Intercom OAuth tokens have no timed expiry or refresh token. Revoking the app
requires signing in again. Replies always resolve the signed-in teammate using
`GET /me`; cached or manually supplied admin IDs are never used to impersonate
another teammate. Avoid shared workspace access tokens.

Workspace IDs for browser links are inferred automatically when available. If a
link cannot be built, paste an Intercom inbox URL into the Workspace field.

### Advanced token configuration

An existing personal OAuth token can be pasted into the masked token field in
Settings and saved with F2. Token resolution is:

1. `INTERCOM_ACCESS_TOKEN`
2. `--token-file PATH`, or the Access token file setting
3. The app's own `token` file
4. A `token` file beside the executable (portable deployments)

No credentials, caches, or other data are imported from the old Omarchy plugin.
Environment/external-file credentials take precedence and must be cleared before
using OAuth sign-in or sign-out. The app never deletes external token files.

## Controls

Press **?** or **F1** for complete in-app help.

| Context | Keys |
| --- | --- |
| Inbox/reader | `j`/`k` or arrows move; `h`/`l` switch panes |
| Filters | Tab / Shift+Tab, or `f` then `o`/`w`/`c`/`s`/`a` |
| Reader size | `e` toggles split/zoom; narrow terminals show one pane |
| Scroll | Ctrl+U / Ctrl+D, PgUp / PgDn; `g`/`G` start/end |
| Reply | Enter opens composer or existing draft |
| Close/reopen | `c`, then confirm |
| Browser | `o` conversation, `O` inbox, `L` choose a thread link |
| Settings/archive | `s` or Ctrl+,; `y` sync archive |
| Refresh/quit | `r`; `q` (drafts saved) |
| Composer | Enter newline; **F2**, Ctrl+Enter, or Alt+Enter sends |
| Note/schedule | Ctrl+N public/private; Ctrl+S opens scheduling |
| Draft | Esc saves; **F8** or Ctrl+Shift+D confirms discarding |
| Editing | Ctrl+W previous word; Ctrl+U line start; Ctrl+Z/Y undo/redo |
| Clipboard | Shift+arrows select, Ctrl+A/C/X/V; mouse-drag selection |
| Scheduled cards | Focus thread (`l`), `j`/`k` selects; Enter edits/reschedules; `x` cancels |
| Search | `/` types; Down/Tab results; **F6** toggles local/API; Esc returns |
| Settings | F2 save; F3 sign in; F4 sign out; F5 sync; Tab navigates |

Some terminals cannot distinguish Ctrl+Enter from Enter, Ctrl+I from Tab, or
Ctrl+Shift+D from Ctrl+D. Use F2, F6, and F8 instead. Enhanced keyboard events are
enabled when supported. Mouse capture can usually be bypassed with Shift for the
terminal's own text selection. Color and formatting do not require a Nerd Font.

## Scheduling and send safety

Examples: `in 2h`, `in 30 minutes`, `tomorrow at 8`, `Monday`, `Tuesday afternoon`,
`22:10`, `April 7`, or `2027-04-07 09:30`. Typed dates use the system's month/day
order unless overridden in Settings. The confirmation includes the local timezone.
Ambiguous/nonexistent times at daylight-saving transitions are rejected.

F6 in the schedule dialog switches between typing and the calendar. In the
calendar, arrows move days; PgUp/PgDn move months; Tab selects hour/minute, and
arrows adjust them. Enter reviews the time; Enter again commits it.

- A reply is checked against a newly fetched, complete thread immediately before
  sending. New comments **or notes** block it, retaining your draft for review.
- Scheduled messages retain the conversation snapshot from composition. New
  comments/notes cause **Needs review**, rather than sending an outdated reply.
- Read requests back off on transient errors/rate limits. **Writes are never
  automatically retried**, even after an HTTP error or timeout.
- A scheduled send is journaled before posting. An interrupted or failed send
  requires review; it cannot silently send twice after a restart.
- After an ambiguous interactive send, check Intercom before unlocking another
  attempt. A preflight check cannot eliminate a server-side race between checking
  a thread and posting; Intercom provides no atomic conditional-send operation.
- Due messages are held while you are composing or answering a confirmation, and
  are sent serially. Keep the TUI open and the machine awake to send on time.

## Unread state, notifications, and search

Unread flags are local and do not modify Intercom's own read status. Selecting a
conversation in the focused TUI marks all loaded messages read; new messages
loaded while it remains selected are also read. Focus tracking depends on terminal
support for focus events. Only **Open** conversations can remain unread. Your own
replies and notes never create unread alerts. Unread conversations remain reachable
in their matching filter.

Native notifications announce newly observed **customer** messages in open
conversations, including follow-ups with unchanged waiting timestamps. Historical
loading/archiving and your own messages do not produce toasts. At most five toasts
are emitted per poll. The initial poll establishes a baseline instead of flooding
notifications. Notifications operate regardless of the current inbox filter.

Local search matches all query words across the cached/archived title, sender,
email, state, and **all message bodies**. Intercom API search uses the service's
source author/subject/body search semantics; it is not a full-thread server-side
search. Results are limited to 50 in either mode. Inbox lists have no 150-row limit:
every API page is fetched, and a pagination failure never installs a partial inbox.

**Sync all conversations** downloads every complete thread with progress. Closed
threads already archived are skipped; failures can be retried by running sync
again. Closed archives are not continuously watched for new replies. A conversation
reopened in Intercom reappears in normal Open polling.

## Storage and privacy

| Platform | Configuration | Account data |
| --- | --- | --- |
| Linux | `$XDG_CONFIG_HOME/intercom-tui` (normally `~/.config/intercom-tui`) | `$XDG_DATA_HOME/intercom-tui` (normally `~/.local/share/intercom-tui`) |
| macOS | `~/Library/Application Support/intercom-tui` | Same application directory |

`--data-dir PATH` instead uses `PATH/config/` and `PATH/data/` for portable installs
or testing. Config, app credentials, and the token are separate files; each
account/workspace has its own cache, drafts, scheduled queue, and archive.

App directories are private (`0700`), files are private (`0600`), and writes use
atomic replacement and disk synchronization. Tokens are stored locally in plaintext
with those permissions (not in a system keychain). The archive contains customer
data: protect the directory and any backups. There is no telemetry or message log.
Only one instance can use a data directory, preventing concurrent queue sends.
Sign-out retains local account data and shared app credentials, but clears the
active profile. Malformed storage files are reported rather than silently replaced.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
python3 tests/pty_smoke.py target/release/intercom-tui
```

The tests use mocked HTTP servers and isolated temporary data. They do not require
an Intercom account or send real customer messages. Linux/macOS CI compiles and
tests the platform-specific code. Native notification permissions, terminal focus,
clipboard integration, and real OAuth authorization additionally need a desktop
smoke test on each platform.

Licensed under MIT, preserving the original plugin's copyright.
