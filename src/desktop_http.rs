//! Runtime HTTP wiring for GPUI. Its default client cannot send requests.

use std::{sync::Arc, time::Duration};

use futures::future::BoxFuture;
use gpui::http_client::{AsyncBody, HttpClient, Inner, Request, Response};
use isahc::config::Configurable as _;

pub struct DesktopHttpClient(isahc::HttpClient);

impl DesktopHttpClient {
    pub fn new() -> anyhow::Result<Arc<Self>> {
        let client = isahc::HttpClient::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            // Never forward credentials to a redirect destination.
            .redirect_policy(isahc::config::RedirectPolicy::None)
            .build()?;
        Ok(Arc::new(Self(client)))
    }
}

impl HttpClient for DesktopHttpClient {
    fn send(
        &self,
        request: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        let client = self.0.clone();
        Box::pin(async move {
            let (parts, body) = request.into_parts();
            let mut request = isahc::Request::builder()
                .method(parts.method.as_str())
                .uri(parts.uri.to_string())
                .header("User-Agent", "cururu-desktop/0.1");
            for (name, value) in &parts.headers {
                request = request.header(name.as_str(), value.as_bytes());
            }
            let length = match &body.0 {
                Inner::Empty => Some(0),
                Inner::Bytes(cursor) => {
                    Some(u64::try_from(cursor.get_ref().len())?.saturating_sub(cursor.position()))
                }
                Inner::AsyncReader(_) => None,
            };
            let body = if let Some(length) = length {
                isahc::AsyncBody::from_reader_sized(body, length)
            } else {
                isahc::AsyncBody::from_reader(body)
            };
            let request = request.body(body)?;
            // Do not include URLs, request bodies or headers in transport errors.
            let response = client.send_async(request).await.map_err(|_| {
                anyhow::anyhow!(
                    "Network request failed. Check your connection, proxy and TLS certificates."
                )
            })?;
            let (parts, body) = response.into_parts();
            let mut response = Response::builder().status(parts.status.as_u16());
            for (name, value) in &parts.headers {
                response = response.header(name.as_str(), value.as_bytes());
            }
            Ok(response.body(AsyncBody::from_reader(body))?)
        })
    }

    fn user_agent(&self) -> Option<&gpui::http_client::http::HeaderValue> {
        None
    }
    fn proxy(&self) -> Option<&url::Url> {
        None
    }
    fn type_name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::AsyncReadExt as _;
    use std::{
        io::{Read as _, Write as _},
        net::TcpListener,
        thread,
    };

    #[test]
    fn runtime_client_sends_requests_and_streams_responses_without_tokio() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut buffer = [0; 4096];
            let count = socket.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..count]);
            assert!(request.starts_with("GET /device-test HTTP/1.1"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer synthetic")
            );
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        });
        futures::executor::block_on(async {
            let client = DesktopHttpClient::new().unwrap();
            let request = Request::builder()
                .uri(format!("http://{address}/device-test"))
                .header("Authorization", "Bearer synthetic")
                .body(AsyncBody::empty())
                .unwrap();
            let response = client.send(request).await.unwrap();
            assert_eq!(response.status(), 200);
            let mut text = String::new();
            response
                .into_body()
                .read_to_string(&mut text)
                .await
                .unwrap();
            assert_eq!(text, "ok");
        });
        server.join().unwrap();
    }

    #[test]
    fn redirects_are_returned_without_forwarding_authorization() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let redirect_target = TcpListener::bind("127.0.0.1:0").unwrap();
        let target_address = redirect_target.local_addr().unwrap();
        redirect_target.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut buffer = [0; 4096];
            let count = socket.read(&mut buffer).unwrap();
            assert!(count > 0);
            write!(socket, "HTTP/1.1 302 Found\r\nLocation: http://{target_address}/must-not-contact\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        futures::executor::block_on(async {
            let request = Request::builder()
                .uri(format!("http://{address}/redirect"))
                .header("Authorization", "Bearer synthetic")
                .body(AsyncBody::empty())
                .unwrap();
            let response = DesktopHttpClient::new()
                .unwrap()
                .send(request)
                .await
                .unwrap();
            assert_eq!(response.status(), 302);
            assert_eq!(
                redirect_target.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        });
        server.join().unwrap();
    }

    #[test]
    fn device_style_form_post_preserves_body_length_and_http_errors() {
        use std::io::{BufRead as _, BufReader};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = BufReader::new(socket);
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                assert!(socket.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line);
                assert!(headers.len() < 8192);
            }
            assert!(headers.starts_with("POST /device HTTP/1.1"));
            assert!(headers.to_ascii_lowercase().contains("content-length: 5"));
            let mut body = [0; 5];
            socket.read_exact(&mut body).unwrap();
            assert_eq!(&body, b"id=ab");
            socket
                .get_mut()
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                )
                .unwrap();
        });
        futures::executor::block_on(async {
            let request = Request::builder()
                .method("POST")
                .uri(format!("http://{address}/device"))
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(AsyncBody::from("id=ab"))
                .unwrap();
            let response = DesktopHttpClient::new()
                .unwrap()
                .send(request)
                .await
                .unwrap();
            assert_eq!(response.status(), 400);
        });
        server.join().unwrap();
    }
}
