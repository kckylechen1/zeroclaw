use super::ModelProvider;
use super::dispatch::ProviderDispatch;
use super::traits::{
    ChatMessage, ChatRequest, ChatResponse, StreamChunk, StreamEvent, StreamOptions, StreamResult,
};
use async_trait::async_trait;
use futures_util::stream::BoxStream;

pub struct ModelPinnedProvider {
    alias: String,
    pinned_model: String,
    inner: Box<dyn ModelProvider>,
}

impl ModelPinnedProvider {
    pub fn new(alias: &str, pinned_model: &str, inner: Box<dyn ModelProvider>) -> Self {
        Self {
            alias: alias.to_string(),
            pinned_model: pinned_model.to_string(),
            inner,
        }
    }
}

#[async_trait]
impl ModelProvider for ModelPinnedProvider {
    fn set_credential(&self, key: Option<String>) -> bool {
        self.inner.set_credential(key)
    }

    fn capabilities(&self) -> super::traits::ProviderCapabilities {
        self.inner.capabilities()
    }

    fn default_temperature(&self) -> f64 {
        self.inner.default_temperature()
    }

    fn default_max_tokens(&self) -> u32 {
        self.inner.default_max_tokens()
    }

    fn default_timeout_secs(&self) -> u64 {
        self.inner.default_timeout_secs()
    }

    fn default_base_url(&self) -> Option<&str> {
        self.inner.default_base_url()
    }

    fn default_wire_api(&self) -> &str {
        self.inner.default_wire_api()
    }

    fn convert_tools(&self, tools: &[zeroclaw_api::tool::ToolSpec]) -> super::traits::ToolsPayload {
        self.inner.convert_tools(tools)
    }

    fn supports_native_tools(&self) -> bool {
        self.inner.supports_native_tools()
    }

    fn supports_vision(&self) -> bool {
        self.inner.supports_vision()
    }

    fn supports_streaming(&self) -> bool {
        self.inner.supports_streaming()
    }

    fn supports_streaming_tool_events(&self) -> bool {
        self.inner.supports_streaming_tool_events()
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        ProviderDispatch::from_ref(&*self.inner).list_models().await
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        ProviderDispatch::from_ref(&*self.inner).warmup().await
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        _model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<String> {
        ProviderDispatch::from_ref(&*self.inner)
            .chat_with_system(system_prompt, message, &self.pinned_model, temperature)
            .await
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<String> {
        ProviderDispatch::from_ref(&*self.inner)
            .chat_with_history(messages, &self.pinned_model, temperature)
            .await
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        _model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<ChatResponse> {
        ProviderDispatch::from_ref(&*self.inner)
            .chat(request, &self.pinned_model, temperature)
            .await
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        _model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<ChatResponse> {
        ProviderDispatch::from_ref(&*self.inner)
            .chat_with_tools(messages, tools, &self.pinned_model, temperature)
            .await
    }

    fn stream_chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        _model: &str,
        temperature: Option<f64>,
        options: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamChunk>> {
        // stream_chat_with_system is not on ProviderDispatch's protected
        // surface — the dispatcher only wraps stream_chat. Pass through.
        self.inner.stream_chat_with_system(
            system_prompt,
            message,
            &self.pinned_model,
            temperature,
            options,
        )
    }

    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        temperature: Option<f64>,
        options: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamChunk>> {
        // Same passthrough rationale as stream_chat_with_system.
        self.inner
            .stream_chat_with_history(messages, &self.pinned_model, temperature, options)
    }

    fn stream_chat(
        &self,
        request: ChatRequest<'_>,
        _model: &str,
        temperature: Option<f64>,
        options: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamEvent>> {
        ProviderDispatch::from_ref(&*self.inner).stream_chat(
            request,
            &self.pinned_model,
            temperature,
            options,
        )
    }
}

impl zeroclaw_api::attribution::Attributable for ModelPinnedProvider {
    fn role(&self) -> zeroclaw_api::attribution::Role {
        self.inner.role()
    }
    fn alias(&self) -> &str {
        &self.alias
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::RwLock;
    use std::sync::Arc;

    /// Inner provider that actually applies runtime credential swaps.
    struct CredentialTrackingMock {
        credential: Arc<RwLock<Option<String>>>,
    }

    #[async_trait]
    impl ModelProvider for CredentialTrackingMock {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }

        fn set_credential(&self, key: Option<String>) -> bool {
            *self.credential.write() = key;
            true
        }
    }

    impl zeroclaw_api::attribution::Attributable for CredentialTrackingMock {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            zeroclaw_api::attribution::Role::Provider(
                zeroclaw_api::attribution::ProviderKind::Model(
                    zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            "CredentialTrackingMock"
        }
    }

    /// Discriminating: without forward, trait default returns false and inner
    /// credential is unchanged; with forward, pin wrapper updates the inner.
    #[test]
    fn set_credential_forwards_to_inner() {
        let credential = Arc::new(RwLock::new(Some("initial-key".into())));
        let pinned = ModelPinnedProvider::new(
            "pinned-alias",
            "pinned-model",
            Box::new(CredentialTrackingMock {
                credential: credential.clone(),
            }),
        );

        assert!(
            pinned.set_credential(Some("rotated-key".into())),
            "ModelPinnedProvider must report that the inner applied the credential"
        );
        assert_eq!(
            credential.read().as_deref(),
            Some("rotated-key"),
            "set_credential through ModelPinnedProvider must update the inner provider"
        );
    }
}
