use chrono::{DateTime, TimeDelta, Utc};
use rocket::{
    fairing::AdHoc,
    post, routes,
    serde::json::{Json, Value},
    Config, State,
};
use secrecy::SecretString;
use serde::{Deserialize, Deserializer, Serialize};
use shuttle_runtime::CustomError;
use sqlx::{Executor, FromRow, PgPool, Postgres, Transaction};
use std::time::Duration;
use tokio::time;
use tracing::info;

type PgTransaction = Transaction<'static, Postgres>;
type Result<T, E = rocket::response::Debug<sqlx::Error>> = std::result::Result<T, E>;

#[derive(Debug, Clone, Deserialize, Serialize, FromRow)]
#[serde(crate = "rocket::serde")]
struct Issue {
    id: String,
    updated_at: DateTime<Utc>,
    reminded: bool,
}

/// We receive this in the webhook POST
///
/// Example:
/// ```json
/// {
///   "action": "update",
///   "actor": {
///     "id": "2e6eea91-1e2c-43a4-9486-acea0603004e",
///     "name": "Luke Hsiao"
///   },
///   "createdAt": "2024-03-28T05:10:45.264Z",
///   "data": {
///     "id": "bf740309-ed5f-48da-a0f7-b8b26e18b33b",
///     "createdAt": "2024-03-23T15:32:11.774Z",
///     "updatedAt": "2024-03-28T05:10:45.264Z",
///     "number": 339,
///     "title": "2023 Taxes",
///     "priority": 2,
///     "estimate": 4,
///     "boardOrder": 0,
///     "sortOrder": -11061.79,
///     "startedAt": "2024-03-23T15:32:11.806Z",
///     "labelIds": [],
///     "teamId": "4d869526-74de-48de-92b2-2f0dc171849a",
///     "cycleId": "8d86d606-8b1f-4387-aa34-e6f8dfc00ebc",
///     "previousIdentifiers": [],
///     "creatorId": "2e6eea91-1e2c-43a4-9486-acea0603004e",
///     "assigneeId": "2e6eea91-1e2c-43a4-9486-acea0603004e",
///     "stateId": "478ce2a9-1874-4cd0-b2ee-9dbe810352f9",
///     "priorityLabel": "High",
///     "botActor": {
///       "id": "5c07d33f-5e8f-484b-8100-67908589ec45",
///       "type": "workflow",
///       "name": "Linear",
///       "avatarUrl": "https://static.linear.app/assets/pwa/icon_maskable_512.png"
///     },
///     "identifier": "HSI-339",
///     "url": "https://linear.app/hsiao/issue/HSI-339/2023-taxes",
///     "assignee": {
///       "id": "2e6eea91-1e2c-43a4-9486-acea0603004e",
///       "name": "Luke Hsiao"
///     },
///     "cycle": {
///       "id": "8d86d606-8b1f-4387-aa34-e6f8dfc00ebc",
///       "number": 19,
///       "startsAt": "2024-03-25T07:00:00.000Z",
///       "endsAt": "2024-04-08T07:00:00.000Z"
///     },
///     "state": {
///       "id": "478ce2a9-1874-4cd0-b2ee-9dbe810352f9",
///       "color": "#f2c94c",
///       "name": "In Progress",
///       "type": "started"
///     },
///     "team": {
///       "id": "4d869526-74de-48de-92b2-2f0dc171849a",
///       "key": "HSI",
///       "name": "Hsiao"
///     },
///     "subscriberIds": [
///       "2e6eea91-1e2c-43a4-9486-acea0603004e",
///       "233a3b9e-68d5-4e3e-b350-4b1f85ce733b"
///     ],
///     "labels": []
///   },
///   "updatedFrom": {
///     "updatedAt": "2024-03-28T05:10:18.275Z",
///     "sortOrder": 84.27,
///     "stateId": "3e0d1574-f23c-441c-953d-42e08ad719eb"
///   },
///   "url": "https://linear.app/hsiao/issue/HSI-339/2023-taxes",
///   "type": "Issue",
///   "organizationId": "15a23696-00bb-44b4-ad4a-84e751d82d13",
///   "webhookTimestamp": 1711602645358,
///   "webhookId": "3f106cc1-617f-4398-83ed-238cece0b5e2"
/// }
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(crate = "rocket::serde")]
struct Payload {
    action: String,
    #[serde(rename = "type")]
    event_type: String,
    #[serde(alias = "createdAt")]
    created_at: DateTime<Utc>,
    data: IssueData,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(crate = "rocket::serde")]
struct IssueData {
    id: String,
    state: StateData,
    #[serde(skip)]
    _ignored_fields: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(crate = "rocket::serde")]
struct StateData {
    name: String,
    #[serde(skip)]
    _ignored_fields: Option<Value>,
}

#[derive(Deserialize, Debug, Clone)]
struct AppConfig {
    linear: LinearConfig,
    #[serde(deserialize_with = "deserialize_duration")]
    time_to_remind: Duration,
}

#[derive(Deserialize, Debug, Clone)]
struct LinearConfig {
    api_key: SecretString,
    signing_key: SecretString,
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

async fn dequeue_issue(pool: &PgPool) -> Result<Option<(PgTransaction, String, DateTime<Utc>)>> {
    let mut transaction = pool.begin().await?;
    let r = sqlx::query!(
        r#"
        SELECT id, updated_at, reminded
        FROM issues
        WHERE reminded = false
        ORDER BY updated_at ASC
        FOR UPDATE
        SKIP LOCKED
        LIMIT 1
        "#,
    )
    .fetch_optional(&mut *transaction)
    .await?;
    if let Some(r) = r {
        Ok(Some((transaction, r.id, r.updated_at)))
    } else {
        Ok(None)
    }
}

#[post("/", format = "json", data = "<payload>")]
async fn webhook(
    payload: Json<Payload>,
    state: &State<AppState>,
    app_config: &State<AppConfig>,
) -> Result<()> {
    info!(linear=?app_config.linear, time_to_remind=?app_config.time_to_remind, api_key=?app_config.linear.api_key, api_key=?app_config.linear.signing_key, "config");

    // TODO: verify the signature of the webhook

    // Use `ON CONFLICT DO NOTHING` because after the `time_to_remind`,
    // we will check again, whether or not an issue was updated twice.
    sqlx::query!(
        "INSERT INTO issues( id, updated_at, reminded) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        &payload.data.id,
        payload.created_at,
        false
    )
    .execute(&state.pool)
    .await?;
    info!(payload=?payload, "added issue to remind");

    // TODO: if task is already in the DB and reminded, and state.name != merged, delete the row
    Ok(())
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

    // Worker: separate task which sends the reminder comments
    let worker_pool = pool.clone();
    let worker_config = Config::figment()
        .extract::<AppConfig>()
        .expect("failed to parse app config");
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(5));
        loop {
            let issue = dequeue_issue(&worker_pool).await;
            if let Ok(Some((transaction, id, updated_at))) = issue {
                let now = Utc::now();

                if now.signed_duration_since(updated_at)
                    > TimeDelta::from_std(worker_config.time_to_remind)
                        .expect("failed to convert Duration to TimeDelta")
                {
                    info!(id=?(transaction, id, updated_at), "remind!");
                    // Post reminder comment to Linear
                    // Mark reminded = true
                }
            }
            interval.tick().await;
        }
    });

    let state = AppState { pool };
    let rocket = rocket::build()
        .attach(AdHoc::config::<AppConfig>())
        .mount("/webhook/issue", routes![webhook])
        .manage(state);
    Ok(rocket.into())
}
