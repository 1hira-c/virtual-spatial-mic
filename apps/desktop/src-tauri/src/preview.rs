//! Binary latest-frame protocol. No TCP listener, no JSON/base64 image copies.
use crate::{Shared, video};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, AtomicU64, Ordering},
    },
};
use tauri::{
    AppHandle, Manager,
    http::{Request, Response, StatusCode},
};

struct Cached {
    id: i64,
    width: usize,
    height: usize,
    bytes: Arc<Vec<u8>>,
}
#[derive(Default)]
pub struct Cache {
    image: Mutex<Option<Cached>>,
    metrics: Metrics,
}

#[derive(Default)]
struct Metrics {
    responses: AtomicU64,
    fresh: AtomicU64,
    last_qpc: AtomicI64,
    first_served_ns: AtomicI64,
    recent: Mutex<VecDeque<i64>>,
}
impl Metrics {
    fn served(&self, qpc: i64, now: i64) {
        self.responses.fetch_add(1, Ordering::Relaxed);
        if self.last_qpc.swap(qpc, Ordering::Relaxed) == qpc {
            return;
        }
        self.fresh.fetch_add(1, Ordering::Relaxed);
        let _ = self
            .first_served_ns
            .compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed);
        // Never delay the client, Studio, audio callback or capture to collect
        // diagnostics. Dropped window samples do not affect lifetime counters.
        if let Ok(mut recent) = self.recent.try_lock() {
            while recent.front().is_some_and(|at| now - *at > 4_000_000_000) {
                recent.pop_front();
            }
            recent.push_back(now);
            while recent.len() > 256 {
                recent.pop_front();
            }
        }
    }
    fn snapshot(&self, now: i64) -> Value {
        let fresh = self.fresh.load(Ordering::Relaxed);
        let first = self.first_served_ns.load(Ordering::Relaxed);
        let elapsed = if first > 0 {
            (now - first).max(0) as f64 / 1e9
        } else {
            0.
        };
        let window = self.recent.try_lock().ok().map(|recent| {
            let mut times = recent.iter().filter(|at| now - **at <= 4_000_000_000);
            let first = times.next().copied();
            let count = first.is_some() as usize + times.count();
            let fps = if count >= 2 {
                (count - 1) as f64 * 1e9 / (now - first.unwrap()).max(1) as f64
            } else {
                0.
            };
            (count, fps)
        });
        json!({"fresh_frames_served":fresh,"responses_200":self.responses.load(Ordering::Relaxed),"recent_window_seconds":4,"recent_frames":window.map(|v|v.0),"recent_fps":window.map(|v|v.1),"elapsed_seconds":elapsed,"lifetime_fps":if fresh>1&&elapsed>0.{(fresh-1)as f64/elapsed}else{0.},"last_frame_qpc_ns":self.last_qpc.load(Ordering::Relaxed).to_string()})
    }
}
impl Cache {
    pub fn metrics(&self) -> Value {
        self.metrics.snapshot(vsm_live::clock::now_ns())
    }
}

fn origin(request: &Request<Vec<u8>>) -> Option<&str> {
    let value = request.headers().get("Origin")?.to_str().ok()?;
    [
        "http://tauri.localhost",
        "https://tauri.localhost",
        "tauri://localhost",
    ]
    .contains(&value)
    .then_some(value)
}
fn after(request: &Request<Vec<u8>>) -> Option<i64> {
    request.uri().query()?.split('&').find_map(|pair| {
        pair.strip_prefix("after=")
            .and_then(|value| value.parse().ok())
    })
}
fn response(
    request: &Request<Vec<u8>>,
    status: StatusCode,
    body: Vec<u8>,
    frame: Option<(i64, usize, usize)>,
) -> Response<Vec<u8>> {
    let mut builder = Response::builder()
        .status(status)
        .header("Cache-Control", "no-store")
        .header("Content-Type", "image/bmp")
        .header(
            "Access-Control-Expose-Headers",
            "X-VSM-Frame-Ns, X-VSM-Width, X-VSM-Height",
        )
        .header("X-Content-Type-Options", "nosniff");
    if let Some(origin) = origin(request) {
        builder = builder.header("Access-Control-Allow-Origin", origin);
    }
    if let Some((id, width, height)) = frame {
        builder = builder
            .header("X-VSM-Frame-Ns", id.to_string())
            .header("X-VSM-Width", width.to_string())
            .header("X-VSM-Height", height.to_string());
    }
    builder.body(body).unwrap()
}
pub fn serve(app: &AppHandle, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
    if request.method() != "GET" || request.uri().path() != "/frame" {
        return response(&request, StatusCode::NOT_FOUND, Vec::new(), None);
    }
    if request.headers().contains_key("Origin") && origin(&request).is_none() {
        return response(&request, StatusCode::FORBIDDEN, Vec::new(), None);
    }
    let frame = app.try_state::<Shared>().and_then(|studio| {
        studio
            .try_lock()
            .ok()
            .and_then(|studio| studio.video.preview_frame())
    });
    let Some(frame) = frame else {
        return response(&request, StatusCode::NO_CONTENT, Vec::new(), None);
    };
    if after(&request).is_some_and(|id| frame.qpc_ns <= id) {
        return response(&request, StatusCode::NO_CONTENT, Vec::new(), None);
    }
    let Some(cache) = app.try_state::<Cache>() else {
        return response(&request, StatusCode::SERVICE_UNAVAILABLE, Vec::new(), None);
    };
    // This is an independent image cache lock. Studio/audio locks are gone.
    let Ok(mut image) = cache.image.try_lock() else {
        return response(&request, StatusCode::NO_CONTENT, Vec::new(), None);
    };
    if image
        .as_ref()
        .is_none_or(|cached| cached.id != frame.qpc_ns)
    {
        let Some(bitmap) = video::bitmap(&frame) else {
            return response(&request, StatusCode::NO_CONTENT, Vec::new(), None);
        };
        *image = Some(Cached {
            id: frame.qpc_ns,
            width: bitmap.width,
            height: bitmap.height,
            bytes: Arc::new(bitmap.bytes),
        });
    }
    let cached = image.as_ref().unwrap();
    let id = cached.id;
    let response = response(
        &request,
        StatusCode::OK,
        (*cached.bytes).clone(),
        Some((cached.id, cached.width, cached.height)),
    );
    drop(image);
    cache.metrics.served(id, vsm_live::clock::now_ns());
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transport_metrics_count_unique_frames_and_age_out_without_blocking() {
        let metrics = Metrics::default();
        metrics.served(100, 1_000_000_000);
        metrics.served(100, 1_010_000_000);
        metrics.served(101, 1_100_000_000);
        metrics.served(102, 1_200_000_000);
        let value = metrics.snapshot(1_200_000_000);
        assert_eq!(value["fresh_frames_served"], 3);
        assert_eq!(value["responses_200"], 4);
        assert_eq!(value["recent_fps"], 10.);
        assert_eq!(value["lifetime_fps"], 10.);
        assert_eq!(value["last_frame_qpc_ns"], "102");
        let guard = metrics.recent.lock().unwrap();
        metrics.served(103, 1_300_000_000);
        let value = metrics.snapshot(1_300_000_000);
        assert_eq!(value["fresh_frames_served"], 4);
        assert!(value["recent_fps"].is_null());
        drop(guard);
        assert_eq!(metrics.snapshot(6_000_000_000)["recent_fps"], 0.);
    }
    #[test]
    fn query_clock_is_exact_and_unrelated_origins_are_rejected() {
        let request = Request::builder()
            .uri("http://vsm-preview.localhost/frame?after=9007199254740993")
            .header("Origin", "http://tauri.localhost")
            .body(Vec::new())
            .unwrap();
        assert_eq!(after(&request), Some(9007199254740993));
        assert_eq!(origin(&request), Some("http://tauri.localhost"));
        let response = response(
            &request,
            StatusCode::OK,
            vec![1, 2, 3],
            Some((123, 960, 540)),
        );
        assert_eq!(response.headers()["X-VSM-Frame-Ns"], "123");
        assert_eq!(
            response.headers()["Access-Control-Allow-Origin"],
            "http://tauri.localhost"
        );
        assert_eq!(response.body(), &[1, 2, 3]);
        let other = Request::builder()
            .uri("http://vsm-preview.localhost/frame")
            .header("Origin", "https://example.com")
            .body(Vec::new())
            .unwrap();
        assert!(origin(&other).is_none());
    }
}
