//! A loopback HTTP server, and a transport that reaches it, for tests that
//! drive a real HTTP client.

use abnegate_http::ReqwestHttpClient;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

const HEADER_END: &[u8] = b"\r\n\r\n";

/// Answer the first request on a loopback port with `response`, written
/// verbatim, and return the URL that reaches it.
pub async fn serve_once(response: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port binds");
    let address = listener
        .local_addr()
        .expect("a bound listener has an address");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("the client connects");
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request
            .windows(HEADER_END.len())
            .any(|window| window == HEADER_END)
        {
            let read = stream.read(&mut buffer).await.expect("the request arrives");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        let _ = stream.write_all(&response).await;
        let _ = stream.shutdown().await;
    });
    format!("http://{address}/")
}

/// A `200 OK` response that declares `length` bytes and carries `body`.
pub fn ok_response(length: usize, body: &[u8]) -> Vec<u8> {
    let mut response =
        format!("HTTP/1.1 200 OK\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n")
            .into_bytes();
    response.extend_from_slice(body);
    response
}

/// A transport that reaches loopback directly, whatever proxy the
/// environment configures, and reads bodies up to `body_limit` bytes.
pub fn loopback_transport(body_limit: usize) -> ReqwestHttpClient {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("a client without a proxy builds");
    ReqwestHttpClient::from(client).with_body_limit(body_limit)
}
