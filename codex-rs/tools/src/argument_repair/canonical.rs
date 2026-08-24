use super::candidate::AtomicCandidate;
use serde_json::Map as JsonMap;
use serde_json::Value as JsonValue;
use std::cmp::Ordering;
use std::collections::BTreeMap;

const MAX_CANONICAL_NUMBER_BYTES: usize = 64 * 1024;

pub(super) fn value_key(value: &JsonValue) -> String {
    canonicalize(value).to_string()
}

fn canonicalize(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::Array(values) => JsonValue::Array(values.iter().map(canonicalize).collect()),
        JsonValue::Object(values) => {
            let mut names = values.keys().collect::<Vec<_>>();
            names.sort_unstable();
            let mut canonical = JsonMap::new();
            for name in names {
                if let Some(value) = values.get(name) {
                    canonical.insert(name.to_string(), canonicalize(value));
                }
            }
            JsonValue::Object(canonical)
        }
        JsonValue::Number(number) => canonical_number(number).unwrap_or_else(|| value.clone()),
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::String(_) => value.clone(),
    }
}

fn canonical_number(number: &serde_json::Number) -> Option<JsonValue> {
    let (negative, digits, exponent) = decimal_signature(&number.to_string())?;
    let body_len = if exponent >= 0 {
        digits.len().checked_add(usize::try_from(exponent).ok()?)?
    } else {
        let decimal_places = usize::try_from(exponent.checked_neg()?).ok()?;
        if decimal_places < digits.len() {
            digits.len().checked_add(1)?
        } else {
            2usize
                .checked_add(decimal_places.checked_sub(digits.len())?)?
                .checked_add(digits.len())?
        }
    };
    let output_len = body_len.checked_add(usize::from(negative))?;
    if output_len > MAX_CANONICAL_NUMBER_BYTES {
        return None;
    }
    let mut text = String::with_capacity(output_len);
    if negative {
        text.push('-');
    }
    if exponent >= 0 {
        text.push_str(&digits);
        let zeros = usize::try_from(exponent).ok()?;
        text.extend(std::iter::repeat_n('0', zeros));
    } else {
        let decimal_places = usize::try_from(exponent.checked_neg()?).ok()?;
        if decimal_places < digits.len() {
            let split = digits.len() - decimal_places;
            text.push_str(&digits[..split]);
            text.push('.');
            text.push_str(&digits[split..]);
        } else {
            text.push_str("0.");
            text.extend(std::iter::repeat_n('0', decimal_places - digits.len()));
            text.push_str(&digits);
        }
    }
    serde_json::from_str(&text).ok()
}

fn decimal_signature(value: &str) -> Option<(bool, String, i64)> {
    let (negative, value) = value
        .strip_prefix('-')
        .map_or((false, value), |value| (true, value));
    let exponent_index = value.find('e').or_else(|| value.find('E'));
    let (mantissa, exponent) = match exponent_index {
        Some(index) => (&value[..index], value[index + 1..].parse::<i64>().ok()?),
        None => (value, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{whole}{fraction}");
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Some((false, "0".to_string(), 0));
    }
    let trailing_zeros = digits
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'0')
        .count();
    let coefficient = &digits[..digits.len() - trailing_zeros];
    let exponent = exponent
        .checked_sub(i64::try_from(fraction.len()).ok()?)?
        .checked_add(i64::try_from(trailing_zeros).ok()?)?;
    Some((negative, coefficient.to_string(), exponent))
}

pub(super) fn deduplicate(candidates: Vec<AtomicCandidate>) -> Vec<AtomicCandidate> {
    let mut candidates_by_value = BTreeMap::<String, AtomicCandidate>::new();
    for candidate in candidates {
        let value_key = value_key(&candidate.value);
        match candidates_by_value.get(&value_key) {
            Some(existing) if compare_candidates(existing, &candidate) != Ordering::Greater => {}
            Some(_) | None => {
                candidates_by_value.insert(value_key, candidate);
            }
        }
    }
    let mut candidates = candidates_by_value.into_values().collect::<Vec<_>>();
    candidates.sort_by(compare_candidates);
    candidates
}

fn compare_candidates(left: &AtomicCandidate, right: &AtomicCandidate) -> Ordering {
    left.step
        .cmp(&right.step)
        .then_with(|| value_key(&left.value).cmp(&value_key(&right.value)))
}
