use std::{fmt::Display, fs};

use actix::{Actor, Addr};
use actix_files::Files;
use actix_web::{
    body::BoxBody,
    post,
    web::{Data, Json},
    App, HttpResponse, HttpServer, Responder, ResponseError, Result,
};
use argon2::PasswordHash;
use manager::Manager;
use serde::{Deserialize, Serialize};
use sqlx::{mysql::MySqlPoolOptions, MySqlPool};

mod admin;
mod dashboard;
mod manager;
mod player;

struct StreamConfig {
    id: String,
    key_hash: String,
}

#[derive(Deserialize)]
pub struct Credentials {
    id: String,
    key: String,
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

#[derive(Deserialize, Clone)]
pub struct Config {
    db_url: String,
    bind: String,
    ome_host: String,
    ome_api_host: String,
    ome_api_credentials: String,
    admin_id: String,
    admin_key_hash: String,
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
pub enum Error {
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
    config: Data<Config>,
    pool: Data<MySqlPool>,
    manager: Data<Addr<Manager>>,
    request: Json<AdmissionRequest>,
) -> Result<AuthResponse, Error> {
    match request.request.status.as_str() {
        "opening" => match request.request.direction.as_str() {
            "incoming" => {
                let mut split = request.request.url.split('/').skip(3);
                let id = split.next().ok_or(Error::BadRequest)?;
                let key = split.next().ok_or(Error::BadRequest)?;

                if auth_as_streamer(pool, id, key).await.is_ok() {
                    tracing::info!(
                        "Allowing incoming stream from {} to stream to ID: {}",
                        request.client.address,
                        id,
                    );

                    Ok(AuthResponse::Opening(Response {
                        allowed: true,
                        new_url: Some(format!("rtmp://{}/app/{}", config.ome_host, id)),
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

/// Helper function to authenticate as the streamer
async fn auth_as_streamer(pool: Data<MySqlPool>, id: &str, key: &str) -> Result<(), Error> {
    let stream = sqlx::query_as!(StreamConfig, "SELECT * FROM streams WHERE id = ?", id)
        .fetch_one(&*pool.into_inner())
        .await
        .map_err(|_| Error::Unauthorized)?;

    let hash = PasswordHash::parse(&stream.key_hash, argon2::password_hash::Encoding::B64).unwrap();

    hash.verify_password(&[&argon2::Argon2::default()], key)
        .map_err(|_| Error::Unauthorized)
}

fn ome_statistics(
    config: Data<Config>,
    agent: Data<ureq::Agent>,
    stream: &str,
) -> Result<OmeStatisticsResponse, Error> {
    Ok(agent
        .get(&format!(
            "{}/v1/stats/current/vhosts/default/apps/app/streams/{}",
            config.ome_api_host, stream
        ))
        .set("Authorization", &config.ome_api_credentials)
        .call()
        .map_err(|_why| Error::InternalError)?
        .into_json::<OmeStatistics>()
        .map_err(|_why| Error::InternalError)?
        .response)
}

#[actix_web::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config: Data<Config> =
        Data::new(ron::de::from_bytes(&fs::read("config.ron").unwrap()).unwrap());
    let bind = config.bind.clone();

    let pool = Data::new(
        MySqlPoolOptions::new()
            .connect(&config.db_url)
            .await
            .unwrap(),
    );

    let streams = sqlx::query_as!(StreamConfig, "SELECT * FROM streams")
        .fetch_all(&*pool.clone().into_inner())
        .await
        .unwrap();

    let queue_actor = Manager::new(
        &streams
            .into_iter()
            .map(|stream| stream.id)
            .collect::<Vec<_>>(),
    )
    .start();

    let agent = Data::new(ureq::Agent::new());

    HttpServer::new(move || {
        App::new()
            .app_data(Data::new(queue_actor.clone()))
            .app_data(pool.clone())
            .app_data(config.clone())
            .app_data(agent.clone())
            .service(auth)
            .service(dashboard::index)
            .service(dashboard::dashboard)
            .service(dashboard::queue)
            .service(dashboard::statistics)
            .service(player::index)
            .service(player::enqueue)
            .service(player::queue_ws)
            .service(admin::index)
            .service(admin::dashboard)
            .service(admin::create_stream_menu)
            .service(admin::create_stream)
            .service(admin::remove_stream)
            .service(Files::new("/static", "./static").prefer_utf8(true))
    })
    .bind(bind)
    .unwrap()
    .run()
    .await
    .unwrap();
}
