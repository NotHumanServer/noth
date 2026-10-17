pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn format_usd(amount: f64) -> String {
    format!("${amount:.4}")
}

pub(super) fn format_cost(amount: f64, unpriced: u64) -> String {
    if unpriced > 0 {
        format!("{} +不明 {unpriced} 件", format_usd(amount))
    } else {
        format_usd(amount)
    }
}

pub(super) fn format_units(units: &[(String, u64)]) -> String {
    if units.is_empty() {
        return "-".to_string();
    }
    units
        .iter()
        .map(|(name, n)| format!("{} {name}", group_digits(*n)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// 秒数を「3分」「2時間」のように最大の単位 1 つで表す
pub fn format_span(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        0..=59 => format!("{secs}秒"),
        60..=3599 => format!("{}分", secs / 60),
        3600..=86399 => format!("{}時間", secs / 3600),
        _ => format!("{}日", secs / 86400),
    }
}

pub(super) fn display_width(s: &str) -> usize {
    s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}
