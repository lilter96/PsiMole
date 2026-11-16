# PsiMole for Windows

PsiMole runs as a Rust desktop app on Windows x64. Run `PsiMole-Setup-x64.exe`
and open **PsiMole** from the Start menu. Setup installs for your Windows user
and needs no administrator access. The first connection downloads the official
Psiphon engine and its configuration and checks their SHA-256 digests.

Choose an exit country and click **Connect**. Turn on **System proxy** to route
apps that follow your Windows proxy setting. For a manual browser setup, use
`127.0.0.1:8081` for HTTP and HTTPS or `127.0.0.1:1081` for SOCKS5. Both ports
are served directly by Psiphon.

The close button hides the window and leaves Psiphon running. Click the mole by
the clock or open PsiMole again to bring back the same window. Choose **Quit**
from the tray menu to stop the tunnel. Disconnecting restores the Windows proxy
setting PsiMole changed. A helper restores it after a process crash, and a
one-time logon entry covers an interrupted Windows session.

## Country locks

Set the allowed exit separately under **Claude** or **ChatGPT**. Configure an
HTTP and HTTPS proxy in each service's browser profile or in an app that
supports a proxy with no direct fallback.

| Service | HTTP proxy |
| --- | --- |
| Claude | `127.0.0.1:18082` |
| ChatGPT | `127.0.0.1:18083` |

In Firefox, create a separate profile for each service under `about:profiles`.
In that profile's **Network Settings**, enter the service's address in **Manual
proxy configuration**, use the same proxy for HTTPS, and keep its domains out of
the **No proxy for** list.

One tunnel serves both locks. Choose the same explicit country under **Exit
country** as the lock you want to use. A service can connect after Psiphon
reports that country and two sources confirm the exit IP's country. A disconnect,
reconnect, or route change closes protected TCP streams. Verification runs on
route changes and when a lock is first enabled.

This Windows build protects traffic sent through the dedicated HTTP CONNECT
ports. System proxy, plain Psiphon ports, Firefox's **No proxy** mode, native
apps that bypass their proxy, and UDP are outside this Windows protection.
The desktop client reacts to the official engine's notifications, whose
delivery follows the changes inside Psiphon. The [GNOME guard](../guard/README.md)
has a separate Linux firewall and DNS setup.

## Speed

Click **Measure speed** while connected to send 8 MB in each direction through
the regular Psiphon proxy and time both transfers. Compare results across several
runs on the same network and exit country. The selected Psiphon server, the
network route, and Psiphon's traffic limits affect the measured throughput.
The Windows client requests a 2 MiB SSH channel window to improve throughput
on high-delay links. Psiphon still chooses the server and transport for the
selected country.

## Build and test

On Windows x64 with the stable Rust toolchain, the MSVC build tools, and NSIS,
run the following from this directory.

```powershell
cargo test --locked
cargo build --locked --release
makensis.exe installer.nsi
```

The Windows workflow builds the installer and tests installing, hiding the
window, reopening it, quitting, removing the app, and restoring the proxy after
a crash. To exercise the real engine and measure an Austrian tunnel on a
Windows test machine with ports 8081 and 1081 free, run
`cargo test --locked live_austrian_tunnel -- --ignored --nocapture`.
The relay benchmark runs on Linux or Windows with
`cargo test --locked --release loopback_relay_throughput -- --ignored --nocapture`.

The application stores its configuration and Psiphon data under
`%LOCALAPPDATA%\PsiMole` so they survive uninstallation.
The [Psiphon binary](https://github.com/Psiphon-Labs/psiphon-tunnel-core-binaries/tree/33ff79e89260f3adf92e846b96b3369b86a819d8/windows)
runs as a separate process.
