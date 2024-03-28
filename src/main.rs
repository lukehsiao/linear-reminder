use chrono::{DateTime, Utc};
use rocket::{
    fairing::AdHoc, post, response::status::Created, routes, serde::json::Json, Config, State,
};
use serde::{Deserialize, Serialize};
use shuttle_runtime::CustomError;
use sqlx::{Executor, PgPool};
use tracing::info;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(crate = "rocket::serde")]
struct Issue {
    id: String,
    updated_at: DateTime<Utc>,
    reminded: bool,
}

type Result<T, E = rocket::response::Debug<sqlx::Error>> = std::result::Result<T, E>;

#[post("/", data = "<issue>")]
async fn create(issue: Json<Issue>, state: &State<AppState>) -> Result<Created<Json<Issue>>> {
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
        .attach(AdHoc::config::<Config>())
        .mount("/issues", routes![create])
        .manage(state);

    Ok(rocket.into())
}
