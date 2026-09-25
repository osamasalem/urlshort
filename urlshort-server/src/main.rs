// cSpell:disable
use std::{
    collections::HashMap,
    env,
    fmt::{Debug, Display},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use askama::Template;
use axum::{
    Form, Json, Router,
    extract::{Path, Query, State},
    http::{Response, StatusCode},
    response::{self, Html, IntoResponse, Redirect},
    routing::{get, post},
};
use axum_prometheus::{PrometheusMetricLayer, metrics::counter};
use log::{error, info};
use rand::seq::IndexedRandom;
use redis::{AsyncCommands, FromRedisValue, ToSingleRedisArg, aio::ConnectionManager};
use rust_embed::Embed;
use scylla::{
    client::{session::Session, session_builder::SessionBuilder},
    errors::ExecutionError,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use validator::Validate;

use futures::{FutureExt, future::BoxFuture, stream::StreamExt};

#[derive(Debug)]
struct AppState {
    pub_url: String,
    db: Session,
    redis: ConnectionManager,
}

#[derive(Embed)]
#[folder = "assets"]
struct Assets;

const TOKEN_SIZE: usize = 11;

#[tracing::instrument]
fn generate_token() -> String {
    let mut rng = rand::rng();
    let chars = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz1234567890";
    (0..TOKEN_SIZE)
        .map(|_| chars.choose(&mut rng).unwrap())
        .fold(String::with_capacity(TOKEN_SIZE), |mut acc, &i| {
            acc.push(i.into());
            acc
        })
}

#[tracing::instrument]
async fn health() -> axum::response::Result<axum::response::Response> {
    Ok(Json(json!({
        "status":"healthy",
    }))
    .into_response())
}

#[tracing::instrument]
async fn live(
    State(state): State<Arc<AppState>>,
) -> axum::response::Result<axum::response::Response> {
    Ok(Json(json!({
        "status":"healthy",
    }))
    .into_response())
}

#[tracing::instrument]
async fn read_from_cache<T, U>(redis: &ConnectionManager, key: &T) -> Option<U>
where
    T: ToString + Debug,
    U: FromRedisValue + Debug,
{
    let mut redis = redis.clone();
    let key = key.to_string();

    let value = redis.get(&key).await.ok()?;
    info!("Data read from cache {:?}=>{:?}", key, value);

    value
}

#[tracing::instrument]
async fn write_to_cache<T, U>(redis: &ConnectionManager, key: T, val: U)
where
    T: ToSingleRedisArg + Send + Sync + Display + Debug,
    U: ToSingleRedisArg + Send + Sync + Display + Debug,
{
    let mut redis = redis.clone();

    let _ = redis.set_ex::<&T, &U, ()>(&key, &val, 10).await;
    info!("Data written to cache {}=>{}", key, val);
}

#[derive(Debug, Deserialize, Validate)]
struct LinksCreateRequest {
    #[validate(url)]
    url: String,
}

#[derive(Debug, Template)]
#[template(path = "generated_url_part.html")]
struct UrlPartTemplate<'a> {
    url: &'a str,
    full_url: &'a str,
}

#[derive(Debug, Template)]
#[template(path = "generated_url_error_part.html")]
struct UrlErrorPartTemplate<'a> {
    error: &'a str,
}

#[derive(Debug)]
enum UrlShortError {
    NotFound(String),
    Generic(String),
    DBError(String),
}

impl Display for UrlShortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UrlShortError::DBError(e) => write!(f, "Db Error: {e}"),
            UrlShortError::Generic(e) => write!(f, "Generic Error: {e}"),
            UrlShortError::NotFound(e) => write!(f, "Resource not Found: {e}"),
        }
    }
}

impl std::error::Error for UrlShortError {}

impl IntoResponse for UrlShortError {
    fn into_response(self) -> axum::response::Response {
        match self {
            Self::NotFound(e) => Response::builder()
                .status(StatusCode::NOT_FOUND.as_u16())
                .body(axum::body::Body::from(e))
                .unwrap(),
            Self::DBError(e) => Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR.as_u16())
                .body(axum::body::Body::from(e.to_string()))
                .unwrap(),
            Self::Generic(e) => Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR.as_u16())
                .body(axum::body::Body::from(e))
                .unwrap(),
        }
    }
}

#[tracing::instrument]
async fn get_token_from_url(state: &Arc<AppState>, url: &str) -> Result<String, UrlShortError> {
    if let Some(u) = read_from_cache(&state.redis, &url).await {
        return Ok(u);
    }

    let res = state
        .db
        .query_iter(
            r#"SELECT tokenid FROM urlshort.links WHERE url = ? LIMIT 1;"#,
            (&url,),
        )
        .await
        .map_err(|e| UrlShortError::Generic(e.to_string()))?;

    let mut tystream = res
        .rows_stream::<(String,)>()
        .map_err(|e| UrlShortError::Generic(e.to_string()))?;

    let (token,) = tystream
        .next()
        .await
        .ok_or_else(|| UrlShortError::Generic("No record found".to_string()))?
        .map_err(|e| UrlShortError::Generic(e.to_string()))?;

    info!("Record found.. ");
    write_to_cache(&state.redis, url, &token).await;
    Ok(token)
}

#[tracing::instrument]
async fn create_link_internal(state: &Arc<AppState>, url: &str) -> Result<String, UrlShortError> {
    for _ in 0..=10 {
        let token = generate_token();
        info!("Inserting token.. [{token}]");

        let res = state
            .db
            .query_iter(
                r#"INSERT INTO urlshort.links (tokenid, url) VALUES(?, ?) IF NOT EXISTS;"#,
                (&token, &url),
            )
            .await
            .map_err(|e| UrlShortError::Generic(e.to_string()))?;

        let mut tystream = res
            .rows_stream::<(bool, Option<String>, Option<String>)>()
            .map_err(|e| UrlShortError::Generic(e.to_string()))?;

        let (applied, ..) = tystream
            .next()
            .await
            .ok_or_else(|| UrlShortError::Generic("No record found".to_string()))?
            .map_err(|e| UrlShortError::Generic(e.to_string()))?;

        if applied {
            let _ = write_to_cache(&state.redis, &token, url).await;
            return Ok(token);
        }
    }
    Err(UrlShortError::Generic(format!(
        "Unexpected error at {}",
        line!()
    )))
}

#[tracing::instrument]
fn render_template(template: impl Template + Debug) -> axum::response::Html<String> {
    template
        .render()
        .unwrap_or("<Unknown Template Rendering>".into())
        .into()
}

#[tracing::instrument]
async fn generate(
    State(state): State<Arc<AppState>>,
    Form(req): Form<LinksCreateRequest>,
) -> axum::response::Html<String> {
    if req.validate().is_err() {
        return render_template(UrlErrorPartTemplate {
            error: "You entered invalid URL",
        });
    };

    let token = match get_token_from_url(&state, &req.url).await {
        Ok(value) => value,
        _ => match create_link_internal(&state, &req.url).await {
            Ok(value) => value,
            Err(e) => {
                return render_template(UrlErrorPartTemplate {
                    error: &format!(
                        "Server side problem, Please contact support or try again. {e}"
                    ),
                });
            }
        },
    };

    let full_url = format!("{}/{}", state.pub_url, token);

    counter!("urlshort_generate_count").increment(1);
    render_template(UrlPartTemplate {
        url: &token,
        full_url: &full_url,
    })
}

#[tracing::instrument]
async fn assets(Path(path): Path<String>) -> axum::response::Result<axum::response::Response> {
    let asset = match Assets::get(&path) {
        Some(asset) => asset,
        None => match Assets::get("404.html") {
            Some(asset) => asset,
            None => {
                return Err((StatusCode::INTERNAL_SERVER_ERROR, "Asset resolution failed").into());
            }
        },
    };

    let mime = mime_guess::from_path(&path).first_or_octet_stream();

    let resp = axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, mime.as_ref())
        .header(axum::http::header::CACHE_CONTROL, "public, max-age=300")
        .body(axum::body::Body::from(asset.data))
        .unwrap_or((StatusCode::INTERNAL_SERVER_ERROR, "Error rendering asset").into_response());

    Ok(resp)
}

#[tracing::instrument]
async fn redirect_to_link(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
) -> axum::response::Result<response::Response> {
    if token.len() != TOKEN_SIZE {
        error!("Bad request");
        return assets(Path("404.html".into())).await;
    }

    if let Some(url) = read_from_cache::<_, String>(&state.redis, &token).await {
        info!("Redirecting to [{url}]");
        return Ok(Redirect::permanent(&url).into_response());
    }

    // not found in cache..
    let query_pager = state
        .db
        .query_iter(
            r"SELECT url FROM urlshort.links WHERE tokenid = ? LIMIT 1;",
            (&token,),
        )
        .await
        .map_err(|e| UrlShortError::Generic(e.to_string()))?;

    let mut type_rows = query_pager
        .rows_stream::<(String,)>()
        .map_err(|e| UrlShortError::Generic(e.to_string()))?;

    let Some(Ok(row)) = type_rows.next().await else {
        return assets(Path("404.html".into())).await;
    };

    let _ = write_to_cache(&state.redis, &token, &row.0).await;
    counter!("urlshort_redirect_count").increment(1);

    Ok(Redirect::permanent(&row.0).into_response())
}

#[tracing::instrument]
async fn home() -> axum::response::Result<axum::response::Html<String>> {
    let Some(data) = Assets::get("home.html") else {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Cannot get home page data",
        )
            .into());
    };

    let Ok(text) = str::from_utf8(&data.data) else {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Cannot get encode home page",
        )
            .into());
    };

    Ok(Html(text.to_string()))
}

async fn migrate(session: &Session) -> Result<(), ExecutionError> {
    session
        .query_unpaged(
            "CREATE KEYSPACE IF NOT EXISTS urlshort WITH REPLICATION = \
            {'class' : 'NetworkTopologyStrategy', 'datacenter1' : 2 }",
            &[],
        )
        .await?;

    session
        .query_unpaged(
            r#"CREATE TABLE IF NOT EXISTS urlshort.links (tokenid TEXT PRIMARY KEY, url TEXT)"#,
            &[],
        )
        .await?;
    Ok(())
}

#[tokio::main]
#[tracing::instrument]
async fn main() {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt().init();

    let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();

    let known_nodes = env::var("SCYLLADB_KNOWN_NODES").expect("The db nodes should be there");
    let redis_url = env::var("REDIS_URL").expect("The cache url should be there");
    let host = env::var("LISTEN_HOST").expect("The listening host should be there");
    let pub_url = env::var("PUBLIC_URL").unwrap_or("http://example.com".into());

    let known_nodes: Vec<_> = known_nodes.split(',').collect();
    let db: Session = SessionBuilder::new()
        .known_nodes(known_nodes)
        .connection_timeout(Duration::from_secs(30))
        .cluster_metadata_refresh_interval(Duration::from_secs(30))
        .build()
        .await
        .expect("Cannot connect to DB");

    let redis = redis::Client::open(redis_url)
        .expect("Cannot connect to cache")
        .get_connection_manager()
        .await
        .expect("Cannot get cache connection");

    let api = Router::new()
        .route("/metrics", get(|| async move { metric_handle.render() }))
        .route("/health", get(health))
        .route("/links/generate", post(generate))
        .route("/live", get(live))
        .fallback(async || (StatusCode::NOT_FOUND, "Not Found"))
        .layer(prometheus_layer);

    migrate(&db).await.expect("Migration should work");

    let app = Router::new()
        .route("/", get(home))
        .route("/{id}", get(redirect_to_link))
        .route("/assets/{*path}", get(assets))
        .nest("/api/v1/", api)
        .with_state(Arc::new(AppState { db, redis, pub_url }))
        .fallback(async || assets(Path("404.html".into())).await);

    let listener = tokio::net::TcpListener::bind(&host)
        .await
        .expect(&format!("Cannot bind on {host}"));
    info!("Listening on {host}");

    axum::serve(listener, app)
        .await
        .expect("Cannot start server");
}
