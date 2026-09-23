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
use std::collections::HashMap;

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

/// Region style a phone column should imitate (issue #95).
///
/// Inference is a **style hint only**: the 3-digit mobile prefix is kept
/// because the issue explicitly trades that bit of structure for locality.
/// No observed value ever survives inference; the random tail is drawn fresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "style", rename_all = "snake_case")]
pub enum PhoneStyle {
    /// Legacy behavior: `+1-XXX-XXX-XXXX`.
    UsDefault,
    /// Mainland mobile: `+86-1{prefix}-{8 digits}` with the trained prefix.
    CnMobile {
        /// Observed `1XY` prefix digits, e.g. `[1, 3, 8]` for `138...`.
        prefix: [u8; 3],
    },
}

/// Reject a `CnMobile` prefix that no mainland mobile can have. Corrupted or
/// hand-edited models must fail loading instead of silently emitting numbers
/// outside the trained locale.
pub fn validate_phone_style(style: &PhoneStyle) -> Result<(), String> {
    match style {
        PhoneStyle::UsDefault => Ok(()),
        PhoneStyle::CnMobile { prefix } => {
            if prefix[0] != 1 || !(3..=9).contains(&prefix[1]) || prefix[2] > 9 {
                return Err(format!(
                    "pii_phone_style cn_mobile prefix {:?} is not a mainland mobile prefix \
                     (expected [1, 3-9, 0-9])",
                    prefix
                ));
            }
            Ok(())
        }
    }
}

/// Digit characters plus the separators the phone format check accepts.
fn is_phone_char(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, '+' | '-' | ' ' | '(' | ')')
}

/// Strip separators, then drop a leading country code: `+86` / `86` for CN.
/// A `+` with any other country code returns `None`: that sample is not CN
/// evidence and must not become an 11-digit false positive.
fn normalize_cn_candidate(value: &str) -> Option<String> {
    if !value.chars().all(is_phone_char) {
        return None;
    }
    let digits: String = value.chars().filter(char::is_ascii_digit).collect();
    if value.starts_with('+') {
        if let Some(rest) = digits.strip_prefix("86") {
            return Some(rest.to_string());
        }
        return None;
    }
    if let Some(rest) = digits.strip_prefix("86") {
        // Bare `86` + 11 digits could itself be a landline-style run; only
        // accept the strip when what remains is CN-mobile shaped.
        if is_cn_mobile_digits(rest) {
            return Some(rest.to_string());
        }
        return Some(digits);
    }
    Some(digits)
}

/// CN mobile shape: `1[3-9]` followed by 9 more digits (11 total).
fn is_cn_mobile_digits(digits: &str) -> bool {
    let bytes = digits.as_bytes();
    bytes.len() == 11
        && bytes[0] == b'1'
        && (b'3'..=b'9').contains(&bytes[1])
        && bytes.iter().all(u8::is_ascii_digit)
}

/// Infer the region style of an already-detected phone column from its
/// training samples. At least 80% of the samples must normalize to the CN
/// mobile shape for [`PhoneStyle::CnMobile`] to win; the prefix is the mode
/// of the observed first three digits. Anything else stays `UsDefault`.
pub fn infer_phone_style(samples: &[Value]) -> PhoneStyle {
    let strings: Vec<&str> = samples.iter().filter_map(|v| v.as_str()).collect();
    if strings.is_empty() {
        return PhoneStyle::UsDefault;
    }
    let mut prefixes: HashMap<[u8; 3], usize> = HashMap::new();
    let mut cn_votes = 0usize;
    for sample in &strings {
        let Some(digits) = normalize_cn_candidate(sample) else {
            continue;
        };
        if !is_cn_mobile_digits(&digits) {
            continue;
        }
        cn_votes += 1;
        let prefix = [
            digits.as_bytes()[0] - b'0',
            digits.as_bytes()[1] - b'0',
            digits.as_bytes()[2] - b'0',
        ];
        *prefixes.entry(prefix).or_insert(0) += 1;
    }
    if cn_votes * 5 >= strings.len() * 4 {
        let (prefix, _) = prefixes
            .iter()
            .max_by_key(|(digits, count)| (**count, std::cmp::Reverse(**digits)))
            .expect("cn_votes > 0 implies a recorded prefix");
        return PhoneStyle::CnMobile { prefix: *prefix };
    }
    PhoneStyle::UsDefault
}

/// [`infer_phone_style`] plus the `eprintln`-ready fallback notice (issue
/// #95): a phone column that stays `UsDefault` despite being detected as PII
/// phone explains itself once, so a `138...`-style training set that fails
/// the 80% vote is not silently re-rendered as `+1-...`.
///
/// `qualified_column` is `"table.column"`; the caller deduplicates emission.
pub fn infer_phone_style_with_warning(
    samples: &[Value],
    qualified_column: &str,
) -> (PhoneStyle, Option<String>) {
    let style = infer_phone_style(samples);
    match style {
        PhoneStyle::CnMobile { .. } => (style, None),
        PhoneStyle::UsDefault => {
            let (table, column) = match qualified_column.split_once('.') {
                Some((table, column)) => (table, column),
                None => ("?", qualified_column),
            };
            (
                style,
                Some(format!(
                    "warning: table '{table}': column '{column}' has phone values that do not \
                     look like mainland-CN mobiles; generating +1-XXX-XXX-XXXX (US format)"
                )),
            )
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
const WHOLE_NAME_TOKENS: [&str; 12] = [
    "name",
    "nickname",
    "fullname",
    "realname",
    "firstname",
    "lastname",
    "givenname",
    "familyname",
    "personname",
    "displayname",
    "contactname",
    "nick",
];

/// Column-name pattern, matched on whole tokens (never as substrings) with the
/// type word as the last token.
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
    // `mail` only counts as the whole name (`mail`) or inside `mail_address`;
    // `junk_mail` / `voice_mail` are not address columns.
    let email_tail = last == "email"
        || (tokens.len() == 1 && last == "mail")
        || matches!(
            last,
            "emailaddress" | "emailaddr" | "mailaddress" | "mailaddr"
        )
        || ((has("email") || has("mail")) && matches!(last, "address" | "addr"));
    if email_tail {
        return Some(PiiProvider::Email);
    }

    // Phone: the type word must be the last token (`phone_number` qualifies).
    // `cell` alone is a spreadsheet cell / fuel cell, so it needs a qualifier.
    if matches!(
        last,
        "phone" | "mobile" | "tel" | "telephone" | "msisdn" | "cellphone"
    ) || ((has("phone") || has("mobile") || has("cell"))
        && matches!(last, "number" | "no" | "num"))
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

/// Name-only PII vote for callers without sampled values (the rules draft,
/// issue #89 S4②). Same `provider_from_name` heuristics `detect` uses, so a
/// column the draft suggests is one `train` would anonymize anyway.
pub fn detect_from_name(column: &str) -> Option<PiiProvider> {
    provider_from_name(column)
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
    generate_value_with_style(provider, PhoneStyle::UsDefault, rng)
}

/// [`generate_value`] with an explicit region style (issue #95).
///
/// [`PhoneStyle::CnMobile`] renders `+86-1{prefix}-{8 random digits}` so the
/// generated column matches the trained locale; the tail is drawn fresh and
/// never reproduces a trained value (the prefix is the only retained hint).
pub fn generate_value_with_style(
    provider: PiiProvider,
    style: PhoneStyle,
    rng: &mut StdRng,
) -> String {
    for _ in 0..64 {
        let candidate = match (provider, style) {
            (PiiProvider::Email, _) => SafeEmail().fake_with_rng::<String, _>(rng),
            (PiiProvider::Name, _) => Name().fake_with_rng::<String, _>(rng),
            (PiiProvider::Phone, PhoneStyle::CnMobile { prefix }) => format!(
                "+86-{}{}{}-{:08}",
                prefix[0],
                prefix[1],
                prefix[2],
                rng.gen_range(0..100_000_000u32)
            ),
            (PiiProvider::Phone, PhoneStyle::UsDefault) => format!(
                "+1-{:03}-{:03}-{:04}",
                rng.gen_range(200..1000),
                rng.gen_range(200..1000),
                rng.gen_range(0..10_000)
            ),
            (PiiProvider::IdCard, _) => {
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
    match (provider, style) {
        (PiiProvider::Email, _) => "anon@example.com".to_string(),
        (PiiProvider::Name, _) => "Anonymous".to_string(),
        (PiiProvider::Phone, PhoneStyle::CnMobile { prefix }) => {
            format!("+86-{}{}{}-00000000", prefix[0], prefix[1], prefix[2])
        }
        (PiiProvider::Phone, PhoneStyle::UsDefault) => "+1-000-000-0000".to_string(),
        (PiiProvider::IdCard, _) => "000000000000000000".to_string(),
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
            "fuel_cell",
            "cell",
            "junk_mail",
            "direct_mail",
            "voice_mail",
            "user_mail",
            "username",
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
    fn should_infer_cn_mobile_style_from_samples() {
        // Bare 11-digit mobiles, +86-prefixed and delimited forms all vote CN;
        // the prefix is the mode of the observed first three digits.
        let mixed = strings(&[
            "13812345678",
            "+8613987654321",
            "186-1234-5678",
            "138 0000 0000",
        ]);
        assert_eq!(
            infer_phone_style(&mixed),
            PhoneStyle::CnMobile { prefix: [1, 3, 8] }
        );

        // A pure +86 sample keeps its own prefix mode.
        let plus86 = strings(&["+86-159-1234-5678", "+86-159-8888-6666"]);
        assert_eq!(
            infer_phone_style(&plus86),
            PhoneStyle::CnMobile { prefix: [1, 5, 9] }
        );
    }

    #[test]
    fn should_infer_us_style_from_american_samples() {
        assert_eq!(
            infer_phone_style(&strings(&[
                "+1-800-555-0199",
                "+1-800-555-0100",
                "+1-212-664-7665"
            ])),
            PhoneStyle::UsDefault
        );
    }

    #[test]
    fn should_not_count_non_86_prefixed_samples_as_cn_evidence() {
        // A +44 sample is not CN evidence. 3 CN votes in 4 samples (75%) stay
        // below the 80% line, so the column keeps the US default.
        let diluted = strings(&[
            "13812345678",
            "13987654321",
            "15012345678",
            "+44-20-7183-8750",
        ]);
        assert_eq!(infer_phone_style(&diluted), PhoneStyle::UsDefault);

        // The same +44 sample inside a 5-sample column (80%) does not flip it.
        let at_threshold = strings(&[
            "13812345678",
            "13987654321",
            "15012345678",
            "15887654321",
            "+44-20-7183-8750",
        ]);
        assert_eq!(
            infer_phone_style(&at_threshold),
            PhoneStyle::CnMobile { prefix: [1, 3, 8] }
        );
    }

    #[test]
    fn should_fall_back_to_us_when_samples_are_not_cn_mobiles() {
        // Short/odd digit runs never look like CN mobiles.
        assert_eq!(
            infer_phone_style(&strings(&["12345", "00-8000"])),
            PhoneStyle::UsDefault
        );
        assert_eq!(infer_phone_style(&[]), PhoneStyle::UsDefault);
    }

    /// CN-local 11-digit shape (country code stripped) used to compare
    /// generated values against trained values and to assert the `1`+prefix
    /// structure.
    fn cn_local_digits(value: &str) -> String {
        normalize_cn_candidate(value).expect("phone-shaped test value")
    }

    #[test]
    fn should_generate_cn_format_preserving_prefix() {
        let style = PhoneStyle::CnMobile { prefix: [1, 3, 8] };
        let mut rng = StdRng::seed_from_u64(11);
        let trained = [
            "13812345678".to_string(),
            "+86-138-0000-0000".to_string(),
            "138 8765 4321".to_string(),
        ];
        let trained_locals: std::collections::HashSet<String> =
            trained.iter().map(|v| cn_local_digits(v)).collect();
        for _ in 0..50 {
            let value = generate_value_with_style(PiiProvider::Phone, style, &mut rng);
            assert!(
                PiiProvider::Phone.matches_format(&value),
                "generated value {value:?} fails the phone format check"
            );
            assert!(
                value.starts_with("+86-138-"),
                "expected +86-138- prefix, got {value:?}"
            );
            let local = cn_local_digits(&value);
            assert_eq!(local.len(), 11, "expected 11 local digits, got {value:?}");
            assert_eq!(
                &local[1..3],
                "38",
                "prefix digits after the leading 1 must be 38"
            );
            assert!(
                !trained_locals.contains(&local),
                "generated value {value:?} reproduces a trained value"
            );
        }
    }

    #[test]
    fn should_generate_deterministically_with_style_for_the_same_seed() {
        let style = PhoneStyle::CnMobile { prefix: [1, 8, 6] };
        let mut left = StdRng::seed_from_u64(42);
        let mut right = StdRng::seed_from_u64(42);
        assert_eq!(
            generate_value_with_style(PiiProvider::Phone, style, &mut left),
            generate_value_with_style(PiiProvider::Phone, style, &mut right)
        );
    }

    #[test]
    fn should_report_us_fallback_when_cn_is_not_recognized() {
        // Inference below the 80% line must tell the user why the output
        // stays in the US format.
        let (style, warn) = infer_phone_style_with_warning(
            &strings(&["13812345678", "+1-800-555-0199"]),
            "users.phone",
        );
        assert_eq!(style, PhoneStyle::UsDefault);
        assert_eq!(
            warn.unwrap(),
            "warning: table 'users': column 'phone' has phone values that do not look like \
             mainland-CN mobiles; generating +1-XXX-XXX-XXXX (US format)"
        );
    }

    #[test]
    fn should_not_warn_when_cn_style_is_inferred() {
        let (style, warn) =
            infer_phone_style_with_warning(&strings(&["13812345678", "13987654321"]), "u.p");
        assert_eq!(style, PhoneStyle::CnMobile { prefix: [1, 3, 8] });
        assert!(warn.is_none());
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
