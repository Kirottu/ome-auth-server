use std::{
    collections::HashMap,
    fs,
    time::{Duration, Instant},
};

use poem::{http::StatusCode, listener::TcpListener, EndpointExt, Error, Result, Route, Server};
use poem_openapi::{
    param::{Path, Query},
    payload::{Form, Html, Json},
    ApiResponse, Object, OpenApi, OpenApiService,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, RwLock};

#[derive(Deserialize)]
struct Config {
    /// The API keys mapped to stream keys
    api_keys: HashMap<String, String>,
    bind: String,
    host: String,
    ome_api_host: String,
    ome_api_credentials: String,
}

#[derive(Deserialize)]
struct OmeStatistics {
    response: OmeStatisticsResponse,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OmeStatisticsResponse {
    created_time: String,
    total_connections: u32,
    total_bytes_in: u64,
    total_bytes_out: u64,
}

#[derive(Object, Deserialize, Serialize, Clone, Hash, Eq, PartialEq)]
struct Client {
    address: String,
    port: u16,
    user_agent: Option<String>,
}

#[derive(Object, Deserialize, Clone)]
struct Request {
    direction: String,
    protocol: String,
    status: String,
    url: String,
    new_url: Option<String>,
    time: String,
}

/// The admission request object that OME sends
#[derive(Object, Deserialize, Clone)]
struct AdmissionRequest {
    client: Client,
    request: Request,
}

#[derive(Object, Deserialize)]
struct QueueHandleRequest {
    client: Client,
    allow: bool,
}

#[derive(Clone)]
struct QueuedRequest {
    instant: Instant,
    sender: mpsc::UnboundedSender<bool>,
}

struct Api {
    config: Config,
    streams: HashMap<String, RwLock<StreamData>>,
    client: reqwest::Client,
}

#[derive(Default)]
struct StreamData {
    queue: HashMap<Client, QueuedRequest>,
}

#[derive(Object, Serialize)]
struct Response {
    allowed: bool,
    new_url: Option<String>,
    lifetime: Option<u64>,
    reason: Option<String>,
}

/// OME in the closing state only needs an empty JSON block
#[derive(Object, Serialize)]
struct Empty;

#[derive(ApiResponse)]
enum AuthResponse {
    #[oai(status = 200)]
    Opening(Json<Response>),
    #[oai(status = 200)]
    Closing(Json<Empty>),
}

#[derive(Object, Deserialize)]
struct LoginForm {
    api_key: String,
}

/// Macro to easily fill up templates
macro_rules! html {
    ( $html:literal ) => {
        include_str!($html).to_owned()
    };
    ( $html:literal, $( $from:literal => $to:expr ),* ) => {
        {
            let mut res = include_str!($html).to_owned();

            $(
                res = res.replace($from, &$to.to_string());
            )*

            res
        }
    };
}

#[OpenApi]
impl Api {
    #[oai(path = "/api/auth", method = "post")]
    async fn auth(&self, request: Json<AdmissionRequest>) -> Result<AuthResponse> {
        match request.request.status.as_str() {
            "opening" => match request.request.direction.as_str() {
                "incoming" => {
                    let mut split = request.request.url.split('/').skip(3);
                    let key = split.next().ok_or(StatusCode::BAD_REQUEST)?;

                    if let Some(stream) = self.config.api_keys.get(key) {
                        tracing::info!(
                            "Allowing incoming stream from {} to stream to ID: {}",
                            request.client.address,
                            stream
                        );

                        let new_url = request.request.url.replace(key, &format!("app/{}", stream));

                        Ok(AuthResponse::Opening(Json(Response {
                            allowed: true,
                            new_url: Some(new_url),
                            lifetime: None,
                            reason: None,
                        })))
                    } else {
                        tracing::info!("Denying incoming stream from {}", request.client.address);

                        Ok(AuthResponse::Opening(Json(Response {
                            allowed: false,
                            new_url: None,
                            lifetime: None,
                            reason: Some("Invalid API key".to_string()),
                        })))
                    }
                }
                "outgoing" => {
                    let (tx, mut rx) = mpsc::unbounded_channel();

                    let stream = request.request.url.split('/').last().unwrap();

                    self.streams
                        .get(stream)
                        .ok_or(Error::from_string(
                            "Invalid stream ID",
                            StatusCode::BAD_REQUEST,
                        ))?
                        .write()
                        .await
                        .queue
                        .insert(
                            request.client.clone(),
                            QueuedRequest {
                                instant: Instant::now(),
                                sender: tx,
                            },
                        );

                    tracing::info!(
                        "Admission request for {}:{} queued",
                        request.client.address,
                        request.client.port
                    );

                    tokio::select! {
                        allow = rx.recv() => match allow {
                            Some(true) => {
                                tracing::info!(
                                    "{}:{} authorized for an outgoing stream",
                                    request.client.address,
                                    request.client.port
                                );
                                Ok(AuthResponse::Opening(Json(Response {
                                    allowed: true,
                                    new_url: None,
                                    lifetime: None,
                                    reason: None,
                                })))
                            }
                            _ => {
                                tracing::info!(
                                    "{}:{} denied for an outgoing stream",
                                    request.client.address,
                                    request.client.port
                                );
                                Ok(AuthResponse::Opening(Json(Response {
                                    allowed: false,
                                    new_url: None,
                                    lifetime: None,
                                    reason: Some("Unauthorized".to_string()),
                                })))
                            }
                        },
                        _ = tokio::time::sleep(Duration::from_secs(60)) => {
                            self.streams[stream].write().await.queue.remove(&request.client);
                            Ok(AuthResponse::Opening(Json(Response {
                                allowed: false,
                                new_url: None,
                                lifetime: None,
                                reason: Some("Unauthorized".to_string()),
                            })))
                        }
                    }
                }
                _ => Err(Error::from_string(
                    "Invalid direction",
                    StatusCode::BAD_REQUEST,
                )),
            },
            "closing" => {
                tracing::info!(
                    "{}:{} closed connection",
                    request.client.address,
                    request.client.port
                );

                Ok(AuthResponse::Closing(Json(Empty)))
            }
            _ => Err(Error::from_string(
                "Invalid status",
                StatusCode::BAD_REQUEST,
            )),
        }
    }

    #[oai(path = "/api/queue/accept", method = "post")]
    async fn accept(&self, client: Form<Client>, api_key: Query<String>) -> Result<()> {
        self.handle(&client.0, &api_key.0, true).await
    }

    #[oai(path = "/api/queue/reject", method = "post")]
    async fn reject(&self, client: Form<Client>, api_key: Query<String>) -> Result<()> {
        self.handle(&client.0, &api_key.0, false).await
    }

    async fn handle(&self, client: &Client, api_key: &String, allow: bool) -> Result<()> {
        let stream = self
            .config
            .api_keys
            .get(api_key)
            .ok_or(StatusCode::UNAUTHORIZED)?;

        let mut stream_data = self.streams[stream].write().await;

        match stream_data.queue.remove(client) {
            Some(queued_request) => {
                let _ = queued_request.sender.send(allow);
                Ok(())
            }
            None => Err(Error::from_status(StatusCode::BAD_REQUEST)),
        }
    }

    #[oai(path = "/queue", method = "get")]
    async fn queue(&self, api_key: Query<String>) -> Result<Html<String>> {
        let stream = self
            .config
            .api_keys
            .get(&api_key.0)
            .ok_or(StatusCode::UNAUTHORIZED)?;

        let content = self.streams[stream]
            .read()
            .await
            .queue
            .iter()
            .map(|(client, request)| {
                html! {"../res/queued_connection.html",
                    "{ip}" => client.address,
                    "{port}" => client.port,
                    "{elapsed}" => request.instant.elapsed().as_secs(),
                    "{api_key}" => api_key,
                    "{client}" => serde_json::to_string(client).unwrap()
                }
            })
            .collect::<String>();

        Ok(Html(html! {"../res/queue.html", "{content}" => &content}))
    }

    #[oai(path = "/statistics", method = "get")]
    async fn statistics(&self, api_key: Query<String>) -> Result<Html<String>> {
        let stream = self
            .config
            .api_keys
            .get(&api_key.0)
            .ok_or(StatusCode::UNAUTHORIZED)?;

        let response = self
            .client
            .get(format!(
                "{}/v1/stats/current/vhosts/default/apps/app/streams/{}",
                self.config.ome_api_host, stream
            ))
            .send()
            .await
            .map_err(|why| {
                tracing::error!("Error sending API request to OME: {}", why);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

        if response.status() == StatusCode::OK {
            let statistics = response.json::<OmeStatistics>().await.map_err(|why| {
                tracing::error!("Error decoding JSON received from OME: {}", why);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            Ok(Html(html! {"../res/statistics.html",
                "{connections}" => statistics.response.total_connections,
                "{start_time}" => statistics.response.created_time,
                "{total_in}" => statistics.response.total_bytes_in / 1_000_000,
                "{total_out}" => statistics.response.total_bytes_out / 1_000_000
            }))
        } else {
            let not_running = r#"<span class="error">Not running!</span>"#;
            Ok(Html(html! {"../res/statistics.html",
                "{connections}" => not_running,
                "{start_time}" => not_running,
                "{total_in}" => not_running,
                "{total_out}" => not_running
            }))
        }
    }

    #[oai(path = "/dashboard/login", method = "get")]
    async fn login(&self) -> Html<String> {
        Html(html! { "../res/login.html" })
    }

    #[oai(path = "/dashboard", method = "get")]
    async fn dashboard(&self, api_key: Query<String>) -> Result<Html<String>> {
        let stream = self
            .config
            .api_keys
            .get(&api_key.0)
            .ok_or(StatusCode::UNAUTHORIZED)?;

        Ok(Html(html! {"../res/dashboard.html",
            "{stream}" => stream,
            "{api_key}" => api_key
        }))
    }

    #[oai(path = "/player/:stream", method = "get")]
    async fn player(&self, stream: Path<String>) -> Html<String> {
        Html(html! {"../res/player.html",
            "{stream}" => stream,
            "{host}" => self.config.host
        })
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config: Config = ron::de::from_bytes(&fs::read("config.ron").unwrap()).unwrap();
    let bind = config.bind.clone();

    let mut streams = HashMap::new();

    for stream in config.api_keys.values() {
        streams.insert(stream.clone(), RwLock::new(StreamData::default()));
    }

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "Authorization",
        reqwest::header::HeaderValue::from_str(&config.ome_api_credentials).unwrap(),
    );

    let service = OpenApiService::new(
        Api {
            config,
            streams,
            client: reqwest::Client::builder()
                .default_headers(headers)
                .build()
                .unwrap(),
        },
        "OME Auth Server",
        "0.1.0",
    );

    let ui = service.swagger_ui();

    let app = Route::new()
        .nest("/", service)
        .nest("/docs", ui)
        .inspect_all_err(|why| {
            tracing::error!("{}", why);
        });

    Server::new(TcpListener::bind(bind)).run(app).await.unwrap()
}
