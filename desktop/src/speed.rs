use reqwest::blocking::{Body, Client, Response};
use std::io::{self, Read};
use std::num::NonZeroU16;
use std::thread;
use std::time::{Duration, Instant};

const STREAMS: usize = 4;
const PILOT_BYTES: u64 = 1_000_000;
const SAMPLE_BYTES: u64 = 8_000_000;
const SAMPLE_THRESHOLD_MBPS: f64 = 16.0;
const DOWNLOAD_URL: &str = "https://speed.cloudflare.com/__down";
const UPLOAD_URL: &str = "https://speed.cloudflare.com/__up";

#[derive(Clone, Copy)]
pub enum Route {
    Direct,
    Proxy(NonZeroU16),
}

#[derive(Clone, Copy, Debug)]
pub struct Speed {
    pub down_mbps: f64,
    pub up_mbps: f64,
}

pub fn measure(route: Route, still_connected: impl Fn() -> bool + Sync) -> io::Result<Speed> {
    let mut builder = Client::builder()
        .no_proxy()
        .https_only(true)
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(60));
    if let Route::Proxy(port) = route {
        let proxy =
            reqwest::Proxy::all(format!("http://127.0.0.1:{port}")).map_err(io::Error::other)?;
        builder = builder.proxy(proxy);
    }
    let client = builder.build().map_err(io::Error::other)?;

    let down_mbps = measure_direction(&still_connected, |size| {
        let response = require_success(client.get(DOWNLOAD_URL).query(&[("bytes", size)]).send())?;
        let bytes = io::copy(&mut response.take(size + 1), &mut io::sink())?;
        if bytes != size {
            return Err(io::Error::other("Incomplete download sample"));
        }
        Ok(())
    })?;

    let up_mbps = measure_direction(&still_connected, |size| {
        let body = Body::sized(io::repeat(0).take(size), size);
        require_success(
            client
                .post(UPLOAD_URL)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(body)
                .send(),
        )?;
        Ok(())
    })?;

    Ok(Speed { down_mbps, up_mbps })
}

fn measure_direction(
    still_connected: &(impl Fn() -> bool + Sync),
    transfer: impl Fn(u64) -> io::Result<()> + Sync,
) -> io::Result<f64> {
    let run_sample = |size| {
        measure_phase(still_connected, || {
            transfer(size)?;
            Ok(size)
        })
    };
    let initial = run_sample(PILOT_BYTES)?;
    if initial >= SAMPLE_THRESHOLD_MBPS {
        run_sample(SAMPLE_BYTES)
    } else {
        Ok(initial)
    }
}

fn require_success(response: reqwest::Result<Response>) -> io::Result<Response> {
    let response = response.map_err(io::Error::other)?;
    if !response.status().is_success() {
        return Err(io::Error::other(format!(
            "Speed test endpoint returned {}",
            response.status()
        )));
    }
    Ok(response)
}

fn require_route(still_connected: &impl Fn() -> bool) -> io::Result<()> {
    if still_connected() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "The tunnel changed during the speed test",
        ))
    }
}

fn measure_phase(
    still_connected: &(impl Fn() -> bool + Sync),
    transfer: impl Fn() -> io::Result<u64> + Sync,
) -> io::Result<f64> {
    require_route(still_connected)?;
    let start = Instant::now();
    let bytes = thread::scope(|scope| {
        let workers: [_; STREAMS] = std::array::from_fn(|_| {
            scope.spawn(|| {
                require_route(still_connected)?;
                transfer()
            })
        });
        workers.into_iter().try_fold(0, |total, worker| {
            let bytes = worker
                .join()
                .map_err(|_| io::Error::other("Transfer worker stopped"))??;
            Ok::<_, io::Error>(total + bytes)
        })
    })?;
    require_route(still_connected)?;
    Ok(bytes as f64 * 8.0 / start.elapsed().as_secs_f64() / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};

    #[test]
    fn transfers_overlap() {
        let arrivals = Mutex::new(0);
        let ready = Condvar::new();
        let speed = measure_phase(&|| true, || {
            let mut running = arrivals.lock().unwrap();
            *running += 1;
            ready.notify_all();
            let (running, _) = ready
                .wait_timeout_while(running, Duration::from_secs(2), |count| *count < STREAMS)
                .unwrap();
            assert_eq!(*running, STREAMS);
            Ok(SAMPLE_BYTES)
        })
        .unwrap();
        assert!(speed.is_finite() && speed > 0.0);
    }

    #[test]
    fn a_changed_route_invalidates_the_whole_measurement() {
        let completed = AtomicUsize::new(0);
        let error = measure_phase(&|| completed.load(Ordering::Relaxed) < STREAMS, || {
            completed.fetch_add(1, Ordering::Relaxed);
            Ok(SAMPLE_BYTES)
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn an_invalid_route_never_starts_a_transfer() {
        let error = measure_phase(&|| false, || panic!("unexpected transfer")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }
}
