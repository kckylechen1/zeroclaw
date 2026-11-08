use futures_util::StreamExt;

/// Read a diagnostic prefix, retaining at most `limit + 1` bytes. The extra
/// byte distinguishes truncation without buffering the rest of an error body.
/// A mid-stream error ends the preview; the caller already owns the HTTP status.
/// This is the bounded file-upload reader shared with other diagnostic paths.
pub(crate) async fn read_response_body_prefix(
    response: reqwest::Response,
    limit: usize,
) -> Vec<u8> {
    let hard_cap = limit.saturating_add(1);
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        let remaining = hard_cap - bytes.len();
        if chunk.len() >= remaining {
            bytes.extend_from_slice(&chunk[..remaining]);
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    bytes
}

pub(crate) fn response_preview(bytes: &[u8], limit: usize) -> String {
    let body = String::from_utf8_lossy(bytes);
    if body.len() <= limit {
        return body.into_owned();
    }
    let mut end = limit;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    crate::i18n::get_required_tool_string_with_args(
        "tool-http-response-preview-truncated",
        &[("body", &body[..end])],
    )
}

#[cfg(test)]
pub(crate) mod test_support {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub(crate) struct StalledResponse {
        pub(crate) url: String,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for StalledResponse {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// Serve one prefix and keep the HTTP body open until the fixture drops.
    /// An old unbounded body collector cannot finish on this fixture.
    pub(crate) async fn stalled_response(
        status: &str,
        content_length: Option<usize>,
        prefix: Vec<u8>,
    ) -> StalledResponse {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/fixture", listener.local_addr().unwrap());
        let framing = content_length.map_or_else(
            || "Transfer-Encoding: chunked\r\n".to_owned(),
            |len| format!("Content-Length: {len}\r\n"),
        );
        let head = format!("HTTP/1.1 {status}\r\n{framing}Connection: close\r\n\r\n");
        let task = ::zeroclaw_spawn::spawn!(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buf).await.unwrap();
                assert!(read > 0 && request.len() < 16 * 1024);
                request.extend_from_slice(&buf[..read]);
            }
            stream.write_all(head.as_bytes()).await.unwrap();
            if content_length.is_none() {
                stream
                    .write_all(format!("{:x}\r\n", prefix.len()).as_bytes())
                    .await
                    .unwrap();
            }
            stream.write_all(&prefix).await.unwrap();
            if content_length.is_none() {
                stream.write_all(b"\r\n").await.unwrap();
            }
            std::future::pending::<()>().await;
            drop(stream);
        });
        StalledResponse { url, task }
    }
}
