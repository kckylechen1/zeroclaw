# Browser Automation

This guide covers setting up browser automation capabilities in ZeroClaw, including both headless automation and GUI access via VNC.

## Overview

ZeroClaw supports multiple browser access methods:

| Method | Use Case | Requirements |
|--------|----------|--------------|
| **agent-browser CLI** | Headless automation, AI agents | npm, Chrome |
| **VNC + noVNC** | GUI access, debugging | Xvfb, x11vnc, noVNC |
| **Chrome Remote Desktop** | Remote GUI via Google | XFCE, Google account |

## Quick Start: Headless Automation

### 1. Install agent-browser

<div class="os-tabs-src">

#### Linux

```sh
# Install CLI
npm install -g agent-browser

# Download Chrome for Testing (includes system deps)
agent-browser install --with-deps
```

#### macOS / Windows

```sh
# Install CLI
npm install -g agent-browser

# Download Chrome for Testing
agent-browser install
```

</div>

### 2. Verify ZeroClaw Config

The browser tool is enabled by default with `allowed_domains = ["*"]`. Restrict domains or disable it via `zeroclaw config set`:

<div class="os-tabs-src">

#### sh

```sh
zeroclaw config set browser.allowed_domains '["example.com", "docs.example.com"]'
zeroclaw config set browser.enabled false
```

</div>

{{#config-where browser}}

> **URL scheme policy.** `browser_open` accepts both `http://` and `https://` URLs
> (matching `web_fetch`, `http_request`, and the `browser` tool, which already accept
> plaintext HTTP). The local/private-host block and the `allowed_domains` allowlist apply
> to `http://` exactly as they do to `https://`. Because `allowed_domains` defaults to
> `["*"]`, the shipped default permits opening **any public host over plaintext HTTP**.
> To reduce plaintext-HTTP exposure, replace the wildcard allowlist with the specific
> public hosts you trust. Note this **does not enforce HTTPS** for those hosts: the
> allowlist checks explicit-open hostnames, not schemes; HTTPS-only behavior for an allowed host requires
> that host to redirect/reject plaintext HTTP itself (HSTS, server-side 308), or a future
> scheme-aware browser policy.

For the `agent_browser` backend, set `browser.headed = true` to launch the browser in headed mode for debugging or first-time login setup, or `browser.headed = false` to force headless mode. When `browser.headed` is unset, Zeroclaw preserves the inherited `AGENT_BROWSER_HEADED` environment behavior. (The rust-native backend and its `browser.native_headless` setting were retired; `browser.native_*` keys now produce a retirement warning.)

See the [Config reference](../reference/config.md) for all browser fields and defaults.

### 3. Test

<div class="os-tabs-src">

#### sh

```sh
zeroclaw agent -a assistant -m "Open https://example.com and tell me what it says"
```

</div>

## Browser network boundary and retained SSRF risk

`browser.allowed_domains` checks the hostname in an explicit open URL. It does
not constrain the browser's network connections. An allowed hostname can resolve
to a private IP, redirect to another origin, or load private-network subresources.
A page can also navigate after a click or script. The computer-use endpoint
checks control the sidecar connection; they do not enforce the sidecar's
browser destinations.

This remains an accepted residual risk for the current implementation under
[security finding F7](https://github.com/kckylechen1/zeroclaw/issues/521), not a
claim that browser SSRF is fixed. The upstream snapshot
`1c1cd118ac536a1dd1bc79b9cce69cbdc04e4f29` also has no per-navigation DNS/IP
binding or browser egress enforcement to port. A check of the final URL after
navigation would occur after the network request and would not cover subresources
or subsequent navigation. It cannot supply the missing network boundary.

Before allowing untrusted web content, run the browser and computer-use sidecar
behind independently enforced network egress restrictions that deny loopback,
private, link-local and metadata destinations on every connection, including DNS
changes and redirects. Ensure explicitly authorized private destinations have a
separate, narrow policy. ZeroClaw does not provision or verify that enforcement.
If this boundary is unavailable, disable the browser tool with `browser.enabled =
false`; a smaller hostname allowlist alone does not close F7. Do not use the
browser to bypass restrictions encountered by `web_fetch`.

Acceptance of a future fix requires backend-level regression evidence for DNS
rebinding, redirects, click/script navigation and subresource requests, covering
both `agent_browser` and `computer_use`. URL-parser tests prove only the current
hostname/scheme contract. No real browser or installed network boundary was
validated as part of this source review.

## VNC Setup (GUI Access)

For debugging or when you need visual browser access:

### Install Dependencies

<div class="os-tabs-src">

#### Debian/Ubuntu

```sh
apt-get install -y xvfb x11vnc fluxbox novnc websockify

# Optional: Desktop environment for Chrome Remote Desktop
apt-get install -y xfce4 xfce4-goodies
```

</div>

### Start VNC Server

<div class="os-tabs-src">

#### bash

```bash
#!/bin/bash
# Start virtual display with VNC access

DISPLAY_NUM=99
VNC_PORT=5900
NOVNC_PORT=6080
RESOLUTION=1920x1080x24

# Start Xvfb
Xvfb :$DISPLAY_NUM -screen 0 $RESOLUTION -ac &
sleep 1

# Start window manager
fluxbox -display :$DISPLAY_NUM &
sleep 1

# Start x11vnc
x11vnc -display :$DISPLAY_NUM -rfbport $VNC_PORT -forever -shared -nopw -bg
sleep 1

# Start noVNC (web-based VNC)
websockify --web=/usr/share/novnc $NOVNC_PORT localhost:$VNC_PORT &

echo "VNC available at:"
echo "  VNC Client: localhost:$VNC_PORT"
echo "  Web Browser: http://localhost:$NOVNC_PORT/vnc.html"
```

</div>

### VNC Access

- **VNC Client**: Connect to `localhost:5900`
- **Web Browser**: Open `http://localhost:6080/vnc.html`

### Start Browser on VNC Display

<div class="os-tabs-src">

#### sh

```sh
DISPLAY=:99 google-chrome --no-sandbox https://example.com &
```

</div>

## Chrome Remote Desktop

### Install

<div class="os-tabs-src">

#### sh

```sh
# Download and install
wget https://dl.google.com/linux/direct/chrome-remote-desktop_current_amd64.deb
apt-get install -y ./chrome-remote-desktop_current_amd64.deb

# Configure session
echo "xfce4-session" > ~/.chrome-remote-desktop-session
chmod +x ~/.chrome-remote-desktop-session
```

</div>

### Setup

1. Visit <https://remotedesktop.google.com/headless>
2. Copy the "Debian Linux" setup command
3. Run it on your server
4. Start the service: `systemctl --user start chrome-remote-desktop`

### Remote Access

Go to <https://remotedesktop.google.com/access> from any device.

## Testing

### CLI Tests

<div class="os-tabs-src">

#### sh

```sh
# Basic open and close
agent-browser open https://example.com
agent-browser get title
agent-browser close

# Snapshot with refs
agent-browser open https://example.com
agent-browser snapshot -i
agent-browser close

# Screenshot
agent-browser open https://example.com
agent-browser screenshot /tmp/test.png
agent-browser close
```

</div>

### ZeroClaw Integration Tests

<div class="os-tabs-src">

#### sh

```sh
# Content extraction
zeroclaw agent -a assistant -m "Open https://example.com and summarize it"

# Navigation
zeroclaw agent -a assistant -m "Go to https://github.com/trending and list the top 3 repos"

# Form interaction
zeroclaw agent -a assistant -m "Go to Wikipedia, search for 'Rust programming language', and summarize"
```

</div>

## Troubleshooting

### "Element not found"

The page may not be fully loaded. Add a wait:

<div class="os-tabs-src">

#### sh

```sh
agent-browser open https://slow-site.com
agent-browser wait --load networkidle
agent-browser snapshot -i
```

</div>

### Cookie dialogs blocking access

Handle cookie consent first:

<div class="os-tabs-src">

#### sh

```sh
agent-browser open https://site-with-cookies.com
agent-browser snapshot -i
agent-browser click @accept_cookies  # Click the accept button
agent-browser snapshot -i  # Now get the actual content
```

</div>

## Security Notes

- `agent-browser` runs Chrome in headless mode with sandboxing
- For sensitive sites, use `--session-name` to persist auth state
- `browser.allowed_domains` checks explicit-open hostnames; it does not enforce browser network egress
- VNC ports (5900, 6080) should be behind a firewall or Tailscale

## Related

- [agent-browser Documentation](https://github.com/vercel-labs/agent-browser)
- [Config reference](../reference/config.md)
