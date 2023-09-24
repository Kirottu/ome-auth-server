use std::{
    collections::HashMap,
    fmt::Display,
    fs,
    time::{Duration, Instant},
};

use actix::{Actor, ActorContext, Addr, AsyncContext, StreamHandler};
use actix_web::{
    body::BoxBody,
    get, post,
    web::{Data, Form, Json, Payload, Query},
    App, HttpRequest, HttpResponse, HttpServer, Responder, ResponseError, Result,
};
use actix_web_actors::ws;
use futures::{channel::mpsc, FutureExt, StreamExt};
use queue::QueueActor;
use serde::{Deserialize, Serialize};

use crate::queue::QueueWebSocket;

mod queue;

#[derive(Deserialize, Clone)]
struct Config {
    /// The API keys mapped to stream keys
    api_keys: HashMap<String, String>,
    bind: String,
    host: String,
    ome_host: String,
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

#[derive(Deserialize, Serialize, Clone, Hash, Eq, PartialEq)]
pub struct Client {
    address: String,
    port: u16,
    user_agent: Option<String>,
}

#[derive(Deserialize, Clone)]
pub struct Request {
    direction: String,
    protocol: String,
    status: String,
    url: String,
    new_url: Option<String>,
    time: String,
}

/// The admission request object that OME sends
#[derive(Deserialize, Clone)]
struct AdmissionRequest {
    client: Client,
    request: Request,
}

struct State {
    config: Config,
    agent: ureq::Agent,
}

#[derive(Serialize)]
struct Response {
    allowed: bool,
    new_url: Option<String>,
    lifetime: Option<u64>,
    reason: Option<String>,
}

enum AuthResponse {
    Opening(Response),
    Closing,
}

impl Responder for AuthResponse {
    type Body = BoxBody;

    fn respond_to(self, _req: &actix_web::HttpRequest) -> HttpResponse<Self::Body> {
        match self {
            AuthResponse::Opening(response) => {
                HttpResponse::Ok().body(serde_json::to_string(&response).unwrap())
            }
            AuthResponse::Closing => HttpResponse::Ok().body("{}"),
        }
    }
}

#[derive(Deserialize)]
struct ApiKeyQuery {
    api_key: String,
}

#[derive(Deserialize)]
struct QueueHandleQuery {
    allow: bool,
}

struct Html(String);

impl Responder for Html {
    type Body = BoxBody;

    fn respond_to(self, _req: &actix_web::HttpRequest) -> actix_web::HttpResponse<Self::Body> {
        HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .body(self.0)
    }
}

#[derive(Debug)]
enum Error {
    Unauthorized,
    BadRequest,
    InternalError,
}

impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Error::Unauthorized => "Unauthorized",
                Error::BadRequest => "Bad request",
                Error::InternalError => "Internal server error",
            }
        )
    }
}

impl ResponseError for Error {}

/// Macro to easily fill up templates
#[macro_export]
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

struct StatisticsWebSocket {
    hb: Instant,
    state: Data<State>,
    stream: String,
}

impl StatisticsWebSocket {
    const INTERVAL: Duration = Duration::from_secs(1);
    const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

    fn new(state: Data<State>, stream: String) -> Self {
        Self {
            hb: Instant::now(),
            state,
            stream,
        }
    }

    fn hb(&self, ctx: &mut <Self as Actor>::Context) {
        ctx.run_interval(Self::INTERVAL, |act, ctx| {
            if Instant::now().duration_since(act.hb) > Self::CLIENT_TIMEOUT {
                ctx.stop();

                return;
            }

            let state = act.state.clone();
            let stream = act.stream.clone();

            let content = match ome_statistics(state, &stream) {
                Ok(_statistics) => html! {"../res/statistics.html",
                    "{connections}" => _statistics.total_connections,
                    "{start_time}" => _statistics.created_time,
                    "{total_in}" => _statistics.total_bytes_in / 1_000_000,
                    "{total_out}" => _statistics.total_bytes_out / 1_000_000
                },
                Err(_) => {
                    let not_running = r#"<span class="error">Not running!</span>"#;
                    html! {"../res/statistics.html",
                        "{connections}" => not_running,
                        "{start_time}" => not_running,
                        "{total_in}" => not_running,
                        "{total_out}" => not_running
                    }
                }
            };

            ctx.text(content);
        });
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for StatisticsWebSocket {
    fn handle(&mut self, item: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        match item {
            Ok(ws::Message::Ping(msg)) => {
                self.hb = Instant::now();
                ctx.pong(&msg);
            }
            Ok(ws::Message::Pong(_)) => self.hb = Instant::now(),
            _ => ctx.stop(),
        }
    }
}

impl Actor for StatisticsWebSocket {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.hb(ctx);
    }
}

#[post("/api/auth")]
async fn auth(
    state: Data<State>,
    queue_actor: Data<Addr<QueueActor>>,
    request: Json<AdmissionRequest>,
) -> Result<AuthResponse, Error> {
    match request.request.status.as_str() {
        "opening" => match request.request.direction.as_str() {
            "incoming" => {
                let mut split = request.request.url.split('/').skip(3);
                let key = split.next().ok_or(Error::Unauthorized)?;

                if let Some(stream) = state.config.api_keys.get(key) {
                    tracing::info!(
                        "Allowing incoming stream from {} to stream to ID: {}",
                        request.client.address,
                        stream
                    );

                    let new_url = request.request.url.replace(key, &format!("app/{}", stream));

                    Ok(AuthResponse::Opening(Response {
                        allowed: true,
                        new_url: Some(new_url),
                        lifetime: None,
                        reason: None,
                    }))
                } else {
                    tracing::info!("Denying incoming stream from {}", request.client.address);

                    Ok(AuthResponse::Opening(Response {
                        allowed: false,
                        new_url: None,
                        lifetime: None,
                        reason: Some("Invalid API key".to_string()),
                    }))
                }
            }
            "outgoing" => {
                let (tx, mut rx) = mpsc::unbounded();

                let stream = request.request.url.split('/').last().unwrap();

                queue_actor.do_send(queue::Enqueue {
                    stream: stream.to_string(),
                    client: request.client.clone(),
                    sender: tx,
                });

                tracing::info!(
                    "Admission request for {}:{} queued",
                    request.client.address,
                    request.client.port
                );

                futures::select! {
                    allow = rx.next() => match allow {
                        Some(true) => {
                            tracing::info!(
                                "{}:{} authorized for an outgoing stream",
                                request.client.address,
                                request.client.port
                            );
                            Ok(AuthResponse::Opening(Response {
                                allowed: true,
                                new_url: None,
                                lifetime: None,
                                reason: None,
                            }))
                        }
                        _ => {
                            tracing::info!(
                                "{}:{} denied for an outgoing stream",
                                request.client.address,
                                request.client.port
                            );
                            Ok(AuthResponse::Opening(Response {
                                allowed: false,
                                new_url: None,
                                lifetime: None,
                                reason: Some("Unauthorized".to_string()),
                            }))
                        }
                    },
                    _ = actix_web::rt::time::sleep(Duration::from_secs(60)).fuse() => {
                        queue_actor.do_send(queue::Dequeue {
                            client: request.client.clone(),
                            stream: stream.to_string(),
                        });
                        Ok(AuthResponse::Opening(Response {
                            allowed: false,
                            new_url: None,
                            lifetime: None,
                            reason: Some("Unauthorized".to_string()),
                        }))
                    }
                }
            }
            _ => Err(Error::BadRequest),
        },
        "closing" => {
            tracing::info!(
                "{}:{} closed connection",
                request.client.address,
                request.client.port
            );

            Ok(AuthResponse::Closing)
        }
        _ => Err(Error::BadRequest),
    }
}

#[post("/api/queue/handle")]
async fn handle(
    state: Data<State>,
    queue_actor: Data<Addr<QueueActor>>,
    api_key: Query<ApiKeyQuery>,
    allow: Query<QueueHandleQuery>,
    client: Form<Client>,
) -> Result<Json<()>, Error> {
    let stream = state
        .config
        .api_keys
        .get(&api_key.api_key)
        .ok_or(Error::Unauthorized)?;

    if queue_actor
        .send(queue::Handle {
            client: client.0,
            allow: allow.allow,
            stream: stream.clone(),
        })
        .await
        .unwrap()
    {
        Ok(Json(()))
    } else {
        Err(Error::BadRequest)
    }
}

#[get("/queue")]
async fn queue_html(
    req: HttpRequest,
    payload: Payload,
    state: Data<State>,
    addr: Data<Addr<QueueActor>>,
    query: Query<ApiKeyQuery>,
) -> Result<HttpResponse> {
    let stream = state
        .config
        .api_keys
        .get(&query.api_key)
        .ok_or(Error::Unauthorized)?
        .clone();

    ws::start(
        QueueWebSocket::new(addr.get_ref().clone(), stream),
        &req,
        payload,
    )
}

#[get("/statistics")]
async fn statistics(
    req: HttpRequest,
    payload: Payload,
    state: Data<State>,
    query: Query<ApiKeyQuery>,
) -> Result<HttpResponse> {
    let stream = state
        .config
        .api_keys
        .get(&query.api_key)
        .ok_or(Error::Unauthorized)?;

    ws::start(
        StatisticsWebSocket::new(state.clone(), stream.clone()),
        &req,
        payload,
    )
}

fn ome_statistics(state: Data<State>, stream: &str) -> Result<OmeStatisticsResponse, Error> {
    Ok(state
        .agent
        .get(&format!(
            "{}/v1/stats/current/vhosts/default/apps/app/streams/{}",
            state.config.ome_api_host, stream
        ))
        .set("Authorization", &state.config.ome_api_credentials)
        .call()
        .map_err(|why| Error::InternalError)?
        .into_json::<OmeStatistics>()
        .map_err(|why| Error::InternalError)?
        .response)
}

#[get("/login")]
async fn login() -> Html {
    Html(html! { "../res/login.html" })
}

#[get("/dashboard")]
async fn dashboard(state: Data<State>, query: Option<Query<ApiKeyQuery>>) -> Result<Html> {
    match query {
        Some(query) => {
            let stream = state
                .config
                .api_keys
                .get(&query.api_key)
                .ok_or(Error::Unauthorized)?;

            Ok(Html(html! {"../res/dashboard.html",
                "{stream}" => stream,
                "{api_key}" => query.api_key
            }))
        }
        None => Ok(Html(
            html! {"../res/login-redirect.html", "{host}" => state.config.host},
        )),
    }
}

#[get("/streams")]
async fn streams(state: Data<State>) -> Html {
    let buttons = futures::stream::iter(state.config.api_keys.values())
        .filter_map(|stream| async {
            ome_statistics(state.clone(), stream)
                .ok()
                .map(|_| html! {"../res/available-stream.html", "{stream}" => stream})
        })
        .collect::<String>()
        .await;
    Html(buttons)
}

#[get("/player")]
async fn player(state: Data<State>) -> Html {
    Html(html! {"../res/player.html",
        "{host}" => state.config.ome_host
    })
}

#[get("/notification")]
async fn notification() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("audio/wav")
        .body(&include_bytes!("../res/Oi.wav")[..])
}

#[actix_web::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config: Config = ron::de::from_bytes(&fs::read("config.ron").unwrap()).unwrap();
    let bind = config.bind.clone();

    let queue_actor =
        QueueActor::new(&config.api_keys.clone().into_values().collect::<Vec<_>>()).start();

    let state = Data::new(State {
        config,
        agent: ureq::Agent::new(),
    });

    HttpServer::new(move || {
        App::new()
            .app_data(Data::new(queue_actor.clone()))
            .app_data(state.clone())
            .service(auth)
            .service(handle)
            .service(dashboard)
            .service(login)
            .service(streams)
            .service(queue_html)
            .service(player)
            .service(statistics)
            .service(notification)
    })
    .bind(bind)
    .unwrap()
    .run()
    .await
    .unwrap();
}
