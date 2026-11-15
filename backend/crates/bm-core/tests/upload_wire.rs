//! What `upload` puts on the wire, against a stub that answers like the author
//! API: one manifest read, then a chapter PUT whose sha256 header, query and
//! body are exactly what the server documents. Loopback only, so it runs in the
//! suite without touching a real API.

use bm_core::config::UploadSettings;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn read_request(sock: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = sock.read(&mut chunk).expect("read");
        if n == 0 {
            return buf;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(hend) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..hend]).to_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            if buf.len() >= hend + 4 + len {
                return buf;
            }
        }
    }
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bm-wire-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test]
async fn the_push_speaks_the_documented_author_api() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen_srv = seen.clone();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut sock, _) = listener.accept().unwrap();
            let req = read_request(&mut sock);
            let text = String::from_utf8_lossy(&req).to_string();
            let is_get = text.starts_with("GET ");
            seen_srv.lock().unwrap().push(text);
            let body = if is_get { "{\"chapters\":[]}" } else { "" };
            let (status, ctype) = if is_get {
                ("200 OK", "application/json")
            } else {
                ("201 Created", "text/plain")
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).unwrap();
        }
    });

    let root = tmpdir("layout");
    let layout = bm_core::Layout::new(&root);
    let out = layout.output();
    std::fs::create_dir_all(&out).unwrap();
    let bytes = b"ID3 pretend mp3 bytes";
    std::fs::write(out.join("Ch.1 - One.mp3"), bytes).unwrap();
    std::fs::write(
        out.join("Ch.1 - One.cues.json"),
        r#"{"version":2,"chapter":1,"duration_s":2413.4,"cues":[]}"#,
    )
    .unwrap();

    let settings = UploadSettings {
        url: format!("http://{addr}/api/author/audiobooks/id/1"),
        token: "abk_test".into(),
        token_env: "STORYCAST_UPLOAD_TOKEN_UNSET".into(),
        on_merge: false,
    };
    let http = reqwest::Client::new();
    let report = bm_core::upload::run(&settings, &http, &layout)
        .await
        .expect("upload");
    server.join().unwrap();

    assert_eq!(report.created, 1);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "one GET, one PUT: {seen:#?}");

    let get = &seen[0];
    assert!(get.starts_with("GET /api/author/audiobooks/id/1 "), "{get}");
    assert!(
        get.to_lowercase().contains("authorization: bearer abk_test"),
        "{get}"
    );

    let put = &seen[1];
    let expected_sha = bm_core::upload::sha256_hex(bytes);
    assert!(
        put.starts_with("PUT /api/author/audiobooks/id/1/chapters/1?title=One&duration_seconds=2413 "),
        "{put}"
    );
    let lower = put.to_lowercase();
    assert!(lower.contains("content-type: audio/mpeg"), "{put}");
    assert!(
        lower.contains(&format!("x-content-sha256: {expected_sha}")),
        "{put}"
    );
    assert!(put.ends_with(std::str::from_utf8(bytes).unwrap()), "body");
}
