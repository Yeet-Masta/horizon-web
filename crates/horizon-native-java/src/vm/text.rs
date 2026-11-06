//! How Java prints numbers: `Double.toString` and `Float.toString`.

/// `Double.toString`: the shortest decimal that rounds to the value (at least
/// two significant digits, JDK 19+), laid out plainly for magnitudes in
/// [10^-3, 10^7) and in computerized scientific notation otherwise.
pub fn java_double_to_string(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    let shortest = format!("{:e}", v.abs());
    let two = format!("{:.1e}", v.abs());
    layout(
        v.is_sign_negative(),
        &shortest,
        &two,
        v.abs() >= 1e-3 && v.abs() < 1e7,
    )
}

/// `Float.toString`, the same rules over `float`'s precision.
pub fn java_float_to_string(v: f32) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    let shortest = format!("{:e}", v.abs());
    let two = format!("{:.1e}", v.abs());
    layout(
        v.is_sign_negative(),
        &shortest,
        &two,
        v.abs() >= 1e-3 && v.abs() < 1e7,
    )
}

/// Lays out Rust's shortest `d.ddde±n` digits the way Java prints them.
fn layout(negative: bool, shortest: &str, two_digits: &str, plain: bool) -> String {
    let (mantissa, exponent) = shortest.split_once('e').unwrap_or((shortest, "0"));
    let mut digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let mut exponent: i32 = exponent.parse().unwrap_or(0);
    if digits.len() == 1 {
        // One significant digit: Java prints the closest two-digit decimal.
        let (mantissa, e) = two_digits.split_once('e').unwrap_or((two_digits, "0"));
        digits = mantissa.chars().filter(char::is_ascii_digit).collect();
        exponent = e.parse().unwrap_or(0);
    }
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if plain {
        if exponent >= 0 {
            let point = exponent as usize + 1;
            if digits.len() <= point {
                out.push_str(digits);
                out.extend(std::iter::repeat_n('0', point - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..point]);
                out.push('.');
                out.push_str(&digits[point..]);
            }
        } else {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', (-exponent - 1) as usize));
            out.push_str(digits);
        }
    } else {
        out.push_str(&digits[..1]);
        out.push('.');
        if digits.len() > 1 {
            out.push_str(&digits[1..]);
        } else {
            out.push('0');
        }
        out.push('E');
        out.push_str(&exponent.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{java_double_to_string, java_float_to_string};

    #[test]
    fn doubles_print_as_java_does() {
        // Expected strings are what HotSpot 25 prints for the same values.
        for (value, java) in [
            (1.0, "1.0"),
            (0.1, "0.1"),
            (100.0, "100.0"),
            (1e7, "1.0E7"),
            (9999999.0, "9999999.0"),
            (0.001, "0.001"),
            (0.0001, "1.0E-4"),
            (1e23, "1.0E23"),
            (f64::MIN_POSITIVE * f64::EPSILON, "4.9E-324"),
            (f64::MAX, "1.7976931348623157E308"),
            (-2.5, "-2.5"),
            (1.0 / 3.0, "0.3333333333333333"),
            (123456.789, "123456.789"),
        ] {
            assert_eq!(java_double_to_string(value), java, "{value:e}");
        }
    }

    #[test]
    fn floats_print_as_java_does() {
        for (value, java) in [
            (1.0f32, "1.0"),
            (0.1f32, "0.1"),
            (1.0f32 / 3.0, "0.33333334"),
            (1e10f32, "1.0E10"),
            (f32::MIN_POSITIVE, "1.1754944E-38"),
            (3.4028235e38f32, "3.4028235E38"),
        ] {
            assert_eq!(java_float_to_string(value), java, "{value:e}");
        }
    }
}
