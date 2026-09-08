//! Download + sha256 verify + retry/backoff + proxy (`/scope.md` §11 / ARCHITECTURE §11).
//!
//! Every artifact this installer fetches (VS Code, uv, the offline uv-managed
//! Python) goes through [`fetch_and_verify`]: retries cover transport failures
//! only (connection errors, timeouts, non-2xx status), matching M0's
//! `curl --retry 3` semantics; a completed download whose sha256 does not
//! match the manifest is a hard, non-retried failure -- retrying a
//! deterministic wrong-content response would not fix it, and silently
//! accepting it is exactly the class of bug this exists to prevent. The file
//! is only ever written to its final path atomically (temp file + rename in
//! the same directory), after the hash check passes, so a failed or
//! in-progress download can never be mistaken for a verified artifact.
//!
//! The `reqwest::blocking::Client` proxy detection is left at its default
//! (`Client::builder().build()`, never `.no_proxy()`), which honors
//! `HTTPS_PROXY`/`HTTP_PROXY` from the environment automatically.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("GET {url} failed after {attempts} attempt(s): {source}")]
    RequestFailed {
        url: String,
        attempts: u32,
        #[source]
        source: reqwest::Error,
    },
    #[error("sha256 mismatch for {url}: expected {expected}, got {actual}")]
    Sha256Mismatch {
        url: String,
        expected: String,
        actual: String,
    },
    #[error("could not write {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Retry policy. `max_attempts` counts the FIRST try too (matching curl's
/// `--retry 3` = up to 4 total attempts): `max_attempts: 4` retries three
/// times after an initial failure. Backoff is exponential from
/// `backoff_base`, doubling each subsequent attempt.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub max_attempts: u32,
    pub backoff_base: Duration,
}

impl Default for FetchOptions {
    fn default() -> Self {
        FetchOptions {
            max_attempts: 4,
            backoff_base: Duration::from_millis(500),
        }
    }
}

impl FetchOptions {
    fn backoff_for(&self, attempts_so_far: u32) -> Duration {
        self.backoff_base * 2u32.saturating_pow(attempts_so_far)
    }
}

/// GET `url` with retry, verify the body's sha256 against `expected_sha256_hex`
/// (case-insensitive), then atomically write it to `dest`. `dest`'s parent
/// directory is created if missing. On any failure `dest` is left untouched
/// (never a partial or unverified file at the final path).
pub fn fetch_and_verify(
    client: &reqwest::blocking::Client,
    url: &str,
    expected_sha256_hex: &str,
    dest: &Path,
    opts: &FetchOptions,
) -> Result<(), FetchError> {
    let bytes = fetch_with_retry(client, url, opts)?;
    let actual = sha256_hex(&bytes);
    if !actual.eq_ignore_ascii_case(expected_sha256_hex) {
        return Err(FetchError::Sha256Mismatch {
            url: url.to_string(),
            expected: expected_sha256_hex.to_string(),
            actual,
        });
    }
    write_atomic(dest, &bytes)?;
    Ok(())
}

fn fetch_with_retry(
    client: &reqwest::blocking::Client,
    url: &str,
    opts: &FetchOptions,
) -> Result<Vec<u8>, FetchError> {
    let mut last_err = None;
    for attempt in 0..opts.max_attempts.max(1) {
        let result = client
            .get(url)
            .send()
            .and_then(|resp| resp.error_for_status())
            .and_then(|resp| resp.bytes());
        match result {
            Ok(bytes) => return Ok(bytes.to_vec()),
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < opts.max_attempts {
                    std::thread::sleep(opts.backoff_for(attempt));
                }
            }
        }
    }
    Err(FetchError::RequestFailed {
        url: url.to_string(),
        attempts: opts.max_attempts.max(1),
        source: last_err.expect("loop ran at least once"),
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn write_atomic(dest: &Path, bytes: &[u8]) -> Result<(), FetchError> {
    let parent = dest.parent().ok_or_else(|| FetchError::Io {
        path: dest.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination has no parent directory",
        ),
    })?;
    std::fs::create_dir_all(parent).map_err(|source| FetchError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let file_name = dest.file_name().ok_or_else(|| FetchError::Io {
        path: dest.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination has no file name",
        ),
    })?;
    let tmp = parent.join(format!("{}.tmp", file_name.to_string_lossy()));
    std::fs::write(&tmp, bytes).map_err(|source| FetchError::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, dest).map_err(|source| FetchError::Io {
        path: dest.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A tiny single-purpose HTTP/1.1 server: serves exactly `responses.len()`
    /// connections, one canned (status, body) response each, `Connection:
    /// close` after every response so the client always opens a fresh
    /// connection per attempt (lines the server's accept() count up 1:1 with
    /// the client's retry attempts).
    struct TestServer {
        addr: SocketAddr,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl TestServer {
        /// A server that never receives the connections it expects must fail
        /// fast, not hang the test (and, if this were ever the real bug
        /// instead of a deliberate mutation, not hang the whole CI job):
        /// nonblocking `accept()` polled against a deadline, bailing the
        /// thread (so `Drop`'s `join()` returns) if a connection never comes.
        fn start(responses: Vec<(u16, Vec<u8>)>) -> TestServer {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            listener.set_nonblocking(true).unwrap();
            let handle = std::thread::spawn(move || {
                for (status, body) in responses {
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    let mut stream = loop {
                        match listener.accept() {
                            Ok((s, _)) => break s,
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                if std::time::Instant::now() >= deadline {
                                    return;
                                }
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            Err(_) => return,
                        }
                    };
                    // Windows hands back an accepted socket that INHERITED the
                    // listener's non-blocking mode; Unix does not. The listener is
                    // non-blocking only so accept() can poll a deadline, and
                    // everything below assumes blocking: the read loop turns
                    // WouldBlock into n == 0 and stops without reading the request,
                    // and write_all can WouldBlock into a discarded error, so the
                    // client sees a connection that closed without a response.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut buf = [0u8; 4096];
                    let mut seen = Vec::new();
                    loop {
                        let n = stream.read(&mut buf).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        seen.extend_from_slice(&buf[..n]);
                        if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let reason = if status == 200 { "OK" } else { "Error" };
                    let head = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                    let _ = stream.flush();
                }
            });
            TestServer {
                addr,
                handle: Some(handle),
            }
        }

        fn url(&self) -> String {
            format!("http://{}/asset", self.addr)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn temp_dest(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "blockless-installer-fetch-test-{name}-{}-{n}",
            std::process::id()
        ));
        dir.join("asset.bin")
    }

    fn fast_opts(max_attempts: u32) -> FetchOptions {
        FetchOptions {
            max_attempts,
            backoff_base: Duration::from_millis(1),
        }
    }

    #[test]
    fn sha_mismatch_fails_loudly_and_writes_nothing() {
        let body = b"correct bytes".to_vec();
        let server = TestServer::start(vec![(200, body)]);
        let client = reqwest::blocking::Client::new();
        let dest = temp_dest("sha-mismatch");

        let err = fetch_and_verify(
            &client,
            &server.url(),
            "0000000000000000000000000000000000000000000000000000000000000000",
            &dest,
            &fast_opts(1),
        )
        .unwrap_err();

        assert!(
            matches!(err, FetchError::Sha256Mismatch { .. }),
            "got {err:?}"
        );
        assert!(
            !dest.exists(),
            "a mismatched download must never reach the final path"
        );
    }

    #[test]
    fn retry_then_succeed() {
        let body = b"eventually correct".to_vec();
        let expected = sha256_hex(&body);
        let server = TestServer::start(vec![(503, vec![]), (503, vec![]), (200, body.clone())]);
        let client = reqwest::blocking::Client::new();
        let dest = temp_dest("retry-then-succeed");

        fetch_and_verify(&client, &server.url(), &expected, &dest, &fast_opts(3)).unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), body);
    }

    #[test]
    fn retry_exhausted_fails_loudly() {
        let server = TestServer::start(vec![(503, vec![]), (503, vec![]), (503, vec![])]);
        let client = reqwest::blocking::Client::new();
        let dest = temp_dest("retry-exhausted");

        let err = fetch_and_verify(
            &client,
            &server.url(),
            "deadbeef00000000000000000000000000000000000000000000000000000000",
            &dest,
            &fast_opts(3),
        )
        .unwrap_err();

        match err {
            FetchError::RequestFailed { attempts, .. } => assert_eq!(attempts, 3),
            other => panic!("expected RequestFailed, got {other:?}"),
        }
        assert!(!dest.exists());
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        // sha256("") -- a fixed, independently-verifiable vector, to catch a
        // hasher/encoding mistake that a self-referential round-trip test
        // (hash it, then compare to itself) could never catch.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn backoff_doubles_each_attempt() {
        let opts = FetchOptions {
            max_attempts: 4,
            backoff_base: Duration::from_millis(10),
        };
        assert_eq!(opts.backoff_for(0), Duration::from_millis(10));
        assert_eq!(opts.backoff_for(1), Duration::from_millis(20));
        assert_eq!(opts.backoff_for(2), Duration::from_millis(40));
    }
}
