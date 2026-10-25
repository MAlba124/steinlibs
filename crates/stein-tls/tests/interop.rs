//! The server role against other people's clients, over a real socket.
//!
//! Ignored by default. It needs `openssl` and `curl`, which it takes from
//! `nix shell`, and a free port 18443.
//!
//! ```text
//! cargo test -p stein-tls --test interop -- --ignored --nocapture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::Duration;

use stein_tls::{Certs, KTLS_SUITES, Key, Server, ServerConfig, ServerSetup, Tickets};

const NAME: &str = "stein.test";
const PORT: u16 = 18443;
const BODY: &str = "hello from the server role\n";

/// What the server saw of one connection.
#[derive(Debug)]
struct Seen {
    version: String,
    suite: String,
    alpn: String,
    sni: String,
    resumed: bool,
    request: String,
}

/// One blocking connection. Handshake, a request head, a response, close.
fn serve_one(stream: &mut TcpStream, cfg: &ServerConfig) -> Result<Seen, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let mut s = Server::accept(cfg).map_err(|e| e.to_string())?;
    let mut inbox = Vec::new();
    let mut request = Vec::new();
    let mut tmp = vec![0u8; 32 * 1024];
    let mut out = vec![0u8; 64 * 1024];
    while !request.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut tmp).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("the client hung up".into());
        }
        inbox.extend_from_slice(&tmp[..n]);
        loop {
            let took = s.received(&mut inbox);
            // Whatever happened, what the session has to say goes out.
            let n = s.transmit(&mut out).map_err(|e| e.to_string())?;
            stream.write_all(&out[..n]).map_err(|e| e.to_string())?;
            let took = took.map_err(|e| e.to_string())?;
            inbox.drain(..took);
            let n = s.read_plain(&mut tmp).map_err(|e| e.to_string())?;
            request.extend_from_slice(&tmp[..n]);
            if took == 0 && n == 0 {
                break;
            }
        }
    }
    let seen = Seen {
        version: format!("{:?}", s.version().unwrap()),
        suite: format!("{:?}", s.suite().unwrap()),
        alpn: String::from_utf8_lossy(s.alpn().unwrap_or(b"-")).into_owned(),
        sni: s.sni().unwrap_or("-").to_string(),
        resumed: s.resumed(),
        request: String::from_utf8_lossy(&request).into_owned(),
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BODY}",
        BODY.len()
    );
    let w = s
        .write_plain(response.as_bytes(), &mut out)
        .map_err(|e| e.to_string())?;
    assert_eq!(w.took, response.len());
    stream
        .write_all(&out[..w.wrote])
        .map_err(|e| e.to_string())?;
    s.close();
    let n = s.transmit(&mut out).map_err(|e| e.to_string())?;
    stream.write_all(&out[..n]).map_err(|e| e.to_string())?;
    // Until the client has gone, so the response is not cut by a reset.
    while matches!(stream.read(&mut tmp), Ok(n) if n > 0) {}
    Ok(seen)
}

struct Fixture {
    cert_pem: String,
    key_pem: String,
    ca_path: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let made = rcgen::generate_simple_self_signed(vec![NAME.to_string()]).unwrap();
    let ca_path =
        std::env::temp_dir().join(format!("stein-tls-interop-{}.pem", std::process::id()));
    std::fs::write(&ca_path, made.cert.pem()).unwrap();
    Fixture {
        cert_pem: made.cert.pem(),
        key_pem: made.signing_key.serialize_pem(),
        ca_path,
    }
}

fn config(f: &Fixture, change: impl FnOnce(&mut ServerSetup<'_>)) -> ServerConfig {
    let mut setup = ServerSetup {
        chain: Certs::Pem(f.cert_pem.as_bytes()),
        key: Key::Pem(f.key_pem.as_bytes()),
        alpn: &[b"h2", b"http/1.1"],
        tls13_only: false,
        suites: &[],
        tickets: Tickets::Sealed,
        extract: false,
    };
    change(&mut setup);
    stein_tls::server_config(&setup).unwrap()
}

/// Run `tool` from nix against one served connection.
fn against(cfg: &ServerConfig, tool: &str, args: &[&str], stdin: &str) -> (Seen, String) {
    let listener = TcpListener::bind(("127.0.0.1", PORT)).expect("port 18443 is free");
    let mut child = Command::new("nix")
        .args(["shell", "nixpkgs#openssl", "nixpkgs#curl", "-c", tool])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("nix is there");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let (mut stream, _) = listener.accept().unwrap();
    let seen = serve_one(&mut stream, cfg);
    drop(stream);
    let done = child.wait_with_output().unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&done.stdout),
        String::from_utf8_lossy(&done.stderr)
    );
    match seen {
        Ok(seen) => (seen, said),
        Err(e) => panic!("{tool} {args:?}: the server failed: {e}\n{said}"),
    }
}

const REQUEST: &str = "GET / HTTP/1.1\r\nHost: stein.test\r\n\r\n";

fn openssl(cfg: &ServerConfig, ca: &str, extra: &[&str]) -> (Seen, String) {
    let connect = format!("127.0.0.1:{PORT}");
    let mut args = vec![
        "s_client",
        "-connect",
        &connect,
        "-servername",
        NAME,
        "-CAfile",
        ca,
        "-verify_return_error",
        "-brief",
        "-ign_eof",
    ];
    args.extend_from_slice(extra);
    against(cfg, "openssl", &args, REQUEST)
}

fn line<'a>(said: &'a str, has: &str) -> &'a str {
    said.lines().find(|l| l.contains(has)).unwrap_or("-").trim()
}

/// The defaults, and TLS 1.2.
fn openssl_defaults(f: &Fixture, ca: &str) {
    let cfg = config(f, |_| {});
    let (seen, said) = openssl(&cfg, ca, &["-alpn", "h2,http/1.1"]);
    println!("openssl default: server saw {seen:?}");
    println!("  {}", line(&said, "Protocol version"));
    println!("  {}", line(&said, "Ciphersuite"));
    println!("  {}", line(&said, "ALPN"));
    println!("  {}", line(&said, "Verification"));
    assert!(said.contains(BODY), "no response body:\n{said}");
    assert!(said.contains("Verification: OK"), "{said}");
    assert_eq!(
        (&seen.version[..], &seen.alpn[..], &seen.sni[..]),
        ("TLSv1_3", "h2", NAME)
    );
    assert_eq!(seen.request, REQUEST);
    assert!(!seen.resumed, "a first connection resumed");

    let (seen, said) = openssl(&cfg, ca, &["-tls1_2", "-alpn", "http/1.1"]);
    println!("openssl -tls1_2: server saw {seen:?}");
    println!("  {}", line(&said, "Protocol version"));
    println!("  {}", line(&said, "Ciphersuite"));
    assert!(said.contains(BODY), "no response body:\n{said}");
    assert_eq!((&seen.version[..], &seen.alpn[..]), ("TLSv1_2", "http/1.1"));
}

/// Each suite the kernel takes, on a server that offers only those.
fn openssl_kernel_suites(f: &Fixture, ca: &str) {
    for (suite, name) in KTLS_SUITES.iter().zip([
        "TLS_AES_128_GCM_SHA256",
        "TLS_AES_256_GCM_SHA384",
        "TLS_CHACHA20_POLY1305_SHA256",
    ]) {
        let cfg = config(f, |s| {
            s.tls13_only = true;
            s.suites = &KTLS_SUITES;
        });
        let (seen, said) = openssl(&cfg, ca, &["-ciphersuites", name]);
        println!("openssl {name}: server saw {seen:?}");
        println!("  {}", line(&said, "Ciphersuite"));
        assert!(said.contains(BODY), "no response body:\n{said}");
        assert_eq!(seen.suite, format!("{suite:?}"));
        assert_eq!(seen.alpn, "-");
    }
}

fn openssl_tls12_refused(f: &Fixture) {
    // A server that only speaks 1.3, a client that only speaks 1.2.
    let cfg = config(f, |s| s.tls13_only = true);
    let listener = TcpListener::bind(("127.0.0.1", PORT)).unwrap();
    let child = Command::new("nix")
        .args(["shell", "nixpkgs#openssl", "-c", "openssl", "s_client"])
        .args([
            "-connect",
            &format!("127.0.0.1:{PORT}"),
            "-tls1_2",
            "-brief",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut stream, _) = listener.accept().unwrap();
    let err = serve_one(&mut stream, &cfg).expect_err("1.2 was accepted");
    drop(stream);
    let said = String::from_utf8_lossy(&child.wait_with_output().unwrap().stderr).into_owned();
    println!("openssl -tls1_2 against 1.3 only: server said {err:?}");
    println!("  {}", line(&said, "alert"));
    drop(listener);
}

fn curl_http1(f: &Fixture, ca: &str) {
    let url = format!("https://{NAME}:{PORT}/");
    let resolve = format!("{NAME}:{PORT}:127.0.0.1");
    let cfg = config(f, |_| {});
    let (seen, said) = against(
        &cfg,
        "curl",
        &[
            "-sv",
            "--http1.1",
            "--cacert",
            ca,
            "--resolve",
            &resolve,
            &url,
        ],
        "",
    );
    println!("curl --http1.1: server saw {seen:?}");
    println!("  {}", line(&said, "SSL connection using"));
    println!("  {}", line(&said, "ALPN: server accepted"));
    println!("  {}", line(&said, "SSL certificate verif"));
    assert!(said.contains(BODY), "no response body:\n{said}");
    assert_eq!((&seen.alpn[..], &seen.sni[..]), ("http/1.1", NAME));
    assert!(seen.request.starts_with("GET / HTTP/1.1\r\n"));
}

#[test]
#[ignore = "needs openssl and curl from nix shell, and port 18443"]
fn openssl_and_curl_complete_a_handshake() {
    let f = fixture();
    let ca = f.ca_path.to_str().unwrap().to_string();
    openssl_defaults(&f, &ca);
    openssl_kernel_suites(&f, &ca);
    openssl_tls12_refused(&f);
    curl_http1(&f, &ca);
    let _ = std::fs::remove_file(&f.ca_path);
}
