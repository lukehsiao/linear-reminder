use chrono::{DateTime, Utc};
use rocket::{fairing::AdHoc, post, response::status::Created, routes, serde::json::Json, State};
use secrecy::SecretString;
use serde::{Deserialize, Deserializer, Serialize};
use shuttle_runtime::CustomError;
use sqlx::{Executor, PgPool};
use std::time::Duration;
use tracing::info;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(crate = "rocket::serde")]
struct Issue {
    id: String,
    updated_at: DateTime<Utc>,
    reminded: bool,
}

type Result<T, E = rocket::response::Debug<sqlx::Error>> = std::result::Result<T, E>;

#[derive(Deserialize, Debug, Clone)]
struct AppConfig {
    linear: LinearConfig,
    #[serde(deserialize_with = "deserialize_duration")]
    time_to_remind: Duration,
}

#[derive(Deserialize, Debug, Clone)]
struct LinearConfig {
    api_key: SecretString,
}

/// Custom deserializer from humantime to std::time::Duration
fn deserialize_duration<'de, D>(deserializer: D) -> Result<std::time::Duration, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    match s.parse::<humantime::Duration>() {
        Ok(duration) => Ok(duration.into()),
        Err(_) => Err(serde::de::Error::custom("Invalid duration format")),
    }
}

#[post("/", data = "<issue>")]
async fn create(
    issue: Json<Issue>,
    state: &State<AppState>,
    app_config: &State<AppConfig>,
) -> Result<Created<Json<Issue>>> {
    info!(linear=?app_config.linear, time_to_remind=?app_config.time_to_remind, api_key=?app_config.linear.api_key, "config");
    sqlx::query("SELECT 1").fetch_one(&state.pool).await?;
    Ok(Created::new("/").body(issue))
}

struct AppState {
    pool: PgPool,
}

#[shuttle_runtime::main]
async fn rocket(#[shuttle_shared_db::Postgres] pool: PgPool) -> shuttle_rocket::ShuttleRocket {
    // Run single migration on startup.
    pool.execute(include_str!("../migrations/1_issues.sql"))
        .await
        .map_err(CustomError::new)?;
    info!("ran database migrations");

    let state = AppState { pool };
    let rocket = rocket::build()
        .attach(AdHoc::config::<AppConfig>())
        .mount("/issues", routes![create])
        .manage(state);
    Ok(rocket.into())
}
