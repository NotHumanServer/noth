//! Gemini API(AI Studio)の単価表($/MTok、Standard、テキスト入力)。
//! 出典: https://ai.google.dev/gemini-api/docs/pricing (2026-10-14 UTC 更新分を 2026-10-15 に確認)
//!
//! 値上げ予定がある: `gemini-4-argon` は 2026-12-01 に、`gemini-3.6-flash` / `gemini-3.8-flash` など
//! 多くの flash 系は 2027-01-01 に倍額になる。日付での自動切り替えは持たず、表は現行値のまま、
//! 切り替え後は `[provider.google.pricing]` の上書きで合わせる。
//! 音声入力は別単価(高い)だが、Noth はテキストしか送らないので表にはテキストの値だけを持つ。
//! Vertex(agent-platform)と opencode-go の単価は未確認のため、表を引かず金額を None にする。
use serde::Deserialize;

const TOKENS_PER_UNIT: f64 = 1_000_000.0;
/// pro 系はプロンプト全体(キャッシュ込み)がこれを超えると高い単価になる
const LONG_PROMPT_TOKENS: u64 = 200_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub cached_input: f64,
    /// thinking 込み
    pub output: f64,
}

/// config の `[provider.google.pricing]`。指定したキーだけが表の値より優先される
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overrides {
    pub input: Option<f64>,
    pub cached_input: Option<f64>,
    pub output: Option<f64>,
}

impl Overrides {
    /// 3 キーすべてがあれば表に無いモデルや preset でも金額を出せる
    fn complete(&self) -> Option<Rates> {
        Some(Rates {
            input: self.input?,
            cached_input: self.cached_input?,
            output: self.output?,
        })
    }
}

impl Rates {
    fn with(self, o: &Overrides) -> Self {
        Self {
            input: o.input.unwrap_or(self.input),
            cached_input: o.cached_input.unwrap_or(self.cached_input),
            output: o.output.unwrap_or(self.output),
        }
    }
}

const fn r(input: f64, cached_input: f64, output: f64) -> Rates {
    Rates {
        input,
        cached_input,
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

const GEMINI_3_1_PRO: Entry = Entry {
    prefix: "gemini-3.1-pro",
    rates: r(2.0, 0.2, 12.0),
    long: Some(r(4.0, 0.4, 18.0)),
};

const TABLE: &[Entry] = &[
    e("gemini-4-argon", r(2.0, 0.1, 10.0)),
    e("gemini-3.8-flash", r(0.75, 0.075, 3.75)),
    e("gemini-3.6-flash", r(0.75, 0.075, 3.75)),
    e("gemini-3.5-flash-lite", r(0.3, 0.03, 2.5)),
    e("gemini-3.1-flash-lite", r(0.25, 0.025, 1.5)),
    GEMINI_3_1_PRO,
    Entry {
        prefix: "gemini-3.1-pro-preview-customtools",
        ..GEMINI_3_1_PRO
    },
    e("gemini-3-flash", r(0.5, 0.05, 3.0)),
    Entry {
        prefix: "gemini-2.5-pro",
        rates: r(1.25, 0.125, 10.0),
        long: Some(r(2.5, 0.25, 15.0)),
    },
    e("gemini-2.5-flash-lite", r(0.1, 0.01, 0.4)),
    e("gemini-2.5-flash", r(0.3, 0.03, 2.5)),
];

/// `gemini-3.1-pro` に対する `gemini-3.1-pro-preview` や日付付きだけを同じモデルとみなす。
/// 単なる前方一致だと、表にない `gemini-3.1-pro-ultra` が `gemini-3.1-pro` の単価で計算されてしまう
fn matches(model: &str, prefix: &str) -> bool {
    let Some(rest) = model.strip_prefix(prefix) else {
        return false;
    };
    match rest.strip_prefix('-') {
        None => rest.is_empty(),
        Some(suffix) => {
            suffix == "preview"
                || suffix == "latest"
                || (suffix.len() >= 6 && suffix.bytes().all(|b| b.is_ascii_digit()))
        }
    }
}

/// 表にないモデルや `priced == false`(Vertex / opencode-go)は、上書きが 3 キー揃っているときだけ単価を返す。
/// `prompt_tokens` はキャッシュ分を含む入力全体
pub fn lookup(
    model: &str,
    priced: bool,
    prompt_tokens: u64,
    overrides: &Overrides,
) -> Option<Rates> {
    let entry = priced
        .then(|| TABLE.iter().find(|e| matches(model, e.prefix)))
        .flatten();
    let Some(entry) = entry else {
        return overrides.complete();
    };
    let base = match entry.long {
        Some(long) if prompt_tokens > LONG_PROMPT_TOKENS => long,
        _ => entry.rates,
    };
    Some(base.with(overrides))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TokenCounts {
    /// キャッシュ読み取りを除いた入力
    pub input: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl TokenCounts {
    pub fn prompt_tokens(&self) -> u64 {
        self.input + self.cache_read
    }
}

pub fn amount_usd(rates: &Rates, t: &TokenCounts) -> f64 {
    let sum = t.input as f64 * rates.input
        + t.cache_read as f64 * rates.cached_input
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
        lookup(model, true, t.prompt_tokens(), &none()).map(|r| amount_usd(&r, &t))
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn representative_models() {
        let t = TokenCounts {
            input: 1_000_000,
            cache_read: 1_000_000,
            output: 1_000_000,
        };
        // 1M トークンずつなので、各単価の合計になる
        let cases = [
            ("gemini-4-argon", 2.0 + 0.1 + 10.0),
            ("gemini-3.6-flash", 0.75 + 0.075 + 3.75),
            ("gemini-3.5-flash-lite", 0.3 + 0.03 + 2.5),
            ("gemini-3-flash-preview", 0.5 + 0.05 + 3.0),
            ("gemini-2.5-flash", 0.3 + 0.03 + 2.5),
            ("gemini-2.5-flash-lite", 0.1 + 0.01 + 0.4),
        ];
        for (model, want) in cases {
            // 2M トークンのプロンプトでも、tier のないモデルは同額
            let got = cost(model, t).unwrap();
            assert!(close(got, want), "{model}: {got}");
        }
    }

    #[test]
    fn customtools_variant_has_same_price() {
        let t = TokenCounts {
            input: 1_000_000,
            ..Default::default()
        };
        // 1M のプロンプトは 200k 超なので上位 tier
        for m in [
            "gemini-3.1-pro-preview",
            "gemini-3.1-pro-preview-customtools",
        ] {
            assert!(close(cost(m, t).unwrap(), 4.0), "{m}");
        }
    }

    #[test]
    fn pro_models_have_200k_tier() {
        let short = TokenCounts {
            input: 200_000,
            output: 1_000_000,
            ..Default::default()
        };
        let long = TokenCounts {
            input: 200_001,
            output: 1_000_000,
            ..Default::default()
        };
        assert!(close(
            cost("gemini-2.5-pro", short).unwrap(),
            0.2 * 1.25 + 10.0
        ));
        assert!(close(
            cost("gemini-2.5-pro", long).unwrap(),
            200_001.0 * 2.5 / 1e6 + 15.0
        ));
        assert!(close(
            cost("gemini-3.1-pro-preview", long).unwrap(),
            200_001.0 * 4.0 / 1e6 + 18.0
        ));
    }

    #[test]
    fn long_tier_counts_cached_tokens() {
        let t = TokenCounts {
            input: 10,
            cache_read: 200_000,
            ..Default::default()
        };
        let rates = lookup("gemini-2.5-pro", true, t.prompt_tokens(), &none()).unwrap();
        assert_eq!(rates.input, 2.5);
    }

    #[test]
    fn suffixes_match_only_preview_latest_and_dates() {
        assert!(lookup("gemini-2.5-pro-preview", true, 0, &none()).is_some());
        assert!(lookup("gemini-2.5-flash-20261001", true, 0, &none()).is_some());
        assert!(lookup("gemini-2.5-flash-latest", true, 0, &none()).is_some());
        assert!(lookup("gemini-3.1-pro-ultra", true, 0, &none()).is_none());
        // lite は flash の亜種ではなく別モデル
        assert_eq!(
            lookup("gemini-2.5-flash-lite", true, 0, &none())
                .unwrap()
                .input,
            0.1
        );
    }

    #[test]
    fn unknown_models_and_unpriced_presets_have_no_price() {
        assert!(lookup("gpt-5", true, 0, &none()).is_none());
        assert!(lookup("gemini-9-unknown", true, 0, &none()).is_none());
        assert!(lookup("gemini-2.5-flash", false, 0, &none()).is_none());
    }

    #[test]
    fn overrides_take_precedence_per_key() {
        let o = Overrides {
            input: Some(1.0),
            output: Some(2.0),
            ..Default::default()
        };
        let rates = lookup("gemini-2.5-flash", true, 0, &o).unwrap();
        assert_eq!(rates.input, 1.0);
        assert_eq!(rates.output, 2.0);
        assert_eq!(rates.cached_input, 0.03);
    }

    #[test]
    fn complete_overrides_price_unknown_models_and_vertex() {
        let o = Overrides {
            input: Some(1.0),
            cached_input: Some(0.1),
            output: Some(2.0),
        };
        assert_eq!(lookup("gemini-2.5-flash", false, 0, &o).unwrap().input, 1.0);
        assert_eq!(lookup("gemini-9-unknown", true, 0, &o).unwrap().output, 2.0);
    }
}
