use std::{collections::HashMap, fmt::Display, fs};

use actix::{Actor, Addr};
use actix_web::{
    body::BoxBody,
    post,
    web::{Data, Json},
    App, HttpResponse, HttpServer, Responder, ResponseError, Result,
};
use manager::Manager;
use serde::{Deserialize, Serialize};

mod dashboard;
mod manager;
mod player;
mod static_files;

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

#[derive(Deserialize, Clone)]
struct Config {
    /// The API keys mapped to stream keys
    api_keys: HashMap<String, String>,
    bind: String,
    ome_host: String,
    ome_api_host: String,
    ome_api_credentials: String,
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
    status: String,
    url: String,
}

/// The admission request object that OME sends
#[derive(Deserialize, Clone)]
struct AdmissionRequest {
    client: Client,
    request: Request,
}

pub struct State {
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

#[post("/api/auth")]
async fn auth(
    state: Data<State>,
    manager: Data<Addr<Manager>>,
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
                let mut split = request.request.url.split('/').skip(4);
                let stream = split.next().unwrap();
                let uuid = split.next().unwrap();

                tracing::info!(
                    "Admission request for {}:{} queued",
                    request.client.address,
                    request.client.port
                );

                if manager
                    .send(manager::IsAuthorized {
                        uuid: uuid.parse().unwrap(),
                        stream: stream.to_string(),
                    })
                    .await
                    .unwrap()
                {
                    tracing::info!(
                        "{}:{} authorized for an outgoing stream",
                        request.client.address,
                        request.client.port
                    );
                    Ok(AuthResponse::Opening(Response {
                        allowed: true,
                        new_url: Some(request.request.url.replace(&format!("/{}", uuid), "")),
                        lifetime: None,
                        reason: None,
                    }))
                } else {
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

fn ome_statistics(state: Data<State>, stream: &str) -> Result<OmeStatisticsResponse, Error> {
    Ok(state
        .agent
        .get(&format!(
            "{}/v1/stats/current/vhosts/default/apps/app/streams/{}",
            state.config.ome_api_host, stream
        ))
        .set("Authorization", &state.config.ome_api_credentials)
        .call()
        .map_err(|_why| Error::InternalError)?
        .into_json::<OmeStatistics>()
        .map_err(|_why| Error::InternalError)?
        .response)
}

#[actix_web::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config: Config = ron::de::from_bytes(&fs::read("config.ron").unwrap()).unwrap();
    let bind = config.bind.clone();

    let queue_actor =
        Manager::new(&config.api_keys.clone().into_values().collect::<Vec<_>>()).start();

    let state = Data::new(State {
        config,
        agent: ureq::Agent::new(),
    });

    HttpServer::new(move || {
        App::new()
            .app_data(Data::new(queue_actor.clone()))
            .app_data(state.clone())
            .service(auth)
            .service(dashboard::index)
            .service(dashboard::dashboard)
            .service(dashboard::queue)
            .service(dashboard::statistics)
            .service(player::index)
            .service(player::enqueue)
            .service(player::queue_ws)
            .service(static_files::notification)
            .service(static_files::haroldium)
            .service(static_files::killmeplz)
    })
    .bind(bind)
    .unwrap()
    .run()
    .await
    .unwrap();
}
