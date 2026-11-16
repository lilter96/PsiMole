use std::io::{self, Write};
use std::process::{Command, Stdio};

const CHAIN: &str = "CLAUDE_AT_RAW";
const BACKENDS: [(&str, &str); 2] = [
    (
        "/usr/sbin/iptables-legacy",
        "/usr/sbin/iptables-legacy-restore",
    ),
    (
        "/usr/sbin/ip6tables-legacy",
        "/usr/sbin/ip6tables-legacy-restore",
    ),
];

// A separate kernel ruleset from nftables. The filter runs after OUTPUT
// destination NAT, so the old public proxy addresses keep working when
// redirected to Rust, but cannot reach the original listeners after a flush.
pub fn protect(socks: u16, http: u16, gid: u32) -> io::Result<()> {
    if socks == 0 || http == 0 || socks == http || gid == 0 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let rule = format!(
        "-A {CHAIN} -p tcp -m addrtype --dst-type LOCAL -m multiport --dports {socks},{http} \
         -m owner ! --gid-owner {gid} -j REJECT --reject-with tcp-reset"
    );
    update(Some(&rule))
}

pub fn remove() -> io::Result<()> {
    update(None)
}

fn update(rule: Option<&str>) -> io::Result<()> {
    // Prepare and syntax-check both families before changing either one.
    let mut transactions = Vec::new();
    for (list, restore) in BACKENDS {
        let result = Command::new(list)
            .args(["-w", "5", "-t", "filter", "-S"])
            .output()?;
        if !result.status.success() {
            return Err(failure(list, &result.stderr));
        }
        let rules = String::from_utf8(result.stdout).map_err(io::Error::other)?;
        if let Some(transaction) = transaction(&rules, rule) {
            apply(restore, &transaction, true)?;
            transactions.push((restore, transaction));
        }
    }
    for (restore, transaction) in transactions {
        apply(restore, &transaction, false)?;
    }
    Ok(())
}

fn transaction(existing: &str, rule: Option<&str>) -> Option<String> {
    let exists = existing.lines().any(|line| line == format!("-N {CHAIN}"));
    if !exists && rule.is_none() {
        return None;
    }
    let mut transaction = String::from("*filter\n");
    if rule.is_some() {
        transaction.push_str(&format!(":{CHAIN} - [0:0]\n"));
    }
    for _ in existing
        .lines()
        .filter(|line| *line == format!("-A OUTPUT -j {CHAIN}"))
    {
        transaction.push_str(&format!("-D OUTPUT -j {CHAIN}\n"));
    }
    transaction.push_str(&format!("-F {CHAIN}\n"));
    match rule {
        Some(rule) => transaction.push_str(&format!("{rule}\n-I OUTPUT 1 -j {CHAIN}\n")),
        None => transaction.push_str(&format!("-X {CHAIN}\n")),
    }
    transaction.push_str("COMMIT\n");
    Some(transaction)
}

fn apply(restore: &str, transaction: &str, test: bool) -> io::Result<()> {
    let mut command = Command::new(restore);
    command.args(["--wait", "5", "--noflush"]);
    if test {
        command.arg("--test");
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("missing firewall stdin"))?;
    let written = input.write_all(transaction.as_bytes());
    drop(input);
    let result = child.wait_with_output()?;
    if !result.status.success() {
        return Err(failure(restore, &result.stderr));
    }
    written
}

fn failure(command: &str, error: &[u8]) -> io::Error {
    io::Error::other(format!(
        "{command}: {}",
        String::from_utf8_lossy(error).trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reapplies_the_legacy_guard_ahead_of_existing_accepts_and_removes_only_its_chain() {
        let prior =
            "-P OUTPUT ACCEPT\n-N CLAUDE_AT_RAW\n-A OUTPUT -j ACCEPT\n-A OUTPUT -j CLAUDE_AT_RAW\n";
        let update = transaction(prior, Some("-A CLAUDE_AT_RAW -j REJECT")).unwrap();
        assert!(update.contains("-D OUTPUT -j CLAUDE_AT_RAW\n"));
        assert!(update.contains("-I OUTPUT 1 -j CLAUDE_AT_RAW\n"));
        assert!(!update.contains("-F OUTPUT"));
        assert!(!update.contains("-D OUTPUT -j ACCEPT"));
        assert_eq!(
            transaction(prior, None).unwrap(),
            "*filter\n-D OUTPUT -j CLAUDE_AT_RAW\n-F CLAUDE_AT_RAW\n-X CLAUDE_AT_RAW\nCOMMIT\n"
        );
    }
}
