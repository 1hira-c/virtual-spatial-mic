//! Follows OBS recording state through obs-websocket v5 (built into OBS 28+).
//! The link carries control events only; audio reaches OBS through a normal
//! WASAPI output captured by an OBS audio source. No OBS code is linked.
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::ErrorKind,
    net::{TcpStream, ToSocketAddrs},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tungstenite::{Error as WsError, Message, WebSocket};

/// obs-websocket EventSubscription::General | EventSubscription::Outputs.
const EVENTS: u32 = 1 | 64;
const STATUS_REQUEST: &str = "vsm-record-status";
const AUTH_FAILED: u16 = 4009;

pub fn link_defaults() -> Value {
    json!({"schema_version":1,"enabled":false,"host":"127.0.0.1","port":4455,"password":""})
}
pub fn link_settings(value: &Value) -> Result<Value> {
    ensure!(value.is_object(), "Invalid OBS link settings");
    let mut out = link_defaults();
    for key in ["enabled", "host", "port", "password"] {
        if let Some(v) = value.get(key) {
            out[key] = v.clone();
        }
    }
    ensure!(out["enabled"].is_boolean(), "Invalid OBS link switch");
    let host = out["host"]
        .as_str()
        .context("Invalid OBS host")?
        .trim()
        .to_string();
    ensure!(
        !host.is_empty() && host.len() <= 253 && !host.contains(['/', ' ']),
        "OBSのホスト名が正しくありません"
    );
    out["host"] = json!(host);
    let port = out["port"].as_u64().context("Invalid OBS port")?;
    ensure!(
        (1..=65535).contains(&port),
        "OBSのポート番号が正しくありません"
    );
    ensure!(out["password"].is_string(), "Invalid OBS password");
    Ok(out)
}
pub fn load_link_settings(path: &Path) -> Result<Value> {
    if path.exists() {
        link_settings(&vsm_core::wave::load_json(path)?)
    } else {
        Ok(link_defaults())
    }
}
pub fn save_link_settings(path: &Path, value: &Value) -> Result<()> {
    let normalized = link_settings(value)?;
    fs::create_dir_all(path.parent().context("Settings directory missing")?)?;
    let temp = path.with_extension("json.pending");
    vsm_core::wave::save_json(&temp, &normalized)?;
    fs::copy(&temp, path)?;
    fs::remove_file(temp)?;
    Ok(())
}

#[derive(Clone, Debug)]
pub struct ObsLinkConfig {
    pub host: String,
    pub port: u16,
    pub password: String,
}
impl ObsLinkConfig {
    pub fn from_settings(value: &Value) -> Result<Self> {
        let value = link_settings(value)?;
        Ok(Self {
            host: value["host"].as_str().unwrap().into(),
            port: value["port"].as_u64().unwrap() as u16,
            password: value["password"].as_str().unwrap().into(),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RecordState {
    Starting,
    Started,
    Stopping,
    Stopped,
    Paused,
    Resumed,
    Other(String),
}
impl RecordState {
    fn parse(state: &str) -> Self {
        match state {
            "OBS_WEBSOCKET_OUTPUT_STARTING" => Self::Starting,
            "OBS_WEBSOCKET_OUTPUT_STARTED" => Self::Started,
            "OBS_WEBSOCKET_OUTPUT_STOPPING" => Self::Stopping,
            "OBS_WEBSOCKET_OUTPUT_STOPPED" => Self::Stopped,
            "OBS_WEBSOCKET_OUTPUT_PAUSED" => Self::Paused,
            "OBS_WEBSOCKET_OUTPUT_RESUMED" => Self::Resumed,
            other => Self::Other(other.into()),
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub enum ObsSignal {
    Connected {
        active: bool,
        paused: bool,
    },
    Record {
        state: RecordState,
        path: Option<String>,
    },
    Exiting,
    Disconnected,
}
pub type SignalSink = Arc<dyn Fn(ObsSignal) + Send + Sync>;

pub fn auth(password: &str, salt: &str, challenge: &str) -> String {
    let secret = STANDARD.encode(Sha256::digest(format!("{password}{salt}")));
    STANDARD.encode(Sha256::digest(format!("{secret}{challenge}")))
}

pub struct ObsLink {
    cancel: Arc<AtomicBool>,
    status: Arc<Mutex<Value>>,
    worker: Option<JoinHandle<()>>,
}
impl ObsLink {
    /// Connects in the background and reconnects until `cancel` is set. The
    /// sink runs on the link thread; it may lock host state, so dropping a
    /// link only sets the flag and never waits for the thread.
    pub fn start(config: ObsLinkConfig, cancel: Arc<AtomicBool>, sink: SignalSink) -> Result<Self> {
        let status = Arc::new(Mutex::new(
            json!({"state":"connecting","error":"","obs_version":""}),
        ));
        let info = status.clone();
        let stop = cancel.clone();
        let worker = thread::Builder::new()
            .name("vsm-obs-link".into())
            .spawn(move || run(&config, &stop, &info, &sink))?;
        Ok(Self {
            cancel,
            status,
            worker: Some(worker),
        })
    }
    pub fn status(&self) -> Value {
        self.status.lock().unwrap().clone()
    }
    /// Stops and waits; never call while holding a lock the sink takes.
    pub fn join(mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for ObsLink {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

enum End {
    Cancelled,
    Lost(String),
    AuthFailed,
}
fn set(status: &Mutex<Value>, state: &str, error: &str) {
    let mut s = status.lock().unwrap();
    s["state"] = json!(state);
    s["error"] = json!(error);
}
fn run(config: &ObsLinkConfig, cancel: &AtomicBool, status: &Mutex<Value>, sink: &SignalSink) {
    while !cancel.load(Ordering::Relaxed) {
        set(status, "connecting", "");
        let mut connected = false;
        let end = session(config, cancel, status, sink, &mut connected)
            .unwrap_or_else(|e| End::Lost(format!("{e:#}")));
        if connected && !cancel.load(Ordering::Relaxed) {
            sink(ObsSignal::Disconnected);
        }
        let wait = match end {
            End::Cancelled => break,
            End::AuthFailed => {
                set(status, "auth_failed", "OBS WebSocketのパスワードが違います");
                150
            }
            End::Lost(e) => {
                set(status, "disconnected", &e);
                20
            }
        };
        for _ in 0..wait {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
    set(status, "stopped", "");
}
fn read_json(ws: &mut WebSocket<TcpStream>) -> Result<std::result::Result<Value, Option<u16>>> {
    loop {
        match ws.read()? {
            Message::Text(text) => return Ok(Ok(serde_json::from_str(text.as_str())?)),
            Message::Close(frame) => return Ok(Err(frame.map(|f| u16::from(f.code)))),
            _ => {}
        }
    }
}
fn send(ws: &mut WebSocket<TcpStream>, value: Value) -> Result<()> {
    Ok(ws.send(Message::text(value.to_string()))?)
}
fn session(
    config: &ObsLinkConfig,
    cancel: &AtomicBool,
    status: &Mutex<Value>,
    sink: &SignalSink,
    connected: &mut bool,
) -> Result<End> {
    let address = (config.host.as_str(), config.port)
        .to_socket_addrs()?
        .next()
        .context("OBSのアドレスを解決できません")?;
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(3))
        .context("OBSに接続できません（WebSocketサーバーが無効か、OBSが起動していません）")?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let url = format!("ws://{}:{}/", config.host, config.port);
    let (mut ws, _) = tungstenite::client(url, stream).map_err(|e| anyhow!("{e}"))?;
    let hello = match read_json(&mut ws)? {
        Ok(v) if v["op"] == 0 => v,
        Ok(v) => bail!("Unexpected obs-websocket greeting: {v}"),
        Err(code) => bail!("OBS closed the connection ({code:?})"),
    };
    let mut identify = json!({"op":1,"d":{"rpcVersion":1,"eventSubscriptions":EVENTS}});
    if let Some(a) = hello["d"].get("authentication") {
        let (Some(challenge), Some(salt)) = (a["challenge"].as_str(), a["salt"].as_str()) else {
            bail!("Invalid obs-websocket authentication request");
        };
        identify["d"]["authentication"] = json!(auth(&config.password, salt, challenge));
    }
    send(&mut ws, identify)?;
    loop {
        match read_json(&mut ws)? {
            Ok(v) if v["op"] == 2 => break,
            Ok(_) => {}
            Err(Some(AUTH_FAILED)) => return Ok(End::AuthFailed),
            Err(code) => bail!("OBS closed the connection ({code:?})"),
        }
    }
    status.lock().unwrap()["obs_version"] = hello["d"]["obsWebSocketVersion"].clone();
    send(
        &mut ws,
        json!({"op":6,"d":{"requestType":"GetRecordStatus","requestId":STATUS_REQUEST}}),
    )?;
    // Short reads keep cancellation responsive; tungstenite resumes partial frames.
    ws.get_ref()
        .set_read_timeout(Some(Duration::from_millis(200)))?;
    set(status, "connected", "");
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = ws.close(None);
            let _ = ws.flush();
            return Ok(End::Cancelled);
        }
        let message = match ws.read() {
            Ok(m) => m,
            Err(WsError::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                continue;
            }
            Err(WsError::ConnectionClosed | WsError::AlreadyClosed) => {
                return Ok(End::Lost("OBSとの接続が切れました".into()));
            }
            Err(e) => return Ok(End::Lost(e.to_string())),
        };
        let Message::Text(text) = message else {
            continue;
        };
        let v: Value = serde_json::from_str(text.as_str())?;
        let d = &v["d"];
        match v["op"].as_u64() {
            Some(7) if d["requestId"] == STATUS_REQUEST => {
                let r = &d["responseData"];
                *connected = true;
                sink(ObsSignal::Connected {
                    active: r["outputActive"] == true,
                    paused: r["outputPaused"] == true,
                });
            }
            Some(5) if *connected => match d["eventType"].as_str() {
                Some("RecordStateChanged") => sink(ObsSignal::Record {
                    state: RecordState::parse(d["eventData"]["outputState"].as_str().unwrap_or("")),
                    path: d["eventData"]["outputPath"].as_str().map(String::from),
                }),
                Some("ExitStarted") => sink(ObsSignal::Exiting),
                _ => {}
            },
            _ => {}
        }
    }
}

/// Maps OBS recording state onto engine commands, as the former OBS plugin
/// did from frontend callbacks. `joined` tracks whether this engine run owns
/// a take opened for the current OBS recording.
#[derive(Default)]
pub struct ObsRecording {
    pub obs_active: bool,
    pub obs_paused: bool,
    pub joined: bool,
}
fn row(event: &str, joined: bool) -> Value {
    json!({"event":event,"source":"obs","transport":"obs-websocket","joined_recording":joined})
}
impl ObsRecording {
    /// Call after starting an engine: joins a recording already in progress.
    pub fn on_engine_started(&mut self) -> Vec<Value> {
        self.joined = false;
        if !self.obs_active {
            return vec![];
        }
        self.joined = true;
        let mut rows = vec![row("recording_started", true)];
        if self.obs_paused {
            rows.push(row("recording_paused", false));
        }
        rows
    }
    pub fn on_engine_stopped(&mut self) {
        self.joined = false;
    }
    pub fn on_signal(&mut self, signal: &ObsSignal, running: bool) -> Vec<Value> {
        use ObsSignal::*;
        use RecordState::*;
        match signal {
            Connected { active, paused } => {
                (self.obs_active, self.obs_paused) = (*active, *paused);
                if !*active {
                    self.finish(running, "", Some("obs_not_recording_on_reconnect"))
                } else if running && !self.joined {
                    self.on_engine_started()
                } else {
                    vec![]
                }
            }
            Record { state: Started, .. } => {
                (self.obs_active, self.obs_paused) = (true, false);
                if running && !self.joined {
                    self.joined = true;
                    vec![row("recording_started", false)]
                } else {
                    vec![]
                }
            }
            Record {
                state: Stopping, ..
            } if self.joined && running => {
                vec![row("recording_stopping", false)]
            }
            Record {
                state: Stopped,
                path,
            } => {
                (self.obs_active, self.obs_paused) = (false, false);
                self.finish(running, path.as_deref().unwrap_or(""), None)
            }
            Record { state: Paused, .. } => {
                self.obs_paused = true;
                if self.joined && running {
                    vec![row("recording_paused", false)]
                } else {
                    vec![]
                }
            }
            Record { state: Resumed, .. } => {
                self.obs_paused = false;
                if self.joined && running {
                    vec![row("recording_unpaused", false)]
                } else {
                    vec![]
                }
            }
            Exiting => {
                self.obs_active = false;
                self.finish(running, "", Some("obs_exit"))
            }
            Disconnected => {
                self.obs_active = false;
                self.finish(running, "", Some("obs_link_lost"))
            }
            Record { .. } => vec![],
        }
    }
    fn finish(&mut self, running: bool, path: &str, reason: Option<&str>) -> Vec<Value> {
        if !std::mem::take(&mut self.joined) || !running {
            return vec![];
        }
        let mut r = row("recording_stopped", false);
        r["video_path"] = json!(path);
        if let Some(reason) = reason {
            r["stop_reason"] = json!(reason);
        }
        vec![r]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(rows: &[Value]) -> Vec<&str> {
        rows.iter().map(|r| r["event"].as_str().unwrap()).collect()
    }
    fn record(state: RecordState) -> ObsSignal {
        ObsSignal::Record { state, path: None }
    }

    #[test]
    fn auth_matches_protocol_example() {
        // Salt and challenge from the Hello example in the obs-websocket
        // protocol document; the expected value was computed independently.
        assert_eq!(
            auth(
                "supersecretpassword",
                "lM1GncleQOaCu9lT1yeUZhFYnqhsLLP1G5lAGo3ixaI=",
                "+IxH4CnCiqpX1rM9scsNynZzbOe4KhDeYcTNS3PDaeY="
            ),
            "1Ct943GAT+6YQUUX47Ia/ncufilbe6+oD6lY+5kaCu4="
        );
    }
    #[test]
    fn settings_are_validated() {
        assert_eq!(link_settings(&json!({})).unwrap()["port"], 4455);
        assert!(link_settings(&json!({"port":0})).is_err());
        assert!(link_settings(&json!({"host":""})).is_err());
        assert!(link_settings(&json!({"host":"a b"})).is_err());
        assert_eq!(
            link_settings(&json!({"host":" obs.local "})).unwrap()["host"],
            "obs.local"
        );
    }
    #[test]
    fn follows_obs_recording_while_engine_runs() {
        let mut r = ObsRecording::default();
        assert!(r.on_signal(&record(RecordState::Started), false).is_empty());
        assert!(r.obs_active && !r.joined);
        assert_eq!(events(&r.on_engine_started()), ["recording_started"]);
        assert!(r.on_signal(&record(RecordState::Started), true).is_empty());
        assert_eq!(
            events(&r.on_signal(&record(RecordState::Paused), true)),
            ["recording_paused"]
        );
        assert_eq!(
            events(&r.on_signal(&record(RecordState::Resumed), true)),
            ["recording_unpaused"]
        );
        assert_eq!(
            events(&r.on_signal(&record(RecordState::Stopping), true)),
            ["recording_stopping"]
        );
        let stop = r.on_signal(
            &ObsSignal::Record {
                state: RecordState::Stopped,
                path: Some("C:/v.mkv".into()),
            },
            true,
        );
        assert_eq!(events(&stop), ["recording_stopped"]);
        assert_eq!(stop[0]["video_path"], "C:/v.mkv");
        assert!(stop[0].get("stop_reason").is_none());
        assert!(!r.joined && !r.obs_active);
        assert_eq!(
            events(&r.on_signal(&record(RecordState::Started), true)),
            ["recording_started"]
        );
    }
    #[test]
    fn joins_paused_recording_and_closes_on_loss() {
        let mut r = ObsRecording::default();
        let joined = r.on_signal(
            &ObsSignal::Connected {
                active: true,
                paused: true,
            },
            true,
        );
        assert_eq!(events(&joined), ["recording_started", "recording_paused"]);
        assert_eq!(joined[0]["joined_recording"], true);
        let lost = r.on_signal(&ObsSignal::Disconnected, true);
        assert_eq!(lost[0]["stop_reason"], "obs_link_lost");
        assert!(r.on_signal(&ObsSignal::Exiting, true).is_empty());
        r.on_signal(
            &ObsSignal::Connected {
                active: true,
                paused: false,
            },
            true,
        );
        assert_eq!(
            r.on_signal(&ObsSignal::Exiting, true)[0]["stop_reason"],
            "obs_exit"
        );
        r.on_signal(&record(RecordState::Started), true);
        r.on_engine_stopped();
        assert!(r.on_signal(&record(RecordState::Stopped), true).is_empty());
    }
}
