//! [axum](https://github.com/tokio-rs/axum) OpenTelemetry Metrics middleware
//!
//! ## Simple Usage
//! 
//! by default, the metrics will using the [otlp exporter](https://opentelemetry.io/docs/specs/otel/metrics/sdk_exporters/otlp/), 
//! if you want to use the [prometheus exporter](https://opentelemetry.io/docs/specs/otel/metrics/sdk_exporters/prometheus/), 
//! you need to set the `metric_reader` to `PrometheusExporter`, see [Advanced Usage](#advanced-usage) below
//! 
//! ```
//! use axum_otel_metrics::HttpMetricsLayerBuilder;
//! use axum::{response::Html, routing::get, Router};
//! 
//! let metrics = HttpMetricsLayerBuilder::new()
//!     .build();
//!
//! let app = Router::<()>::new()
//!     .route("/", get(handler))
//!     .route("/hello", get(handler))
//!     .route("/world", get(handler))
//!     // add the metrics middleware
//!     .layer(metrics);
//!
//! async fn handler() -> Html<&'static str> {
//!     Html("<h1>Hello, World!</h1>")
//! }
//! ```
//!
//! ## Advanced Usage
//! 
//! this is an example to use the [prometheus exporter](https://opentelemetry.io/docs/specs/otel/metrics/sdk_exporters/prometheus/)
//! 
//! it will export the metrics at `/metrics` endpoint
//! 
//! 
//! ```
//! use axum_otel_metrics::HttpMetricsLayerBuilder;
//! use axum::{response::Html, routing::get, Router};
//! use prometheus::{Encoder, Registry, TextEncoder};
//!
//! let metrics = HttpMetricsLayerBuilder::new()
//! .with_service_name(env!("CARGO_PKG_NAME").to_string())
//! .with_service_version(env!("CARGO_PKG_VERSION").to_string())
//! .with_metric_reader(opentelemetry_prometheus::exporter().with_registry(prometheus::default_registry().clone()).build().unwrap())
//! .build();
//!
//! let app = Router::<()>::new()
//!     // export metrics at `/metrics` endpoint
//!     .route("/metrics", get(|| async {
//!         let mut buffer = Vec::new();
//!         let encoder = TextEncoder::new();
//!         encoder.encode(&prometheus::gather(), &mut buffer).unwrap();
//!         // return metrics
//!         String::from_utf8(buffer).unwrap()
//!     }))
//!     .route("/", get(handler))
//!     .route("/hello", get(handler))
//!     .route("/world", get(handler))
//!     // add the metrics middleware
//!     .layer(metrics);
//!
//! async fn handler() -> Html<&'static str> {
//!     Html("<h1>Hello, World!</h1>")
//! }
//! ```

use axum::http::Response;
use axum::{extract::MatchedPath, http::Request};
use std::env;
use std::sync::Arc;
use std::time::Duration;

use std::future::Future;
use std::pin::Pin;
use std::task::Poll::Ready;
use std::task::{Context, Poll};
use std::time::Instant;

use opentelemetry::{Key, KeyValue};

use opentelemetry::metrics::{Counter, Histogram, UpDownCounter};

use opentelemetry::metrics::MeterProvider;

use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider, Temporality};
use opentelemetry_sdk::resource::{EnvResourceDetector, SdkProvidedResourceDetector, TelemetryResourceDetector};
use opentelemetry_semantic_conventions::resource::{SERVICE_NAME, SERVICE_NAMESPACE, SERVICE_VERSION};

use opentelemetry::global;

use tower::{Layer, Service};

use futures_util::ready;
use http_body::Body as httpBody;
use opentelemetry_sdk::Resource;
use pin_project_lite::pin_project; // for `Body::size_hint`
                                   // service.instance used by Tencent Cloud TKE APM only, for view application metrics by pod IP
const SERVICE_INSTANCE: Key = Key::from_static_str("service.instance");

/// the metrics we used in the middleware
#[derive(Clone)]
pub struct Metric {
    pub requests_total: Counter<u64>,

    // before opentelemetry 0.18.0, Histogram called ValueRecorder
    pub req_duration: Histogram<f64>,

    pub req_size: Histogram<u64>,

    pub res_size: Histogram<u64>,

    pub req_active: UpDownCounter<i64>,
}

#[derive(Clone)]
pub struct MetricState {
    /// hold the metrics we used in the middleware
    pub metric: Metric,

    /// PathSkipper used to skip some paths for not recording metrics
    skipper: PathSkipper,

    /// whether the service is running as a TLS server or not.
    /// this is used to help determine the `url.scheme` otel meter attribute.
    /// because there is no way to get the scheme from the request in http server
    /// (except for absolute uri request, but which is only used when as a proxy server).
    is_tls: bool,
}

/// the service wrapper
#[derive(Clone)]
pub struct HttpMetrics<S> {
    pub(crate) state: MetricState,

    /// inner service which is wrapped by this middleware
    service: S,
}

#[derive(Clone)]
pub struct HttpMetricsLayer {
    /// the metric state, use both by the middleware handler and metrics export endpoint
    pub(crate) state: MetricState,
}

// TODO support custom buckets
// allocation not allowed in statics: static HTTP_REQ_DURATION_HISTOGRAM_BUCKETS: Vec<f64> = vec![0, 0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1, 2.5, 5, 7.5, 10];
// as https://github.com/open-telemetry/semantic-conventions/blob/main/docs/http/http-metrics.md#metric-httpserverrequestduration spec
// This metric SHOULD be specified with ExplicitBucketBoundaries of [ 0, 0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1, 2.5, 5, 7.5, 10 ].
// the unit of the buckets is second
const HTTP_REQ_DURATION_HISTOGRAM_BUCKETS: &[f64] = &[
    0.0, 0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

const KB: f64 = 1024.0;
const MB: f64 = 1024.0 * KB;

const HTTP_REQ_SIZE_HISTOGRAM_BUCKETS: &[f64] = &[
    1.0 * KB,   // 1 KB
    2.0 * KB,   // 2 KB
    5.0 * KB,   // 5 KB
    10.0 * KB,  // 10 KB
    100.0 * KB, // 100 KB
    500.0 * KB, // 500 KB
    1.0 * MB,   // 1 MB
    2.5 * MB,   // 2 MB
    5.0 * MB,   // 5 MB
    10.0 * MB,  // 10 MB
];

/// A helper that instructs the metrics layer to ignore
/// certain paths.
///
/// The [HttpMetricsLayerBuilder] uses this helper during the
/// construction of the [HttpMetricsLayer] that will be called
/// by Axum / Hyper / Tower when a request comes in.
#[derive(Clone)]
pub struct PathSkipper {
    skip: Arc<dyn Fn(&str) -> bool + 'static + Send + Sync>,
}

impl PathSkipper {
    /// Returns a [PathSkipper] that skips recording metrics
    /// for requests whose path, when passed to `fn`, returns
    /// `true`.
    ///
    /// Only static functions are accepted -- callables such
    /// as closures that capture their surrounding context will
    /// not work here.  For a variant that works, consult the
    /// [PathSkipper::new_with_fn] method.
    pub fn new(skip: fn(&str) -> bool) -> Self {
        Self { skip: Arc::new(skip) }
    }

    /// Dynamic variant of [PathSkipper::new].
    ///
    /// This variant requires the callable to be wrapped in an
    /// [Arc] but, in exchange for this requirement, the caller
    /// can use closures that capture variables from their context.
    ///
    /// The callable argument *must be thread-safe*.  You, as
    /// the implementor and user of this code, have that
    /// responsibility.
    pub fn new_with_fn(skip: Arc<dyn Fn(&str) -> bool + 'static + Send + Sync>) -> Self {
        Self { skip }
    }
}

impl Default for PathSkipper {
    /// Returns a `PathSkipper` that skips any path which
    /// starts with `/metrics` or `/favicon.ico``.
    ///
    /// This is the default implementation used when
    /// building an HttpMetricsLayerBuilder from scratch.
    fn default() -> Self {
        Self::new(|s| s.starts_with("/metrics") || s.starts_with("/favicon.ico"))
    }
}

#[derive(Clone)]
pub struct HttpMetricsLayerBuilder<T: opentelemetry_sdk::metrics::reader::MetricReader + Send + Sync + 'static> {
    service_name: Option<String>,
    service_version: Option<String>,
    target_labels: Vec<KeyValue>,
    skipper: PathSkipper,
    is_tls: bool,
    metric_reader: Option<T>,
}

impl<T: opentelemetry_sdk::metrics::reader::MetricReader + Send + Sync + 'static> Default for HttpMetricsLayerBuilder<T> {
    fn default() -> Self {
        Self {
            service_name: None,
            service_version: None,
            target_labels: vec![],
            skipper: PathSkipper::default(),
            is_tls: false,
            metric_reader: None,
        }
    }
}

impl<T: opentelemetry_sdk::metrics::reader::MetricReader + Send + Sync + 'static> HttpMetricsLayerBuilder<T> {
    pub fn new() -> Self {
        HttpMetricsLayerBuilder::default()
    }

    pub fn with_service_name(mut self, service_name: String) -> Self {
        self.service_name = Some(service_name);
        self
    }

    pub fn with_service_version(mut self, service_version: String) -> Self {
        self.service_version = Some(service_version);
        self
    }

    pub fn with_target_labels(mut self, labels: Vec<KeyValue>) -> Self {
        self.target_labels = labels;
        self
    }

    pub fn with_skipper(mut self, skipper: PathSkipper) -> Self {
        self.skipper = skipper;
        self
    }

    pub fn with_metric_reader(mut self, metric_reader: T) -> Self {
        self.metric_reader = Some(metric_reader);
        self
    }

    pub fn build(self) -> HttpMetricsLayer {
        let mut resource = vec![];

        let ns = env::var("INSTANCE_NAMESPACE").unwrap_or_default();
        if !ns.is_empty() {
            resource.push(KeyValue::new(SERVICE_NAMESPACE, ns.clone()));
        }

        let instance_ip = env::var("INSTANCE_IP").unwrap_or_default();
        if !instance_ip.is_empty() {
            resource.push(KeyValue::new(SERVICE_INSTANCE, instance_ip));
        }

        if let Some(service_name) = self.service_name.clone() {
            // `foo.ns`
            if !ns.is_empty() && !service_name.starts_with(format!("{}.", &ns).as_str()) {
                resource.push(KeyValue::new(SERVICE_NAME, format!("{}.{}", service_name, &ns)));
            } else {
                resource.push(KeyValue::new(SERVICE_NAME, service_name));
            }
        }
        if let Some(service_version) = self.service_version.clone() {
            resource.push(KeyValue::new(SERVICE_VERSION, service_version));
        }

        let res = Resource::from_detectors(
            Duration::from_secs(6),
            vec![
                // set service.name from env OTEL_SERVICE_NAME > env OTEL_RESOURCE_ATTRIBUTES > option_env! CARGO_BIN_NAME > unknown_service
                Box::new(SdkProvidedResourceDetector),
                // detect res from env OTEL_RESOURCE_ATTRIBUTES (resources string like key1=value1,key2=value2,...)
                Box::new(EnvResourceDetector::new()),
                // set telemetry.sdk.{name, language, version}
                Box::new(TelemetryResourceDetector),
            ],
        );

        let res = if !resource.is_empty() {
            res.merge(&mut Resource::new(resource))
        } else {
            res
        };

        let res = res.merge(&mut Resource::new(self.target_labels.clone()));


        let mut builder = SdkMeterProvider::builder().with_resource(res);

        // exporter
        if let Some(metric_reader) = self.metric_reader {
            builder = builder.with_reader(metric_reader);
        } else {
            builder = builder.with_reader(self.build_otlp());
        }

        let provider = builder.build();

        // init the global meter provider
        global::set_meter_provider(provider.clone());
        // this must called after the global meter provider has ben initialized
        // let meter = global::meter("axum-app");
        // let meter = provider.meter("axum-app");
        let meter = provider.meter_with_scope(
            opentelemetry::InstrumentationScope::builder(env!("CARGO_PKG_NAME"))
                .with_version(env!("CARGO_PKG_VERSION"))
                .with_schema_url("https://opentelemetry.io/schema/1.0.0")
                .build(),
        );

        // requests_total
        let requests_total = meter
            .u64_counter("requests")
            .with_description("How many HTTP requests processed, partitioned by status code and HTTP method.")
            .build();

        // request_duration_seconds
        let req_duration = meter
            .f64_histogram("http.server.request.duration")
            .with_unit("s")
            .with_description("The HTTP request latencies in seconds.")
            .with_boundaries(HTTP_REQ_DURATION_HISTOGRAM_BUCKETS.to_vec())
            .build();

        // request_size_bytes
        let req_size = meter
            .u64_histogram("http.server.request.size")
            .with_unit("By")
            .with_description("The HTTP request sizes in bytes.")
            .with_boundaries(HTTP_REQ_SIZE_HISTOGRAM_BUCKETS.to_vec())
            .build();

        let res_size = meter
            .u64_histogram("http.server.response.size")
            .with_unit("By")
            .with_description("The HTTP reponse sizes in bytes.")
            .with_boundaries(HTTP_REQ_SIZE_HISTOGRAM_BUCKETS.to_vec())
            .build();

        // no u64_up_down_counter because up_down_counter maybe < 0 since it allow negative values
        let req_active = meter
            .i64_up_down_counter("http.server.active_requests")
            .with_description("The number of active HTTP requests.")
            .build();

        let meter_state = MetricState {
            metric: Metric {
                requests_total,
                req_duration,
                req_size,
                res_size,
                req_active,
            },
            skipper: self.skipper,
            is_tls: self.is_tls,
        };

        HttpMetricsLayer { state: meter_state }
    }

    /// init otlp metrics exporter
    /// read from env var:
    /// OTEL_EXPORTER_OTLP_METRICS_ENDPOINT, OTEL_EXPORTER_OTLP_METRICS_HEADERS,OTEL_EXPORTER_OTLP_METRICS_TIMEOUT
    /// ref https://github.com/tokio-rs/tracing-opentelemetry/blob/5e3354ec24debcfbf856bfd1eb7022459dca1e6a/examples/opentelemetry-otlp.rs#L32
    fn build_otlp(&self) -> PeriodicReader {
        let protocol = match env::var("OTEL_EXPORTER_OTLP_METRICS_PROTOCOL")
            .ok()
            .or(env::var("OTEL_EXPORTER_OTLP_PROTOCOL").ok())
        {
            Some(val) => val,
            None => "http/protobuf".to_string(),
        };

        let exporter = if protocol.starts_with("http") {
            opentelemetry_otlp::MetricExporter::builder()
                .with_http()
                .with_temporality(Temporality::default())
                .build()
                .unwrap()
        } else {
            opentelemetry_otlp::MetricExporter::builder()
                .with_tonic()
                .with_temporality(Temporality::default())
                .build()
                .unwrap()
        };

        PeriodicReader::builder(exporter, opentelemetry_sdk::runtime::Tokio)
            .with_interval(std::time::Duration::from_secs(30))
            .build()
    }
}

impl<S> Layer<S> for HttpMetricsLayer {
    type Service = HttpMetrics<S>;

    fn layer(&self, service: S) -> Self::Service {
        HttpMetrics {
            state: self.state.clone(),
            service,
        }
    }
}

pin_project! {
    /// Response future for [`HttpMetrics`] Service.
    pub struct ResponseFuture<F> {
        #[pin]
        inner: F,
        start: Instant,
        state: MetricState,
        path: String,
        method: String,
        url_scheme: String,
        host: String,
        req_size: u64,
    }
}

impl<S, R, ResBody> Service<Request<R>> for HttpMetrics<S>
where
    S: Service<Request<R>, Response = Response<ResBody>>,
    ResBody: httpBody,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: Request<R>) -> Self::Future {
        let url_scheme = if self.state.is_tls {
            "https".to_string()
        } else {
            (|| {
                if let Some(scheme) = req.headers().get("X-Forwarded-Proto") {
                    return scheme.to_str().unwrap().to_string();
                } else if let Some(scheme) = req.headers().get("X-Forwarded-Protocol") {
                    return scheme.to_str().unwrap().to_string();
                }
                if req.headers().get("X-Forwarded-Ssl").is_some().to_string() == "on" {
                    return "https".to_string();
                }
                if let Some(scheme) = req.headers().get("X-Url-Scheme") {
                     scheme.to_str().unwrap().to_string()
                } else {
                    "http".to_string()
                }
            })()
        };
        // ref https://github.com/open-telemetry/semantic-conventions/blob/main/docs/http/http-metrics.md#metric-httpserveractive_requests
        // http.request.method and url.scheme is required
        self.state.metric.req_active.add(
            1,
            &[
                KeyValue::new("http.request.method", req.method().as_str().to_string()),
                KeyValue::new("url.scheme", url_scheme.clone()),
            ],
        );
        let start = Instant::now();
        let method = req.method().clone().to_string();
        let path = if let Some(matched_path) = req.extensions().get::<MatchedPath>() {
            matched_path.as_str().to_owned()
        } else {
            "".to_owned()
        };

        let host = req
            .headers()
            .get(http::header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("unknown")
            .to_string();

        let req_size = compute_approximate_request_size(&req);

        // for scheme, see github.com/labstack/echo/v4@v4.11.1/context.go
        // we can not use req.uri().scheme() since for non-absolute uri, it is always None

        ResponseFuture {
            inner: self.service.call(req),
            start,
            method,
            path,
            host,
            req_size: req_size as u64,
            state: self.state.clone(),
            url_scheme,
        }
    }
}

/// compute approximate request size
///
/// the implimentation refs [labstack/echo-contrib 's prometheus middleware](https://github.com/labstack/echo-contrib/blob/db8911a1af7abb6bdafbd999adada548fd9c0849/echoprometheus/prometheus.go#L329)
fn compute_approximate_request_size<T>(req: &Request<T>) -> usize {
    let mut s = 0;
    s += req.uri().path().len();
    s += req.method().as_str().len();

    req.headers().iter().for_each(|(k, v)| {
        s += k.as_str().len();
        s += v.as_bytes().len();
    });

    s += req.uri().host().map(|h| h.len()).unwrap_or(0);

    s += req
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .map(|v| v.to_str().unwrap().parse::<usize>().unwrap_or(0))
        .unwrap_or(0);
    s
}

impl<F, B: httpBody, E> Future for ResponseFuture<F>
where
    F: Future<Output = Result<Response<B>, E>>,
{
    type Output = Result<Response<B>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let response = ready!(this.inner.poll(cx))?;

        this.state.metric.req_active.add(
            -1,
            &[
                KeyValue::new("http.request.method", this.method.clone()),
                KeyValue::new("url.scheme", this.url_scheme.clone()),
            ],
        );

        if (this.state.skipper.skip)(this.path.as_str()) {
            return Poll::Ready(Ok(response));
        }

        let latency = this.start.elapsed().as_secs_f64();
        let status = response.status().as_u16().to_string();

        let res_size = response.body().size_hint().upper().unwrap_or(0);

        let labels = [
            KeyValue::new("http.request.method", this.method.clone()),
            KeyValue::new("http.route", this.path.clone()),
            KeyValue::new("http.response.status_code", status),
            // server.address: Name of the local HTTP server that received the request.
            // Determined by using the first of the following that applies
            //
            // 1. The primary server name of the matched virtual host. MUST only include host identifier.
            // 2. Host identifier of the request target if it's sent in absolute-form.
            // 3. Host identifier of the Host header
            KeyValue::new("server.address", this.host.clone()),
        ];

        this.state.metric.requests_total.add(1, &labels);

        this.state.metric.req_size.record(*this.req_size, &labels);

        this.state.metric.res_size.record(res_size, &labels);

        this.state.metric.req_duration.record(latency, &labels);

        Ready(Ok(response))
    }
}

#[cfg(test)]
mod tests {
    use crate::HttpMetricsLayer;
    use crate::HttpMetricsLayerBuilder;
    use axum::extract::State;
    use axum::routing::get;
    use axum::Router;
    use opentelemetry::{global, Context, KeyValue};
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use prometheus::{Encoder, Registry, TextEncoder};
    use std::sync::Arc;

    #[tokio::test]
    async fn test_prometheus_exporter() {
        let _cx = Context::current();

        let registry = Registry::new();

        // init prometheus exporter
        let exporter = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .build()
            .unwrap();

        let provider = SdkMeterProvider::builder().with_reader(exporter).build();

        // init the global meter provider
        global::set_meter_provider(provider.clone());

        let meter = global::meter("my-app");

        // Use two instruments
        let counter = meter.u64_counter("a.counter").with_description("Counts things").build();
        let recorder = meter.u64_histogram("a.histogram").with_description("Records values").build();

        counter.add(100, &[KeyValue::new("key", "value")]);
        recorder.record(100, &[KeyValue::new("key", "value")]);

        // Encode data as text or protobuf
        let encoder = TextEncoder::new();
        let metric_families = registry.gather();
        let mut result = Vec::new();
        encoder.encode(&metric_families, &mut result).expect("encode failed");
        println!("{}", String::from_utf8(result).unwrap());
    }

    #[tokio::test]
    async fn test_prom_exporter_builder() {
        let metrics = HttpMetricsLayerBuilder::<opentelemetry_prometheus::PrometheusExporter>::new().build();
        let _app = Router::<HttpMetricsLayer>::new()
            // export metrics at `/metrics` endpoint
            .route(
                "/metrics",
                get(|| async {
                    let mut buffer = Vec::new();
                    let encoder = TextEncoder::new();
                    encoder.encode(&prometheus::gather(), &mut buffer).unwrap();
                    // return metrics
                    String::from_utf8(buffer).unwrap()
                }),
            )
            .route("/", get(handler))
            .route("/hello", get(handler))
            .route("/world", get(handler))
            // add the metrics middleware
            .layer(metrics);

        async fn handler() -> &'static str {
            "<h1>Hello, World!</h1>"
        }
    }

    #[tokio::test]
    async fn test_builder_with_state_router() {
        #[derive(Clone)]
        struct AppState {}

        let metrics = HttpMetricsLayerBuilder::<opentelemetry_prometheus::PrometheusExporter>::new()
            .with_metric_reader(opentelemetry_prometheus::exporter().build().unwrap())
            .build();
        let _app: Router<AppState> = Router::new()
            .route(
                "/metrics",
                get(|| async {
                    let mut buffer = Vec::new();
                    let encoder = TextEncoder::new();
                    encoder.encode(&prometheus::gather(), &mut buffer).unwrap();
                    // return metrics
                    String::from_utf8(buffer).unwrap()
                }),
            )
            .route("/", get(handler))
            .route("/hello", get(handler))
            .route("/world", get(handler))
            // add the metrics middleware
            .layer(metrics)
            .with_state(AppState {});

        async fn handler(_state: State<AppState>) -> &'static str {
            "<h1>Hello, World!</h1>"
        }
    }

    #[tokio::test]
    async fn test_builder_with_arced_skipper() {
        #[derive(Clone)]
        struct AppState {}

        let metrics = HttpMetricsLayerBuilder::<opentelemetry_prometheus::PrometheusExporter>::new()
            .with_skipper(crate::PathSkipper::new_with_fn(Arc::new(|_: &str| true)))
            .build();
        let _app: Router<AppState> = Router::new()
            .route(
                "/metrics",
                get(|| async {
                    let mut buffer = Vec::new();
                    let encoder = TextEncoder::new();
                    encoder.encode(&prometheus::gather(), &mut buffer).unwrap();
                    // return metrics
                    String::from_utf8(buffer).unwrap()
                }),
            )
            .route("/", get(handler))
            // add the metrics middleware
            .layer(metrics)
            .with_state(AppState {});

        async fn handler(_state: State<AppState>) -> &'static str {
            "<h1>Hello, World!</h1>"
        }
    }
}
