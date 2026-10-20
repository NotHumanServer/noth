//! OpenAI の単価表($/MTok、Standard)。
//! 出典: https://developers.openai.com/api/docs/pricing (2026-10-15 時点)。
//! 推論トークンは出力として課金される: https://developers.openai.com/api/docs/guides/reasoning
use serde::Deserialize;

const TOKENS_PER_UNIT: f64 = 1_000_000.0;
/// 入力がこれを超えると long context の単価になる
const LONG_PROMPT_TOKENS: u64 = 272_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub cached_input: f64,
    pub cache_write: f64,
    pub output: f64,
}

/// config の `[provider.openai.pricing]`。指定したキーだけが表の値より優先される
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overrides {
    pub input: Option<f64>,
    pub cached_input: Option<f64>,
    pub cache_write: Option<f64>,
    pub output: Option<f64>,
}

impl Rates {
    fn with(self, o: &Overrides) -> Self {
        Self {
            input: o.input.unwrap_or(self.input),
            cached_input: o.cached_input.unwrap_or(self.cached_input),
            cache_write: o.cache_write.unwrap_or(self.cache_write),
            output: o.output.unwrap_or(self.output),
        }
    }
}

impl Overrides {
    /// 表にないモデル(互換サーバーなど)でも、input と output を指定すれば金額を出せる。
    /// キャッシュ系は指定が無ければ通常の入力単価で数える(割引を勝手に仮定しない)
    fn standalone(&self) -> Option<Rates> {
        let (input, output) = (self.input?, self.output?);
        Some(Rates {
            input,
            cached_input: self.cached_input.unwrap_or(input),
            cache_write: self.cache_write.unwrap_or(input),
            output,
        })
    }
}

/// 表の「-」(キャッシュ非対応・書き込み課金なし)は通常の入力単価で数える
const fn r(input: f64, cached: f64, write: f64, output: f64) -> Rates {
    Rates {
        input,
        cached_input: cached,
        cache_write: write,
        output,
    }
}

struct Entry {
    id: &'static str,
    rates: Rates,
    long: Option<Rates>,
}

const fn e(id: &'static str, rates: Rates, long: Option<Rates>) -> Entry {
    Entry { id, rates, long }
}

// gpt-5.6-sol には 2026-11-21 以降まで続く促進価格の注記があるが、どの値が該当するか確認できていない。
// 変わったら `[provider.openai.pricing]` で上書きする
const TABLE: &[Entry] = &[
    e(
        "gpt-6-astra",
        r(10.0, 1.0, 12.5, 50.0),
        Some(r(20.0, 2.0, 25.0, 75.0)),
    ),
    e(
        "gpt-6.1-sol",
        r(2.0, 0.1, 2.5, 10.0),
        Some(r(4.0, 0.2, 5.0, 15.0)),
    ),
    e(
        "gpt-6-sol",
        r(2.0, 0.2, 2.5, 10.0),
        Some(r(4.0, 0.4, 5.0, 15.0)),
    ),
    e(
        "gpt-6-luna",
        r(0.1, 0.01, 0.125, 0.5),
        Some(r(0.2, 0.02, 0.25, 0.75)),
    ),
    e(
        "gpt-5.6-sol",
        r(4.0, 0.4, 5.0, 20.0),
        Some(r(8.0, 0.8, 10.0, 30.0)),
    ),
    e(
        "gpt-5.6-terra",
        r(2.0, 0.2, 2.5, 12.0),
        Some(r(4.0, 0.4, 5.0, 18.0)),
    ),
    e(
        "gpt-5.6-luna",
        r(0.2, 0.02, 0.25, 1.2),
        Some(r(0.4, 0.04, 0.5, 1.8)),
    ),
    e(
        "gpt-5.5",
        r(5.0, 0.5, 5.0, 30.0),
        Some(r(10.0, 1.0, 10.0, 45.0)),
    ),
    e(
        "gpt-5.5-pro",
        r(30.0, 30.0, 30.0, 180.0),
        Some(r(60.0, 60.0, 60.0, 270.0)),
    ),
    e(
        "gpt-5.4",
        r(2.5, 0.25, 2.5, 15.0),
        Some(r(5.0, 0.5, 5.0, 22.5)),
    ),
    e(
        "gpt-5.4-pro",
        r(30.0, 30.0, 30.0, 180.0),
        Some(r(60.0, 60.0, 60.0, 270.0)),
    ),
    e("gpt-5.4-mini", r(0.75, 0.075, 0.75, 4.5), None),
    e("gpt-5.4-nano", r(0.2, 0.02, 0.2, 1.25), None),
    e("gpt-5.2", r(1.75, 0.175, 1.75, 14.0), None),
    e("gpt-5.1", r(1.25, 0.125, 1.25, 10.0), None),
    e("gpt-5", r(1.25, 0.125, 1.25, 10.0), None),
    e("gpt-5-mini", r(0.25, 0.025, 0.25, 2.0), None),
    e("gpt-5-nano", r(0.05, 0.005, 0.05, 0.4), None),
    e("gpt-4.1", r(2.0, 0.5, 2.0, 8.0), None),
    e("gpt-4.1-mini", r(0.4, 0.1, 0.4, 1.6), None),
    e("gpt-4.1-nano", r(0.1, 0.025, 0.1, 0.4), None),
    e("gpt-4o-mini", r(0.15, 0.075, 0.15, 0.6), None),
];

/// `gpt-5.4` に対する `gpt-5.4-2026-03-05` のような日付・latest 付きだけを同じモデルとみなす。
/// 単なる前方一致だと `gpt-5.4-mini` が `gpt-5.4` の単価で計算されてしまう
fn matches(model: &str, id: &str) -> bool {
    let Some(rest) = model.strip_prefix(id) else {
        return false;
    };
    match rest.strip_prefix('-') {
        None => rest.is_empty(),
        Some(suffix) => {
            suffix == "latest"
                || (suffix.len() >= 8 && suffix.bytes().all(|b| b.is_ascii_digit() || b == b'-'))
        }
    }
}

/// `use_table` が false のとき(互換サーバー)は表を引かず、上書きだけで単価を決める。
/// `prompt_tokens` はキャッシュ込みの入力全体
pub fn lookup(
    model: &str,
    prompt_tokens: u64,
    use_table: bool,
    overrides: &Overrides,
) -> Option<Rates> {
    let entry = use_table
        .then(|| TABLE.iter().find(|e| matches(model, e.id)))
        .flatten();
    match entry {
        Some(entry) => {
            let base = match entry.long {
                Some(long) if prompt_tokens > LONG_PROMPT_TOKENS => long,
                _ => entry.rates,
            };
            Some(base.with(overrides))
        }
        None => overrides.standalone(),
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TokenCounts {
    /// キャッシュの読み書きを除いた入力
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// 推論トークンを含む
    pub output: u64,
}

impl TokenCounts {
    /// OpenAI の入力トークンはキャッシュ分を含んで報告されるため、そこから読み書き分を引いて分ける
    pub fn from_reported(prompt: u64, cache_read: u64, cache_write: u64, output: u64) -> Self {
        Self {
            input: prompt.saturating_sub(cache_read + cache_write),
            cache_read,
            cache_write,
            output,
        }
    }

    pub fn prompt_tokens(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

pub fn amount_usd(rates: &Rates, t: &TokenCounts) -> f64 {
    let sum = t.input as f64 * rates.input
        + t.cache_read as f64 * rates.cached_input
        + t.cache_write as f64 * rates.cache_write
        + t.output as f64 * rates.output;
    sum / TOKENS_PER_UNIT
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cost(model: &str, t: TokenCounts) -> Option<f64> {
        lookup(model, t.prompt_tokens(), true, &Overrides::default()).map(|r| amount_usd(&r, &t))
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn representative_models() {
        // 入力側は合計 270K で long 階層の手前。各 90K なので単価の和 * 0.09 に出力 1M 分を足す
        let t = TokenCounts {
            input: 90_000,
            cache_read: 90_000,
            cache_write: 90_000,
            output: 1_000_000,
        };
        let cases = [
            ("gpt-6-astra", 10.0, 1.0, 12.5, 50.0),
            ("gpt-6-luna", 0.1, 0.01, 0.125, 0.5),
            ("gpt-5.5", 5.0, 0.5, 5.0, 30.0),
            ("gpt-5.4-pro", 30.0, 30.0, 30.0, 180.0),
            ("gpt-5-mini", 0.25, 0.025, 0.25, 2.0),
        ];
        for (model, i, c, w, o) in cases {
            let want = 0.09 * (i + c + w) + o;
            assert!(close(cost(model, t).unwrap(), want), "{model}");
        }
    }

    #[test]
    fn long_context_tier() {
        let at = TokenCounts {
            input: 272_000,
            output: 1_000_000,
            ..Default::default()
        };
        let over = TokenCounts {
            input: 272_001,
            output: 1_000_000,
            ..Default::default()
        };
        assert!(close(
            cost("gpt-5.4", at).unwrap(),
            272_000.0 * 2.5 / 1e6 + 15.0
        ));
        assert!(close(
            cost("gpt-5.4", over).unwrap(),
            272_001.0 * 5.0 / 1e6 + 22.5
        ));
        // 長い単価の無いモデルは階層に関係なく同じ
        assert_eq!(
            lookup("gpt-5-mini", 1_000_000, true, &Overrides::default())
                .unwrap()
                .input,
            0.25
        );
    }

    #[test]
    fn long_tier_counts_cached_tokens() {
        let t = TokenCounts::from_reported(300_000, 290_000, 0, 0);
        let rates = lookup("gpt-5.4", t.prompt_tokens(), true, &Overrides::default()).unwrap();
        assert_eq!(rates.cached_input, 0.5);
    }

    #[test]
    fn reported_prompt_is_split() {
        let t = TokenCounts::from_reported(100, 60, 10, 5);
        assert_eq!((t.input, t.cache_read, t.cache_write), (30, 60, 10));
        assert_eq!(TokenCounts::from_reported(10, 60, 0, 0).input, 0);
    }

    #[test]
    fn suffix_matching() {
        let o = Overrides::default();
        assert!(lookup("gpt-5.4-2026-03-05", 0, true, &o).is_some());
        assert!(lookup("gpt-5.4-latest", 0, true, &o).is_some());
        assert_eq!(lookup("gpt-5.4-mini", 0, true, &o).unwrap().input, 0.75);
        assert_eq!(
            lookup("gpt-5.4-mini-2026-03-05", 0, true, &o)
                .unwrap()
                .input,
            0.75
        );
        assert!(lookup("gpt-5.4-turbo", 0, true, &o).is_none());
        assert!(lookup("kimi-k2", 0, true, &o).is_none());
    }

    #[test]
    fn overrides_per_key_and_standalone() {
        let o = Overrides {
            input: Some(1.0),
            output: Some(2.0),
            ..Default::default()
        };
        let rates = lookup("gpt-5.4", 0, true, &o).unwrap();
        assert_eq!(
            (rates.input, rates.output, rates.cached_input),
            (1.0, 2.0, 0.25)
        );
        // 表を使わない場合は上書きだけで決まり、キャッシュ系は input に揃う
        let rates = lookup("gpt-5.4", 0, false, &o).unwrap();
        assert_eq!((rates.cached_input, rates.cache_write), (1.0, 1.0));
        assert!(lookup("gpt-5.4", 0, false, &Overrides::default()).is_none());
        let partial = Overrides {
            input: Some(1.0),
            ..Default::default()
        };
        assert!(lookup("unknown", 0, true, &partial).is_none());
    }
}
