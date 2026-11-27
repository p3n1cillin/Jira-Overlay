//! CPython-compatible arithmetic. Predictions must match the Python build
//! bit-for-bit, so every place Python's semantics differ from the obvious
//! Rust expression is reproduced here.

/// `sum(floats)` as CPython 3.12+ computes it: Neumaier-compensated
/// summation in iteration order, with the compensation added at the end
/// when finite.
pub fn py_sum(values: impl IntoIterator<Item = f64>) -> f64 {
    let mut total = 0.0_f64;
    let mut comp = 0.0_f64;
    for x in values {
        let t = total + x;
        if total.abs() >= x.abs() {
            comp += (total - t) + x;
        } else {
            comp += (x - t) + total;
        }
        total = t;
    }
    if comp != 0.0 && comp.is_finite() { total + comp } else { total }
}

/// `int(round(x))`: round half to even.
pub fn py_round_int(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// `round(x, ndigits)` for `ndigits >= 0`: the decimal nearest the exact
/// binary value, ties to even, as CPython's dtoa-based rounding does.
pub fn py_round(x: f64, ndigits: usize) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    // Rust prints the exact decimal expansion when asked for enough digits;
    // every finite f64 terminates within 1074 fractional digits.
    let exact = format!("{:.1100}", x.abs());
    let (int_part, frac) = exact.split_once('.').expect("fixed-point output has a decimal point");
    let kept = &frac[..ndigits];
    let rest = &frac[ndigits..];
    let mut digits: Vec<u8> = int_part.bytes().chain(kept.bytes()).map(|b| b - b'0').collect();
    let first = rest.as_bytes()[0] - b'0';
    let beyond_half = rest.bytes().skip(1).any(|b| b != b'0');
    let round_up = match first {
        0..=4 => false,
        6..=9 => true,
        _ => beyond_half || digits.last().is_some_and(|d| d % 2 == 1),
    };
    if round_up {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, 1);
                break;
            }
            i -= 1;
            if digits[i] == 9 {
                digits[i] = 0;
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    let split = digits.len() - ndigits;
    let text: String = digits[..split]
        .iter()
        .map(|d| char::from(b'0' + d))
        .chain(std::iter::once('.'))
        .chain(digits[split..].iter().map(|d| char::from(b'0' + d)))
        .collect();
    let value: f64 = text.parse().expect("digits parse as f64");
    if x < 0.0 { -value } else { value }
}

/// First key with the maximum value, as Python's `max(d, key=d.get)`.
pub fn first_max<'a, K, V: PartialOrd + Copy + 'a>(items: impl IntoIterator<Item = (&'a K, &'a V)>) -> Option<&'a K>
where
    K: 'a,
{
    let mut best: Option<(&K, V)> = None;
    for (k, v) in items {
        match best {
            Some((_, b)) if v.partial_cmp(&b) != Some(std::cmp::Ordering::Greater) => {}
            _ => best = Some((k, *v)),
        }
    }
    best.map(|(k, _)| k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neumaier_matches_cpython_examples() {
        // sum([0.1]*10) == 1.0 in CPython >= 3.12 (was 0.9999999999999999).
        assert_eq!(py_sum([0.1; 10]), 1.0);
        assert_eq!(py_sum([1e100, 1.0, -1e100, 1.0]), 2.0);
        assert_eq!(py_sum(std::iter::empty()), 0.0);
    }

    #[test]
    fn rounding_matches_python() {
        assert_eq!(py_round_int(0.5), 0);
        assert_eq!(py_round_int(1.5), 2);
        assert_eq!(py_round_int(92.5), 92);
        assert_eq!(py_round_int(12.5), 12);
        // Exact binary ties round to even; near-ties follow the true value.
        assert_eq!(py_round(0.125, 2), 0.12);
        assert_eq!(py_round(0.375, 2), 0.38);
        assert_eq!(py_round(2.675, 2), 2.67);
        assert_eq!(py_round(0.0625, 3), 0.062);
        assert_eq!(py_round(0.8765, 3), 0.876);
        assert_eq!(py_round(0.9995, 3), 1.0);
        assert_eq!(py_round(0.99951, 3), 1.0);
        assert_eq!(py_round(-0.125, 2), -0.12);
        assert_eq!(py_round(1.0 / 3.0, 4), 0.3333);
        assert_eq!(py_round(99.95, 1), 100.0);
    }

    #[test]
    fn first_max_keeps_the_first_tie() {
        let items = [("a", 1.0), ("b", 3.0), ("c", 3.0)];
        let refs: Vec<(&&str, &f64)> = items.iter().map(|(k, v)| (k, v)).collect();
        assert_eq!(first_max(refs), Some(&"b"));
    }
}
