use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::from_str;

use crate::error::Error;
use crate::prefix::Prefix;
use crate::providers::{Provider, by_name};
use crate::result::Result;
use crate::service::Platforms;

#[derive(Deserialize)]
/// Represents the configuration for the application, including listening addresses, CA directory, routes, conversation settings, and system platforms.
pub struct Config {
    /// The socket address on which the application will listen for incoming connections.
    pub listen: SocketAddr,
    /// The directory where the CA certificates are stored.
    ca_dir: String,
    /// The routing configuration for different API paths and their corresponding providers.
    routes: Vec<Route>,
    /// The socket address on which the application will listen for host connections.
    /// i.e. those that connect via /etc/hosts proxying on port 443 (LLM providers)
    pub hosts_listen: Option<SocketAddr>,
    /// The conversation settings for the application.
    pub prefix: Prefix,
    /// The system platforms configuration for the application.
    pub system: Platforms,
}

#[derive(Deserialize)]
/// Represents a routing rule for a specific API path suffix and its corresponding provider.
struct Route {
    /// The suffix of the API path that this route matches.
    path_suffix: String,
    /// The provider associated with this route.
    #[serde(deserialize_with = "by_name")]
    provider: &'static dyn Provider,
}

impl Config {
    /// Loads the configuration from the default `config.json` file.
    pub fn load() -> Result<Self> {
        Ok(from_str(include_str!("../config.json"))?)
    }

    /// Returns the path to the CA directory, expanding `~` to the user's home directory if necessary.
    pub fn ca_dir(&self) -> Result<PathBuf> {
        match self.ca_dir.strip_prefix("~/") {
            Some(rest) => Ok(PathBuf::from(env::var_os("HOME").ok_or(Error::NoHome)?).join(rest)),
            None => Ok(PathBuf::from(&self.ca_dir)),
        }
    }

    /// Returns the provider associated with the given API path, if any.
    pub fn route(&self, path: &str) -> Option<&'static dyn Provider> {
        self.routes.iter().find(|route| path.ends_with(&route.path_suffix)).map(|route| route.provider)
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::path::PathBuf;

    use serde_json::from_str;

    use super::Config;

    #[test]
    fn routes_each_provider_by_the_end_of_the_path() {
        let config = Config::load().unwrap();
        let routed = |path: &str| config.route(path).map(|provider| provider.name());
        assert!(config.listen.ip().is_loopback());
        assert_eq!(config.hosts_listen, Some("0.0.0.0:443".parse().unwrap()));
        assert_eq!(config.ca_dir().unwrap(), PathBuf::from(env::var_os("HOME").unwrap()).join(".traces"));
        assert_eq!(routed("/v1/messages"), Some("anthropic"));
        assert_eq!(routed("/v1/messages/count_tokens"), None);
        assert_eq!(routed("/v1/chat/completions"), Some("openai_chat"));
        assert_eq!(routed("/chat/completions"), Some("openai_chat"));
        assert_eq!(routed("/v1/responses"), Some("openai_responses"));
        assert_eq!(routed("/backend-api/codex/responses"), Some("openai_responses"));
        assert_eq!(routed("/v1/agent"), Some("openai_responses"));
        assert_eq!(routed("/v1beta/models/gemini-2.5-pro:streamGenerateContent"), Some("gemini"));
        assert_eq!(routed("/v1internal:streamGenerateContent"), Some("gemini"));
        assert_eq!(routed("/v1beta/interactions"), Some("gemini_interactions"));
        assert_eq!(routed("/api/chat"), Some("ollama"));
        assert_eq!(routed("/api/generate"), Some("ollama"));
        assert_eq!(routed("/v1/models"), None);
    }

    #[test]
    fn rejects_a_route_to_an_unknown_provider() {
        let text = r#"{"listen":"127.0.0.1:1","ca_dir":"x","routes":[{"path_suffix":"/x","provider":"nope"}]}"#;
        let error = from_str::<Config>(text).err().unwrap().to_string();
        assert!(error.contains("unknown provider \"nope\""), "{error}");
    }
}
