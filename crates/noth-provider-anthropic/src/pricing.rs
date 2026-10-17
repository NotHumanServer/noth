//! Anthropic の単価表($/MTok)。
//! 出典: https://platform.claude.com/docs/en/about-claude/pricing (2026-10-15 時点)
use serde::Deserialize;

const TOKENS_PER_UNIT: f64 = 1_000_000.0;
/// Haiku 5.5 はプロンプト全体(キャッシュ込み)がこれを超えると高い単価になる
const LONG_PROMPT_TOKENS: u64 = 100_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
    pub output: f64,
}

/// config の `[provider.anthropic.pricing]`。指定したキーだけが表の値より優先される
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overrides {
    pub input: Option<f64>,
    pub cache_write_5m: Option<f64>,
    pub cache_write_1h: Option<f64>,
    pub cache_read: Option<f64>,
    pub output: Option<f64>,
}

impl Rates {
    fn with(self, o: &Overrides) -> Self {
        Self {
            input: o.input.unwrap_or(self.input),
            cache_write_5m: o.cache_write_5m.unwrap_or(self.cache_write_5m),
            cache_write_1h: o.cache_write_1h.unwrap_or(self.cache_write_1h),
            cache_read: o.cache_read.unwrap_or(self.cache_read),
            output: o.output.unwrap_or(self.output),
        }
    }
}

const fn r(input: f64, w5: f64, w1: f64, read: f64, output: f64) -> Rates {
    Rates {
        input,
        cache_write_5m: w5,
        cache_write_1h: w1,
        cache_read: read,
        output,
    }
}

struct Entry {
    prefix: &'static str,
    rates: Rates,
    /// 長いプロンプトにだけ適用される単価
    long: Option<Rates>,
}

const fn e(prefix: &'static str, rates: Rates) -> Entry {
    Entry {
        prefix,
        rates,
        long: None,
    }
}

const FABLE: Rates = r(10.0, 12.5, 20.0, 0.25, 50.0);
const FABLE_5: Rates = r(10.0, 12.5, 20.0, 1.0, 50.0);
const OPUS_4_X: Rates = r(5.0, 6.25, 10.0, 0.5, 25.0);
const OPUS_4_OLD: Rates = r(15.0, 18.75, 30.0, 1.5, 75.0);
const SONNET_4_X: Rates = r(3.0, 3.75, 6.0, 0.3, 15.0);

const TABLE: &[Entry] = &[
    e("claude-fable-5-1", FABLE),
    e("claude-mythos-5-1", FABLE),
    e("claude-fable-5", FABLE_5),
    e("claude-mythos-5", FABLE_5),
    e("claude-opus-5-5", r(4.0, 5.0, 8.0, 0.2, 20.0)),
    e("claude-opus-5", r(5.0, 6.25, 10.0, 0.5, 25.0)),
    e("claude-opus-4-8", OPUS_4_X),
    e("claude-opus-4-7", OPUS_4_X),
    e("claude-opus-4-6", OPUS_4_X),
    e("claude-opus-4-5", OPUS_4_X),
    e("claude-opus-4-1", OPUS_4_OLD),
    e("claude-opus-4", OPUS_4_OLD),
    e("claude-sonnet-5-5", r(2.0, 2.5, 4.0, 0.1, 10.0)),
    e("claude-sonnet-5", r(2.0, 2.5, 4.0, 0.2, 10.0)),
    e("claude-sonnet-4-6", SONNET_4_X),
    e("claude-sonnet-4-5", SONNET_4_X),
    e("claude-sonnet-4", SONNET_4_X),
    Entry {
        prefix: "claude-haiku-5-5",
        rates: r(0.1, 0.125, 0.2, 0.01, 0.5),
        long: Some(r(0.5, 0.625, 1.0, 0.05, 2.5)),
    },
    e("claude-haiku-4-5", r(1.0, 1.25, 2.0, 0.1, 5.0)),
    e("claude-haiku-3-5", r(0.8, 1.0, 1.6, 0.08, 4.0)),
    e("claude-3-5-haiku", r(0.8, 1.0, 1.6, 0.08, 4.0)),
];

/// `claude-opus-5-5` に対する `claude-opus-5-5-20261001` のような日付・latest 付きだけを同じモデルとみなす。
/// 単なる前方一致だと、表にない `claude-opus-5-6` が `claude-opus-5` の単価で計算されてしまう
fn matches(model: &str, prefix: &str) -> bool {
    let Some(rest) = model.strip_prefix(prefix) else {
        return false;
    };
    match rest.strip_prefix('-') {
        None => rest.is_empty(),
        Some(suffix) => {
            suffix == "latest" || (suffix.len() >= 6 && suffix.bytes().all(|b| b.is_ascii_digit()))
        }
    }
}

/// 表にないモデルは None。`prompt_tokens` はキャッシュの読み書きを含む入力全体
pub fn lookup(model: &str, prompt_tokens: u64, overrides: &Overrides) -> Option<Rates> {
    let entry = TABLE.iter().find(|e| matches(model, e.prefix))?;
    let base = match entry.long {
        Some(long) if prompt_tokens > LONG_PROMPT_TOKENS => long,
        _ => entry.rates,
    };
    Some(base.with(overrides))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TokenCounts {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub output: u64,
}

impl TokenCounts {
    pub fn prompt_tokens(&self) -> u64 {
        self.input + self.cache_read + self.cache_write_5m + self.cache_write_1h
    }
}

pub fn amount_usd(rates: &Rates, t: &TokenCounts) -> f64 {
    let sum = t.input as f64 * rates.input
        + t.cache_read as f64 * rates.cache_read
        + t.cache_write_5m as f64 * rates.cache_write_5m
        + t.cache_write_1h as f64 * rates.cache_write_1h
        + t.output as f64 * rates.output;
    sum / TOKENS_PER_UNIT
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> Overrides {
        Overrides::default()
    }

    fn cost(model: &str, t: TokenCounts) -> Option<f64> {
        lookup(model, t.prompt_tokens(), &none()).map(|r| amount_usd(&r, &t))
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn each_model_family_amount() {
        let t = TokenCounts {
            input: 1_000_000,
            cache_read: 1_000_000,
            cache_write_5m: 1_000_000,
            cache_write_1h: 1_000_000,
            output: 1_000_000,
        };
        // 1M トークンずつなので、各単価の合計になる
        let cases = [
            ("claude-fable-5-1", 10.0 + 0.25 + 12.5 + 20.0 + 50.0),
            ("claude-fable-5", 10.0 + 1.0 + 12.5 + 20.0 + 50.0),
            ("claude-opus-5-5", 4.0 + 0.2 + 5.0 + 8.0 + 20.0),
            ("claude-opus-5", 5.0 + 0.5 + 6.25 + 10.0 + 25.0),
            ("claude-opus-4-1", 15.0 + 1.5 + 18.75 + 30.0 + 75.0),
            ("claude-sonnet-5-5", 2.0 + 0.1 + 2.5 + 4.0 + 10.0),
            ("claude-sonnet-5", 2.0 + 0.2 + 2.5 + 4.0 + 10.0),
            ("claude-sonnet-4-6", 3.0 + 0.3 + 3.75 + 6.0 + 15.0),
            ("claude-haiku-4-5", 1.0 + 0.1 + 1.25 + 2.0 + 5.0),
        ];
        for (model, want) in cases {
            assert!(close(cost(model, t).unwrap(), want), "{model}");
        }
    }

    #[test]
    fn haiku_5_5_has_long_prompt_tier() {
        let short = TokenCounts {
            input: 100_000,
            output: 1_000_000,
            ..Default::default()
        };
        let long = TokenCounts {
            input: 100_001,
            output: 1_000_000,
            ..Default::default()
        };
        assert!(close(cost("claude-haiku-5-5", short).unwrap(), 0.01 + 0.5));
        assert!(close(
            cost("claude-haiku-5-5", long).unwrap(),
            100_001.0 * 0.5 / 1e6 + 2.5
        ));
    }

    #[test]
    fn long_tier_counts_cache_tokens() {
        let t = TokenCounts {
            input: 10,
            cache_read: 100_000,
            ..Default::default()
        };
        let rates = lookup("claude-haiku-5-5", t.prompt_tokens(), &none()).unwrap();
        assert_eq!(rates.input, 0.5);
    }

    #[test]
    fn cache_write_ttl_split() {
        let five = TokenCounts {
            cache_write_5m: 1_000_000,
            ..Default::default()
        };
        let hour = TokenCounts {
            cache_write_1h: 1_000_000,
            ..Default::default()
        };
        assert!(close(cost("claude-opus-5-5", five).unwrap(), 5.0));
        assert!(close(cost("claude-opus-5-5", hour).unwrap(), 8.0));
    }

    #[test]
    fn dated_and_latest_suffixes_match() {
        assert!(lookup("claude-haiku-5-5-20261001", 0, &none()).is_some());
        assert!(lookup("claude-opus-4-20250514", 0, &none()).is_some());
        assert!(lookup("claude-sonnet-5-5-latest", 0, &none()).is_some());
        assert_eq!(
            lookup("claude-opus-5-5-20261001", 0, &none())
                .unwrap()
                .input,
            4.0
        );
    }

    #[test]
    fn unknown_models_have_no_price() {
        assert!(lookup("gpt-5", 0, &none()).is_none());
        assert!(lookup("claude-opus-5-6", 0, &none()).is_none());
        assert!(lookup("kimi-k2", 0, &none()).is_none());
    }

    #[test]
    fn overrides_take_precedence_per_key() {
        let o = Overrides {
            input: Some(1.0),
            output: Some(2.0),
            ..Default::default()
        };
        let rates = lookup("claude-opus-5-5", 0, &o).unwrap();
        assert_eq!(rates.input, 1.0);
        assert_eq!(rates.output, 2.0);
        assert_eq!(rates.cache_read, 0.2);
    }
}
