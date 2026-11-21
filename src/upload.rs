use std::sync::Arc;
use std::time::Duration;

use aws_sdk_s3::config::{
    BehaviorVersion, Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation,
};
use aws_sdk_s3::error::DisplayErrorContext;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::{Client, Config};
use tokio::time::sleep;
use tokio::{select, spawn};
use tracing::error;

use crate::settings::Settings;
use crate::store::Store;

/// Handles uploading segments from the store to an S3 bucket
pub struct Uploader {
    /// The S3 client used for uploading segments
    client: Client,
    /// The S3 bucket to which segments are uploaded
    bucket: String,
    /// The store from which segments are taken for uploading
    store: Arc<Store>,
}

impl Uploader {
    /// Starts the uploader, spawning a background task to periodically flush segments to S3
    pub fn start(settings: &Settings, store: Arc<Store>) -> Arc<Self> {
        // Create AWS credentials and configure the S3 client
        let credentials = Credentials::new(&settings.access_key_id, &settings.secret_access_key, None, None, "traces");
        // Build the S3 client configuration with the specified settings
        let config = Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .endpoint_url(&settings.endpoint)
            .region(Region::new(settings.region.clone()))
            .credentials_provider(credentials)
            .force_path_style(true)
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
            .build();
        // Create the uploader instance with the configured S3 client and store
        let uploader = Arc::new(Self { client: Client::from_conf(config), bucket: settings.bucket.clone(), store });
        let (running, every) = (uploader.clone(), Duration::from_secs(settings.purge_seconds));
        // Spawn a background task to periodically flush segments to S3
        spawn(async move {
            loop {
                select! {
                    () = running.store.full.notified() => {}
                    () = sleep(every) => {}
                }
                if !running.flush().await {
                    sleep(every).await;
                }
            }
        });
        uploader
    }

    /// Flushes all segments from the store to S3, returning whether all uploads succeeded
    pub async fn flush(&self) -> bool {
        let mut uploaded = true; // assume all uploads will succeed initially
        for segment in self.store.take() {
            // Prepare the body for the S3 PUT request by wrapping the segment items in an array
            let body = [b"[".as_slice(), &segment.items, b"]"].concat();
            // Build the PUT request for the S3 client with the specified bucket, key, and content type
            let put = self.client.put_object().bucket(&self.bucket).key(&segment.key).content_type("application/json");
            // Send the PUT request and handle any errors that occur during the upload
            if let Err(error) = put.body(ByteStream::from(body)).send().await {
                // Log the error and restore the segment to the store for retry
                error!("upload {}/{}: {}", self.bucket, segment.key, DisplayErrorContext(&error));
                // Restore the segment to the store for retry
                self.store.restore(segment);
                // Mark the upload as failed so the caller knows not all uploads succeeded
                uploaded = false;
            }
        }
        uploaded
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::body::Incoming;
    use hyper::server::conn::http1::Builder;
    use hyper::service::service_fn;
    use hyper::{Request, Response, StatusCode};
    use hyper_util::rt::TokioIo;
    use serde_json::{Value, from_slice, json};
    use tokio::net::TcpListener;
    use tokio::spawn;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
    use tokio::time::timeout;

    use super::Uploader;
    use crate::providers::Trace;
    use crate::settings::Settings;
    use crate::store::Store;

    async fn fake_s3(purge_seconds: u64) -> (Settings, UnboundedReceiver<(String, bool, Value)>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, puts) = unbounded_channel();
        spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let sender = sender.clone();
                let answer = service_fn(move |request: Request<Incoming>| {
                    let sender = sender.clone();
                    async move {
                        let path = request.uri().path().to_string();
                        let signed = request.headers().get("authorization").is_some_and(|value| {
                            value.to_str().unwrap().starts_with("AWS4-HMAC-SHA256 Credential=test-key/")
                        });
                        let body = request.into_body().collect().await?.to_bytes();
                        let mut response = Response::new(Full::new(Bytes::new()));
                        if path.contains("refused") {
                            *response.status_mut() = StatusCode::FORBIDDEN;
                        }
                        sender.send((path, signed, from_slice(&body).unwrap())).unwrap();
                        Ok::<_, hyper::Error>(response)
                    }
                });
                spawn(Builder::new().serve_connection(TokioIo::new(socket), answer));
            }
        });
        let settings = Settings {
            endpoint: format!("http://{address}"),
            region: "auto".into(),
            bucket: "traces".into(),
            access_key_id: "test-key".into(),
            secret_access_key: "test-secret".into(),
            purge_seconds,
            max_memory_bytes: 1 << 20,
        };
        (settings, puts)
    }

    #[tokio::test]
    async fn uploads_a_signed_segment_at_the_memory_limit_and_keeps_a_refused_one() {
        let (settings, mut puts) = fake_s3(3600).await;
        let store = Arc::new(Store::new(20));
        let uploader = Uploader::start(&settings, store.clone());
        store.add("c1".into(), vec![Trace::Output("a".into()), Trace::Output("b".into())]);
        let (path, signed, body) = timeout(Duration::from_secs(5), puts.recv()).await.unwrap().unwrap();
        assert!(path.starts_with("/traces/c1/") && path.ends_with(".json") && signed, "{path}");
        assert_eq!(body, json!([{"output": "a"}, {"output": "b"}]));
        store.add("refused".into(), vec![Trace::Error("x".into())]);
        assert!(!uploader.flush().await);
        assert!(puts.recv().await.unwrap().0.starts_with("/traces/refused/"));
        assert_eq!(store.take().into_iter().map(|segment| segment.prefix).collect::<Vec<_>>(), ["refused"]);
    }

    #[tokio::test]
    async fn uploads_when_the_purge_interval_ends_before_the_memory_limit() {
        let (settings, mut puts) = fake_s3(1).await;
        let store = Arc::new(Store::new(1 << 20));
        let _uploader = Uploader::start(&settings, store.clone());
        store.add("c2".into(), vec![Trace::Input(json!("hi"))]);
        let (path, _, body) = timeout(Duration::from_secs(5), puts.recv()).await.unwrap().unwrap();
        assert!(path.starts_with("/traces/c2/"), "{path}");
        assert_eq!(body, json!([{"input": "hi"}]));
        assert!(store.take().is_empty());
    }
}
