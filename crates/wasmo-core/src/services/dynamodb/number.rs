use std::cmp::Ordering;

use crate::codec::AwsError;

use super::validation;

const MAX_DIGITS: usize = 38;
const MAX_EXPONENT: i64 = 125;
const MIN_EXPONENT: i64 = -130;

/// A DynamoDB number: `digits × 10^exp`, with no leading or trailing zero
/// digits. Zero has no digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Num {
    neg: bool,
    digits: Vec<u8>,
    exp: i64,
}

impl Num {
    pub fn parse(s: &str) -> Result<Self, AwsError> {
        let invalid = || {
            validation(format!(
                "The parameter cannot be converted to a numeric value: {s}"
            ))
        };
        let bytes = s.as_bytes();
        let mut i = 0;
        let mut neg = false;
        if matches!(bytes.first(), Some(b'+' | b'-')) {
            neg = bytes[0] == b'-';
            i = 1;
        }
        let mut digits = Vec::new();
        let mut frac_len: i64 = 0;
        let mut seen_digit = false;
        let mut seen_dot = false;
        while i < bytes.len() {
            match bytes[i] {
                c @ b'0'..=b'9' => {
                    digits.push(c - b'0');
                    seen_digit = true;
                    if seen_dot {
                        frac_len += 1;
                    }
                }
                b'.' if !seen_dot => seen_dot = true,
                b'e' | b'E' => break,
                _ => return Err(invalid()),
            }
            i += 1;
        }
        if !seen_digit {
            return Err(invalid());
        }
        let mut exp: i64 = 0;
        if i < bytes.len() {
            let rest = &s[i + 1..];
            let body = rest.strip_prefix(['+', '-']).unwrap_or(rest);
            if body.is_empty() || !body.bytes().all(|c| c.is_ascii_digit()) || body.len() > 6 {
                return Err(invalid());
            }
            exp = rest.parse().map_err(|_| invalid())?;
        }
        Self::from_parts(neg, digits, exp - frac_len)
    }

    fn from_parts(neg: bool, mut digits: Vec<u8>, mut exp: i64) -> Result<Self, AwsError> {
        let lead = digits.iter().take_while(|d| **d == 0).count();
        digits.drain(..lead);
        while digits.last() == Some(&0) {
            digits.pop();
            exp += 1;
        }
        if digits.is_empty() {
            return Ok(Self::zero());
        }
        if digits.len() > MAX_DIGITS {
            return Err(validation(
                "Attempting to store more than 38 significant digits in a Number",
            ));
        }
        let magnitude = exp + digits.len() as i64 - 1;
        if magnitude > MAX_EXPONENT {
            return Err(validation(
                "Number overflow. Attempting to store a number with magnitude larger than supported range",
            ));
        }
        if magnitude < MIN_EXPONENT {
            return Err(validation(
                "Number underflow. Attempting to store a number with magnitude smaller than supported range",
            ));
        }
        Ok(Self { neg, digits, exp })
    }

    fn zero() -> Self {
        Self {
            neg: false,
            digits: Vec::new(),
            exp: 0,
        }
    }

    pub fn is_zero(&self) -> bool {
        self.digits.is_empty()
    }

    pub fn significant_digits(&self) -> usize {
        self.digits.len()
    }

    pub fn to_f64(&self) -> f64 {
        self.to_string().parse().unwrap_or(0.0)
    }

    pub fn add(&self, other: &Self) -> Result<Self, AwsError> {
        if self.is_zero() {
            return Ok(other.clone());
        }
        if other.is_zero() {
            return Ok(self.clone());
        }
        let exp = self.exp.min(other.exp);
        let a = self.aligned(exp);
        let b = other.aligned(exp);
        let (neg, mag) = if self.neg == other.neg {
            (self.neg, add_mag(&a, &b))
        } else {
            match cmp_mag(&a, &b) {
                Ordering::Equal => return Ok(Self::zero()),
                Ordering::Greater => (self.neg, sub_mag(&a, &b)),
                Ordering::Less => (other.neg, sub_mag(&b, &a)),
            }
        };
        let digits: Vec<u8> = mag.into_iter().rev().collect();
        Self::from_parts(neg, digits, exp)
    }

    pub fn sub(&self, other: &Self) -> Result<Self, AwsError> {
        let mut negated = other.clone();
        if !negated.is_zero() {
            negated.neg = !negated.neg;
        }
        self.add(&negated)
    }

    fn aligned(&self, exp: i64) -> Vec<u8> {
        let mut out = vec![0u8; (self.exp - exp) as usize];
        out.extend(self.digits.iter().rev());
        out
    }

    fn cmp_magnitude(&self, other: &Self) -> Ordering {
        match (self.is_zero(), other.is_zero()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        let ma = self.exp + self.digits.len() as i64;
        let mb = other.exp + other.digits.len() as i64;
        ma.cmp(&mb).then_with(|| self.digits.cmp(&other.digits))
    }
}

fn add_mag(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
    let mut carry = 0;
    for i in 0..a.len().max(b.len()) {
        let s = a.get(i).copied().unwrap_or(0) + b.get(i).copied().unwrap_or(0) + carry;
        out.push(s % 10);
        carry = s / 10;
    }
    if carry > 0 {
        out.push(carry);
    }
    out
}

fn sub_mag(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i8;
    for (i, &x) in a.iter().enumerate() {
        let mut d = x as i8 - b.get(i).copied().unwrap_or(0) as i8 - borrow;
        borrow = 0;
        if d < 0 {
            d += 10;
            borrow = 1;
        }
        out.push(d as u8);
    }
    out
}

fn cmp_mag(a: &[u8], b: &[u8]) -> Ordering {
    let trim = |v: &[u8]| v.len() - v.iter().rev().take_while(|d| **d == 0).count();
    let (la, lb) = (trim(a), trim(b));
    la.cmp(&lb)
        .then_with(|| a[..la].iter().rev().cmp(b[..lb].iter().rev()))
}

impl Ord for Num {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.neg, other.neg) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => self.cmp_magnitude(other),
            (true, true) => other.cmp_magnitude(self),
        }
    }
}

impl PartialOrd for Num {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for Num {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_zero() {
            return f.write_str("0");
        }
        if self.neg {
            f.write_str("-")?;
        }
        let s: String = self.digits.iter().map(|d| (b'0' + d) as char).collect();
        if self.exp >= 0 {
            write!(f, "{s}{}", "0".repeat(self.exp as usize))
        } else {
            let point = s.len() as i64 + self.exp;
            if point > 0 {
                write!(f, "{}.{}", &s[..point as usize], &s[point as usize..])
            } else {
                write!(f, "0.{}{s}", "0".repeat((-point) as usize))
            }
        }
    }
}

pub fn normalize(s: &str) -> Result<String, AwsError> {
    Ok(Num::parse(s)?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Num {
        Num::parse(s).unwrap()
    }

    #[test]
    fn normalizes_like_dynamodb() {
        for (input, want) in [
            ("0", "0"),
            ("-0", "0"),
            ("00012.3400", "12.34"),
            ("1e2", "100"),
            ("1.5E-3", "0.0015"),
            ("+7", "7"),
            ("-.5", "-0.5"),
            (
                "12345678901234567890123456789012345678",
                "12345678901234567890123456789012345678",
            ),
            ("1E125", &format!("1{}", "0".repeat(125))),
        ] {
            assert_eq!(normalize(input).unwrap(), want, "{input}");
        }
    }

    #[test]
    fn rejects_invalid_numbers() {
        for input in [
            "",
            "abc",
            "1.2.3",
            "1e",
            "--1",
            "123456789012345678901234567890123456789",
            "1e126",
            "1e-131",
            " 1",
        ] {
            assert!(Num::parse(input).is_err(), "{input}");
        }
    }

    #[test]
    fn arithmetic_keeps_precision() {
        assert_eq!(n("0.1").add(&n("0.2")).unwrap().to_string(), "0.3");
        assert_eq!(n("1").sub(&n("1.0001")).unwrap().to_string(), "-0.0001");
        assert_eq!(n("-5").add(&n("5")).unwrap().to_string(), "0");
        assert_eq!(
            n("1e37").add(&n("0.1")).unwrap_err().message,
            "Attempting to store more than 38 significant digits in a Number"
        );
        assert_eq!(
            n("1e100").add(&n("1e100")).unwrap().to_string(),
            format!("2{}", "0".repeat(100))
        );
    }

    #[test]
    fn orders_numerically() {
        let mut v = [n("10"), n("-1"), n("2.5"), n("0"), n("-10.5"), n("1e-3")];
        v.sort();
        let s: Vec<String> = v.iter().map(Num::to_string).collect();
        assert_eq!(s, ["-10.5", "-1", "0", "0.001", "2.5", "10"]);
        assert_eq!(n("1.0"), n("1"));
    }
}
