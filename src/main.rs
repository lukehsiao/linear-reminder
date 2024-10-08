use std::{env, time::Duration};

use chrono::{DateTime, TimeDelta, Utc};
use hmac::{Mac, SimpleHmac};
use reqwest::header;
use rocket::{
    data::{self, Data, FromData, ToByteUnit},
    fairing::AdHoc,
    http::{ContentType, Status},
    outcome::Outcome,
    post,
    request::{self, Request},
    routes,
    serde::json::{serde_json, Value},
    Config, State,
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::Sha256;
use shuttle_runtime::CustomError;
use sqlx::{Executor, FromRow, PgPool, Postgres, Transaction};
use tokio::time;
use tracing::{debug, info, warn};

type PgTransaction = Transaction<'static, Postgres>;
type Result<T, E = rocket::response::Debug<sqlx::Error>> = std::result::Result<T, E>;

#[derive(Debug, Clone, Deserialize, Serialize, FromRow)]
struct Issue {
    id: String,
    identifier: String,
    title: String,
    updated_at: DateTime<Utc>,
    reminded: bool,
}

/// We receive this in the webhook POST
///
/// Ref: <https://developers.linear.app/docs/graphql/webhooks#the-webhook-payload>
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
    #[serde(alias = "webhookTimestamp")]
    webhook_timestamp: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(crate = "rocket::serde")]
struct IssueData {
    id: String,
    identifier: String,
    title: String,
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
    target_status: String,
    message: String,
}

/// Custom deserializer from humantime to `std::time::Duration`
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

async fn dequeue_issue(pool: &PgPool) -> Result<Option<(PgTransaction, Issue)>> {
    let mut transaction = pool.begin().await?;
    let r = sqlx::query!(
        r#"
        SELECT id, identifier, title, updated_at, reminded
        FROM issues
        WHERE reminded = FALSE
        ORDER BY updated_at ASC
        FOR UPDATE
        SKIP LOCKED
        LIMIT 1
        "#,
    )
    .fetch_optional(&mut *transaction)
    .await?;
    if let Some(r) = r {
        Ok(Some((
            transaction,
            Issue {
                id: r.id,
                updated_at: r.updated_at,
                identifier: r.identifier,
                title: r.title,
                reminded: r.reminded,
            },
        )))
    } else {
        Ok(None)
    }
}

async fn issue_in_db(transaction: &mut PgTransaction, id: &str) -> Result<bool> {
    let r = sqlx::query!(
        r#"
        SELECT COUNT(*)
        FROM issues  
        WHERE id = $1
        "#,
        id
    )
    .fetch_one(&mut **transaction)
    .await?;

    if r.count == Some(1) {
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Data guard that validates integrity of the request body by comparing with a
/// signature.
const LINEAR_SIGNATURE: &str = "Linear-Signature";

#[rocket::async_trait]
impl<'r> FromData<'r> for Payload {
    type Error = ();

    async fn from_data(req: &'r Request<'_>, data: Data<'r>) -> data::Outcome<'r, Self> {
        // Ensure header is present
        let keys = req.headers().get(LINEAR_SIGNATURE).collect::<Vec<_>>();
        if keys.len() != 1 {
            return Outcome::Error((Status::BadRequest, ()));
        }
        let signature = keys[0];

        // Ensure content type is right
        let ct = ContentType::new("application", "json");
        if req.content_type() != Some(&ct) {
            return Outcome::Forward((data, Status::UnsupportedMediaType));
        }

        // TODO: could also verify IP address, but that makes testing harder.

        // Use a configured limit with name 'json' or fallback to default.
        let limit = req.limits().get("json").unwrap_or(5.kilobytes());

        // Read the data into a string.
        let body = match data.open(limit).into_string().await {
            Ok(string) if string.is_complete() => string.into_inner(),
            Ok(_) => return Outcome::Error((Status::PayloadTooLarge, ())),
            Err(_) => return Outcome::Error((Status::InternalServerError, ())),
        };

        // We store `body` in request-local cache for long-lived borrows.
        let body = request::local_cache!(req, body);
        let Some(config) = req.rocket().state::<AppConfig>() else {
            return Outcome::Error((Status::InternalServerError, ()));
        };

        if !is_valid_signature(signature, body, config.linear.signing_key.expose_secret()) {
            return Outcome::Error((Status::BadRequest, ()));
        }

        let r: Payload = match serde_json::from_str(body) {
            Ok(r) => r,
            Err(_) => return Outcome::Error((Status::BadRequest, ())),
        };

        // Prevent replay attacks
        let Some(webhook_time) = DateTime::from_timestamp(r.webhook_timestamp, 0) else {
            return Outcome::Error((Status::BadRequest, ()));
        };
        let now = Utc::now();
        if now.signed_duration_since(webhook_time).num_seconds() > 60 {
            return Outcome::Error((Status::BadRequest, ()));
        }

        Outcome::Success(r)
    }
}

type HmacSha256 = SimpleHmac<Sha256>;
fn is_valid_signature(signature: &str, body: &str, secret: &str) -> bool {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("failed to create hmac");
    mac.update(body.as_bytes());
    let result = mac.finalize();
    let expected_signature = result.into_bytes();
    let encoded = hex::encode(expected_signature);
    debug!(encoded=%encoded, "actual signature");

    // Some might say this should be constant-time equality check
    encoded == signature
}

#[post("/", format = "json", data = "<payload>")]
async fn webhook_linear(
    payload: Payload,
    state: &State<AppState>,
    app_config: &State<AppConfig>,
) -> Result<()> {
    info!(payload=?payload, "received payload");
    // Do everything in one transaction
    let mut transaction = state.pool.begin().await?;
    if payload.data.state.name == app_config.linear.target_status {
        // Use `ON CONFLICT DO NOTHING` because after the `time_to_remind`,
        // we will check again, whether or not an issue was updated twice.
        sqlx::query!(
            "INSERT INTO issues( id, identifier, title, updated_at, reminded) VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
            &payload.data.id,
            &payload.data.identifier,
            &payload.data.title,
            payload.created_at,
            false
        )
        .execute(&mut *transaction)
        .await?;
        info!(payload=?payload, "added issue to remind");
    } else if let Ok(true) = issue_in_db(&mut transaction, &payload.data.id).await {
        sqlx::query!("DELETE FROM issues WHERE id = $1", &payload.data.id)
            .execute(&mut *transaction)
            .await?;
        info!(payload=?payload, "issue is no longer {}", app_config.linear.target_status);
    }

    transaction.commit().await?;
    Ok(())
}

struct AppState {
    pool: PgPool,
}

#[shuttle_runtime::main]
async fn rocket(
    #[shuttle_shared_db::Postgres] pool: PgPool,
    #[shuttle_runtime::Secrets] secrets: shuttle_runtime::SecretStore,
) -> shuttle_rocket::ShuttleRocket {
    // Transfer Shuttle.rs Secrets to Env Vars
    if let Some(secret) = secrets.get("ROCKET_LINEAR.API_KEY") {
        env::set_var("ROCKET_LINEAR.API_KEY", secret);
    }
    if let Some(secret) = secrets.get("ROCKET_LINEAR.SIGNING_KEY") {
        env::set_var("ROCKET_LINEAR.SIGNING_KEY", secret);
    }
    if let Some(secret) = secrets.get("ROCKET_LINEAR.TARGET_STATUS") {
        env::set_var("ROCKET_LINEAR.TARGET_STATUS", secret);
    }
    if let Some(secret) = secrets.get("ROCKET_LINEAR.MESSAGE") {
        env::set_var("ROCKET_LINEAR.MESSAGE", secret);
    }
    if let Some(secret) = secrets.get("ROCKET_TIME_TO_REMIND") {
        env::set_var("ROCKET_TIME_TO_REMIND", secret);
    }

    // Run single migration on startup.
    pool.execute(include_str!("../migrations/1_issues.sql"))
        .await
        .map_err(CustomError::new)?;
    info!("ran database migrations");

    // Worker Task: periodically checks and sends the reminder comments
    let worker_pool = pool.clone();
    let worker_config = Config::figment()
        .extract::<AppConfig>()
        .expect("failed to parse app config");
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            let issue = dequeue_issue(&worker_pool).await;
            if let Ok(Some((mut transaction, issue_db))) = issue {
                let now = Utc::now();

                if now.signed_duration_since(issue_db.updated_at)
                    > TimeDelta::from_std(worker_config.time_to_remind)
                        .expect("failed to convert Duration to TimeDelta")
                {
                    let client = reqwest::Client::new();
                    // Ref: https://developers.linear.app/docs/graphql/working-with-the-graphql-api#queries-and-mutations
                    let body = serde_json::json!({
                        "query": format!(r#"mutation CommentCreate {{
                            commentCreate(
                                input: {{
                                  body: "{}"
                                  issueId: "{}"
                                }}
                            ) {{
                                success                            
                            }}
                        }}"#, worker_config.linear.message, issue_db.id)
                    });
                    if let Ok(res) = client
                        .post("https://api.linear.app/graphql")
                        .header(
                            header::AUTHORIZATION,
                            worker_config.linear.api_key.expose_secret(),
                        )
                        .header(header::CONTENT_TYPE, "application/json")
                        .json(&body)
                        .send()
                        .await
                    {
                        if !res.status().is_success() {
                            let status = res.status();
                            let text = res.text().await.unwrap_or_default();
                            warn!(issue=?issue_db, status=?status, msg=%text, "failed to post comment, retrying later...");
                            continue;
                        }
                    } else {
                        warn!(issue=?issue_db,"failed to post comment, retrying later...");
                        continue;
                    }

                    if let Ok(r) = sqlx::query!(
                        "UPDATE issues SET reminded = TRUE WHERE id = $1",
                        &issue_db.id
                    )
                    .execute(&mut *transaction)
                    .await
                    {
                        if r.rows_affected() == 1 {
                            let _ = transaction.commit().await;
                            info!(issue=?issue_db, "sent reminder");
                        } else {
                            let _ = transaction.rollback().await;
                        }
                    }
                }
            }
        }
    });

    let state = AppState { pool };
    let rocket = rocket::build()
        .attach(AdHoc::config::<AppConfig>())
        .mount("/webhooks/linear", routes![webhook_linear])
        .manage(state);
    Ok(rocket.into())
}
