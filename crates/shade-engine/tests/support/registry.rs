use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub struct Registry {
    pub url: String,
    routes: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Registry {
    pub fn new() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let routes = Arc::new(Mutex::new(BTreeMap::<String, Vec<u8>>::new()));
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_routes, thread_requests, thread_stop) =
            (routes.clone(), requests.clone(), stop.clone());
        let worker = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(stream) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("registry accept: {error}"),
                };
                // Darwin can inherit O_NONBLOCK from the listening socket.
                // A request that has not arrived yet must wait, not be reset.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let mut buffer = [0; 8192];
                let mut header = Vec::new();
                let complete = loop {
                    let count = match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => break false,
                        Ok(count) => count,
                    };
                    header.extend_from_slice(&buffer[..count]);
                    if header.len() > 65536 {
                        break false;
                    }
                    if header.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        break true;
                    }
                };
                if !complete {
                    continue;
                }
                // Closing with unread request headers resets the connection on
                // macOS, making real package managers retry or time out.
                let request = String::from_utf8_lossy(&header);
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .split('?')
                    .next()
                    .unwrap();
                thread_requests.fetch_add(1, Ordering::SeqCst);
                let routes = thread_routes.lock().unwrap();
                let (status, bytes) = match routes.get(path) {
                    Some(bytes) => ("200 OK", bytes.as_slice()),
                    None => ("404 Not Found", b"{}".as_slice()),
                };
                let content_type = if path.starts_with("/simple/") {
                    "text/html"
                } else if path.ends_with(".whl") || path.ends_with(".tgz") {
                    "application/octet-stream"
                } else {
                    "application/json"
                };
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                let _ = stream
                    .write_all(header.as_bytes())
                    .and_then(|_| stream.write_all(bytes));
            }
        });
        Self {
            url,
            routes,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    pub fn route(&self, path: &str, bytes: Vec<u8>) {
        self.routes.lock().unwrap().insert(path.to_owned(), bytes);
    }

    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        output.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}
