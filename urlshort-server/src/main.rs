use std::{
    env,
    fmt::{Debug, Display},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{Response, StatusCode, Uri},
    response::{IntoResponse, Redirect},
    routing::{get, post},
};
use log::{error, info};
use rand::seq::IndexedRandom;
use redis::{
    AsyncCommands, FromRedisValue, ToSingleRedisArg,
    aio::{ConnectionManager, MultiplexedConnection},
};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, error::DatabaseError, postgres::PgPoolOptions};
use tracing::instrument;
use validator::Validate;

#[derive(Clone, Debug)]
struct AppState {
    pg: PgPool,
    redis: ConnectionManager,
}

fn generate_token() -> String {
    let mut rng = rand::rng();
    let chars = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz1234567890";
    (0..11).map(|_| chars.choose(&mut rng).unwrap()).fold(
        String::with_capacity(11),
        |mut acc, &i| {
            acc.push(i.into());
            acc
        },
    )
}

async fn health() -> impl IntoResponse {}

async fn read_from_cache<T, U>(redis: &ConnectionManager, key: &T) -> Option<U>
where
    T: ToString + Debug,
    U: FromRedisValue + Debug,
{
    let mut redis = redis.clone();
    let key = key.to_string();

    let value = redis.get(&key).await.ok()?;
    info!("Data read from cache {:?}=>>{:?}", key, value);

    value
}

async fn write_to_cache<T, U>(redis: &ConnectionManager, key: T, val: U)
where
    T: ToSingleRedisArg + Send + Sync + Display,
    U: ToSingleRedisArg + Send + Sync + Display,
{
    let mut redis = redis.clone();

    let _ = redis.set_ex::<&T, &U, ()>(&key, &val, 10).await;
    info!("Data written to cache {}=>>{}", key, val);
}

#[derive(Debug, Deserialize, Validate)]
struct LinksCreateRequest {
    #[validate(url)]
    url: String,
}

#[derive(Debug, Serialize)]
struct LinksCreateResponse {
    token: String,
}

#[tracing::instrument]
async fn create_link(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LinksCreateRequest>,
) -> axum::response::Result<Json<LinksCreateResponse>> {
    // not found in cache..
    match sqlx::query!(r"SELECT * FROM links WHERE url = $1;", req.url)
        .fetch_one(&state.pg)
        .await
    {
        Ok(_) => {
            info!("Record already exist.. exit");
            return Err((StatusCode::CONFLICT, "Already Exist").into());
        }
        Err(sqlx::Error::RowNotFound) => {}
        Err(e) => {
            error!("Internal error [{e}]");
            return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into());
        }
    }

    for i in 0..=10 {
        let token = generate_token();
        info!("Inserting token.. [{token}]");
        match sqlx::query!(
            r"INSERT INTO links(token, url) VALUES ($1,$2);",
            token,
            req.url
        )
        .execute(&state.pg)
        .await
        {
            Ok(_) => {
                let _ = write_to_cache(&state.redis, &token, req.url).await;
                return Ok(LinksCreateResponse { token }.into());
            }
            Err(sqlx::Error::Database(err)) if err.is_unique_violation() => {}
            Err(e) if i == 10 => {
                return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into());
            }
            _ => {}
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err((
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Unexpected error at {}", line!()),
    )
        .into())
}

#[tracing::instrument]
async fn redirect_to_link(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
) -> axum::response::Result<Redirect> {
    if token.len() != 11 {
        error!("Bad request");
        return Err((StatusCode::BAD_REQUEST, "Bad token").into());
    }

    if let Some(url) = read_from_cache::<_, String>(&state.redis, &token).await {
        info!("Redirecting to [{url}]");
        return Ok(Redirect::permanent(&url));
    }

    // not found in cache..
    match sqlx::query!(r"SELECT url FROM links WHERE token = $1;", token)
        .fetch_one(&state.pg)
        .await
    {
        Ok(row) => {
            let _ = write_to_cache(&state.redis, &token, &row.url).await;
            Ok(Redirect::permanent(&token))
        }
        Err(sqlx::Error::RowNotFound) => Err((StatusCode::NOT_FOUND, "Record not found!!").into()),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into()),
    }
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt().init();

    let db_url = env::var("DATABASE_URL").expect("The database url should be there");
    let redis_url = env::var("REDIS_URL").expect("The cache url should be there");
    let pg = PgPoolOptions::new()
        .max_connections(50)
        .acquire_timeout(Duration::from_secs(3))
        .idle_timeout(Duration::from_secs(10))
        .connect(&db_url)
        .await
        .expect("Cannot connect to database");

    let redis = redis::Client::open(redis_url)
        .expect("Cannot connect to cache")
        .get_connection_manager()
        .await
        .expect("Cannot get cache connection");

    info!("{}", db_url);

    let api = Router::new()
        .route("/links", post(create_link))
        .route("/health", get(health));

    let app = Router::new()
        .route("/{id}", get(redirect_to_link))
        .nest("/api/v1/", api)
        .with_state(Arc::new(AppState { pg: pg, redis }));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8001").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
