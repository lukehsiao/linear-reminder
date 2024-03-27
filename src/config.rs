//! Defines the configuration required to start the server application.
use anyhow::Result;
use serde::{Deserialize, Deserializer};
use std::time::Duration;

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    pub linear: LinearConfig,
    pub application: ApplicationConfig,
}

#[derive(Deserialize, Debug, Clone)]
pub struct LinearConfig {
    pub api_key: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ApplicationConfig {
    #[serde(deserialize_with = "deserialize_duration")]
    pub time_to_remind: Duration,
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

/// The possible runtime environment for our application.
pub enum Environment {
    Local,
    Production,
}

impl Environment {
    pub fn as_str(&self) -> &'static str {
        match self {
            Environment::Local => "local",
            Environment::Production => "production",
        }
    }
}

impl TryFrom<String> for Environment {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        match s.to_lowercase().as_str() {
            "local" => Ok(Self::Local),
            "production" => Ok(Self::Production),
            other => Err(format!(
                "{} is not a supported environment. Use either `local` or `production`.",
                other
            )),
        }
    }
}

pub fn get_config() -> Result<Config, config::ConfigError> {
    let base_path = std::env::current_dir().expect("Unable to determine current directory");
    let config_dir = base_path.join("config");

    // Detect the running environment, default to local if unspecified.
    let env: Environment = std::env::var("LR_ENVIRONMENT")
        .unwrap_or_else(|_| "local".into())
        .try_into()
        .expect("Failed to parse LR_ENVIRONMENT.");

    let config = config::Config::builder()
        // Read the default config
        .add_source(config::File::from(config_dir.join("base")).required(true))
        // Add in the current environment file
        .add_source(config::File::from(config_dir.join(env.as_str())))
        .add_source(
            config::Environment::with_prefix("LR")
                .prefix_separator("_")
                .separator("__"),
        )
        .build()?;

    config.try_deserialize()
}
#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;

    #[test]
    fn get_config_env_vars() -> Result<()> {
        std::env::set_var("LR_APPLICATION__TIME_TO_REMIND", "1s");
        std::env::set_var("LR_LINEAR__API_KEY", "key");
        dbg!(std::env::vars());
        let config = get_config()?;
        assert_eq!(config.application.time_to_remind, Duration::from_secs(1));
        assert_eq!(config.linear.api_key, "key");
        Ok(())
    }
}
