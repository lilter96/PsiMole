use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::net::IpAddr;
use std::time::Instant;

pub const REGIONS: &[&str] = &[
    "AT", "AU", "BE", "CA", "CH", "CZ", "DE", "DK", "ES", "FI", "FR", "GB", "ID", "IE", "IN", "IT",
    "JP", "KR", "LT", "NL", "NO", "PL", "RO", "RS", "SE", "SG", "US",
];

pub fn valid_region(code: &str) -> bool {
    code.len() == 2 && code.bytes().all(|byte| byte.is_ascii_uppercase())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    Claude,
    ChatGpt,
}

impl Service {
    pub const ALL: [Self; 2] = [Self::Claude, Self::ChatGpt];

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::ChatGpt => "ChatGPT",
        }
    }

    pub fn port(self) -> u16 {
        match self {
            Self::Claude => 18082,
            Self::ChatGpt => 18083,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct Countries {
    pub claude: Option<String>,
    pub chatgpt: Option<String>,
}

impl Countries {
    pub fn get(&self, service: Service) -> Option<&str> {
        match service {
            Service::Claude => self.claude.as_deref(),
            Service::ChatGpt => self.chatgpt.as_deref(),
        }
    }

    pub fn set(&mut self, service: Service, value: Option<String>) {
        match service {
            Service::Claude => self.claude = value,
            Service::ChatGpt => self.chatgpt = value,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    pub region: String,
    pub countries: Countries,
    pub system_proxy: bool,
    pub russian: Option<bool>,
}

impl Settings {
    pub fn valid(&self) -> bool {
        (self.region.is_empty() || valid_region(&self.region))
            && Service::ALL
                .into_iter()
                .all(|service| self.countries.get(service).is_none_or(valid_region))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Notice {
    Http(u16),
    Socks(u16),
    Tunnels(u64),
    Region(String),
    Regions(Vec<String>),
    Traffic { sent: u64, received: u64 },
    Exiting,
    PortInUse,
}

impl Notice {
    pub fn parse(line: &[u8]) -> Option<Self> {
        let notice: Value = serde_json::from_slice(line).ok()?;
        let data = &notice["data"];
        let port = || {
            u16::try_from(data["port"].as_u64()?)
                .ok()
                .filter(|port| *port != 0)
        };
        Some(match notice["noticeType"].as_str()? {
            "ListeningHttpProxyPort" => Self::Http(port()?),
            "ListeningSocksProxyPort" => Self::Socks(port()?),
            "Tunnels" => Self::Tunnels(data["count"].as_u64()?),
            "ConnectedServerRegion" => {
                let code = data["serverRegion"].as_str()?;
                if !valid_region(code) {
                    return None;
                }
                Self::Region(code.to_owned())
            }
            "AvailableEgressRegions" => Self::Regions(
                data["regions"]
                    .as_array()?
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|code| valid_region(code))
                    .map(str::to_owned)
                    .collect(),
            ),
            "BytesTransferred" => Self::Traffic {
                sent: data["sent"].as_u64()?,
                received: data["received"].as_u64()?,
            },
            "Exiting" => Self::Exiting,
            "HttpProxyPortInUse" | "SocksProxyPortInUse" => Self::PortInUse,
            _ => return None,
        })
    }

    pub fn changes_route(&self) -> bool {
        matches!(
            self,
            Self::Tunnels(_) | Self::Region(_) | Self::Exiting | Self::PortInUse
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct Tunnel {
    pub http: Option<u16>,
    pub socks: Option<u16>,
    pub region: Option<String>,
    pub connected_at: Option<Instant>,
    pub connected_once: bool,
    pub sent: u64,
    pub received: u64,
    pub port_in_use: bool,
    count: u64,
}

impl Tunnel {
    pub fn update(&mut self, notice: &Notice, now: Instant) {
        match notice {
            Notice::Http(port) => self.http = Some(*port),
            Notice::Socks(port) => self.socks = Some(*port),
            Notice::Tunnels(count) => {
                self.count = *count;
                self.region = None;
                self.connected_at = (*count == 1).then_some(now);
                self.connected_once |= *count == 1;
                self.sent = 0;
                self.received = 0;
            }
            Notice::Region(region) if self.count == 1 => self.region = Some(region.clone()),
            Notice::Traffic { sent, received } if self.count == 1 => {
                self.sent = *sent;
                self.received = *received;
            }
            Notice::PortInUse => self.port_in_use = true,
            Notice::Exiting => {
                self.count = 0;
                self.connected_at = None;
                self.region = None;
            }
            _ => {}
        }
    }

    pub fn guard_port(&self, selected: &str, allowed: &str) -> Option<u16> {
        if self.count == 1 && self.region.as_deref() == Some(allowed) && selected == allowed {
            self.socks
        } else {
            None
        }
    }
}

pub fn parse_trace(body: &str, country: &str) -> Option<IpAddr> {
    let field = |key| body.lines().find_map(|line| line.strip_prefix(key));
    (field("loc=")? == country).then_some(())?;
    field("ip=")?.parse().ok()
}

pub fn db_ip_country(body: &str, expected: IpAddr, country: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    let ip = value["ipAddress"]
        .as_str()
        .and_then(|ip| ip.parse::<IpAddr>().ok());
    ip == Some(expected) && value["countryCode"].as_str() == Some(country)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reconnect_needs_a_new_country_notice() {
        let mut tunnel = Tunnel::default();
        let now = Instant::now();
        tunnel.update(&Notice::Socks(1081), now);
        tunnel.update(&Notice::Tunnels(1), now);
        assert_eq!(tunnel.guard_port("AT", "AT"), None);
        tunnel.update(&Notice::Region("AT".into()), now);
        assert_eq!(tunnel.guard_port("AT", "AT"), Some(1081));
        assert_eq!(tunnel.guard_port("DE", "AT"), None);
        tunnel.update(&Notice::Tunnels(0), now);
        assert_eq!(tunnel.guard_port("AT", "AT"), None);
        tunnel.update(&Notice::Tunnels(1), now);
        tunnel.update(&Notice::Region("DE".into()), now);
        assert_eq!(tunnel.guard_port("AT", "AT"), None);
        tunnel.update(&Notice::Region("AT".into()), now);
        tunnel.update(&Notice::Tunnels(2), now);
        assert_eq!(tunnel.guard_port("AT", "AT"), None);
    }

    #[test]
    fn published_notices_carry_ports_traffic_and_regions() {
        assert_eq!(
            Notice::parse(br#"{"noticeType":"ListeningHttpProxyPort","data":{"port":8081}}"#),
            Some(Notice::Http(8081))
        );
        assert_eq!(
            Notice::parse(
                br#"{"noticeType":"BytesTransferred","data":{"sent":120,"received":2000}}"#
            ),
            Some(Notice::Traffic {
                sent: 120,
                received: 2000
            })
        );
        assert_eq!(
            Notice::parse(
                br#"{"noticeType":"AvailableEgressRegions","data":{"regions":["AT","-","US"]}}"#
            ),
            Some(Notice::Regions(vec!["AT".into(), "US".into()]))
        );
        assert!(
            Notice::parse(br#"{"noticeType":"ListeningSocksProxyPort","data":{"port":65536}}"#)
                .is_none()
        );
    }

    #[test]
    fn verification_checks_the_country_and_same_ip() {
        let trace = "ip=2001:db8::1\nloc=AT\n";
        let ip = parse_trace(trace, "AT").unwrap();
        assert_eq!(parse_trace(trace, "DE"), None);
        assert!(db_ip_country(
            r#"{"ipAddress":"2001:db8:0:0::1","countryCode":"AT"}"#,
            ip,
            "AT"
        ));
        assert!(!db_ip_country(
            r#"{"ipAddress":"2001:db8::2","countryCode":"AT"}"#,
            ip,
            "AT"
        ));
    }
}
