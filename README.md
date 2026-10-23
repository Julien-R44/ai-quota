# AI Quota

A Windows tray app that shows your **Claude Code**, **Codex**, **Amp**, and **OpenCode Go** usage limits. Click the tray icon to open a Windows 11 flyout.

![The flyout, a session limit expanded into its usage chart, and the settings view](docs/screenshot.png)

## Features

- Session, weekly, and per-model limits, each with its reset time.
- A pace marker on every bar. It shows where you would be if you used the window evenly. A bar past the marker means you are burning too fast.
- Click a limit to expand its chart: recorded usage, the steady-pace line, and a projection of the recent rate to the reset.
- Free resets you have banked (Claude and Codex).
- Amp: Agent budget in dollars, Orb hours, Amp Free, and credits.
- OpenCode Go: five-hour, weekly, and monthly usage windows.
- Settings: English or French, light, dark, or system theme, start with Windows, tracked tools, refresh interval.
- Keys: `R` or `F5` refreshes. `Esc` goes back or closes.

## Build

```powershell
cargo build --release
.\target\release\ai-quota.exe
```

Pass `--open` to open the flyout on launch. Starting the exe again while it runs opens the flyout of the running instance.

Windows 11 hides new tray icons under the `^` overflow. To keep the icon visible, drag it onto the taskbar.

## Set up the tools

Claude Code and Codex need no setup. The app reads the credentials their CLIs already saved.

To track Amp:

1. Create an access token in your Amp settings and copy it.
2. In the flyout, open **Settings**, then select the gear next to **Amp**.
3. Select **Paste**.

The app also reads `AMP_API_KEY` when it is set.

To track OpenCode Go:

1. Create an API key in the [OpenCode console](https://opencode.ai/auth) and copy it.
2. In the flyout, open **Settings**, then select the gear next to **OpenCode**.
3. Select **Paste**.

The app also reads `OPENCODE_API_KEY` when it is set, and the key saved by `opencode auth login` in `auth.json` (older OpenCode versions; recent ones keep credentials in their database).

## Data sources

| Tool | Credentials | Endpoint |
|---|---|---|
| Claude Code | `%USERPROFILE%\.claude\.credentials.json` | `GET api.anthropic.com/api/oauth/usage` |
| Codex | `%USERPROFILE%\.codex\auth.json` | `GET chatgpt.com/backend-api/wham/usage` |
| Amp | Pasted token or `AMP_API_KEY` | `POST ampcode.com/api/internal?userDisplayBalanceInfo` |
| OpenCode Go | Pasted key, `OPENCODE_API_KEY`, or `auth.json` | `GET opencode.ai/zen/go/v1/usage` |

These endpoints are not documented and can change. They report usage for the whole account, so cloud tasks count too.

The app never starts a CLI. Each refresh is one HTTPS request per tool, made with WinHTTP, and HTTP redirects are refused.

`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, and `XDG_DATA_HOME` are respected.

### Claude token refresh

Claude Code refreshes its token while it runs. Once the token has expired, the app refreshes it the way CodexBar does and writes back only `accessToken`, `refreshToken`, and `expiresAt`. If Claude Code refreshed in the meantime, its token wins.

### Rate limits and outages

After a `429`, the app waits at least 5 minutes, or longer if `Retry-After` says so. The pause survives restarts. A manual refresh skips it.

During a pause, an outage, or a `5xx`, the flyout shows the last good response, up to 12 hours old, with its age.

### Amp text format

The Amp endpoint returns display text, not structured data. The app parses it with the patterns CodexBar uses. It also accepts any `a1.*` Orb size, because the Gigawatt plan counts Orb hours in `a1.xxlarge`.

## Resource use

Measured on the release build:

| | Value |
|---|---|
| Binary | ~510 KB |
| Working set at idle | < 1 MB |
| Private memory at idle | ~4.5 MB |
| Private memory with the flyout open | ~8–20 MB |
| CPU at idle | 0 |

The flyout is drawn with Direct2D in software, so no GPU driver loads into the process. It is created on click and destroyed on close, and the app then returns its memory to Windows.

Polling runs every 5 minutes by default, or every 2 minutes above 80 %. It keeps running while the session is locked, so the history sees cloud tasks.
