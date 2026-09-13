use crate::clock;
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use vsm_core::osc::live_types;

use std::collections::BTreeSet;

const SERVICE: &str = "_oscjson._tcp.local.";
const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
type Receiver = Arc<dyn Fn(Value) -> Result<()> + Send + Sync>;
#[cfg(any(not(windows), test))]
fn u16_at(b: &[u8], at: usize) -> Result<u16> {
    ensure!(at + 2 <= b.len(), "Short DNS word");
    Ok(u16::from_be_bytes(b[at..at + 2].try_into()?))
}
#[cfg(any(not(windows), test))]
fn name_at(b: &[u8], at: &mut usize) -> Result<String> {
    let mut cursor = *at;
    let mut jumped = false;
    let mut text = String::new();
    for _ in 0..128 {
        ensure!(cursor < b.len(), "Truncated DNS name");
        let n = b[cursor];
        if n & 0xc0 == 0xc0 {
            let p = (u16_at(b, cursor)? & 0x3fff) as usize;
            if !jumped {
                *at = cursor + 2;
                jumped = true;
            }
            ensure!(p < cursor, "Invalid DNS compression pointer");
            cursor = p;
            continue;
        }
        ensure!(n <= 63, "Invalid DNS label");
        cursor += 1;
        if n == 0 {
            if !jumped {
                *at = cursor;
            }
            return Ok(text);
        }
        ensure!(cursor + n as usize <= b.len(), "Truncated DNS label");
        text.push_str(std::str::from_utf8(&b[cursor..cursor + n as usize])?);
        text.push('.');
        ensure!(text.len() <= 255, "DNS name exceeds bounds");
        cursor += n as usize;
    }
    anyhow::bail!("DNS name recursion limit")
}
fn name(out: &mut Vec<u8>, value: &str) {
    for label in value.trim_end_matches('.').split('.') {
        out.push(label.len() as u8);
        out.extend(label.as_bytes());
    }
    out.push(0);
}
fn record(out: &mut Vec<u8>, owner: &str, kind: u16, ttl: u32, data: &[u8]) {
    name(out, owner);
    out.extend(kind.to_be_bytes());
    out.extend((if kind == 12 { 1u16 } else { 0x8001 }).to_be_bytes());
    out.extend(ttl.to_be_bytes());
    out.extend((data.len() as u16).to_be_bytes());
    out.extend(data);
}
fn announcement(instance: &str, host: &str, port: u16, ttl: u32, id: u16) -> Vec<u8> {
    let mut b = Vec::new();
    for v in [id, 0x8400, 0, 1, 0, 3] {
        b.extend(v.to_be_bytes());
    }
    let mut ptr = Vec::new();
    name(&mut ptr, instance);
    record(&mut b, SERVICE, 12, ttl, &ptr);
    let mut srv = vec![0; 4];
    srv.extend(port.to_be_bytes());
    name(&mut srv, host);
    record(&mut b, instance, 33, ttl, &srv);
    record(&mut b, host, 1, ttl, &[127, 0, 0, 1]);
    record(&mut b, instance, 16, ttl, b"\x05VSM=1");
    b
}
#[cfg(not(windows))]
fn question() -> Vec<u8> {
    let mut b = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    name(&mut b, SERVICE);
    b.extend([0, 12, 0, 1]);
    b
}
#[cfg(any(not(windows), test))]
struct DnsResult {
    ports: Vec<(u16, u32)>,
    reply: bool,
    unicast: bool,
    id: u16,
}
#[cfg(any(not(windows), test))]
fn parse_dns(b: &[u8], instance: &str, host: &str) -> Result<DnsResult> {
    ensure!(b.len() >= 12 && b.len() <= 65536, "DNS packet bounds");
    let id = u16_at(b, 0)?;
    let questions = u16_at(b, 4)? as usize;
    let records = (u16_at(b, 6)? as usize) + (u16_at(b, 8)? as usize) + (u16_at(b, 10)? as usize);
    ensure!(questions + records <= 512, "DNS record limit");
    let mut result = DnsResult {
        ports: Vec::new(),
        reply: false,
        unicast: false,
        id,
    };
    let mut at = 12;
    for _ in 0..questions {
        let n = name_at(b, &mut at)?;
        let kind = u16_at(b, at)?;
        let class = u16_at(b, at + 2)?;
        at += 4;
        if (n == SERVICE && (kind == 12 || kind == 255))
            || (n == instance && [16, 33, 255].contains(&kind))
            || (n == host && (kind == 1 || kind == 255))
        {
            result.reply = true;
            result.unicast |= class & 0x8000 != 0;
        }
    }
    for _ in 0..records {
        let n = name_at(b, &mut at)?;
        let kind = u16_at(b, at)?;
        ensure!(at + 10 <= b.len(), "Short DNS record");
        let ttl = u32::from_be_bytes(b[at + 4..at + 8].try_into()?);
        let len = u16_at(b, at + 8)? as usize;
        at += 10;
        ensure!(at + len <= b.len(), "Invalid DNS data length");
        if kind == 33 && ttl > 0 && n.starts_with("VRChat-Client-") && n.ends_with(SERVICE) {
            ensure!(len >= 7, "Short SRV record");
            let port = u16_at(b, at + 4)?;
            if port > 0 {
                result.ports.push((port, ttl));
            }
        }
        at += len;
    }
    Ok(result)
}
fn query_tree(types: &BTreeMap<String, String>) -> Value {
    let mut root = json!({"FULL_PATH":"/","ACCESS":0,"CONTENTS":{}});
    for (address, kind) in types {
        let mut node = &mut root;
        let mut path = String::new();
        let mut parts = address.trim_start_matches('/').split('/').peekable();
        while let Some(part) = parts.next() {
            path.push('/');
            path.push_str(part);
            if node.get("CONTENTS").is_none() {
                node["CONTENTS"] = json!({});
            }
            node = node["CONTENTS"]
                .as_object_mut()
                .unwrap()
                .entry(part)
                .or_insert_with(|| json!({"FULL_PATH":path,"ACCESS":0}));
            if parts.peek().is_none() {
                node["ACCESS"] = json!(2);
                node["TYPE"] = json!(kind);
            }
        }
    }
    root
}
fn serve(
    mut stream: TcpStream,
    types: &BTreeMap<String, String>,
    tree: &Value,
    instance: &str,
    udp: u16,
) -> Result<()> {
    // Windows may inherit the nonblocking listener mode on accepted sockets.
    // The bounded HTTP reader below expects blocking reads with a timeout.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    stream.set_write_timeout(Some(Duration::from_millis(200)))?;
    let mut request = Vec::new();
    let mut bytes = [0; 1024];
    while !request.windows(4).any(|v| v == b"\r\n\r\n") {
        ensure!(request.len() < 8192, "HTTP request exceeds bounds");
        let n = stream.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        request.extend(&bytes[..n]);
    }
    let text = std::str::from_utf8(&request)?;
    let line = text.lines().next().unwrap_or("");
    let path = line
        .strip_prefix("GET ")
        .and_then(|s| s.split_once(" HTTP/"))
        .map(|p| p.0)
        .unwrap_or("");
    let (code, body) = if path == "/?HOST_INFO" {
        (
            200,
            json!({"NAME":instance.split('.').next().unwrap_or(instance),"OSC_IP":"127.0.0.1","OSC_PORT":udp,"OSC_TRANSPORT":"UDP","EXTENSIONS":{"ACCESS":true,"DESCRIPTION":true}}),
        )
    } else if path == "/" {
        (200, tree.clone())
    } else if let Some(kind) = types.get(path) {
        (200, json!({"FULL_PATH":path,"ACCESS":2,"TYPE":kind}))
    } else {
        (404, json!({"error":"unknown endpoint"}))
    };
    let body = body.to_string();
    write!(
        stream,
        "HTTP/1.1 {code} {}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        if code == 200 { "OK" } else { "Not Found" },
        body.len()
    )?;
    Ok(())
}
fn get(client: &reqwest::blocking::Client, port: u16, path: &str) -> Result<String> {
    let response = client
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()?
        .error_for_status()?;
    let mut body = Vec::new();
    response.take(65537).read_to_end(&mut body)?;
    ensure!(body.len() <= 65536, "OSCQuery response exceeds 64 KiB");
    Ok(String::from_utf8(body)?)
}
pub struct OscLive {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    info: Arc<Mutex<Value>>,
    packets: Arc<AtomicU64>,
    #[cfg(windows)]
    _discovery: crate::windows_dnssd::Discovery,
}
impl OscLive {
    pub fn new(origin: i64, receive: Receiver) -> Result<Self> {
        let udp = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        udp.bind(&SocketAddr::from((Ipv4Addr::LOCALHOST, 0)).into())?;
        udp.set_recv_buffer_size(1024 * 1024)?;
        let udp: UdpSocket = udp.into();
        udp.set_read_timeout(Some(Duration::from_millis(20)))?;
        let udp_port = udp.local_addr()?.port();
        let http = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        http.set_nonblocking(true)?;
        let http_port = http.local_addr()?.port();
        #[cfg(not(windows))]
        let (mdns, interfaces) = {
            let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
            socket.set_reuse_address(true)?;
            socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 5353)).into())?;
            socket.set_nonblocking(true)?;
            socket.set_multicast_ttl_v4(255)?;
            socket.set_multicast_loop_v4(true)?;
            let mut interfaces = BTreeSet::from([Ipv4Addr::LOCALHOST]);
            for iface in if_addrs::get_if_addrs()? {
                if let std::net::IpAddr::V4(ip) = iface.ip() {
                    interfaces.insert(ip);
                }
            }
            for ip in &interfaces {
                let _ = socket.join_multicast_v4(&GROUP, ip);
            }
            let mdns: UdpSocket = socket.into();
            (mdns, interfaces)
        };
        #[cfg(windows)]
        let (mdns, interfaces) = {
            // VRChat does not discover advertisements sent from a loopback source.
            // Send from the selected interface, but never receive on this socket:
            // DNSAPI owns discovery reception; OSC and HTTP stay on loopback.
            let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
            socket.set_reuse_address(true)?;
            socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 5353)).into())?;
            socket.set_multicast_ttl_v4(255)?;
            socket.set_multicast_loop_v4(true)?;
            let mut interfaces = BTreeSet::from([Ipv4Addr::LOCALHOST]);
            for iface in if_addrs::get_if_addrs()? {
                if let std::net::IpAddr::V4(ip) = iface.ip() {
                    interfaces.insert(ip);
                }
            }
            (UdpSocket::from(socket), interfaces)
        };
        let instance = format!("VSM-Live-{}-{http_port}.{SERVICE}", std::process::id());
        let host = format!("VSM-Live-{http_port}.local.");
        let stop = Arc::new(AtomicBool::new(false));
        let packets = Arc::new(AtomicU64::new(0));
        let info = Arc::new(Mutex::new(
            json!({"udp_port":udp_port,"http_port":http_port,"service":instance,"error":""}),
        ));
        let candidates = Arc::new(Mutex::new(BTreeMap::<u16, Instant>::new()));
        #[cfg(windows)]
        let discovery = crate::windows_dnssd::Discovery::new(candidates.clone(), info.clone())?;
        info.lock().unwrap()["discovery_backend"] = json!(if cfg!(windows) {
            "windows_dns_sd"
        } else {
            "process_mdns"
        });
        info.lock().unwrap()["osc_http_loopback_only"] = json!(true);
        info.lock().unwrap()["mdns_send_only"] = json!(cfg!(windows));
        let mut threads = Vec::new();
        {
            let stop = stop.clone();
            let info = info.clone();
            let instance = instance.clone();
            threads.push(thread::spawn(move || {
                let types = live_types();
                let tree = query_tree(&types);
                while !stop.load(Ordering::Relaxed) {
                    match http.accept() {
                        Ok((stream, peer)) => {
                            if peer.ip().is_loopback() {
                                let _ = serve(stream, &types, &tree, &instance, udp_port);
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(20))
                        }
                        Err(e) => {
                            info.lock().unwrap()["error"] = json!(e.to_string());
                            stop.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }));
        }
        {
            let stop = stop.clone();
            let info = info.clone();
            let receive = receive.clone();
            let packets = packets.clone();
            #[cfg(not(windows))]
            let candidates = candidates.clone();
            threads.push(thread::spawn(move || {
                let mut buffer = [0u8; 65536];
                let multicast = SocketAddr::from((GROUP, 5353));
                let announce = |ttl| {
                    let data = announcement(&instance, &host, http_port, ttl, 0);
                    #[cfg(not(windows))]
                    let query = question();
                    let sock = socket2::SockRef::from(&mdns);
                    for ip in &interfaces {
                        if sock.set_multicast_if_v4(ip).is_ok() {
                            let _ = mdns.send_to(&data, multicast);
                            #[cfg(not(windows))]
                            if ttl > 0 { let _ = mdns.send_to(&query, multicast); }
                        }
                    }
                };
                let mut last = Instant::now() - Duration::from_secs(3);
                let run = (|| -> Result<()> {
                    while !stop.load(Ordering::Relaxed) {
                        if last.elapsed() >= Duration::from_secs(2) { announce(120); last = Instant::now(); }
                        match udp.recv_from(&mut buffer) {
                            Ok((n, peer)) => {
                                if n > 0 && peer.ip().is_loopback() {
                                    receive(json!({"kind":"udp","receive_time_ns":clock::now_ns()-origin,
                                        "sequence":packets.fetch_add(1,Ordering::Relaxed),"peer_port":peer.port(),
                                        "datagram_base64":STANDARD.encode(&buffer[..n])}))?;
                                }
                            }
                            Err(e) if [std::io::ErrorKind::WouldBlock, std::io::ErrorKind::TimedOut].contains(&e.kind()) => {},
                            Err(e) => return Err(e.into()),
                        }
                        #[cfg(not(windows))]
                        for _ in 0..128 {
                            match mdns.recv_from(&mut buffer) {
                                Ok((n, peer)) => {
                                    if let Ok(result) = parse_dns(&buffer[..n], &instance, &host) {
                                        let mut ports = candidates.lock().unwrap();
                                        ports.retain(|_, expiry| *expiry > Instant::now());
                                        for (port, _) in result.ports {
                                            if ports.len() < 16 || ports.contains_key(&port) {
                                                ports.insert(port, Instant::now() + Duration::from_secs(10));
                                            }
                                        }
                                        drop(ports);
                                        if result.reply {
                                            let _ = mdns.send_to(&announcement(&instance, &host, http_port, 120,
                                                if result.unicast { result.id } else { 0 }),
                                                if result.unicast { peer } else { multicast });
                                        }
                                    }
                                }
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                                Err(e) => return Err(e.into()),
                            }
                        }
                    }
                    Ok(())
                })();
                announce(0);
                if let Err(e) = run {
                    info.lock().unwrap()["error"] = json!(e.to_string());
                    stop.store(true, Ordering::Relaxed);
                }
            }));
        }
        {
            let stop = stop.clone();
            let info = info.clone();
            threads.push(thread::spawn(move||{
            let run=(||->Result<()>{let client=reqwest::blocking::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_millis(200)).build()?;let types=live_types();let mut last=Instant::now()-Duration::from_secs(3);
                while !stop.load(Ordering::Relaxed){if last.elapsed()<Duration::from_secs(2){thread::sleep(Duration::from_millis(25));continue;}last=Instant::now();
                    let ports:Vec<u16>=candidates.lock().unwrap().iter().filter_map(|(&p,at)|(*at>Instant::now()).then_some(p)).collect();let verified:Vec<u16>=ports.into_iter().filter(|&p|get(&client,p,"/?HOST_INFO").ok().and_then(|s|serde_json::from_str::<Value>(&s).ok()).is_some_and(|h|h["NAME"].as_str().is_some_and(|s|s.starts_with("VRChat-Client-")))).collect();
                    if verified.len()!=1{continue;}info.lock().unwrap()["verified_query_port"]=json!(verified[0]);
                    for(address,kind)in &types{if stop.load(Ordering::Relaxed){break;}if address=="/avatar/change"||kind.len()!=1{continue;}let start=clock::now_ns()-origin;
                        let row=match get(&client,verified[0],address){Ok(body)=>json!({"kind":"snapshot","receive_time_ns":clock::now_ns()-origin,"request_start_ns":start,"http_port":verified[0],"address":address,"body":body}),Err(e)=>json!({"kind":"snapshot_error","receive_time_ns":clock::now_ns()-origin,"address":address,"error":e.to_string()})};receive(row)?;
                    }
                }Ok(())})();if let Err(e)=run{info.lock().unwrap()["error"]=json!(e.to_string());stop.store(true,Ordering::Relaxed);}
        }));
        }
        Ok(Self {
            stop,
            threads,
            info,
            packets,
            #[cfg(windows)]
            _discovery: discovery,
        })
    }
    pub fn status(&self) -> Value {
        let mut info = self.info.lock().unwrap().clone();
        info["packets"] = json!(self.packets.load(Ordering::Relaxed));
        info
    }
}
impl Drop for OscLive {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "Explicit loopback network check; Windows may prompt for each rebuilt test binary"]
    fn loopback_transport_and_query_discovery() -> Result<()> {
        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = events.clone();
        let service = OscLive::new(
            clock::now_ns(),
            Arc::new(move |event| {
                captured.lock().unwrap().push(event);
                Ok(())
            }),
        )?;
        let info = service.status();
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(500))
            .build()?;
        let port = info["http_port"].as_u64().unwrap() as u16;
        // Exercise a client that connects before it sends its HTTP request.
        // A nonblocking accepted stream used to close this connection early.
        let mut delayed = TcpStream::connect((Ipv4Addr::LOCALHOST, port))?;
        delayed.set_read_timeout(Some(Duration::from_secs(1)))?;
        thread::sleep(Duration::from_millis(60));
        delayed.write_all(b"GET /?HOST_INFO HTTP/1.1\r\nHost: localhost\r\n\r\n")?;
        let mut response = String::new();
        delayed.read_to_string(&mut response)?;
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        let host: Value = serde_json::from_str(&get(&client, port, "/?HOST_INFO")?)?;
        assert_eq!(host["OSC_PORT"], info["udp_port"]);
        let camera: Value = serde_json::from_str(&get(&client, port, "/usercamera/Pose")?)?;
        assert_eq!(camera["TYPE"], "ffffff");
        assert_eq!(camera["ACCESS"], 2);
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        sender.send_to(
            b"invalid OSC retained as raw",
            (
                Ipv4Addr::LOCALHOST,
                info["udp_port"].as_u64().unwrap() as u16,
            ),
        )?;
        let mock = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        mock.set_nonblocking(true)?;
        let mock_port = mock.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let end = stop.clone();
        let server = thread::spawn(move || {
            while !end.load(Ordering::Relaxed) {
                match mock.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                        let mut bytes = [0; 8192];
                        if let Ok(n) = stream.read(&mut bytes) {
                            let request = String::from_utf8_lossy(&bytes[..n]);
                            let path = request.split_whitespace().nth(1).unwrap_or("/");
                            let body = if path == "/?HOST_INFO" {
                                json!({"NAME":"VRChat-Client-vsm-loopback-test"})
                            } else {
                                json!({"FULL_PATH":path,"TYPE":"f","ACCESS":1,"VALUE":[0.5]})
                            }
                            .to_string();
                            let _ = write!(
                                stream,
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        let (announce, packet) = {
            let announce = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
            announce.set_reuse_address(true)?;
            announce.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 5353)).into())?;
            announce.set_multicast_if_v4(&Ipv4Addr::LOCALHOST)?;
            announce.set_multicast_loop_v4(true)?;
            let packet = announcement(
                "VRChat-Client-vsm-loopback-test._oscjson._tcp.local.",
                "vsm-loopback-test.local.",
                mock_port,
                120,
                0,
            );
            (announce, packet)
        };
        let start = Instant::now();
        let mut found = false;
        while start.elapsed() < Duration::from_secs(8) {
            for iface in if_addrs::get_if_addrs()? {
                if let std::net::IpAddr::V4(ip) = iface.ip() {
                    if announce.set_multicast_if_v4(&ip).is_ok() {
                        let _ = announce.send_to(&packet, &SocketAddr::from((GROUP, 5353)).into());
                    }
                }
            }
            thread::sleep(Duration::from_millis(100));
            if events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e["kind"] == "snapshot" && e["http_port"] == mock_port)
            {
                found = true;
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap();
        let shutdown = Instant::now();
        drop(service);
        assert!(shutdown.elapsed() < Duration::from_secs(2));
        let events = events.lock().unwrap();
        assert!(found, "OSCQuery client was not discovered");
        let raw = events.iter().find(|e| e["kind"] == "udp").unwrap();
        assert_eq!(
            STANDARD.decode(raw["datagram_base64"].as_str().unwrap())?,
            b"invalid OSC retained as raw"
        );
        for e in events.iter().filter(|e| e["kind"] == "snapshot") {
            assert!(e["receive_time_ns"].as_i64() >= e["request_start_ns"].as_i64());
        }
        Ok(())
    }
    #[test]
    fn discovery_roundtrip_and_bounds() {
        let packet = announcement(
            "VRChat-Client-fixture._oscjson._tcp.local.",
            "fixture.local.",
            12345,
            120,
            0,
        );
        let result = parse_dns(&packet, "other", "other").unwrap();
        assert_eq!(result.id, 0);
        assert_eq!(result.ports, vec![(12345, 120)]);
        assert!(parse_dns(&packet[..20], "other", "other").is_err());
        let mut at = 0;
        assert!(name_at(&[0xc0, 0], &mut at).is_err());
        let tree = query_tree(&live_types());
        assert_eq!(
            tree["CONTENTS"]["usercamera"]["CONTENTS"]["Pose"]["ACCESS"],
            2
        );
    }
}
