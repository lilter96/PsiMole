# Country protection for Claude

On Ubuntu with systemd-resolved, nftables, iptables-legacy and dnsmasq-base, the optional Rust
guard routes known Claude traffic through Psiphon. It protects a single local
desktop account and needs root to install the firewall and DNS policy. The
main GNOME extension and Psiphon must already be installed.

Build as your normal desktop user, then install the guard. Replace `AT` with the
country you want to allow. `--firefox` is optional. It locks Firefox to the
guard's SOCKS listener on `127.0.0.1:11081` and requires an available Firefox policy slot.

```sh
cargo build --release --locked --manifest-path guard/Cargo.toml
sudo guard/target/release/claude-at --install "$(id -un)" AT --firefox
/usr/local/bin/claude-at --wrap-cli
```

`--wrap-cli` supports the native Claude Code installation in `~/.local/bin`.
The native installer can replace this symlink during an update. For a stable
launcher across those updates, use `/usr/local/bin/claude-at` or define a shell
alias for it. The domain firewall applies to direct launches as well.

In the mole's menu, open **Claude** and choose a country. The regular **Country**
row controls Psiphon. When they match, the guard verifies the new tunnel's
public IP before allowing requests. A failed verification remains closed. To
request a fresh check, select the same country in the Claude row.

The guard watches Psiphon's configuration, notices and process exit with Linux
inotify and pidfd. It closes active protected TCP connections when it loses the
approved tunnel. Every reconnect needs a fresh connected-region notice and
an HTTPS check of the exit IP. A new IP is also checked with IPinfo. There is
no recurring network probe while a tunnel stays connected.

The firewall redirects applications configured for Psiphon's original proxy
ports to the guard. A separate `iptables-legacy` OUTPUT rule rejects access
to the raw listeners from sockets outside the guard group, including if
`nft flush ruleset` removes the redirect. Firefox configured with `--firefox`
and the guarded Claude Code launcher already connect directly to the guard.

For native apps the firewall
uses locally resolved names, Anthropic's published inbound IP blocks, and
the hostname in HTTP or TLS to recognize requests with cached or alternate DNS,
including the [standard and alternate CDN web ports](https://developers.cloudflare.com/fundamentals/reference/network-ports/).
Connections without a usable name on these ports require a verified exit.
An encrypted hello with an IP destination also requires a verified exit.
UDP to the covered HTTPS ports is rejected for the protected account.
Separate containers, remote tunnels, arbitrary third-party gateways and direct
traffic outside these ports to shared addresses require their own routing policy.

The protected names are in [assets/domains.txt](assets/domains.txt). The core
zones come from [Claude Desktop](https://code.claude.com/docs/en/desktop#network-access-requirements)
and the [Claude Code network table](https://code.claude.com/docs/en/network-config#network-access-requirements).
Extra domains include the `clau.de` and `claude.new` redirects, the
`claudeacademy.com` redirect, and hosts found in
[community routing rules](https://github.com/v2fly/domain-list-community/blob/master/data/anthropic).
The firewall anchors the [published inbound IP ranges](https://platform.claude.com/docs/en/api/ip-addresses).
Shared hosts such as GitHub, npm and Google Cloud Storage are routed via the
guard when requested through the protected Claude launcher.

```sh
/usr/local/bin/claude-at --vpn-status
sudo /usr/local/bin/claude-at --uninstall
```

The firewall, DNS and runtime services start at boot. If you change Psiphon's
assigned local ports, rerun the installer so the firewall and Firefox policy
follow them. Restart Firefox and log out and back into GNOME after installing
new policy and extension files.
