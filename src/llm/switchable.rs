//! Runtime-switchable wrapper for registry-backed LLM providers.
//!
//! Some providers (notably rig-core adapters) bake the model at construction
//! time and cannot implement in-place `set_model()`. This wrapper rebuilds the
//! inner provider from a stored `RegistryProviderConfig` and swaps it
//! atomically, enabling hot model switching at the agent layer.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use rust_decimal::Decimal;

use crate::llm::config::RegistryProviderConfig;
use crate::llm::error::LlmError;
use crate::llm::provider::{
    CompletionRequest, CompletionResponse, LlmProvider, ModelMetadata, ToolCompletionRequest,
    ToolCompletionResponse,
};

type ProviderFactory = fn(&RegistryProviderConfig) -> Result<Arc<dyn LlmProvider>, LlmError>;

/// Wrapper that supports runtime model switching by recreating the inner provider.
pub struct SwitchableProvider {
    provider: RwLock<Arc<dyn LlmProvider>>,
    config: RwLock<RegistryProviderConfig>,
    factory: ProviderFactory,
    configured_model_name: String,
}

impl SwitchableProvider {
    /// Build a switchable provider around a registry config and factory.
    pub fn new(
        config: RegistryProviderConfig,
        factory: ProviderFactory,
    ) -> Result<SwitchableProvider, LlmError> {
        let provider = factory(&config)?;
        Ok(Self {
            provider: RwLock::new(provider),
            configured_model_name: config.model.clone(),
            config: RwLock::new(config),
            factory,
        })
    }

    fn current_provider(&self) -> Arc<dyn LlmProvider> {
        match self.provider.read() {
            Ok(guard) => Arc::clone(&guard),
            Err(poisoned) => {
                tracing::warn!("provider lock poisoned while reading; continuing");
                Arc::clone(&poisoned.into_inner())
            }
        }
    }
}

#[async_trait]
impl LlmProvider for SwitchableProvider {
    fn model_name(&self) -> &str {
        &self.configured_model_name
    }

    fn cost_per_token(&self) -> (Decimal, Decimal) {
        self.current_provider().cost_per_token()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        self.current_provider().complete(request).await
    }

    async fn complete_with_tools(
        &self,
        request: ToolCompletionRequest,
    ) -> Result<ToolCompletionResponse, LlmError> {
        self.current_provider().complete_with_tools(request).await
    }

    async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        self.current_provider().list_models().await
    }

    async fn model_metadata(&self) -> Result<ModelMetadata, LlmError> {
        self.current_provider().model_metadata().await
    }

    fn effective_model_name(&self, requested_model: Option<&str>) -> String {
        self.current_provider().effective_model_name(requested_model)
    }

    fn active_model_name(&self) -> String {
        self.current_provider().active_model_name()
    }

    fn set_model(&self, model: &str) -> Result<(), LlmError> {
        // Rebuild provider first so failures don't disrupt the current active provider.
        let next_config = match self.config.read() {
            Ok(guard) => {
                let mut cfg = guard.clone();
                cfg.model = model.to_string();
                cfg
            }
            Err(poisoned) => {
                tracing::warn!("config lock poisoned while reading; continuing");
                let mut cfg = poisoned.into_inner().clone();
                cfg.model = model.to_string();
                cfg
            }
        };

        let next_provider = (self.factory)(&next_config)?;

        match self.provider.write() {
            Ok(mut guard) => *guard = next_provider,
            Err(poisoned) => {
                tracing::warn!("provider lock poisoned while writing; continuing");
                *poisoned.into_inner() = next_provider;
            }
        }

        match self.config.write() {
            Ok(mut guard) => *guard = next_config,
            Err(poisoned) => {
                tracing::warn!("config lock poisoned while writing; continuing");
                *poisoned.into_inner() = next_config;
            }
        }

        Ok(())
    }

    fn calculate_cost(&self, input_tokens: u32, output_tokens: u32) -> Decimal {
        self.current_provider()
            .calculate_cost(input_tokens, output_tokens)
    }

    fn cache_write_multiplier(&self) -> Decimal {
        self.current_provider().cache_write_multiplier()
    }

    fn cache_read_discount(&self) -> Decimal {
        self.current_provider().cache_read_discount()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::provider::{ChatMessage, FinishReason, ToolCall};
    use crate::llm::registry::ProviderProtocol;

    struct ModelEchoProvider {
        model: String,
    }

    #[async_trait]
    impl LlmProvider for ModelEchoProvider {
        fn model_name(&self) -> &str {
            &self.model
        }

        fn cost_per_token(&self) -> (Decimal, Decimal) {
            (Decimal::ZERO, Decimal::ZERO)
        }

        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            let content = format!(
                "{}:{}",
                self.model,
                request
                    .messages
                    .last()
                    .map(|m| m.content.as_str())
                    .unwrap_or_default()
            );
            Ok(CompletionResponse {
                content,
                input_tokens: 1,
                output_tokens: 1,
                finish_reason: FinishReason::Stop,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            })
        }

        async fn complete_with_tools(
            &self,
            _request: ToolCompletionRequest,
        ) -> Result<ToolCompletionResponse, LlmError> {
            Ok(ToolCompletionResponse {
                content: Some(self.model.clone()),
                tool_calls: Vec::<ToolCall>::new(),
                input_tokens: 1,
                output_tokens: 1,
                finish_reason: FinishReason::Stop,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            })
        }

        fn active_model_name(&self) -> String {
            self.model.clone()
        }

        async fn list_models(&self) -> Result<Vec<String>, LlmError> {
            Ok(vec!["model-a".to_string(), "model-b".to_string()])
        }

        async fn model_metadata(&self) -> Result<ModelMetadata, LlmError> {
            Ok(ModelMetadata {
                id: self.model.clone(),
                context_length: None,
            })
        }
    }

    fn test_factory(config: &RegistryProviderConfig) -> Result<Arc<dyn LlmProvider>, LlmError> {
        if config.model == "broken-model" {
            return Err(LlmError::RequestFailed {
                provider: config.provider_id.clone(),
                reason: "simulated factory failure".to_string(),
            });
        }
        Ok(Arc::new(ModelEchoProvider {
            model: config.model.clone(),
        }))
    }

    fn config(model: &str) -> RegistryProviderConfig {
        RegistryProviderConfig {
            protocol: ProviderProtocol::OpenAiCompletions,
            provider_id: "test-provider".to_string(),
            api_key: None,
            base_url: "http://localhost:1234/v1".to_string(),
            model: model.to_string(),
            extra_headers: Vec::new(),
            oauth_token: None,
            cache_retention: crate::llm::config::CacheRetention::None,
        }
    }

    #[tokio::test]
    async fn set_model_rebuilds_provider_and_switches_model() {
        let switchable = SwitchableProvider::new(config("model-a"), test_factory).unwrap();
        assert_eq!(switchable.active_model_name(), "model-a");

        switchable.set_model("model-b").unwrap();
        assert_eq!(switchable.active_model_name(), "model-b");

        let resp = switchable
            .complete(CompletionRequest::new(vec![ChatMessage::user("ping")]))
            .await
            .unwrap();
        assert!(resp.content.starts_with("model-b:"));
    }

    #[tokio::test]
    async fn set_model_failure_keeps_current_provider() {
        let switchable = SwitchableProvider::new(config("model-a"), test_factory).unwrap();
        assert_eq!(switchable.active_model_name(), "model-a");

        let err = switchable.set_model("broken-model").unwrap_err();
        assert!(err.to_string().contains("simulated factory failure"));
        assert_eq!(switchable.active_model_name(), "model-a");
    }

    #[test]
    fn model_name_reports_initial_configured_model() {
        let switchable = SwitchableProvider::new(config("model-a"), test_factory).unwrap();
        assert_eq!(switchable.model_name(), "model-a");
    }
}
