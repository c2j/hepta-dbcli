//! PII detection and anonymized value generation (issue #71).
//!
//! A column recognised as PII (by name pattern, by sample content, or forced
//! through rules) is kept out of the model's value dictionary: at generation
//! time it is filled with format-valid fake values from [`fake`] or from a
//! fixed template, never with an observed value.

use fake::faker::internet::en::SafeEmail;
use fake::faker::name::en::Name;
use fake::Fake;
use rand::rngs::StdRng;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::synth::marginal::{Marginal, UniformParams};
use crate::synth::model::ColumnModel;

/// PII kinds the recognizer and generator understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PiiProvider {
    Email,
    Phone,
    Name,
    IdCard,
}

impl PiiProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            PiiProvider::Email => "email",
            PiiProvider::Phone => "phone",
            PiiProvider::Name => "name",
            PiiProvider::IdCard => "id_card",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "email" | "mail" => Some(PiiProvider::Email),
            "phone" | "phone_number" | "mobile" | "tel" => Some(PiiProvider::Phone),
            "name" | "real_name" | "full_name" | "nickname" => Some(PiiProvider::Name),
            "id_card" | "idcard" | "ssn" => Some(PiiProvider::IdCard),
            _ => None,
        }
    }

    /// Does `value` have this provider's legal shape? The generator rejects
    /// and redraws values that fail, so every emitted value satisfies it.
    pub fn matches_format(self, value: &str) -> bool {
        match self {
            PiiProvider::Email => is_email(value),
            PiiProvider::Phone => {
                let digits = value.chars().filter(char::is_ascii_digit).count();
                (7..=15).contains(&digits)
                    && value
                        .chars()
                        .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | ' ' | '(' | ')'))
            }
            PiiProvider::Name => {
                !value.is_empty()
                    && value.chars().count() <= 64
                    && value.chars().any(char::is_alphabetic)
            }
            PiiProvider::IdCard => {
                let chars: Vec<char> = value.chars().collect();
                chars.len() == 18
                    && chars[..17].iter().all(char::is_ascii_digit)
                    && (chars[17].is_ascii_digit() || chars[17] == 'X' || chars[17] == 'x')
            }
        }
    }
}

fn is_email(value: &str) -> bool {
    let mut parts = value.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty()
        && local
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
}

/// Mark a model column as PII and erase the observed values it carried: a
/// categorical dictionary becomes placeholder level names (keeping the
/// count/frequency structure `stable_mapping` needs), anything else becomes a
/// neutral uniform. Range/format metadata is dropped so no observed value or
/// range survives in the model.
pub fn anonymize_model_column(column: &mut ColumnModel, provider: PiiProvider) {
    column.pii = Some(provider);
    match &mut column.marginal {
        Marginal::Categorical(params) => {
            for (index, value) in params.values.iter_mut().enumerate() {
                *value = format!("__pii_level_{index}");
            }
        }
        marginal => {
            *marginal = Marginal::Uniform(UniformParams {
                low: 0.0,
                high: 1.0,
            });
        }
    }
    column.min = None;
    column.max = None;
    column.rounding = None;
    column.decimal_scale = None;
    column.datetime_format = None;
    column.datetime_epoch = None;
}

/// Column-name patterns (English and pinyin/Chinese conventions).
fn provider_from_name(column: &str) -> Option<PiiProvider> {
    let name = column.to_ascii_lowercase();
    let tokens: Vec<&str> = name.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    let has = |wanted: &[&str]| tokens.iter().any(|t| wanted.contains(t));

    if has(&["email", "mail", "e_mail"]) || name.contains("email") || name.contains("邮箱") {
        return Some(PiiProvider::Email);
    }
    if has(&["phone", "mobile", "tel", "telephone", "msisdn"])
        || name.contains("phone")
        || name.contains("手机")
        || name.contains("电话")
    {
        return Some(PiiProvider::Phone);
    }
    if has(&["idcard", "id_card", "ssn", "idno", "id_no"])
        || name.contains("id_card")
        || name.contains("身份证")
    {
        return Some(PiiProvider::IdCard);
    }
    if has(&[
        "name",
        "realname",
        "real_name",
        "fullname",
        "full_name",
        "nickname",
        "nick",
    ]) || name.contains("姓名")
        || name.contains("昵称")
    {
        return Some(PiiProvider::Name);
    }
    None
}

/// Content-based vote over the sampled values.
fn provider_from_content(samples: &[Value]) -> Option<PiiProvider> {
    let strings: Vec<&str> = samples.iter().filter_map(|value| value.as_str()).collect();
    if strings.is_empty() {
        return None;
    }
    let share = |predicate: fn(&str) -> bool| -> f64 {
        strings.iter().filter(|s| predicate(s)).count() as f64 / strings.len() as f64
    };
    if share(is_email) >= 0.8 {
        return Some(PiiProvider::Email);
    }
    if share(|s| PiiProvider::Phone.matches_format(s)) >= 0.8 {
        return Some(PiiProvider::Phone);
    }
    if share(|s| PiiProvider::IdCard.matches_format(s)) >= 0.8 {
        return Some(PiiProvider::IdCard);
    }
    None
}

/// Name pattern and content vote together. Name wins when both fire (a column
/// called `email` holding odd data is still an email column). `data_type` is
/// accepted for future type-aware rules; string columns are the ones scored.
pub fn detect(column: &str, samples: &[Value]) -> Option<PiiProvider> {
    let by_name = provider_from_name(column);
    let by_content = provider_from_content(samples);

    match (by_name, by_content) {
        (Some(provider), _) => Some(provider),
        (None, Some(provider)) => Some(provider),
        (None, None) => None,
    }
}

/// Produce one format-valid fake value, redrawing up to a bounded number of
/// times so the emitted value always satisfies [`PiiProvider::matches_format`].
pub fn generate_value(provider: PiiProvider, rng: &mut StdRng) -> String {
    for _ in 0..64 {
        let candidate = match provider {
            PiiProvider::Email => SafeEmail().fake_with_rng::<String, _>(rng),
            PiiProvider::Name => Name().fake_with_rng::<String, _>(rng),
            PiiProvider::Phone => format!(
                "+1-{:03}-{:03}-{:04}",
                rng.gen_range(200..1000),
                rng.gen_range(200..1000),
                rng.gen_range(0..10_000)
            ),
            PiiProvider::IdCard => {
                let mut out = String::with_capacity(18);
                for _ in 0..17 {
                    out.push(char::from(b'0' + rng.gen_range(0..10)));
                }
                out.push(char::from(b'0' + rng.gen_range(0..10)));
                out
            }
        };
        if provider.matches_format(&candidate) {
            return candidate;
        }
    }
    // The templates above always match; this is a last-resort guard.
    match provider {
        PiiProvider::Email => "anon@example.com".to_string(),
        PiiProvider::Name => "Anonymous".to_string(),
        PiiProvider::Phone => "+1-000-000-0000".to_string(),
        PiiProvider::IdCard => "000000000000000000".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    fn strings(values: &[&str]) -> Vec<Value> {
        values.iter().map(|v| Value::from(*v)).collect()
    }

    #[test]
    fn should_detect_email_by_name_and_content() {
        assert_eq!(provider_from_name("email"), Some(PiiProvider::Email));
        assert_eq!(provider_from_name("user_mail"), Some(PiiProvider::Email));
        assert_eq!(
            provider_from_content(&strings(&["a@b.com", "c@d.org"])),
            Some(PiiProvider::Email)
        );
        assert_eq!(
            detect("contact", &strings(&["a@b.com", "c@d.org"])),
            Some(PiiProvider::Email)
        );
    }

    #[test]
    fn should_prefer_the_name_vote_over_content() {
        // A column named `phone` holding junk is still treated as a phone.
        assert_eq!(
            detect("phone", &strings(&["not-a-phone"])),
            Some(PiiProvider::Phone)
        );
    }

    #[test]
    fn should_not_flag_ordinary_columns() {
        assert_eq!(provider_from_name("status"), None);
        assert_eq!(provider_from_name("amount"), None);
        assert_eq!(provider_from_content(&strings(&["open", "closed"])), None);
        assert_eq!(detect("note", &strings(&["hello", "world"])), None);
    }

    #[test]
    fn should_generate_values_matching_every_provider_format() {
        let mut rng = StdRng::seed_from_u64(7);
        for provider in [
            PiiProvider::Email,
            PiiProvider::Phone,
            PiiProvider::Name,
            PiiProvider::IdCard,
        ] {
            for _ in 0..50 {
                let value = generate_value(provider, &mut rng);
                assert!(
                    provider.matches_format(&value),
                    "{:?} produced invalid value {value:?}",
                    provider
                );
            }
        }
    }

    #[test]
    fn should_generate_deterministically_for_the_same_seed() {
        let mut left = StdRng::seed_from_u64(42);
        let mut right = StdRng::seed_from_u64(42);
        for provider in [PiiProvider::Email, PiiProvider::Name, PiiProvider::Phone] {
            assert_eq!(
                generate_value(provider, &mut left),
                generate_value(provider, &mut right)
            );
        }
    }

    #[test]
    fn should_validate_phone_and_id_card_shapes() {
        assert!(PiiProvider::Phone.matches_format("+1-800-555-0199"));
        assert!(!PiiProvider::Phone.matches_format("12"));
        assert!(PiiProvider::IdCard.matches_format("11010119900307123X"));
        assert!(!PiiProvider::IdCard.matches_format("1101011990030712"));
    }
    #[test]
    fn should_erase_observed_values_when_anonymizing_a_model_column() {
        use crate::synth::marginal::CategoricalParams;

        let mut column = ColumnModel {
            logical_type: crate::synth::model::LogicalType::Categorical,
            marginal: crate::synth::marginal::Marginal::Categorical(CategoricalParams {
                values: vec!["alice@corp.com".to_string(), "bob@corp.com".to_string()],
                weights: vec![0.5, 0.5],
            }),
            ..Default::default()
        };
        anonymize_model_column(&mut column, PiiProvider::Email);

        assert_eq!(column.pii, Some(PiiProvider::Email));
        match &column.marginal {
            crate::synth::marginal::Marginal::Categorical(params) => {
                assert_eq!(
                    params.values,
                    vec!["__pii_level_0".to_string(), "__pii_level_1".to_string()]
                );
            }
            other => panic!("categorical dictionary expected, got {other:?}"),
        }
    }
}
