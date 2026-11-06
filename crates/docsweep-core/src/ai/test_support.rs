//! A local stand-in for a provider's API, which records what it is sent. No test
//! ever talks to a real provider.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// One request as the server saw it.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl RecordedRequest {
    /// The value of a header.
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }
}

#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<String>,
    body: String,
}

/// Answers every request in the same way.
pub struct FakeProviderServer {
    url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl FakeProviderServer {
    fn start(reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a free local port");
        let url = format!(
            "http://127.0.0.1:{}/fake-llm",
            listener.local_addr().expect("a bound socket").port()
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let shared = Arc::clone(&shared);
                let reply = reply.clone();
                std::thread::spawn(move || {
                    let _ = serve(stream, &shared, &reply);
                });
            }
        });
        Self { url, requests }
    }

    /// Answers with this status and body, as they are.
    pub fn with_status(status: u16, body: &str) -> Self {
        Self::start(Reply {
            status,
            headers: Vec::new(),
            body: body.to_owned(),
        })
    }

    /// Answers like the Anthropic Messages API.
    pub fn answering_anthropic(text: &str) -> Self {
        let body = serde_json::json!({
            "stop_reason": "end_turn",
            "content": [
                { "type": "thinking", "thinking": "" },
                { "type": "text", "text": text },
            ],
        });
        Self::with_status(200, &body.to_string())
    }

    /// Answers like the Chat Completions API.
    pub fn answering_chat(text: &str) -> Self {
        let body = serde_json::json!({
            "choices": [{ "finish_reason": "stop", "message": { "content": text } }],
        });
        Self::with_status(200, &body.to_string())
    }

    /// Sends every request elsewhere.
    pub fn redirecting_to(target: &str) -> Self {
        Self::start(Reply {
            status: 307,
            headers: vec![format!("Location: {target}")],
            body: String::new(),
        })
    }

    /// Where it listens.
    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// What it was sent so far.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("not poisoned").clone()
    }
}

fn serve(
    mut stream: TcpStream,
    requests: &Mutex<Vec<RecordedRequest>>,
    reply: &Reply,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut headers = Vec::new();
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.to_ascii_lowercase();
            let value = value.trim().to_owned();
            if name == "content-length" {
                length = value.parse().unwrap_or(0);
            }
            headers.push((name, value));
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    requests
        .lock()
        .expect("not poisoned")
        .push(RecordedRequest {
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        });

    write!(
        stream,
        "HTTP/1.1 {} Fake\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    )?;
    for header in &reply.headers {
        write!(stream, "{header}\r\n")?;
    }
    stream.write_all(b"\r\n")?;
    stream.write_all(reply.body.as_bytes())
}
