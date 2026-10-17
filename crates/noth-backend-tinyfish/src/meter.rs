//! tool が Core の計測 sink に送るイベントの組み立て。送信失敗は無視する(計測で調査を止めない)
use noth_core::types::{
    Billing, CostRecord, MeterEvent, MeterSink, Purpose, RateLimitSnapshot, ToolContext,
};

use crate::RateLimit;

pub fn report_rate_limit(
    ctx: &ToolContext,
    source: &str,
    bucket: &str,
    rate_limit: Option<&RateLimit>,
) {
    let (Some(meter), Some(rl)) = (&ctx.meter, rate_limit) else {
        return;
    };
    send(
        meter,
        MeterEvent::RateLimit(RateLimitSnapshot {
            source: source.into(),
            bucket: bucket.into(),
            limit: rl.limit,
            remaining: rl.remaining,
            reset_at: rl.reset_at,
        }),
    );
}

/// `amount_usd` が None なら単価不明。無料の呼び出しは `Some(0.0)` と `Billing::Free` を渡す
pub fn report_cost(
    ctx: &ToolContext,
    source: &str,
    model: &str,
    unit: (&str, u64),
    amount_usd: Option<f64>,
    billing: Billing,
) {
    let Some(meter) = &ctx.meter else {
        return;
    };
    send(
        meter,
        MeterEvent::Cost(CostRecord {
            source: source.into(),
            model: model.into(),
            purpose: Purpose::Worker,
            run_id: Some(ctx.run_id.clone()),
            input_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 0,
            unit: Some((unit.0.into(), unit.1)),
            amount_usd,
            billing,
        }),
    );
}

fn send(meter: &MeterSink, event: MeterEvent) {
    let _ = meter.send(event);
}
