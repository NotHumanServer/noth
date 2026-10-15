use serde::Deserialize;

/// 金額は文字列で返ってくる
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Wallet {
    #[serde(default)]
    pub available_balance: String,
    #[serde(default)]
    pub currency: String,
    #[serde(default)]
    pub as_of: Option<String>,
    #[serde(default)]
    pub rates: WalletRates,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WalletRates {
    #[serde(default)]
    pub meters: Vec<WalletMeter>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WalletMeter {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub unit_amount: String,
    #[serde(default)]
    pub per: String,
}

impl Wallet {
    /// Agent の1ステップあたり USD。見つからない・読めないなら None
    pub fn agent_step_usd(&self) -> Option<f64> {
        self.rates
            .meters
            .iter()
            .find(|m| m.per == "step" && m.label.contains("Agent"))?
            .unit_amount
            .trim()
            .parse()
            .ok()
    }

    pub(crate) fn status_line(&self) -> String {
        let amount = if self.currency.is_empty() || self.currency == "USD" {
            format!("${}", self.available_balance)
        } else {
            format!("{} {}", self.available_balance, self.currency)
        };
        let at = self.as_of.as_deref().map(|raw| {
            let local = raw.parse::<jiff::Timestamp>().ok().map(|t| {
                t.to_zoned(jiff::tz::TimeZone::system())
                    .strftime("%Y-%m-%d %H:%M")
                    .to_string()
            });
            format!("({} 時点)", local.as_deref().unwrap_or(raw))
        });
        format!("TinyFish 残高 {amount}{}", at.unwrap_or_default())
    }
}
