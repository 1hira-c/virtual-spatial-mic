use serde_json::{Value, json};
use std::{
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::AtomicBool,
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};
use tungstenite::{
    Message, WebSocket,
    protocol::{CloseFrame, frame::coding::CloseCode},
};
use vsm_live::obs_link::{ObsLink, ObsLinkConfig, ObsSignal, RecordState, auth};

const SALT: &str = "c2FsdA==";
const CHALLENGE: &str = "Y2hhbGxlbmdl";

fn text(ws: &mut WebSocket<TcpStream>, value: Value) {
    ws.send(Message::text(value.to_string())).unwrap();
}
fn read(ws: &mut WebSocket<TcpStream>) -> Value {
    loop {
        if let Message::Text(t) = ws.read().unwrap() {
            return serde_json::from_str(t.as_str()).unwrap();
        }
    }
}
fn record(ws: &mut WebSocket<TcpStream>, state: &str, path: Value) {
    text(
        ws,
        json!({"op":5,"d":{"eventType":"RecordStateChanged","eventIntent":64,"eventData":{"outputActive":state.ends_with("STARTED"),"outputState":state,"outputPath":path}}}),
    );
}
/// Minimal obs-websocket v5 server: Hello with auth, Identify check, then `script`.
fn serve(
    password: &'static str,
    script: impl FnOnce(&mut WebSocket<TcpStream>) + Send + 'static,
) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut ws = tungstenite::accept(stream).unwrap();
        text(
            &mut ws,
            json!({"op":0,"d":{"obsWebSocketVersion":"5.5.0","rpcVersion":1,"authentication":{"challenge":CHALLENGE,"salt":SALT}}}),
        );
        let identify = read(&mut ws);
        assert_eq!(identify["op"], 1);
        assert_eq!(identify["d"]["rpcVersion"], 1);
        assert_ne!(
            identify["d"]["eventSubscriptions"].as_u64().unwrap() & 64,
            0
        );
        if identify["d"]["authentication"] != auth(password, SALT, CHALLENGE) {
            let _ = ws.close(Some(CloseFrame {
                code: CloseCode::from(4009),
                reason: "Authentication failed.".into(),
            }));
            while ws.read().is_ok() {}
            return;
        }
        text(&mut ws, json!({"op":2,"d":{"negotiatedRpcVersion":1}}));
        script(&mut ws);
    });
    (port, server)
}
fn link(port: u16, password: &str) -> (ObsLink, Receiver<ObsSignal>) {
    let (tx, rx) = mpsc::channel();
    let tx = std::sync::Mutex::new(tx);
    let link = ObsLink::start(
        ObsLinkConfig {
            host: "127.0.0.1".into(),
            port,
            password: password.into(),
        },
        Arc::new(AtomicBool::new(false)),
        Arc::new(move |s| {
            let _ = tx.lock().unwrap().send(s);
        }),
    )
    .unwrap();
    (link, rx)
}

#[test]
fn follows_record_state_changes() {
    let (port, server) = serve("secret", |ws| {
        let request = read(ws);
        assert_eq!(request["op"], 6);
        assert_eq!(request["d"]["requestType"], "GetRecordStatus");
        text(
            ws,
            json!({"op":7,"d":{"requestType":"GetRecordStatus","requestId":request["d"]["requestId"],"requestStatus":{"result":true,"code":100},"responseData":{"outputActive":false,"outputPaused":false}}}),
        );
        record(ws, "OBS_WEBSOCKET_OUTPUT_STARTED", Value::Null);
        record(ws, "OBS_WEBSOCKET_OUTPUT_PAUSED", Value::Null);
        record(ws, "OBS_WEBSOCKET_OUTPUT_RESUMED", Value::Null);
        record(ws, "OBS_WEBSOCKET_OUTPUT_STOPPING", Value::Null);
        record(
            ws,
            "OBS_WEBSOCKET_OUTPUT_STOPPED",
            json!("C:/Videos/take.mkv"),
        );
        let _ = ws.close(None);
        while ws.read().is_ok() {}
    });
    let (link, rx) = link(port, "secret");
    let mut signals = Vec::new();
    while signals.last() != Some(&ObsSignal::Disconnected) {
        signals.push(rx.recv_timeout(Duration::from_secs(10)).expect("signal"));
    }
    link.join();
    server.join().unwrap();
    let record = |state| ObsSignal::Record { state, path: None };
    assert_eq!(
        signals,
        [
            ObsSignal::Connected {
                active: false,
                paused: false
            },
            record(RecordState::Started),
            record(RecordState::Paused),
            record(RecordState::Resumed),
            record(RecordState::Stopping),
            ObsSignal::Record {
                state: RecordState::Stopped,
                path: Some("C:/Videos/take.mkv".into())
            },
            ObsSignal::Disconnected,
        ]
    );
}

#[test]
fn wrong_password_reports_auth_failure() {
    let (port, server) = serve("secret", |_| unreachable!());
    let (link, rx) = link(port, "wrong");
    let deadline = Instant::now() + Duration::from_secs(10);
    while link.status()["state"] != "auth_failed" {
        assert!(Instant::now() < deadline, "status: {}", link.status());
        thread::sleep(Duration::from_millis(20));
    }
    link.join();
    server.join().unwrap();
    assert!(rx.try_recv().is_err(), "no signals before identification");
}
