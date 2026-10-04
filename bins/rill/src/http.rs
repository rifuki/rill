//! The signer's HTTP: fetching owner-signed grants and asking the builder for an action's
//! transaction. Nothing else in this binary talks to anything but a Sui node.
//!
//! Written on the hyper and rustls already in the tree rather than a client crate, which would have
//! added packages and build scripts to the one binary that holds a key. One request per connection,
//! because a signer makes a handful of calls per action and a pool would be state with no payoff.
//!
//! Every byte that comes back is untrusted. A grant is used only after its owner's signature checks
//! out against the chain, and an envelope only after the signer's own validation and simulation;
//! this module's job ends at handing over the JSON.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::{ACCEPT, CONTENT_TYPE, HOST, USER_AGENT};
use hyper::{Method, Request, Uri};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

/// How long one request may take end to end, connection included.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Responses larger than this are refused: a grant list or an envelope is a few kilobytes.
const MAX_BODY: usize = 4 * 1024 * 1024;

pub async fn get_json(url: &str) -> Result<Value, String> {
    request(Method::GET, url, None, None).await
}

pub async fn post_json(url: &str, body: &Value) -> Result<Value, String> {
    let bytes = serde_json::to_vec(body).map_err(|e| e.to_string())?;
    request(Method::POST, url, Some(bytes), None).await
}

/// A POST carrying a session token, for the owner's own calls (`rill grant`). Never used with the
/// agent's tools, which read public grants and build without a session.
pub async fn post_json_as(url: &str, body: &Value, bearer: &str) -> Result<Value, String> {
    let bytes = serde_json::to_vec(body).map_err(|e| e.to_string())?;
    request(Method::POST, url, Some(bytes), Some(bearer)).await
}

async fn request(
    method: Method,
    url: &str,
    body: Option<Vec<u8>>,
    bearer: Option<&str>,
) -> Result<Value, String> {
    tokio::time::timeout(TIMEOUT, send(method, url, body, bearer))
        .await
        .map_err(|_| format!("{url} did not answer within {}s", TIMEOUT.as_secs()))?
}

/// Plain HTTP only to this machine. An API URL typed as `http://` for a remote host is refused
/// rather than sent in the clear, because what comes back decides what the signer is asked to sign.
fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// Returning drops the set and cancels unused connection attempts.
async fn first_connection(
    mut attempts: tokio::task::JoinSet<std::io::Result<TcpStream>>,
) -> std::io::Result<TcpStream> {
    let mut last_error = std::io::Error::other("DNS returned no usable addresses");
    while let Some(result) = attempts.join_next().await {
        match result {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(error)) => last_error = error,
            Err(error) => last_error = std::io::Error::other(error),
        }
    }
    Err(last_error)
}

async fn send(
    method: Method,
    url: &str,
    body: Option<Vec<u8>>,
    bearer: Option<&str>,
) -> Result<Value, String> {
    let uri: Uri = url.parse().map_err(|_| format!("{url} is not a URL"))?;
    let https = match uri.scheme_str() {
        Some("https") => true,
        Some("http") => false,
        _ => return Err(format!("{url} is neither https nor http")),
    };
    let host = uri
        .host()
        .ok_or_else(|| format!("{url} names no host"))?
        .to_owned();
    if !https && !is_loopback(&host) {
        return Err(format!(
            "{url} is plain http to another machine. Use https, or http only to localhost."
        ));
    }
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
    let authority = uri
        .authority()
        .map(|a| a.as_str().to_owned())
        .unwrap_or_else(|| host.clone());
    let path = uri.path_and_query().map_or("/", |p| p.as_str()).to_owned();

    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(HOST, authority)
        .header(ACCEPT, "application/json")
        .header(CONTENT_TYPE, "application/json")
        .header(
            USER_AGENT,
            concat!("rill-wallet/", env!("CARGO_PKG_VERSION")),
        );
    if let Some(token) = bearer {
        request = request.header(hyper::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = request
        .body(Full::new(Bytes::from(body.unwrap_or_default())))
        .map_err(|e| e.to_string())?;

    // DNS can contain an unreachable IPv6 address before a working IPv4 one.
    // Race transport connections, then send the request only on the first success.
    let addresses = tokio::net::lookup_host((host.trim_matches(|c| c == '[' || c == ']'), port))
        .await
        .map_err(|e| format!("resolving {host}:{port}: {e}"))?;
    let mut attempts = tokio::task::JoinSet::new();
    for address in addresses {
        attempts.spawn(TcpStream::connect(address));
    }
    let tcp = first_connection(attempts)
        .await
        .map_err(|e| format!("connecting to {host}:{port}: {e}"))?;
    let (status, bytes) = if https {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|_| format!("{host} is not a valid TLS server name"))?;
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .map_err(|e| format!("TLS with {host}: {e}"))?;
        exchange(tls, request).await?
    } else {
        exchange(tcp, request).await?
    };

    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        format!(
            "{url} answered {status} with something that is not JSON: {}",
            String::from_utf8_lossy(&bytes[..bytes.len().min(200)])
        )
    })?;
    if !(200..300).contains(&status) {
        let reason = value
            .get("error")
            .and_then(|e| {
                e.as_str()
                    .map(str::to_owned)
                    .or_else(|| Some(e.to_string()))
            })
            .unwrap_or_else(|| value.to_string());
        return Err(format!("{url} answered {status}: {reason}"));
    }
    Ok(value)
}

async fn exchange<S>(io: S, request: Request<Full<Bytes>>) -> Result<(u16, Vec<u8>), String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(io))
        .await
        .map_err(|e| e.to_string())?;
    let driver = tokio::spawn(connection);
    let response = sender
        .send_request(request)
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let mut body = response.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| e.to_string())?;
        if let Some(chunk) = frame.data_ref() {
            if bytes.len() + chunk.len() > MAX_BODY {
                driver.abort();
                return Err(format!("the response exceeded {MAX_BODY} bytes"));
            }
            bytes.extend_from_slice(chunk);
        }
    }
    driver.abort();
    Ok((status, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One canned HTTP response from a listener on this machine, and what the request said.
    async fn serve_once(status: &str, body: &str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        (url, handle)
    }

    #[tokio::test]
    async fn a_json_answer_comes_back_and_the_request_names_its_host_and_path() {
        let (url, server) = serve_once("200 OK", r#"{"ok":true}"#).await;
        let value = get_json(&format!("{url}/api/grants/0x3e")).await.unwrap();
        assert_eq!(value, serde_json::json!({"ok": true}));
        let seen = server.await.unwrap();
        assert!(seen.starts_with("GET /api/grants/0x3e HTTP/1.1"), "{seen}");
        assert!(seen.to_lowercase().contains("host: 127.0.0.1:"), "{seen}");
    }

    #[tokio::test]
    async fn a_refusal_is_an_error_carrying_the_servers_reason() {
        let (url, _server) = serve_once("403 Forbidden", r#"{"error":"not yours"}"#).await;
        let error = post_json(&format!("{url}/api/grants"), &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            error.contains("403") && error.contains("not yours"),
            "{error}"
        );
    }

    #[tokio::test]
    #[ignore = "reaches a public HTTPS endpoint"]
    async fn https_verifies_a_real_certificate_and_reads_json() {
        let value = get_json("https://api.github.com/repos/rifuki/rill")
            .await
            .unwrap();
        assert_eq!(value["full_name"], "rifuki/rill");
    }

    #[tokio::test]
    async fn plain_http_to_another_machine_is_refused_before_connecting() {
        let error = get_json("http://api.example.com/api/grants/0x3e")
            .await
            .unwrap_err();
        assert!(error.contains("plain http"), "{error}");
    }
    #[tokio::test]
    #[ignore = "read-only production API; requires network"]
    async fn production_api_connects_without_waiting_for_broken_ipv6() {
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            get_json("https://api.rill.rifuki.dev/health"),
        )
        .await;
        assert!(
            result.is_ok(),
            "reachable IPv4 must not wait behind a stalled IPv6 address"
        );
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn a_stalled_or_failed_address_cannot_hide_a_reachable_one() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut attempts = tokio::task::JoinSet::new();
        attempts.spawn(std::future::pending::<std::io::Result<TcpStream>>());
        attempts.spawn(async { Err(std::io::Error::other("unreachable address")) });
        attempts.spawn(TcpStream::connect(listener.local_addr().unwrap()));
        let connected = tokio::time::timeout(Duration::from_secs(1), first_connection(attempts))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            connected.peer_addr().unwrap(),
            listener.local_addr().unwrap()
        );
    }

    #[tokio::test]
    async fn no_addresses_or_all_failures_return_an_error() {
        assert!(first_connection(tokio::task::JoinSet::new()).await.is_err());
        let mut attempts = tokio::task::JoinSet::new();
        attempts.spawn(async { Err(std::io::Error::other("refused")) });
        assert!(first_connection(attempts)
            .await
            .unwrap_err()
            .to_string()
            .contains("refused"));
    }
}
