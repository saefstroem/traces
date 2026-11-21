mod anthropic;
mod capture;
mod events;
mod gemini;
mod gemini_interactions;
mod ollama;
mod openai_chat;
mod openai_responses;
mod trace;

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use anthropic::Anthropic;
use events::Events;
use gemini::Gemini;
use gemini_interactions::GeminiInteractions;
use ollama::Ollama;
use openai_chat::OpenAiChat;
use openai_responses::OpenAiResponses;

pub use capture::Capture;
pub use trace::{Exchange, Trace};

/// A trait for providers that can read events and generate traces.
pub trait Provider: Sync {
    fn name(&self) -> &'static str;
    /// Extracts user prompt from a request, that may or may not return the prompt
    fn prompt<'a>(&self, request: &'a Value) -> Option<&'a Value>;
    fn read(&self, event: &Value, exchange: &mut Exchange);
}

const PROVIDERS: [&dyn Provider; 6] =
    [&Anthropic, &OpenAiChat, &OpenAiResponses, &Gemini, &GeminiInteractions, &Ollama];

/// Deserializes a provider by its name from a string. Returns an error if the provider is unknown.
/// Used for the config.json to ensure its a valid provider name.
pub fn by_name<'de, D: Deserializer<'de>>(deserializer: D) -> Result<&'static dyn Provider, D::Error> {
    let name = String::deserialize(deserializer)?;
    let found = PROVIDERS.into_iter().find(|provider| provider.name() == name);
    found.ok_or_else(|| DeError::custom(format!("unknown provider {name:?}")))
}
