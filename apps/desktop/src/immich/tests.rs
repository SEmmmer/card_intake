use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

struct Mock {
    settings: ImmichSettings,
    requests: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Mock {
    fn new(status: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let records = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            let library = serde_json::json!({"id":"lib1","name":"MyPhoto","assetCount":42,"importPaths":["/photos/MyPhoto"],"exclusionPatterns":["**/@eaDir/**"]});
            while !stopped.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0; 1];
                while !bytes.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut buf).unwrap();
                    bytes.push(buf[0]);
                }
                let header = String::from_utf8(bytes).unwrap();
                assert!(header.to_lowercase().contains("x-api-key: test-key"));
                let length: usize = header
                    .lines()
                    .find_map(|l| {
                        let (name, value) = l.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                let body = if body.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::from_slice(&body).unwrap()
                };
                let request = header.lines().next().unwrap().to_string();
                records
                    .lock()
                    .unwrap()
                    .push((request.clone(), body.clone()));
                let (code, response) = if status != 200 {
                    (status, "{}".into())
                } else if request.starts_with("GET /api/libraries ") {
                    (200, serde_json::json!([library]).to_string())
                } else if request.starts_with("POST ") {
                    (204, String::new())
                } else {
                    (200, library.to_string())
                };
                write!(stream,"HTTP/1.1 {code} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).unwrap();
            }
        });
        Self {
            settings: ImmichSettings {
                url: format!("http://{address}"),
                api_key: "test-key".into(),
                library_id: "lib1".into(),
            },
            requests,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn lists_libraries_and_rescans_without_changing_any_library() {
    let server = Mock::new(200);
    let client = Client::new(&server.settings).unwrap();
    let libraries = client.libraries().unwrap();
    assert_eq!(
        (
            libraries[0].asset_count,
            libraries[0].import_paths[0].as_str()
        ),
        (42, "/photos/MyPhoto")
    );
    client.scan("lib1").unwrap();
    assert!(client.scan("../x").is_err());
    let requests = server.requests.lock().unwrap();
    let lines: Vec<&str> = requests.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with("GET /api/libraries "));
    assert!(lines[1].starts_with("POST /api/libraries/lib1/scan "));
}

#[test]
fn auth_errors_are_plain_and_do_not_echo_key() {
    for code in [401, 403] {
        let server = Mock::new(code);
        let client = Client::new(&server.settings).unwrap();
        let e = client.libraries().unwrap_err();
        assert!(e.contains("admin") && e.contains("library.read") && e.contains("library.update"));
        assert!(!e.contains("test-key"));
    }
}
