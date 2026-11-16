use reqwest::blocking::Client;
use std::io::{self, Read};
use std::time::{Duration, Instant};

const SAMPLE_BYTES: u64 = 8_000_000;
const DOWNLOAD_URL: &str = "https://speed.cloudflare.com/__down?bytes=8000000";
const UPLOAD_URL: &str = "https://speed.cloudflare.com/__up";

#[derive(Clone, Copy, Debug)]
pub struct Speed {
    pub down_mbps: f64,
    pub up_mbps: f64,
}

pub fn measure(port: u16, still_connected: impl Fn() -> bool) -> io::Result<Speed> {
    let proxy =
        reqwest::Proxy::all(format!("http://127.0.0.1:{port}")).map_err(io::Error::other)?;
    let client = Client::builder()
        .no_proxy()
        .proxy(proxy)
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(io::Error::other)?;

    let start = Instant::now();
    let response = client
        .get(DOWNLOAD_URL)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(io::Error::other)?;
    let bytes = io::copy(&mut response.take(SAMPLE_BYTES + 1), &mut io::sink())?;
    let download_time = start.elapsed();
    if bytes != SAMPLE_BYTES {
        return Err(io::Error::other("Incomplete download sample"));
    }
    if !still_connected() {
        return Err(io::Error::other("The tunnel changed during the speed test"));
    }

    let data = vec![0_u8; SAMPLE_BYTES as usize];
    let start = Instant::now();
    client
        .post(UPLOAD_URL)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(data)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(io::Error::other)?;
    Ok(Speed {
        down_mbps: mbps(bytes, download_time),
        up_mbps: mbps(SAMPLE_BYTES, start.elapsed()),
    })
}

fn mbps(bytes: u64, time: Duration) -> f64 {
    bytes as f64 * 8.0 / time.as_secs_f64() / 1_000_000.0
}
