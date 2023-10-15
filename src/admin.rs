use actix::Addr;
use actix_web::{
    error, get, post,
    web::{Data, Form, Query},
    Result,
};
use argon2::{password_hash::SaltString, PasswordHash};
use askama::Template;
use serde::Deserialize;
use sqlx::MySqlPool;

#[derive(Deserialize)]
struct StreamIdQuery {
    stream_id: String,
}

#[derive(Deserialize)]
struct CreateStreamQuery {
    stream_id: String,
    stream_key: String,
}

use crate::{manager, Config, Credentials, Html, StreamConfig};

mod templates {
    use askama::Template;

    #[derive(Template)]
    #[template(path = "admin/index.html")]
    pub struct Index;

    #[derive(Template)]
    #[template(path = "admin/create_stream_menu.html")]
    pub struct CreateStreamMenu<'a> {
        pub id: &'a str,
        pub key: &'a str,
    }

    #[derive(Template)]
    #[template(path = "admin/dashboard.html")]
    pub struct Dashboard<'a> {
        pub streams: &'a [String],
        pub id: &'a str,
        pub key: &'a str,
    }
}

async fn auth_as_admin(config: Data<Config>, id: &str, key: &str) -> Result<()> {
    let hash =
        PasswordHash::parse(&config.admin_key_hash, argon2::password_hash::Encoding::B64).unwrap();

    if id != config.admin_id {
        return Err(error::ErrorUnauthorized("Invalid admin ID"));
    }

    hash.verify_password(&[&argon2::Argon2::default()], key)
        .map_err(|_| error::ErrorUnauthorized("Invalid admin key"))
}

#[get("/admin/create_stream_menu")]
async fn create_stream_menu(credentials: Query<Credentials>) -> Html {
    Html(
        templates::CreateStreamMenu {
            id: &credentials.id,
            key: &credentials.key,
        }
        .render()
        .unwrap(),
    )
}

#[post("/admin/create_stream")]
async fn create_stream(
    pool: Data<MySqlPool>,
    config: Data<Config>,
    manager: Data<Addr<manager::Manager>>,
    credentials: Query<Credentials>,
    create_stream: Form<CreateStreamQuery>,
) -> Result<Html> {
    auth_as_admin(config, &credentials.id, &credentials.key).await?;

    let salt = SaltString::generate(rand::thread_rng());
    let hash = PasswordHash::generate(
        argon2::Argon2::default(),
        &create_stream.stream_key,
        salt.as_salt(),
    )
    .unwrap();

    sqlx::query!(
        "INSERT INTO streams (id, key_hash) VALUES (?, ?)",
        create_stream.stream_id,
        hash.serialize().to_string()
    )
    .execute(&*pool.clone().into_inner())
    .await
    .unwrap();

    manager.do_send(manager::StreamCreated {
        stream: create_stream.stream_id.clone(),
    });

    Ok(dashboard_inner(pool, credentials).await)
}

#[post("/admin/remove_stream")]
async fn remove_stream(
    pool: Data<MySqlPool>,
    manager: Data<Addr<manager::Manager>>,
    config: Data<Config>,
    credentials: Query<Credentials>,
    stream_id: Query<StreamIdQuery>,
) -> Result<Html> {
    auth_as_admin(config, &credentials.id, &credentials.key).await?;

    sqlx::query!("DELETE FROM streams WHERE id = ?", stream_id.stream_id)
        .execute(&*pool.clone().into_inner())
        .await
        .unwrap();

    manager.do_send(manager::StreamRemoved {
        stream: stream_id.stream_id.clone(),
    });

    Ok(dashboard_inner(pool, credentials).await)
}

#[get("/admin/dashboard")]
async fn dashboard(
    pool: Data<MySqlPool>,
    config: Data<Config>,
    credentials: Query<Credentials>,
) -> Result<Html> {
    auth_as_admin(config, &credentials.id, &credentials.key).await?;

    Ok(dashboard_inner(pool, credentials).await)
}

async fn dashboard_inner(pool: Data<MySqlPool>, credentials: Query<Credentials>) -> Html {
    let streams = sqlx::query_as!(StreamConfig, "SELECT * FROM streams")
        .fetch_all(&*pool.into_inner())
        .await
        .unwrap()
        .into_iter()
        .map(|stream| stream.id)
        .collect::<Vec<_>>();

    Html(
        templates::Dashboard {
            streams: &streams,
            id: &credentials.id,
            key: &credentials.key,
        }
        .render()
        .unwrap(),
    )
}

#[get("/admin")]
async fn index() -> Html {
    Html(templates::Index.render().unwrap())
}
