//! provider の呼び出しを記録する包み。Worker や Compactor が provider を直接呼んでも取りこぼさないよう、
//! Registry が組み立て時に包んで全員に同じものを渡す
use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::traits::{Completion, LayeredPrompt, Provider};
use crate::types::{Billing, CompletionParams, CostRecord, MeterEvent, MeterSink};

pub struct MeteredProvider {
    inner: Arc<dyn Provider>,
    source: &'static str,
    sink: MeterSink,
}

impl MeteredProvider {
    pub fn new(inner: Arc<dyn Provider>, source: &'static str, sink: MeterSink) -> Self {
        Self {
            inner,
            source,
            sink,
        }
    }

    fn record(&self, completion: &Completion, params: &CompletionParams) -> CostRecord {
        // 単価表を持たない provider でもトークン数だけは集計できるよう、usage から最低限の記録を作る
        let mut record = completion.cost.clone().unwrap_or_else(|| CostRecord {
            source: self.source.to_string(),
            model: String::new(),
            purpose: params.purpose,
            run_id: None,
            input_tokens: completion
                .usage
                .input_tokens
                .saturating_sub(completion.usage.cached_tokens),
            cache_read_tokens: completion.usage.cached_tokens,
            cache_write_tokens: 0,
            output_tokens: completion.usage.output_tokens,
            unit: None,
            amount_usd: None,
            billing: Billing::Metered,
        });
        record.source = self.source.to_string();
        record.purpose = params.purpose;
        record.run_id = params.run_id.clone();
        record
    }
}

#[async_trait]
impl Provider for MeteredProvider {
    async fn complete(
        &self,
        prompt: &LayeredPrompt,
        params: &CompletionParams,
    ) -> Result<Completion> {
        let completion = self.inner.complete(prompt, params).await?;
        let _ = self
            .sink
            .send(MeterEvent::Cost(self.record(&completion, params)));
        Ok(completion)
    }

    async fn keep_alive(&self, prefix: &LayeredPrompt) -> Result<()> {
        self.inner.keep_alive(prefix).await
    }

    async fn health_check(&self) -> Result<String> {
        self.inner.health_check().await
    }
}
