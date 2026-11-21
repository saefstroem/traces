use std::collections::HashMap;
use std::env;
use std::fs;

use crate::error::Error;
use crate::result::Result;

/// Application settings loaded from environment variables or a .env file.
pub struct Settings {
    /// The endpoint URL for the S3-compatible object store.
    pub endpoint: String,
    /// The region for the S3-compatible object store.
    pub region: String,
    /// The bucket name for storing traces.
    pub bucket: String,
    /// The access key ID for the S3-compatible object store.
    pub access_key_id: String,
    /// The secret access key for the S3-compatible object store.
    pub secret_access_key: String,
    /// The number of seconds to retain traces before purging.
    pub purge_seconds: u64,
    /// The maximum memory usage in bytes for the application.
    pub max_memory_bytes: usize,
}

impl Settings {
    pub fn load() -> Result<Self> {
        Self::parse(&fs::read_to_string(".env").unwrap_or_default(), |name| env::var(name).ok())
    }

    pub fn from_env_file() -> Result<Self> {
        Self::parse(&fs::read_to_string(".env").unwrap_or_default(), |_| None)
    }

    fn parse(dotenv: &str, shell: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let file: HashMap<&str, &str> = dotenv
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with('#'))
            .filter_map(|line| line.split_once('='))
            .map(|(name, value)| (name.trim(), value.trim().trim_matches('"')))
            .collect();
        let mut missing = Vec::new();
        let mut value = |name: &'static str| {
            let found = shell(name).or_else(|| file.get(name).map(|value| value.to_string()));
            found.filter(|value| !value.is_empty()).unwrap_or_else(|| {
                missing.push(name);
                String::new()
            })
        };
        let (endpoint, region, bucket) =
            (value("TRACES_S3_ENDPOINT"), value("TRACES_S3_REGION"), value("TRACES_S3_BUCKET"));
        let (access_key_id, secret_access_key) =
            (value("TRACES_S3_ACCESS_KEY_ID"), value("TRACES_S3_SECRET_ACCESS_KEY"));
        let (purge, memory) = (value("TRACES_PURGE_SECONDS"), value("TRACES_MAX_MEMORY_MIB"));
        if !missing.is_empty() {
            return Err(Error::MissingEnv(missing.join(", ")));
        }
        let number = |name: &'static str, text: &str| {
            text.parse::<u64>().ok().filter(|number| *number > 0).ok_or(Error::InvalidEnv(name))
        };
        let purge_seconds = number("TRACES_PURGE_SECONDS", &purge)?;
        let max_memory_bytes = number("TRACES_MAX_MEMORY_MIB", &memory)?
            .checked_mul(1 << 20)
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or(Error::InvalidEnv("TRACES_MAX_MEMORY_MIB"))?;
        Ok(Self { endpoint, region, bucket, access_key_id, secret_access_key, purge_seconds, max_memory_bytes })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::Settings;

    const DOTENV: &str = "# object store\nTRACES_S3_ENDPOINT=http://127.0.0.1:9000\nTRACES_S3_REGION = auto\n\
                          TRACES_S3_BUCKET=\"traces\"\nTRACES_S3_ACCESS_KEY_ID=file-key\nTRACES_S3_SECRET_ACCESS_KEY=s3cret\n\
                          TRACES_PURGE_SECONDS=30\nTRACES_MAX_MEMORY_MIB=2\n";

    #[test]
    fn reads_the_shell_first_and_then_the_env_file() {
        let shell = HashMap::from([("TRACES_S3_ACCESS_KEY_ID", "shell-key")]);
        let settings = Settings::parse(DOTENV, |name| shell.get(name).map(|value| value.to_string())).unwrap();
        assert_eq!((settings.endpoint.as_str(), settings.region.as_str()), ("http://127.0.0.1:9000", "auto"));
        assert_eq!((settings.bucket.as_str(), settings.access_key_id.as_str()), ("traces", "shell-key"));
        assert_eq!(settings.secret_access_key, "s3cret");
        assert_eq!((settings.purge_seconds, settings.max_memory_bytes), (30, 2 << 20));
    }

    #[test]
    fn names_every_missing_or_invalid_variable_without_its_value() {
        let error = Settings::parse("TRACES_S3_BUCKET=traces\nTRACES_S3_SECRET_ACCESS_KEY=\n", |_| None).err().unwrap();
        let expected = "missing environment variables: TRACES_S3_ENDPOINT, TRACES_S3_REGION, TRACES_S3_ACCESS_KEY_ID, \
                        TRACES_S3_SECRET_ACCESS_KEY, TRACES_PURGE_SECONDS, TRACES_MAX_MEMORY_MIB (see .env.template)";
        assert_eq!(error.to_string(), expected);
        for (setting, invalid) in
            [("TRACES_PURGE_SECONDS", "soon"), ("TRACES_PURGE_SECONDS", "0"), ("TRACES_MAX_MEMORY_MIB", "-1")]
        {
            let text = format!("{DOTENV}{setting}={invalid}\n");
            let error = Settings::parse(&text, |_| None).err().unwrap().to_string();
            assert_eq!(error, format!("{setting} must be a whole number above 0"));
        }
    }
}
