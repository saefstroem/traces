use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::combinators::BoxBody;
use hyper::body::Body;
use hyper::header::ACCEPT_ENCODING;
use hyper::http::request::Parts;
use serde_json::Value;
use tokio::spawn;
use tokio::sync::mpsc::unbounded_channel;

use super::{Events, Exchange, Provider, Trace};

pub struct Capture {
    provider: &'static dyn Provider,
    exchange: Exchange,
}

impl Capture {
    /// Creates a new capture for the given provider and request, 
    /// removing the accept-encoding header and recording the prompt if available.
    pub fn request(provider: &'static dyn Provider, parts: &mut Parts, request: &Value) -> Self {
        parts.headers.remove(ACCEPT_ENCODING);
        // create a new exchange to record traces
        let mut exchange = Exchange::default();
        // extract the initial prompt of the capture
        if let Some(prompt) = provider.prompt(request) {
            exchange.push(Trace::Input(prompt.clone()));
        }
        Self { provider, exchange }
    }

    pub fn response<B, F>(self, body: B, done: F) -> BoxBody<Bytes, B::Error>
    where
        B: Body<Data = Bytes> + Send + Sync + 'static,
        F: FnOnce(Exchange) + Send + 'static,
    {
        let Self { provider, mut exchange } = self;
        let (sender, mut receiver) = unbounded_channel::<Bytes>();
        // create a task to process incoming events and update the exchange accordingly
        spawn(async move {
            let mut events = Events::default();
            while let Some(chunk) = receiver.recv().await {
                for event in events.feed(&chunk) {
                    provider.read(&event, &mut exchange);
                }
            }
            done(exchange);
        });
        body.map_frame(move |frame| {
            if let Some(data) = frame.data_ref() {
                let _ = sender.send(data.clone());
            }
            frame
        })
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::Request;
    use serde_json::{Value, json};
    use tokio::sync::oneshot;

    use super::Capture;
    use crate::providers::{Exchange, Provider, Trace};

    struct Echo;

    impl Provider for Echo {
        fn name(&self) -> &'static str {
            "echo"
        }

        fn prompt<'a>(&self, request: &'a Value) -> Option<&'a Value> {
            request.get("prompt")
        }

        fn read(&self, event: &Value, exchange: &mut Exchange) {
            exchange.append(Trace::Output(event.as_str().unwrap_or_default().to_string()));
        }
    }

    #[test]
    fn records_the_prompt_and_asks_upstream_for_a_plain_body() {
        let (mut parts, ()) = Request::post("/").header("accept-encoding", "gzip").body(()).unwrap().into_parts();
        let capture = Capture::request(&Echo, &mut parts, &json!({"prompt": "hi"}));
        assert!(parts.headers.get("accept-encoding").is_none());
        assert_eq!(capture.exchange.traces, [Trace::Input(json!("hi"))]);
        assert!(Capture::request(&Echo, &mut parts, &Value::Null).exchange.traces.is_empty());
    }

    #[tokio::test]
    async fn passes_the_response_through_and_hands_over_the_finished_exchange() {
        let (mut parts, ()) = Request::post("/").body(()).unwrap().into_parts();
        let stream = Bytes::from_static(b"data: \"a\"\n\ndata: \"b\"\n\n");
        let (sender, finished) = oneshot::channel();
        let capture = Capture::request(&Echo, &mut parts, &json!({"prompt": "hi"}));
        let body = capture.response(Full::new(stream.clone()), move |exchange| sender.send(exchange.traces).unwrap());
        assert_eq!(body.collect().await.unwrap().to_bytes(), stream);
        assert_eq!(finished.await.unwrap(), [Trace::Input(json!("hi")), Trace::Output("ab".into())]);
    }
}
