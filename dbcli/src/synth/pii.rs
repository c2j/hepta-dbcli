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

fn name_tokens(name: &str) -> Vec<&str> {
    name.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect()
}

/// Whole names that collapse camelCase into one token, plus the multi-word
/// forms whose *last* token is the type word.
const PERSONAL_NAME_PREFIXES: [&str; 11] = [
    "full", "real", "first", "last", "given", "family", "person", "user", "display", "contact",
    "nick",
];

/// Single-token names (including camelCase collapses) that are personal names.
const WHOLE_NAME_TOKENS: [&str; 13] = [
    "name",
    "nickname",
    "fullname",
    "realname",
    "firstname",
    "lastname",
    "givenname",
    "familyname",
    "personname",
    "username",
    "displayname",
    "contactname",
    "nick",
];

/// Column-name pattern. Matching is anchored on whole tokens: `last == "email"`
/// or `[email|mail] + [address|addr]`. Substring matching is deliberately not
/// used, so `is_email_verified`, `microphone`, `mail_id`, `file_name`,
/// `table_name` and `country_name` stay untouched.
fn provider_from_name(column: &str) -> Option<PiiProvider> {
    let name = column.to_ascii_lowercase();
    let tokens = name_tokens(&name);
    let (Some(first), Some(last)) = (tokens.first().copied(), tokens.last().copied()) else {
        return None;
    };
    let has = |wanted: &str| tokens.contains(&wanted);

    // Chinese column names have no token boundaries.
    if name.contains("邮箱") || name.contains("电子邮件") {
        return Some(PiiProvider::Email);
    }
    if name.contains("手机") || name.contains("电话") {
        return Some(PiiProvider::Phone);
    }
    if name.contains("身份证") {
        return Some(PiiProvider::IdCard);
    }
    if name.contains("姓名") || name.contains("昵称") {
        return Some(PiiProvider::Name);
    }

    // Email: `email` / `mail` as the last token, `email_address`, or the
    // camelCase collapse.
    let email_tail = matches!(
        last,
        "email" | "mail" | "emailaddress" | "emailaddr" | "mailaddress" | "mailaddr"
    ) || ((has("email") || has("mail")) && matches!(last, "address" | "addr"));
    if email_tail {
        return Some(PiiProvider::Email);
    }

    // Phone: the type word must be the last token (`phone_number` qualifies).
    if matches!(
        last,
        "phone" | "mobile" | "tel" | "telephone" | "msisdn" | "cell" | "cellphone"
    ) || ((has("phone") || has("mobile")) && matches!(last, "number" | "no" | "num"))
    {
        return Some(PiiProvider::Phone);
    }

    // Id card: explicit forms only; a bare `id` is a primary key, not PII.
    if matches!(
        last,
        "idcard" | "idno" | "ssn" | "nationalid" | "identitycard"
    ) || ((first == "id" || first == "identity") && matches!(last, "card" | "no" | "number"))
    {
        return Some(PiiProvider::IdCard);
    }

    // Personal names: the type word is `name`/`nickname` and the qualifier is a
    // known personal prefix, so `file_name` / `table_name` are not matched.
    if last == "name" && PERSONAL_NAME_PREFIXES.contains(&first) {
        return Some(PiiProvider::Name);
    }
    if tokens.len() == 1 && WHOLE_NAME_TOKENS.contains(&last) {
        return Some(PiiProvider::Name);
    }
    None
}

/// Text-like SQL types. Any other declared type (numeric, boolean, datetime)
/// is never anonymized: replacing it with a string fake would break the load.
fn is_text_sql_type(data_type: &str) -> bool {
    let base = data_type
        .split('(')
        .next()
        .unwrap_or(data_type)
        .trim()
        .to_ascii_lowercase();
    matches!(
        base.as_str(),
        "char"
            | "varchar"
            | "nvarchar"
            | "varchar2"
            | "nvarchar2"
            | "nchar"
            | "character"
            | "character varying"
            | "text"
            | "tinytext"
            | "mediumtext"
            | "longtext"
            | "clob"
            | "citext"
            | "enum"
            | "set"
            | "json"
    )
}

/// A phone-looking *content* vote is deliberately stricter than the generator
/// format check: a bare digit run (order numbers, ids) is not a phone.
fn looks_like_phone(value: &str) -> bool {
    if !PiiProvider::Phone.matches_format(value) {
        return false;
    }
    value.starts_with('+') || value.contains([' ', '-', '(', ')'])
}

/// Content-based vote over the sampled values. Only email and `+`-prefixed
/// phones are detected from content: a digit column of 18-char codes or 10-digit
/// order numbers must not become an anonymized id card.
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
    if share(looks_like_phone) >= 0.8 {
        return Some(PiiProvider::Phone);
    }
    None
}

/// Name pattern and content vote together. The name wins when both fire (a
/// column called `email` holding odd data is still an email column).
///
/// `data_type` gates by the declared SQL type: a non-text column is never
/// anonymized. When it is `None` (older callers, unit tests) the sample values
/// must be strings instead.
pub fn detect(column: &str, samples: &[Value], data_type: Option<&str>) -> Option<PiiProvider> {
    if let Some(data_type) = data_type {
        if !is_text_sql_type(data_type) {
            return None;
        }
    } else if !samples.iter().any(|value| value.is_string()) {
        return None;
    }

    match (provider_from_name(column), provider_from_content(samples)) {
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
        assert_eq!(provider_from_name("user_email"), Some(PiiProvider::Email));
        assert_eq!(
            provider_from_name("email_address"),
            Some(PiiProvider::Email)
        );
        assert_eq!(provider_from_name("emailAddress"), Some(PiiProvider::Email));
        assert_eq!(
            provider_from_content(&strings(&["a@b.com", "c@d.org"])),
            Some(PiiProvider::Email)
        );
        assert_eq!(
            detect("contact", &strings(&["a@b.com", "c@d.org"]), None),
            Some(PiiProvider::Email)
        );
    }

    #[test]
    fn should_prefer_the_name_vote_over_content() {
        // A column named `phone` holding junk is still treated as a phone.
        assert_eq!(
            detect("phone", &strings(&["not-a-phone"]), None),
            Some(PiiProvider::Phone)
        );
    }

    #[test]
    fn should_not_flag_lookalike_column_names() {
        // Substring matches used to flag these; every one is a false positive.
        for column in [
            "is_email_verified",
            "email_sent_at",
            "mail_id",
            "microphone",
            "file_name",
            "table_name",
            "schema_name",
            "country_name",
            "customer_id",
            "status",
            "amount",
        ] {
            assert_eq!(
                provider_from_name(column),
                None,
                "'{column}' must not be a PII column"
            );
        }
    }

    #[test]
    fn should_not_flag_ordinary_columns() {
        assert_eq!(provider_from_content(&strings(&["open", "closed"])), None);
        assert_eq!(detect("note", &strings(&["hello", "world"]), None), None);
    }

    #[test]
    fn should_skip_columns_whose_declared_type_is_not_text() {
        // A numeric `phone` or boolean `is_email_verified` would be filled with
        // a string fake and fail the SQL load.
        assert_eq!(detect("phone", &strings(&["123"]), Some("bigint")), None);
        assert_eq!(detect("email", &strings(&["x"]), Some("int")), None);
        assert_eq!(
            detect("email", &strings(&["a@b.com"]), Some("datetime")),
            None
        );
        assert_eq!(
            detect("email", &strings(&["a@b.com"]), Some("varchar(64)")),
            Some(PiiProvider::Email)
        );
    }

    #[test]
    fn should_not_treat_digit_runs_as_phones() {
        // 10-digit order numbers are not phones.
        assert_eq!(
            provider_from_content(&strings(&["1234567890", "0987654321"])),
            None
        );
        assert_eq!(
            provider_from_content(&strings(&["+1-800-555-0199", "+1-800-555-0100"])),
            Some(PiiProvider::Phone)
        );
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
