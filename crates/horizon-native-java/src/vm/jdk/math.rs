//! `java.math`: `BigInteger` and `BigDecimal` over `num-bigint`, with Java's
//! own rules for what differs (`hashCode`, rounding modes, `BigDecimal`'s
//! `toString` and scale arithmetic, the exceptions and their messages).
use super::{FINAL, M, NativeClass, PUBLIC, S, State, arg, ret_bool, ret_ref, ret_str, state, this, void};
use crate::natives;
use crate::vm::interp::Vm;
use crate::vm::{Data, Exc, ObjId, Res, Value};
use num_bigint::{BigInt, BigUint, Sign};
use num_integer::Integer;
use num_traits::{One, Signed, ToPrimitive, Zero};

pub fn classes() -> &'static [NativeClass] {
    CLASSES
}

/// A `BigInteger` (`scale` 0) or a `BigDecimal` (`unscaled × 10^-scale`).
#[derive(Debug, Clone)]
pub struct Big {
    pub int: BigInt,
    pub scale: i32,
}

/// A `BigInteger`'s sign and magnitude, for `Formatter`.
pub struct BigView {
    pub sign: i32,
    mag: BigUint,
}

impl BigView {
    pub fn magnitude_string(&self, radix: u32) -> String {
        self.mag.to_str_radix(radix)
    }
}

pub fn big_integer_value(vm: &Vm, r: ObjId) -> BigView {
    let b = big(vm, r);
    BigView {
        sign: match b.int.sign() {
            Sign::Minus => -1,
            Sign::NoSign => 0,
            Sign::Plus => 1,
        },
        mag: b.int.magnitude().clone(),
    }
}

pub fn big_decimal_plain(vm: &Vm, r: ObjId) -> String {
    plain_string(&big(vm, r))
}

fn big(vm: &Vm, r: ObjId) -> Big {
    match state(vm, r) {
        Some(State::Big(b)) => (**b).clone(),
        _ => Big { int: BigInt::zero(), scale: 0 },
    }
}

fn this_big(vm: &mut Vm, a: &[Value]) -> Res<Big> {
    let t = this(vm, a)?;
    Ok(big(vm, t))
}

fn arg_big(vm: &mut Vm, a: &[Value], i: usize) -> Res<Big> {
    let r = arg(a, i).r();
    if r == 0 {
        return Err(vm.npe(None));
    }
    Ok(big(vm, r))
}

fn new_big(vm: &mut Vm, class: &str, b: Big) -> Res<ObjId> {
    let c = vm.load(class)?;
    let r = vm.new_instance(c)?;
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Big(Box::new(b))));
    }
    Ok(r)
}

fn int_value(vm: &mut Vm, i: BigInt) -> Res<Value> {
    ret_ref(new_big(vm, "java/math/BigInteger", Big { int: i, scale: 0 })?)
}

fn dec_value(vm: &mut Vm, int: BigInt, scale: i64) -> Res<Value> {
    if scale > i32::MAX as i64 || scale < i32::MIN as i64 {
        return Err(arith(vm, if scale > 0 { "Underflow" } else { "Overflow" }));
    }
    ret_ref(new_big(vm, "java/math/BigDecimal", Big { int, scale: scale as i32 })?)
}

fn arith(vm: &mut Vm, msg: &str) -> Exc {
    vm.throw_new("java/lang/ArithmeticException", Some(msg))
}

fn set_this(vm: &mut Vm, a: &[Value], b: Big) -> Res<Value> {
    let t = this(vm, a)?;
    if let Some(o) = vm.obj_mut(t) {
        o.data = Data::Jdk(Box::new(State::Big(Box::new(b))));
    }
    void()
}

/// `BigInteger.hashCode`: over the big-endian 32-bit words of the magnitude.
fn java_hash(i: &BigInt) -> i32 {
    let mut h: i32 = 0;
    for w in i.magnitude().to_u32_digits().iter().rev() {
        h = h.wrapping_mul(31).wrapping_add(*w as i32);
    }
    h.wrapping_mul(match i.sign() {
        Sign::Minus => -1,
        Sign::NoSign => 0,
        Sign::Plus => 1,
    })
}

fn signum(i: &BigInt) -> i32 {
    match i.sign() {
        Sign::Minus => -1,
        Sign::NoSign => 0,
        Sign::Plus => 1,
    }
}

fn low_i64(i: &BigInt) -> i64 {
    let digits = i.to_u64_digits().1;
    let low = digits.first().copied().unwrap_or(0) as i64;
    if i.sign() == Sign::Minus { low.wrapping_neg() } else { low }
}

fn nfe(vm: &mut Vm, msg: &str) -> Exc {
    vm.throw_new("java/lang/NumberFormatException", Some(msg))
}

fn parse_int(vm: &mut Vm, s: &str, radix: u32) -> Res<BigInt> {
    if !(2..=36).contains(&radix) {
        return Err(nfe(vm, "Radix out of range"));
    }
    if s.is_empty() {
        return Err(nfe(vm, "Zero length BigInteger"));
    }
    let (neg, digits) = match s.as_bytes()[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() || digits.contains(['-', '+']) {
        return Err(nfe(vm, "Zero length BigInteger"));
    }
    match BigUint::parse_bytes(digits.as_bytes(), radix) {
        Some(m) if !digits.contains('_') => {
            let i = BigInt::from_biguint(if m.is_zero() { Sign::NoSign } else if neg { Sign::Minus } else { Sign::Plus }, m);
            Ok(i)
        }
        _ => {
            let msg = format!("For input string: \"{digits}\"{}", if radix != 10 { format!(" under radix {radix}") } else { String::new() });
            Err(nfe(vm, &msg))
        }
    }
}

fn biginteger_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let x = this_big(vm, a)?.int;
    let other = |vm: &mut Vm| -> Res<BigInt> { Ok(arg_big(vm, a, 1)?.int) };
    match op {
        "add" => {
            let y = other(vm)?;
            int_value(vm, x + y)
        }
        "subtract" => {
            let y = other(vm)?;
            int_value(vm, x - y)
        }
        "multiply" => {
            let y = other(vm)?;
            int_value(vm, x * y)
        }
        "divide" | "remainder" | "divideAndRemainder" => {
            let y = other(vm)?;
            if y.is_zero() {
                return Err(arith(vm, "BigInteger divide by zero"));
            }
            let (q, r) = (&x / &y, &x % &y);
            match op {
                "divide" => int_value(vm, q),
                "remainder" => int_value(vm, r),
                _ => {
                    let q = int_value(vm, q)?.r();
                    let r = int_value(vm, r)?.r();
                    ret_ref(vm.new_ref_array("Ljava/math/BigInteger;", &[q, r])?)
                }
            }
        }
        "mod" => {
            let m = other(vm)?;
            if m.sign() != Sign::Plus {
                return Err(arith(vm, "BigInteger: modulus not positive"));
            }
            int_value(vm, x.mod_floor(&m))
        }
        "modPow" => {
            let (e, m) = (arg_big(vm, a, 1)?.int, arg_big(vm, a, 2)?.int);
            if m.sign() != Sign::Plus {
                return Err(arith(vm, "BigInteger: modulus not positive"));
            }
            let base = x.mod_floor(&m);
            let r = if e.sign() == Sign::Minus {
                let Some(inv) = mod_inverse(&base, &m) else { return Err(arith(vm, "BigInteger not invertible.")) };
                inv.modpow(&-e, &m)
            } else {
                base.modpow(&e, &m)
            };
            int_value(vm, r)
        }
        "modInverse" => {
            let m = other(vm)?;
            if m.sign() != Sign::Plus {
                return Err(arith(vm, "BigInteger: modulus not positive"));
            }
            match mod_inverse(&x.mod_floor(&m), &m) {
                Some(i) => int_value(vm, i),
                None => Err(arith(vm, "BigInteger not invertible.")),
            }
        }
        "gcd" => {
            let y = other(vm)?;
            int_value(vm, x.gcd(&y))
        }
        "pow" => {
            let e = arg(a, 1).i();
            if e < 0 {
                return Err(arith(vm, "Negative exponent"));
            }
            int_value(vm, num_traits::pow(x, e as usize))
        }
        "sqrt" => {
            if x.sign() == Sign::Minus {
                return Err(arith(vm, "Negative BigInteger"));
            }
            int_value(vm, x.sqrt())
        }
        "negate" => int_value(vm, -x),
        "abs" => int_value(vm, x.abs()),
        "signum" => Ok(Value::Int(signum(&x))),
        "min" | "max" => {
            let y = other(vm)?;
            let pick_this = if op == "min" { x <= y } else { x >= y };
            if pick_this { ret_ref(arg(a, 0).r()) } else { ret_ref(arg(a, 1).r()) }
        }
        "compareTo" => {
            let y = other(vm)?;
            Ok(Value::Int(x.cmp(&y) as i32))
        }
        "equals" => {
            let o = arg(a, 1).r();
            let same_class = vm.class_of(o).is_some_and(|c| vm.class_name(c) == "java/math/BigInteger");
            ret_bool(same_class && big(vm, o).int == x)
        }
        "hashCode" => Ok(Value::Int(java_hash(&x))),
        "toString" => ret_str(vm, &x.to_str_radix(10)),
        "toStringRadix" => {
            let radix = arg(a, 1).i();
            let radix = if (2..=36).contains(&radix) { radix as u32 } else { 10 };
            ret_str(vm, &x.to_str_radix(radix))
        }
        "intValue" => Ok(Value::Int(low_i64(&x) as i32)),
        "longValue" => Ok(Value::Long(low_i64(&x))),
        "shortValue" => Ok(Value::Int(low_i64(&x) as i16 as i32)),
        "byteValue" => Ok(Value::Int(low_i64(&x) as i8 as i32)),
        "doubleValue" => Ok(Value::Double(x.to_f64().unwrap_or(if x.sign() == Sign::Minus { f64::NEG_INFINITY } else { f64::INFINITY }))),
        "floatValue" => Ok(Value::Float(x.to_f32().unwrap_or(if x.sign() == Sign::Minus { f32::NEG_INFINITY } else { f32::INFINITY }))),
        "intValueExact" | "longValueExact" | "shortValueExact" | "byteValueExact" => {
            let fits = match op {
                "intValueExact" => x.to_i32().map(Value::Int),
                "longValueExact" => x.to_i64().map(Value::Long),
                "shortValueExact" => x.to_i16().map(|v| Value::Int(v as i32)),
                _ => x.to_i8().map(|v| Value::Int(v as i32)),
            };
            match fits {
                Some(v) => Ok(v),
                None => {
                    let kind = &op[..op.len() - "ValueExact".len()];
                    Err(arith(vm, &format!("BigInteger out of {kind} range")))
                }
            }
        }
        "shiftLeft" | "shiftRight" => {
            let n = arg(a, 1).i() as i64;
            let left = (op == "shiftLeft") == (n >= 0);
            let n = n.unsigned_abs() as usize;
            int_value(vm, if left { x << n } else { x >> n })
        }
        "and" | "or" | "xor" | "andNot" => {
            let y = other(vm)?;
            int_value(
                vm,
                match op {
                    "and" => x & y,
                    "or" => x | y,
                    "xor" => x ^ y,
                    _ => x & !y,
                },
            )
        }
        "not" => int_value(vm, !x),
        "testBit" => {
            let n = arg(a, 1).i();
            if n < 0 {
                return Err(arith(vm, "Negative bit address"));
            }
            ret_bool(((x >> n as usize) & BigInt::one()).is_one())
        }
        "setBit" | "clearBit" | "flipBit" => {
            let n = arg(a, 1).i();
            if n < 0 {
                return Err(arith(vm, "Negative bit address"));
            }
            let bit = BigInt::one() << n as usize;
            int_value(
                vm,
                match op {
                    "setBit" => x | bit,
                    "clearBit" => x & !bit,
                    _ => x ^ bit,
                },
            )
        }
        "bitLength" => {
            // Two's complement length, sign bit excluded.
            let n = if x.sign() == Sign::Minus { (-x - 1u32).bits() } else { x.bits() };
            Ok(Value::Int(n as i32))
        }
        "bitCount" => {
            let m = if x.sign() == Sign::Minus { (-x - 1u32).magnitude().clone() } else { x.magnitude().clone() };
            Ok(Value::Int(m.count_ones() as i32))
        }
        "getLowestSetBit" => Ok(Value::Int(x.trailing_zeros().map_or(-1, |n| n as i32))),
        "isProbablePrime" => ret_bool(is_prime(&x)),
        "nextProbablePrime" => {
            if x.sign() == Sign::Minus {
                return Err(arith(vm, &format!("start < 0: {x}")));
            }
            let mut n = x + 1u32;
            if n < BigInt::from(2) {
                n = BigInt::from(2);
            }
            while !is_prime(&n) {
                n += 1u32;
            }
            int_value(vm, n)
        }
        "toByteArray" => {
            let bytes = x.to_signed_bytes_be();
            super::string::bytes_value(vm, bytes)
        }
        _ => Err(Exc::Unsupported(format!("BigInteger.{op}"))),
    }
}

fn mod_inverse(a: &BigInt, m: &BigInt) -> Option<BigInt> {
    let e = a.extended_gcd(m);
    e.gcd.is_one().then(|| e.x.mod_floor(m))
}

/// Deterministic Miller-Rabin: exact below 3.3·10^24, overwhelmingly right above.
fn is_prime(n: &BigInt) -> bool {
    if n < &BigInt::from(2) {
        return false;
    }
    for p in [2u32, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41] {
        if n == &BigInt::from(p) {
            return true;
        }
        if (n % p).is_zero() {
            return false;
        }
    }
    let one = BigInt::one();
    let n1 = n - &one;
    let s = n1.trailing_zeros().unwrap_or(0);
    let d = &n1 >> s as usize;
    'witness: for w in [2u32, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41] {
        let mut x = BigInt::from(w).modpow(&d, n);
        if x == one || x == n1 {
            continue;
        }
        for _ in 1..s {
            x = (&x * &x) % n;
            if x == n1 {
                continue 'witness;
            }
        }
        return false;
    }
    true
}

fn biginteger_init(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let v = match &a[1..] {
        [Value::Ref(s)] if vm.string_units(*s).is_some() => {
            let text = vm.string(*s).unwrap_or_default();
            parse_int(vm, &text, 10)?
        }
        [Value::Ref(s), Value::Int(radix)] if vm.string_units(*s).is_some() => {
            let text = vm.string(*s).unwrap_or_default();
            parse_int(vm, &text, *radix as u32)?
        }
        [Value::Ref(bytes)] => {
            let b = super::string::bytes_of(vm, *bytes)?;
            if b.is_empty() {
                return Err(nfe(vm, "Zero length BigInteger"));
            }
            BigInt::from_signed_bytes_be(&b)
        }
        [Value::Int(sign), Value::Ref(bytes)] => {
            let b = super::string::bytes_of(vm, *bytes)?;
            let m = BigUint::from_bytes_be(&b);
            match sign {
                -1 => -BigInt::from(m),
                0 if m.is_zero() => BigInt::zero(),
                1 => BigInt::from(m),
                _ => return Err(nfe(vm, "Invalid signum value")),
            }
        }
        _ => return Err(Exc::Unsupported("BigInteger constructor".into())),
    };
    set_this(vm, a, Big { int: v, scale: 0 })
}

fn biginteger_clinit(vm: &mut Vm, _: &[Value]) -> Res<Value> {
    let c = vm.load("java/math/BigInteger")?;
    for (name, v) in [("ZERO", 0), ("ONE", 1), ("TWO", 2), ("TEN", 10)] {
        let r = new_big(vm, "java/math/BigInteger", Big { int: BigInt::from(v), scale: 0 })?;
        vm.set_static(c, name, Value::Ref(r));
    }
    void()
}

// ------------------------------------------------------------------ BigDecimal

fn pow10(n: u32) -> BigInt {
    num_traits::pow(BigInt::from(10), n as usize)
}

fn digit_count(i: &BigInt) -> u32 {
    if i.is_zero() { 1 } else { i.magnitude().to_str_radix(10).len() as u32 }
}

/// `BigDecimal.toString`: scientific notation when the scale is negative or
/// the adjusted exponent is below -6.
fn sci_string(b: &Big) -> String {
    let digits = b.int.magnitude().to_str_radix(10);
    let neg = b.int.sign() == Sign::Minus;
    let adjusted = -(b.scale as i64) + (digits.len() as i64 - 1);
    let body = if b.scale >= 0 && adjusted >= -6 {
        place_point(&digits, b.scale as i64)
    } else {
        let mut s = digits[..1].to_string();
        if digits.len() > 1 {
            s.push('.');
            s.push_str(&digits[1..]);
        }
        if adjusted != 0 {
            s.push('E');
            if adjusted > 0 {
                s.push('+');
            }
            s.push_str(&adjusted.to_string());
        }
        s
    };
    if neg { format!("-{body}") } else { body }
}

fn engineering_string(b: &Big) -> String {
    let digits = b.int.magnitude().to_str_radix(10);
    let neg = b.int.sign() == Sign::Minus;
    let adjusted = -(b.scale as i64) + (digits.len() as i64 - 1);
    if b.scale >= 0 && adjusted >= -6 {
        return sci_string(b);
    }
    let mut sig = adjusted.rem_euclid(3);
    let mut exp = adjusted - sig;
    let body = if b.int.is_zero() {
        // Zero: the exponent moves up to a multiple of three.
        sig = 0;
        let s = match adjusted.rem_euclid(3) {
            1 => {
                exp = adjusted + 2;
                "0.00".to_string()
            }
            2 => {
                exp = adjusted + 1;
                "0.0".to_string()
            }
            _ => "0".to_string(),
        };
        s
    } else {
        let sig = sig as usize + 1;
        if digits.len() <= sig {
            format!("{digits}{}", "0".repeat(sig - digits.len()))
        } else {
            format!("{}.{}", &digits[..sig], &digits[sig..])
        }
    };
    let _ = sig;
    let mut s = body;
    if exp != 0 {
        s.push('E');
        if exp > 0 {
            s.push('+');
        }
        s.push_str(&exp.to_string());
    }
    if neg { format!("-{s}") } else { s }
}

fn place_point(digits: &str, scale: i64) -> String {
    if scale == 0 {
        return digits.to_string();
    }
    if scale < 0 {
        return format!("{digits}{}", "0".repeat((-scale) as usize));
    }
    let scale = scale as usize;
    if digits.len() > scale {
        format!("{}.{}", &digits[..digits.len() - scale], &digits[digits.len() - scale..])
    } else {
        format!("0.{}{digits}", "0".repeat(scale - digits.len()))
    }
}

fn plain_string(b: &Big) -> String {
    let digits = b.int.magnitude().to_str_radix(10);
    let body = place_point(&digits, b.scale as i64);
    if b.int.sign() == Sign::Minus { format!("-{body}") } else { body }
}

/// Java's `RoundingMode` ordinals.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Rounding {
    Up,
    Down,
    Ceiling,
    Floor,
    HalfUp,
    HalfDown,
    HalfEven,
    Unnecessary,
}

impl Rounding {
    fn of(ordinal: i32) -> Rounding {
        match ordinal {
            0 => Rounding::Up,
            1 => Rounding::Down,
            2 => Rounding::Ceiling,
            3 => Rounding::Floor,
            4 => Rounding::HalfUp,
            5 => Rounding::HalfDown,
            6 => Rounding::HalfEven,
            _ => Rounding::Unnecessary,
        }
    }
}

/// `n / d`, rounded as `mode` says; `None` when UNNECESSARY would have to round.
fn div_round(n: &BigInt, d: &BigInt, mode: Rounding) -> Option<BigInt> {
    let (q, r) = n.div_rem(d);
    if r.is_zero() {
        return Some(q);
    }
    let positive = (n.sign() == Sign::Minus) == (d.sign() == Sign::Minus);
    let twice = (r.abs() * 2u32).cmp(&d.abs());
    let away = match mode {
        Rounding::Unnecessary => return None,
        Rounding::Up => true,
        Rounding::Down => false,
        Rounding::Ceiling => positive,
        Rounding::Floor => !positive,
        Rounding::HalfUp => twice != std::cmp::Ordering::Less,
        Rounding::HalfDown => twice == std::cmp::Ordering::Greater,
        Rounding::HalfEven => twice == std::cmp::Ordering::Greater || (twice == std::cmp::Ordering::Equal && q.is_odd()),
    };
    Some(if away { if positive { q + 1u32 } else { q - 1u32 } } else { q })
}

/// The decimal rescaled to `scale`.
fn set_scale(b: &Big, scale: i32, mode: Rounding) -> Option<Big> {
    if scale >= b.scale {
        return Some(Big { int: &b.int * pow10((scale - b.scale) as u32), scale });
    }
    let d = pow10((b.scale - scale) as u32);
    div_round(&b.int, &d, mode).map(|int| Big { int, scale })
}

/// Rounds to `precision` significant digits (0: unlimited).
fn round_to(b: Big, precision: u32, mode: Rounding) -> Option<Big> {
    if precision == 0 {
        return Some(b);
    }
    let mut b = b;
    loop {
        let digits = digit_count(&b.int);
        if digits <= precision {
            return Some(b);
        }
        let drop = digits - precision;
        let new_scale = b.scale as i64 - drop as i64;
        b = set_scale(&b, new_scale as i32, mode)?;
    }
}

fn mc_of(vm: &Vm, r: ObjId) -> (u32, Rounding) {
    match state(vm, r) {
        Some(State::Generic(v)) if v.len() == 2 => (v[0].i() as u32, Rounding::of(v[1].i())),
        _ => (0, Rounding::HalfUp),
    }
}

fn rounding_of(vm: &Vm, r: ObjId) -> Rounding {
    Rounding::of(super::lang::enum_parts(vm, r).1)
}

fn parse_decimal(text: &str) -> Option<Big> {
    let s = text.trim_start_matches('+');
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) if !text.starts_with('+') => (true, rest),
        _ => (false, s),
    };
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], s[i + 1..].parse::<i64>().ok()?),
        None => (s, 0),
    };
    let (int_part, frac) = match mantissa.find('.') {
        Some(i) => (&mantissa[..i], &mantissa[i + 1..]),
        None => (mantissa, ""),
    };
    if int_part.is_empty() && frac.is_empty() {
        return None;
    }
    if !int_part.bytes().chain(frac.bytes()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let digits = format!("{int_part}{frac}");
    let mut int = BigInt::parse_bytes(digits.as_bytes(), 10)?;
    if neg {
        int = -int;
    }
    let scale = frac.len() as i64 - exp;
    if scale > i32::MAX as i64 || scale < i32::MIN as i64 {
        return None;
    }
    Some(Big { int, scale: scale as i32 })
}

/// `new BigDecimal(double)`: the double's exact binary value.
fn exact_double(d: f64) -> Big {
    if d == 0.0 {
        return Big { int: BigInt::zero(), scale: 0 };
    }
    let bits = d.to_bits();
    let neg = bits >> 63 != 0;
    let exp = ((bits >> 52) & 0x7ff) as i64;
    let mut mant = (bits & ((1 << 52) - 1)) as i64;
    let mut e = if exp == 0 { -1074 } else { exp - 1075 };
    if exp != 0 {
        mant |= 1 << 52;
    }
    while mant & 1 == 0 {
        mant >>= 1;
        e += 1;
    }
    let mut int = BigInt::from(mant);
    let scale;
    if e >= 0 {
        int <<= e as usize;
        scale = 0;
    } else {
        // m × 2^e = m × 5^-e / 10^-e.
        int *= num_traits::pow(BigInt::from(5), (-e) as usize);
        scale = (-e) as i32;
    }
    if neg {
        int = -int;
    }
    Big { int, scale }
}

fn strip_zeros(mut b: Big) -> Big {
    if b.int.is_zero() {
        return Big { int: BigInt::zero(), scale: 0 };
    }
    let ten = BigInt::from(10);
    loop {
        let (q, r) = b.int.div_rem(&ten);
        if !r.is_zero() {
            return b;
        }
        b.int = q;
        b.scale -= 1;
    }
}

fn align(x: &Big, y: &Big) -> (BigInt, BigInt, i32) {
    let scale = x.scale.max(y.scale);
    let xi = &x.int * pow10((scale - x.scale) as u32);
    let yi = &y.int * pow10((scale - y.scale) as u32);
    (xi, yi, scale)
}

fn compare(x: &Big, y: &Big) -> std::cmp::Ordering {
    let (a, b, _) = align(x, y);
    a.cmp(&b)
}

fn bigdecimal_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let x = this_big(vm, a)?;
    match op {
        "add" | "subtract" => {
            let y = arg_big(vm, a, 1)?;
            let (p, q, scale) = align(&x, &y);
            let r = if op == "add" { p + q } else { p - q };
            let result = Big { int: r, scale };
            if a.len() > 2 {
                let (prec, mode) = mc_of(vm, arg(a, 2).r());
                let Some(r) = round_to(result, prec, mode) else { return Err(arith(vm, "Rounding necessary")) };
                return dec_value(vm, r.int, r.scale as i64);
            }
            dec_value(vm, result.int, scale as i64)
        }
        "multiply" => {
            let y = arg_big(vm, a, 1)?;
            let result = Big { int: &x.int * &y.int, scale: 0 };
            let scale = x.scale as i64 + y.scale as i64;
            if a.len() > 2 {
                let (prec, mode) = mc_of(vm, arg(a, 2).r());
                let Some(r) = round_to(Big { int: result.int, scale: scale as i32 }, prec, mode) else { return Err(arith(vm, "Rounding necessary")) };
                return dec_value(vm, r.int, r.scale as i64);
            }
            dec_value(vm, result.int, scale)
        }
        "divideScale" | "divideMode" | "divideExact" | "divideMc" => {
            let y = arg_big(vm, a, 1)?;
            if y.int.is_zero() {
                return Err(arith(vm, if x.int.is_zero() { "Division undefined" } else { "Division by zero" }));
            }
            let (scale, mode, precision) = match op {
                "divideScale" => (arg(a, 2).i(), rounding_of(vm, arg(a, 3).r()), None),
                "divideMode" => (x.scale, rounding_of(vm, arg(a, 2).r()), None),
                "divideMc" => {
                    let (p, m) = mc_of(vm, arg(a, 2).r());
                    (0, m, Some(p))
                }
                _ => (0, Rounding::Unnecessary, None),
            };
            match (op, precision) {
                ("divideExact", _) | ("divideMc", Some(0)) => {
                    // The exact quotient, or ArithmeticException when it does not end.
                    let preferred = x.scale as i64 - y.scale as i64;
                    let mut d = y.int.clone();
                    let (mut twos, mut fives) = (0u32, 0u32);
                    let g = x.int.gcd(&d);
                    d /= &g;
                    while (&d % 2u32).is_zero() {
                        d /= 2u32;
                        twos += 1;
                    }
                    while (&d % 5u32).is_zero() {
                        d /= 5u32;
                        fives += 1;
                    }
                    if !d.abs().is_one() {
                        return Err(arith(vm, "Non-terminating decimal expansion; no exact representable decimal result."));
                    }
                    let extra = twos.max(fives) as i64;
                    let scale = preferred + extra;
                    let num = &x.int * pow10(extra as u32);
                    let q = &num / &y.int;
                    let mut r = Big { int: q, scale: scale as i32 };
                    // Down to the preferred scale where the zeros allow.
                    while (r.scale as i64) > preferred {
                        let (q, rem) = r.int.div_rem(&BigInt::from(10));
                        if !rem.is_zero() {
                            break;
                        }
                        r = Big { int: q, scale: r.scale - 1 };
                    }
                    dec_value(vm, r.int, r.scale as i64)
                }
                ("divideMc", Some(prec)) => {
                    let preferred = x.scale as i64 - y.scale as i64;
                    // Enough digits for the precision, then round.
                    let shift = prec as i64 + digit_count(&y.int) as i64 + 3;
                    let num = &x.int * pow10(shift as u32);
                    let q = div_round(&num, &y.int, Rounding::Down).unwrap_or_default();
                    let exact = (&q * &y.int) == num;
                    // A sticky digit keeps the rounding of what was cut off right.
                    let q = if exact { q * 10 } else { q * 10 + if num.sign() == y.int.sign() { 1 } else { -1 } };
                    let raw = Big { int: q, scale: (x.scale as i64 - y.scale as i64 + shift + 1) as i32 };
                    let Some(mut r) = round_to(raw, prec, mode) else { return Err(arith(vm, "Rounding necessary")) };
                    while (r.scale as i64) > preferred {
                        let (q, rem) = r.int.div_rem(&BigInt::from(10));
                        if !rem.is_zero() {
                            break;
                        }
                        r = Big { int: q, scale: r.scale - 1 };
                    }
                    dec_value(vm, r.int, r.scale as i64)
                }
                _ => {
                    // x / y at `scale`: (x.int × 10^(scale - x.scale + y.scale)) / y.int.
                    let shift = scale as i64 - x.scale as i64 + y.scale as i64;
                    let (num, den) = if shift >= 0 {
                        (&x.int * pow10(shift as u32), y.int.clone())
                    } else {
                        (x.int.clone(), &y.int * pow10((-shift) as u32))
                    };
                    match div_round(&num, &den, mode) {
                        Some(q) => dec_value(vm, q, scale as i64),
                        None => Err(arith(vm, "Rounding necessary")),
                    }
                }
            }
        }
        "remainder" | "divideToIntegralValue" => {
            let y = arg_big(vm, a, 1)?;
            if y.int.is_zero() {
                return Err(arith(vm, "Division by zero"));
            }
            let (p, q, scale) = align(&x, &y);
            let quotient = &p / &q;
            if op == "remainder" {
                let rem = p - &quotient * q;
                dec_value(vm, rem, scale as i64)
            } else {
                let preferred = x.scale as i64 - y.scale as i64;
                let mut r = Big { int: quotient, scale: 0 };
                if preferred > 0 {
                    r = set_scale(&r, preferred as i32, Rounding::Unnecessary).unwrap_or(r);
                } else {
                    while (r.scale as i64) > preferred && !r.int.is_zero() {
                        let (q, rem) = r.int.div_rem(&BigInt::from(10));
                        if !rem.is_zero() {
                            break;
                        }
                        r = Big { int: q, scale: r.scale - 1 };
                    }
                }
                dec_value(vm, r.int, r.scale as i64)
            }
        }
        "setScale" => {
            let scale = arg(a, 1).i();
            let mode = if a.len() > 2 {
                match arg(a, 2) {
                    Value::Int(m) => Rounding::of(m),
                    v => rounding_of(vm, v.r()),
                }
            } else {
                Rounding::Unnecessary
            };
            match set_scale(&x, scale, mode) {
                Some(r) => dec_value(vm, r.int, r.scale as i64),
                None => Err(arith(vm, "Rounding necessary")),
            }
        }
        "round" => {
            let (prec, mode) = mc_of(vm, arg(a, 1).r());
            match round_to(x, prec, mode) {
                Some(r) => dec_value(vm, r.int, r.scale as i64),
                None => Err(arith(vm, "Rounding necessary")),
            }
        }
        "pow" => {
            let n = arg(a, 1).i();
            if !(0..=999_999_999).contains(&n) {
                return Err(arith(vm, "Invalid operation"));
            }
            let r = num_traits::pow(x.int, n as usize);
            dec_value(vm, r, x.scale as i64 * n as i64)
        }
        "negate" => dec_value(vm, -x.int, x.scale as i64),
        "abs" => dec_value(vm, x.int.abs(), x.scale as i64),
        "plus" => ret_ref(arg(a, 0).r()),
        "signum" => Ok(Value::Int(signum(&x.int))),
        "scale" => Ok(Value::Int(x.scale)),
        "precision" => Ok(Value::Int(digit_count(&x.int) as i32)),
        "unscaledValue" => int_value(vm, x.int),
        "stripTrailingZeros" => {
            let r = strip_zeros(x);
            dec_value(vm, r.int, r.scale as i64)
        }
        "movePointLeft" | "movePointRight" | "scaleByPowerOfTen" => {
            let n = arg(a, 1).i() as i64;
            let new_scale = match op {
                "movePointLeft" => x.scale as i64 + n,
                _ => x.scale as i64 - n,
            };
            if op == "scaleByPowerOfTen" || new_scale >= 0 {
                dec_value(vm, x.int, new_scale)
            } else {
                // movePoint* never leaves a negative scale.
                let r = set_scale(&Big { int: x.int, scale: new_scale as i32 }, 0, Rounding::Unnecessary).unwrap();
                dec_value(vm, r.int, 0)
            }
        }
        "compareTo" => {
            let y = arg_big(vm, a, 1)?;
            Ok(Value::Int(compare(&x, &y) as i32))
        }
        "equals" => {
            let o = arg(a, 1).r();
            let same_class = vm.class_of(o).is_some_and(|c| vm.class_name(c) == "java/math/BigDecimal");
            if !same_class {
                return ret_bool(false);
            }
            let y = big(vm, o);
            ret_bool(x.scale == y.scale && x.int == y.int)
        }
        "min" | "max" => {
            let y = arg_big(vm, a, 1)?;
            let c = compare(&x, &y);
            let pick_this = if op == "min" { c != std::cmp::Ordering::Greater } else { c != std::cmp::Ordering::Less };
            if pick_this { ret_ref(arg(a, 0).r()) } else { ret_ref(arg(a, 1).r()) }
        }
        "hashCode" => Ok(Value::Int(java_hash(&x.int).wrapping_mul(31).wrapping_add(x.scale))),
        "toString" => ret_str(vm, &sci_string(&x)),
        "toEngineeringString" => ret_str(vm, &engineering_string(&x)),
        "toPlainString" => ret_str(vm, &plain_string(&x)),
        "toBigInteger" | "toBigIntegerExact" => {
            let r = set_scale(&x, 0, if op == "toBigInteger" { Rounding::Down } else { Rounding::Unnecessary });
            match r {
                Some(r) => int_value(vm, r.int),
                None => Err(arith(vm, "Rounding necessary")),
            }
        }
        "intValue" | "longValue" => {
            let r = set_scale(&x, 0, Rounding::Down).unwrap();
            let v = low_i64(&r.int);
            Ok(if op == "intValue" { Value::Int(v as i32) } else { Value::Long(v) })
        }
        "intValueExact" | "longValueExact" => {
            let Some(r) = set_scale(&x, 0, Rounding::Unnecessary) else { return Err(arith(vm, "Rounding necessary")) };
            match (op, r.int.to_i32(), r.int.to_i64()) {
                ("intValueExact", Some(v), _) => Ok(Value::Int(v)),
                ("longValueExact", _, Some(v)) => Ok(Value::Long(v)),
                _ => Err(arith(vm, "Overflow")),
            }
        }
        "doubleValue" | "floatValue" => {
            let text = sci_string(&x);
            let d: f64 = text.parse().unwrap_or(0.0);
            Ok(if op == "doubleValue" { Value::Double(d) } else { Value::Float(text.parse::<f32>().unwrap_or(d as f32)) })
        }
        "ulp" => dec_value(vm, BigInt::one(), x.scale as i64),
        "sqrt" => {
            let (prec, mode) = mc_of(vm, arg(a, 1).r());
            if x.int.sign() == Sign::Minus {
                return Err(arith(vm, "Attempted square root of negative BigDecimal"));
            }
            if x.int.is_zero() {
                return dec_value(vm, BigInt::zero(), (x.scale / 2) as i64);
            }
            // Enough digits, then the integer root, then round.
            let digits = prec.max(digit_count(&x.int)) as i64 + 4;
            let mut scale = 2 * digits;
            if (scale - x.scale as i64) % 2 != 0 {
                scale += 1;
            }
            let n = &x.int * pow10((scale - x.scale as i64) as u32);
            let root = n.sqrt();
            let exact = &root * &root == n;
            let root = if exact { root * 10 } else { root * 10 + 1 };
            let raw = Big { int: root, scale: (scale / 2 + 1) as i32 };
            let Some(r) = round_to(raw, prec, mode) else { return Err(arith(vm, "Rounding necessary")) };
            let r = if exact { strip_to(r, x.scale / 2) } else { r };
            dec_value(vm, r.int, r.scale as i64)
        }
        _ => Err(Exc::Unsupported(format!("BigDecimal.{op}"))),
    }
}

/// Trailing zeros off, down to the preferred scale.
fn strip_to(mut r: Big, preferred: i32) -> Big {
    while r.scale > preferred {
        let (q, rem) = r.int.div_rem(&BigInt::from(10));
        if !rem.is_zero() {
            break;
        }
        r = Big { int: q, scale: r.scale - 1 };
    }
    r
}

fn bigdecimal_init(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let parse = |vm: &mut Vm, text: &str| -> Res<Big> {
        match parse_decimal(text) {
            Some(b) => Ok(b),
            None => {
                let msg = if text.is_empty() { "Index 0 out of bounds for length 0".to_string() } else { format!("Character {} is neither a decimal digit number, decimal point, nor \"e\" notation exponential mark.", text.chars().find(|c| !c.is_ascii_digit() && !".eE+-".contains(*c)).unwrap_or('?')) };
                Err(nfe(vm, &msg))
            }
        }
    };
    let b = match &a[1..] {
        [Value::Ref(s)] if *s == 0 => return Err(vm.npe(None)),
        [Value::Ref(s)] if vm.string_units(*s).is_some() => {
            let text = vm.string(*s).unwrap_or_default();
            parse(vm, &text)?
        }
        [Value::Ref(chars)] => {
            let c = super::string::chars_of(vm, *chars, None)?;
            parse(vm, &String::from_utf16_lossy(&c))?
        }
        [Value::Ref(s), Value::Ref(mc)] => {
            let text = if vm.string_units(*s).is_some() {
                vm.string(*s).unwrap_or_default()
            } else if vm.array(*s).is_some() {
                String::from_utf16_lossy(&super::string::chars_of(vm, *s, None)?)
            } else {
                // new BigDecimal(BigInteger, MathContext)
                plain_string(&big(vm, *s))
            };
            let b = parse(vm, &text)?;
            let (p, m) = mc_of(vm, *mc);
            match round_to(b, p, m) {
                Some(b) => b,
                None => return Err(arith(vm, "Rounding necessary")),
            }
        }
        [Value::Double(d)] | [Value::Double(d), Value::Ref(_)] => {
            if !d.is_finite() {
                return Err(nfe(vm, "Infinite or NaN"));
            }
            let b = exact_double(*d);
            if let Some(Value::Ref(mc)) = a.get(2) {
                let (p, m) = mc_of(vm, *mc);
                round_to(b, p, m).unwrap_or_else(|| exact_double(*d))
            } else {
                b
            }
        }
        [Value::Int(v)] => Big { int: BigInt::from(*v), scale: 0 },
        [Value::Long(v)] => Big { int: BigInt::from(*v), scale: 0 },
        [Value::Ref(bi), Value::Int(scale)] => Big { int: big(vm, *bi).int, scale: *scale },
        _ => return Err(Exc::Unsupported("BigDecimal constructor".into())),
    };
    let t = this(vm, a)?;
    // new BigDecimal(BigInteger)
    let b = if let [Value::Ref(bi)] = &a[1..]
        && vm.class_of(*bi).is_some_and(|c| vm.class_name(c) == "java/math/BigInteger")
    {
        Big { int: big(vm, *bi).int, scale: 0 }
    } else {
        b
    };
    if let Some(o) = vm.obj_mut(t) {
        o.data = Data::Jdk(Box::new(State::Big(Box::new(b))));
    }
    void()
}

fn bigdecimal_value_of(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    match a {
        [Value::Long(v)] => dec_value(vm, BigInt::from(*v), 0),
        [Value::Long(v), Value::Int(s)] => dec_value(vm, BigInt::from(*v), *s as i64),
        [Value::Double(d)] => {
            if !d.is_finite() {
                return Err(nfe(vm, "Infinite or NaN"));
            }
            let text = crate::vm::java_double_to_string(*d);
            let b = parse_decimal(&text).unwrap_or(Big { int: BigInt::zero(), scale: 0 });
            dec_value(vm, b.int, b.scale as i64)
        }
        _ => Err(Exc::Unsupported("BigDecimal.valueOf".into())),
    }
}

fn bigdecimal_clinit(vm: &mut Vm, _: &[Value]) -> Res<Value> {
    let c = vm.load("java/math/BigDecimal")?;
    for (name, v) in [("ZERO", 0), ("ONE", 1), ("TWO", 2), ("TEN", 10)] {
        let r = new_big(vm, "java/math/BigDecimal", Big { int: BigInt::from(v), scale: 0 })?;
        vm.set_static(c, name, Value::Ref(r));
    }
    for (name, v) in [("ROUND_UP", 0), ("ROUND_DOWN", 1), ("ROUND_CEILING", 2), ("ROUND_FLOOR", 3), ("ROUND_HALF_UP", 4), ("ROUND_HALF_DOWN", 5), ("ROUND_HALF_EVEN", 6), ("ROUND_UNNECESSARY", 7)] {
        vm.set_static(c, name, Value::Int(v));
    }
    void()
}

const ROUNDING_NAMES: &[&str] = &["UP", "DOWN", "CEILING", "FLOOR", "HALF_UP", "HALF_DOWN", "HALF_EVEN", "UNNECESSARY"];

fn new_math_context(vm: &mut Vm, precision: i32, mode: i32) -> Res<ObjId> {
    let c = vm.load("java/math/MathContext")?;
    let r = vm.new_instance(c)?;
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Generic(vec![Value::Int(precision), Value::Int(mode)])));
    }
    Ok(r)
}

fn math_context_clinit(vm: &mut Vm, _: &[Value]) -> Res<Value> {
    let c = vm.load("java/math/MathContext")?;
    for (name, p, m) in [("UNLIMITED", 0, 4), ("DECIMAL32", 7, 6), ("DECIMAL64", 16, 6), ("DECIMAL128", 34, 6)] {
        let r = new_math_context(vm, p, m)?;
        vm.set_static(c, name, Value::Ref(r));
        vm.pinned.push(r);
    }
    void()
}

fn math_context_init(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let p = arg(a, 1).i();
    if p < 0 {
        return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("Digits < 0")));
    }
    let mode = match a.get(2) {
        Some(Value::Ref(r)) => super::lang::enum_parts(vm, *r).1,
        _ => 4,
    };
    if let Some(o) = vm.obj_mut(t) {
        o.data = Data::Jdk(Box::new(State::Generic(vec![Value::Int(p), Value::Int(mode)])));
    }
    void()
}

fn math_context_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let (p, m) = mc_of(vm, t);
    let mode_index = ROUNDING_NAMES.iter().position(|n| match m {
        Rounding::Up => *n == "UP",
        Rounding::Down => *n == "DOWN",
        Rounding::Ceiling => *n == "CEILING",
        Rounding::Floor => *n == "FLOOR",
        Rounding::HalfUp => *n == "HALF_UP",
        Rounding::HalfDown => *n == "HALF_DOWN",
        Rounding::HalfEven => *n == "HALF_EVEN",
        Rounding::Unnecessary => *n == "UNNECESSARY",
    });
    match op {
        "getPrecision" => Ok(Value::Int(p as i32)),
        "getRoundingMode" => {
            let name = ROUNDING_NAMES[mode_index.unwrap_or(4)];
            vm.get_static("java/math/RoundingMode", name, "Ljava/math/RoundingMode;")
        }
        "toString" => ret_str(vm, &format!("precision={p} roundingMode={}", ROUNDING_NAMES[mode_index.unwrap_or(4)])),
        "equals" => {
            let o = arg(a, 1).r();
            let same = vm.class_of(o) == vm.class_of(t) && {
                let (q, n) = mc_of(vm, o);
                q == p && n == m
            };
            ret_bool(same)
        }
        "hashCode" => Ok(Value::Int((mode_index.unwrap_or(4) as i32).wrapping_add((p as i32) << 2))),
        _ => Err(Exc::Unsupported(format!("MathContext.{op}"))),
    }
}

/// A native enum's constants, made in its class initializer.
pub fn make_enum_constants(vm: &mut Vm, class: &str, names: &[&str]) -> Res<Value> {
    let c = vm.load(class)?;
    let mut all = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let r = vm.new_instance(c)?;
        let n = vm.intern(name)?;
        if let Some(o) = vm.obj_mut(r) {
            o.data = Data::Jdk(Box::new(State::Enum { name: n, ordinal: i as i32 }));
        }
        vm.set_static(c, name, Value::Ref(r));
        vm.pinned.push(r);
        all.push(r);
    }
    let values = vm.new_ref_array(&format!("L{class};"), &all)?;
    vm.pinned.push(values);
    vm.set_static(c, "$VALUES", Value::Ref(values));
    void()
}

/// `values()` of a native enum: a copy of `$VALUES`.
pub fn enum_values(vm: &mut Vm, class: &str) -> Res<Value> {
    let v = vm.get_static(class, "$VALUES", &format!("[L{class};"))?.r();
    let items = vm.ref_elements(v);
    ret_ref(vm.new_ref_array(&format!("L{class};"), &items)?)
}

fn rounding_value_of_int(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let i = arg(a, 0).i();
    match ROUNDING_NAMES.get(i as usize) {
        Some(name) if i >= 0 => vm.get_static("java/math/RoundingMode", name, "Ljava/math/RoundingMode;"),
        _ => Err(vm.throw_new("java/lang/IllegalArgumentException", Some("argument out of range"))),
    }
}

static BIGINTEGER_METHODS: &[super::NativeMethod] = natives![
    "<init>" "(Ljava/lang/String;)V" M => Some(biginteger_init),
    "<init>" "(Ljava/lang/String;I)V" M => Some(biginteger_init),
    "<init>" "([B)V" M => Some(biginteger_init),
    "<init>" "(I[B)V" M => Some(biginteger_init),
    "valueOf" "(J)Ljava/math/BigInteger;" S => Some(|vm, a| int_value(vm, BigInt::from(arg(a, 0).j()))),
    "probablePrime" "(ILjava/util/Random;)Ljava/math/BigInteger;" S => Some(|_, _| Err(Exc::Unsupported("BigInteger.probablePrime".into()))),
    "add" "(Ljava/math/BigInteger;)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "add")),
    "subtract" "(Ljava/math/BigInteger;)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "subtract")),
    "multiply" "(Ljava/math/BigInteger;)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "multiply")),
    "divide" "(Ljava/math/BigInteger;)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "divide")),
    "remainder" "(Ljava/math/BigInteger;)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "remainder")),
    "divideAndRemainder" "(Ljava/math/BigInteger;)[Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "divideAndRemainder")),
    "mod" "(Ljava/math/BigInteger;)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "mod")),
    "modPow" "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "modPow")),
    "modInverse" "(Ljava/math/BigInteger;)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "modInverse")),
    "gcd" "(Ljava/math/BigInteger;)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "gcd")),
    "pow" "(I)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "pow")),
    "sqrt" "()Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "sqrt")),
    "negate" "()Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "negate")),
    "abs" "()Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "abs")),
    "signum" "()I" M => Some(|vm, a| biginteger_op(vm, a, "signum")),
    "min" "(Ljava/math/BigInteger;)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "min")),
    "max" "(Ljava/math/BigInteger;)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "max")),
    "compareTo" "(Ljava/math/BigInteger;)I" M => Some(|vm, a| biginteger_op(vm, a, "compareTo")),
    "compareTo" "(Ljava/lang/Object;)I" M => Some(|vm, a| biginteger_op(vm, a, "compareTo")),
    "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| biginteger_op(vm, a, "equals")),
    "hashCode" "()I" M => Some(|vm, a| biginteger_op(vm, a, "hashCode")),
    "toString" "()Ljava/lang/String;" M => Some(|vm, a| biginteger_op(vm, a, "toString")),
    "toString" "(I)Ljava/lang/String;" M => Some(|vm, a| biginteger_op(vm, a, "toStringRadix")),
    "intValue" "()I" M => Some(|vm, a| biginteger_op(vm, a, "intValue")),
    "longValue" "()J" M => Some(|vm, a| biginteger_op(vm, a, "longValue")),
    "shortValue" "()S" M => Some(|vm, a| biginteger_op(vm, a, "shortValue")),
    "byteValue" "()B" M => Some(|vm, a| biginteger_op(vm, a, "byteValue")),
    "doubleValue" "()D" M => Some(|vm, a| biginteger_op(vm, a, "doubleValue")),
    "floatValue" "()F" M => Some(|vm, a| biginteger_op(vm, a, "floatValue")),
    "intValueExact" "()I" M => Some(|vm, a| biginteger_op(vm, a, "intValueExact")),
    "longValueExact" "()J" M => Some(|vm, a| biginteger_op(vm, a, "longValueExact")),
    "shortValueExact" "()S" M => Some(|vm, a| biginteger_op(vm, a, "shortValueExact")),
    "byteValueExact" "()B" M => Some(|vm, a| biginteger_op(vm, a, "byteValueExact")),
    "shiftLeft" "(I)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "shiftLeft")),
    "shiftRight" "(I)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "shiftRight")),
    "and" "(Ljava/math/BigInteger;)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "and")),
    "or" "(Ljava/math/BigInteger;)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "or")),
    "xor" "(Ljava/math/BigInteger;)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "xor")),
    "andNot" "(Ljava/math/BigInteger;)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "andNot")),
    "not" "()Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "not")),
    "testBit" "(I)Z" M => Some(|vm, a| biginteger_op(vm, a, "testBit")),
    "setBit" "(I)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "setBit")),
    "clearBit" "(I)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "clearBit")),
    "flipBit" "(I)Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "flipBit")),
    "bitLength" "()I" M => Some(|vm, a| biginteger_op(vm, a, "bitLength")),
    "bitCount" "()I" M => Some(|vm, a| biginteger_op(vm, a, "bitCount")),
    "getLowestSetBit" "()I" M => Some(|vm, a| biginteger_op(vm, a, "getLowestSetBit")),
    "isProbablePrime" "(I)Z" M => Some(|vm, a| biginteger_op(vm, a, "isProbablePrime")),
    "nextProbablePrime" "()Ljava/math/BigInteger;" M => Some(|vm, a| biginteger_op(vm, a, "nextProbablePrime")),
    "toByteArray" "()[B" M => Some(|vm, a| biginteger_op(vm, a, "toByteArray")),
];

static BIGDECIMAL_METHODS: &[super::NativeMethod] = natives![
    "<init>" "(Ljava/lang/String;)V" M => Some(bigdecimal_init),
    "<init>" "([C)V" M => Some(bigdecimal_init),
    "<init>" "(Ljava/lang/String;Ljava/math/MathContext;)V" M => Some(bigdecimal_init),
    "<init>" "(D)V" M => Some(bigdecimal_init),
    "<init>" "(DLjava/math/MathContext;)V" M => Some(bigdecimal_init),
    "<init>" "(I)V" M => Some(bigdecimal_init),
    "<init>" "(J)V" M => Some(bigdecimal_init),
    "<init>" "(Ljava/math/BigInteger;)V" M => Some(bigdecimal_init),
    "<init>" "(Ljava/math/BigInteger;I)V" M => Some(bigdecimal_init),
    "<init>" "(Ljava/math/BigInteger;Ljava/math/MathContext;)V" M => Some(bigdecimal_init),
    "valueOf" "(J)Ljava/math/BigDecimal;" S => Some(bigdecimal_value_of),
    "valueOf" "(JI)Ljava/math/BigDecimal;" S => Some(bigdecimal_value_of),
    "valueOf" "(D)Ljava/math/BigDecimal;" S => Some(bigdecimal_value_of),
    "add" "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "add")),
    "add" "(Ljava/math/BigDecimal;Ljava/math/MathContext;)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "add")),
    "subtract" "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "subtract")),
    "subtract" "(Ljava/math/BigDecimal;Ljava/math/MathContext;)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "subtract")),
    "multiply" "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "multiply")),
    "multiply" "(Ljava/math/BigDecimal;Ljava/math/MathContext;)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "multiply")),
    "divide" "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "divideExact")),
    "divide" "(Ljava/math/BigDecimal;ILjava/math/RoundingMode;)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "divideScale")),
    "divide" "(Ljava/math/BigDecimal;II)Ljava/math/BigDecimal;" M => Some(|vm, a| { let mode = rounding_value_of_int(vm, &[arg(a, 3)])?; bigdecimal_op(vm, &[a[0], a[1], a[2], mode], "divideScale") }),
    "divide" "(Ljava/math/BigDecimal;Ljava/math/RoundingMode;)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "divideMode")),
    "divide" "(Ljava/math/BigDecimal;I)Ljava/math/BigDecimal;" M => Some(|vm, a| { let mode = rounding_value_of_int(vm, &[arg(a, 2)])?; bigdecimal_op(vm, &[a[0], a[1], mode], "divideMode") }),
    "divide" "(Ljava/math/BigDecimal;Ljava/math/MathContext;)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "divideMc")),
    "remainder" "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "remainder")),
    "divideToIntegralValue" "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "divideToIntegralValue")),
    "setScale" "(I)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "setScale")),
    "setScale" "(ILjava/math/RoundingMode;)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "setScale")),
    "setScale" "(II)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "setScale")),
    "round" "(Ljava/math/MathContext;)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "round")),
    "pow" "(I)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "pow")),
    "sqrt" "(Ljava/math/MathContext;)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "sqrt")),
    "negate" "()Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "negate")),
    "abs" "()Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "abs")),
    "plus" "()Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "plus")),
    "signum" "()I" M => Some(|vm, a| bigdecimal_op(vm, a, "signum")),
    "scale" "()I" M => Some(|vm, a| bigdecimal_op(vm, a, "scale")),
    "precision" "()I" M => Some(|vm, a| bigdecimal_op(vm, a, "precision")),
    "unscaledValue" "()Ljava/math/BigInteger;" M => Some(|vm, a| bigdecimal_op(vm, a, "unscaledValue")),
    "stripTrailingZeros" "()Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "stripTrailingZeros")),
    "movePointLeft" "(I)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "movePointLeft")),
    "movePointRight" "(I)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "movePointRight")),
    "scaleByPowerOfTen" "(I)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "scaleByPowerOfTen")),
    "compareTo" "(Ljava/math/BigDecimal;)I" M => Some(|vm, a| bigdecimal_op(vm, a, "compareTo")),
    "compareTo" "(Ljava/lang/Object;)I" M => Some(|vm, a| bigdecimal_op(vm, a, "compareTo")),
    "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| bigdecimal_op(vm, a, "equals")),
    "min" "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "min")),
    "max" "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "max")),
    "hashCode" "()I" M => Some(|vm, a| bigdecimal_op(vm, a, "hashCode")),
    "toString" "()Ljava/lang/String;" M => Some(|vm, a| bigdecimal_op(vm, a, "toString")),
    "toEngineeringString" "()Ljava/lang/String;" M => Some(|vm, a| bigdecimal_op(vm, a, "toEngineeringString")),
    "toPlainString" "()Ljava/lang/String;" M => Some(|vm, a| bigdecimal_op(vm, a, "toPlainString")),
    "toBigInteger" "()Ljava/math/BigInteger;" M => Some(|vm, a| bigdecimal_op(vm, a, "toBigInteger")),
    "toBigIntegerExact" "()Ljava/math/BigInteger;" M => Some(|vm, a| bigdecimal_op(vm, a, "toBigIntegerExact")),
    "intValue" "()I" M => Some(|vm, a| bigdecimal_op(vm, a, "intValue")),
    "longValue" "()J" M => Some(|vm, a| bigdecimal_op(vm, a, "longValue")),
    "intValueExact" "()I" M => Some(|vm, a| bigdecimal_op(vm, a, "intValueExact")),
    "longValueExact" "()J" M => Some(|vm, a| bigdecimal_op(vm, a, "longValueExact")),
    "doubleValue" "()D" M => Some(|vm, a| bigdecimal_op(vm, a, "doubleValue")),
    "floatValue" "()F" M => Some(|vm, a| bigdecimal_op(vm, a, "floatValue")),
    "ulp" "()Ljava/math/BigDecimal;" M => Some(|vm, a| bigdecimal_op(vm, a, "ulp")),
];

static CLASSES: &[NativeClass] = &[
    NativeClass {
        name: "java/math/BigInteger",
        super_name: Some("java/lang/Number"),
        interfaces: &["java/lang/Comparable"],
        access: PUBLIC,
        methods: BIGINTEGER_METHODS,
        statics: &[("ZERO", "Ljava/math/BigInteger;"), ("ONE", "Ljava/math/BigInteger;"), ("TWO", "Ljava/math/BigInteger;"), ("TEN", "Ljava/math/BigInteger;")],
        clinit: Some(biginteger_clinit),
        data: Some(|| Data::Jdk(Box::new(State::Big(Box::new(Big { int: BigInt::zero(), scale: 0 }))))),
    },
    NativeClass {
        name: "java/math/BigDecimal",
        super_name: Some("java/lang/Number"),
        interfaces: &["java/lang/Comparable"],
        access: PUBLIC,
        methods: BIGDECIMAL_METHODS,
        statics: &[
            ("ZERO", "Ljava/math/BigDecimal;"),
            ("ONE", "Ljava/math/BigDecimal;"),
            ("TWO", "Ljava/math/BigDecimal;"),
            ("TEN", "Ljava/math/BigDecimal;"),
            ("ROUND_UP", "I"),
            ("ROUND_DOWN", "I"),
            ("ROUND_CEILING", "I"),
            ("ROUND_FLOOR", "I"),
            ("ROUND_HALF_UP", "I"),
            ("ROUND_HALF_DOWN", "I"),
            ("ROUND_HALF_EVEN", "I"),
            ("ROUND_UNNECESSARY", "I"),
        ],
        clinit: Some(bigdecimal_clinit),
        data: Some(|| Data::Jdk(Box::new(State::Big(Box::new(Big { int: BigInt::zero(), scale: 0 }))))),
    },
    NativeClass {
        name: "java/math/RoundingMode",
        super_name: Some("java/lang/Enum"),
        interfaces: &[],
        access: PUBLIC | FINAL | 0x4000,
        methods: natives![
            "values" "()[Ljava/math/RoundingMode;" S => Some(|vm, _| enum_values(vm, "java/math/RoundingMode")),
            "valueOf" "(Ljava/lang/String;)Ljava/math/RoundingMode;" S => Some(|vm, a| { let c = vm.load("java/math/RoundingMode")?; let m = vm.mirror(c)?; vm.invoke_static("java/lang/Enum", "valueOf", "(Ljava/lang/Class;Ljava/lang/String;)Ljava/lang/Enum;", &[Value::Ref(m), arg(a, 0)]) }),
            "valueOf" "(I)Ljava/math/RoundingMode;" S => Some(rounding_value_of_int),
        ],
        statics: &[
            ("UP", "Ljava/math/RoundingMode;"),
            ("DOWN", "Ljava/math/RoundingMode;"),
            ("CEILING", "Ljava/math/RoundingMode;"),
            ("FLOOR", "Ljava/math/RoundingMode;"),
            ("HALF_UP", "Ljava/math/RoundingMode;"),
            ("HALF_DOWN", "Ljava/math/RoundingMode;"),
            ("HALF_EVEN", "Ljava/math/RoundingMode;"),
            ("UNNECESSARY", "Ljava/math/RoundingMode;"),
            ("$VALUES", "[Ljava/math/RoundingMode;"),
        ],
        clinit: Some(|vm, _| make_enum_constants(vm, "java/math/RoundingMode", ROUNDING_NAMES)),
        data: None,
    },
    NativeClass {
        name: "java/math/MathContext",
        super_name: Some("java/lang/Object"),
        interfaces: &["java/io/Serializable"],
        access: PUBLIC | FINAL,
        methods: natives![
            "<init>" "(I)V" M => Some(math_context_init),
            "<init>" "(ILjava/math/RoundingMode;)V" M => Some(math_context_init),
            "getPrecision" "()I" M => Some(|vm, a| math_context_op(vm, a, "getPrecision")),
            "getRoundingMode" "()Ljava/math/RoundingMode;" M => Some(|vm, a| math_context_op(vm, a, "getRoundingMode")),
            "toString" "()Ljava/lang/String;" M => Some(|vm, a| math_context_op(vm, a, "toString")),
            "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| math_context_op(vm, a, "equals")),
            "hashCode" "()I" M => Some(|vm, a| math_context_op(vm, a, "hashCode")),
        ],
        statics: &[("UNLIMITED", "Ljava/math/MathContext;"), ("DECIMAL32", "Ljava/math/MathContext;"), ("DECIMAL64", "Ljava/math/MathContext;"), ("DECIMAL128", "Ljava/math/MathContext;")],
        clinit: Some(math_context_clinit),
        data: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(s: &str) -> Big {
        parse_decimal(s).unwrap()
    }

    #[test]
    fn decimal_text_follows_java_notation_rules() {
        // Values from Java's BigDecimal.toString documentation.
        assert_eq!(sci_string(&Big { int: BigInt::from(123), scale: 0 }), "123");
        assert_eq!(sci_string(&Big { int: BigInt::from(-123), scale: 0 }), "-123");
        assert_eq!(sci_string(&Big { int: BigInt::from(123), scale: -1 }), "1.23E+3");
        assert_eq!(sci_string(&Big { int: BigInt::from(123), scale: -3 }), "1.23E+5");
        assert_eq!(sci_string(&Big { int: BigInt::from(123), scale: 1 }), "12.3");
        assert_eq!(sci_string(&Big { int: BigInt::from(123), scale: 5 }), "0.00123");
        assert_eq!(sci_string(&Big { int: BigInt::from(123), scale: 10 }), "1.23E-8");
        assert_eq!(sci_string(&Big { int: BigInt::from(-123), scale: 12 }), "-1.23E-10");
    }

    #[test]
    fn half_even_rounds_ties_to_even() {
        let r = set_scale(&dec("2.5"), 0, Rounding::HalfEven).unwrap();
        assert_eq!(r.int, BigInt::from(2));
        let r = set_scale(&dec("-2.5"), 0, Rounding::HalfUp).unwrap();
        assert_eq!(r.int, BigInt::from(-3));
        assert!(set_scale(&dec("1.5"), 0, Rounding::Unnecessary).is_none());
    }

    #[test]
    fn exact_double_is_the_binary_value() {
        assert_eq!(plain_string(&exact_double(0.1)), "0.1000000000000000055511151231257827021181583404541015625");
    }

    #[test]
    fn biginteger_hash_matches_java() {
        // new BigInteger("123456789012345678901234567890").hashCode() on HotSpot.
        let i: BigInt = "123456789012345678901234567890".parse().unwrap();
        assert_eq!(java_hash(&i), 1915528825);
    }
}
