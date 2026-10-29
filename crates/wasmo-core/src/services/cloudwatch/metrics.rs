//! Metric storage, period aggregation, statistics, and metric math.

use std::collections::BTreeMap;

pub type Dims = Vec<(String, String)>;

#[derive(Debug, Clone, Default)]
pub struct Point {
    pub ts: f64,
    pub count: f64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
    pub values: Vec<f64>,
    pub unit: String,
}

impl Point {
    pub fn single(ts: f64, value: f64, unit: &str) -> Self {
        Self {
            ts,
            count: 1.0,
            sum: value,
            min: value,
            max: value,
            values: vec![value],
            unit: unit.to_string(),
        }
    }
}

/// Points of one period merged into a single sample set.
#[derive(Debug, Clone, Default)]
pub struct Bucket {
    pub count: f64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
    pub values: Vec<f64>,
}

impl Bucket {
    fn add(&mut self, p: &Point) {
        if self.count == 0.0 {
            self.min = p.min;
            self.max = p.max;
        } else {
            self.min = self.min.min(p.min);
            self.max = self.max.max(p.max);
        }
        self.count += p.count;
        self.sum += p.sum;
        self.values.extend(&p.values);
    }

    /// Value of `stat` (`Average`, `Sum`, `SampleCount`, `Minimum`, `Maximum`,
    /// `pNN`), or `None` for a statistic the samples cannot answer.
    pub fn stat(&self, stat: &str) -> Option<f64> {
        Some(match stat {
            "Average" | "Avg" => self.sum / self.count,
            "Sum" => self.sum,
            "SampleCount" => self.count,
            "Minimum" | "Min" => self.min,
            "Maximum" | "Max" => self.max,
            p => {
                let q: f64 = p.strip_prefix(['p', 'P'])?.parse().ok()?;
                if self.values.is_empty() || !(0.0..=100.0).contains(&q) {
                    return None;
                }
                let mut v = self.values.clone();
                v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let rank = ((q / 100.0) * v.len() as f64).ceil().max(1.0) as usize;
                v[rank.min(v.len()) - 1]
            }
        })
    }
}

/// Groups `points` in `[start, end)` into `period`-second buckets counted from
/// `start`, keyed by bucket start.
pub fn aggregate<'a>(
    points: impl Iterator<Item = &'a Point>,
    start: f64,
    end: f64,
    period: f64,
    unit: Option<&str>,
) -> BTreeMap<i64, Bucket> {
    let mut out: BTreeMap<i64, Bucket> = BTreeMap::new();
    for p in points {
        if p.ts < start || p.ts >= end || unit.is_some_and(|u| u != p.unit) {
            continue;
        }
        let at = start + ((p.ts - start) / period).floor() * period;
        out.entry(at as i64).or_default().add(p);
    }
    out
}

#[derive(Debug, Clone)]
pub enum Val {
    Scalar(f64),
    Series(BTreeMap<i64, f64>),
}

fn combine(a: Val, b: Val, f: fn(f64, f64) -> f64) -> Val {
    match (a, b) {
        (Val::Scalar(x), Val::Scalar(y)) => Val::Scalar(f(x, y)),
        (Val::Series(s), Val::Scalar(y)) => {
            Val::Series(s.into_iter().map(|(t, x)| (t, f(x, y))).collect())
        }
        (Val::Scalar(x), Val::Series(s)) => {
            Val::Series(s.into_iter().map(|(t, y)| (t, f(x, y))).collect())
        }
        (Val::Series(a), Val::Series(b)) => Val::Series(
            a.into_iter()
                .filter_map(|(t, x)| Some((t, f(x, *b.get(&t)?))))
                .collect(),
        ),
    }
}

struct Math<'a> {
    toks: Vec<String>,
    pos: usize,
    vars: &'a BTreeMap<String, Val>,
}

fn tokenize(expr: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = expr.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '.')
            {
                i += 1;
            }
            out.push(chars[start..i].iter().collect());
        } else {
            out.push(c.to_string());
            i += 1;
        }
    }
    out
}

impl Math<'_> {
    fn peek(&self) -> Option<&str> {
        self.toks.get(self.pos).map(String::as_str)
    }

    fn eat(&mut self, t: &str) -> Result<(), String> {
        if self.peek() == Some(t) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected {t} in metric math expression"))
        }
    }

    fn expr(&mut self) -> Result<Val, String> {
        let mut v = self.term()?;
        while let Some(op @ ("+" | "-")) = self.peek() {
            let add = op == "+";
            self.pos += 1;
            let r = self.term()?;
            v = combine(v, r, if add { |a, b| a + b } else { |a, b| a - b });
        }
        Ok(v)
    }

    fn term(&mut self) -> Result<Val, String> {
        let mut v = self.factor()?;
        while let Some(op @ ("*" | "/")) = self.peek() {
            let mul = op == "*";
            self.pos += 1;
            let r = self.factor()?;
            v = combine(v, r, if mul { |a, b| a * b } else { |a, b| a / b });
        }
        Ok(v)
    }

    fn list(&mut self) -> Result<Vec<Val>, String> {
        if self
            .peek()
            .is_some_and(|t| t.eq_ignore_ascii_case("METRICS"))
        {
            self.pos += 1;
            self.eat("(")?;
            self.eat(")")?;
            return Ok(self.vars.values().cloned().collect());
        }
        let mut items = Vec::new();
        if self.peek() == Some("[") {
            self.pos += 1;
            loop {
                items.push(self.expr()?);
                match self.peek() {
                    Some(",") => self.pos += 1,
                    _ => break,
                }
            }
            self.eat("]")?;
        } else {
            items.push(self.expr()?);
        }
        Ok(items)
    }

    fn factor(&mut self) -> Result<Val, String> {
        let t = self
            .peek()
            .ok_or("unexpected end of metric math expression")?
            .to_string();
        self.pos += 1;
        if t == "(" {
            let v = self.expr()?;
            self.eat(")")?;
            return Ok(v);
        }
        if t == "-" {
            return Ok(combine(Val::Scalar(-1.0), self.factor()?, |a, b| a * b));
        }
        if let Ok(n) = t.parse::<f64>() {
            return Ok(Val::Scalar(n));
        }
        if self.peek() == Some("(") {
            self.pos += 1;
            let args = if self.peek() == Some(")") {
                Vec::new()
            } else {
                self.list()?
            };
            self.eat(")")?;
            return function(&t.to_ascii_uppercase(), args, self.vars);
        }
        self.vars
            .get(&t)
            .cloned()
            .ok_or_else(|| format!("Unrecognized metric math identifier: {t}"))
    }
}

fn function(name: &str, args: Vec<Val>, vars: &BTreeMap<String, Val>) -> Result<Val, String> {
    let args = if name == "METRICS" || args.is_empty() {
        vars.values().cloned().collect()
    } else {
        args
    };
    let reduce = |f: fn(&[f64]) -> f64| -> Val {
        let mut by_ts: BTreeMap<i64, Vec<f64>> = BTreeMap::new();
        let mut scalars = Vec::new();
        for a in &args {
            match a {
                Val::Scalar(x) => scalars.push(*x),
                Val::Series(s) => s
                    .iter()
                    .for_each(|(t, x)| by_ts.entry(*t).or_default().push(*x)),
            }
        }
        if by_ts.is_empty() {
            return Val::Scalar(f(&scalars));
        }
        Val::Series(
            by_ts
                .into_iter()
                .map(|(t, mut v)| {
                    v.extend(&scalars);
                    (t, f(&v))
                })
                .collect(),
        )
    };
    let single = |f: fn(f64) -> f64| match args.first() {
        Some(Val::Scalar(x)) => Ok(Val::Scalar(f(*x))),
        Some(Val::Series(s)) => Ok(Val::Series(s.iter().map(|(t, x)| (*t, f(*x))).collect())),
        None => Err(format!("{name} expects an argument")),
    };
    match name {
        "SUM" => Ok(reduce(|v| v.iter().sum())),
        "AVG" => Ok(reduce(|v| v.iter().sum::<f64>() / v.len() as f64)),
        "MIN" => Ok(reduce(|v| v.iter().copied().fold(f64::INFINITY, f64::min))),
        "MAX" => Ok(reduce(|v| {
            v.iter().copied().fold(f64::NEG_INFINITY, f64::max)
        })),
        "ABS" => single(f64::abs),
        "CEIL" => single(f64::ceil),
        "FLOOR" => single(f64::floor),
        "METRICS" => Ok(reduce(|v| v.iter().sum())),
        _ => Err(format!("Unsupported metric math function: {name}")),
    }
}

pub fn eval(expr: &str, vars: &BTreeMap<String, Val>) -> Result<Val, String> {
    let mut m = Math {
        toks: tokenize(expr),
        pos: 0,
        vars,
    };
    let v = m.expr()?;
    if m.pos != m.toks.len() {
        return Err(format!(
            "Unexpected token in metric math expression: {expr}"
        ));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points() -> Vec<Point> {
        [
            (0.0, 1.0),
            (30.0, 3.0),
            (60.0, 10.0),
            (119.0, 20.0),
            (120.0, 5.0),
        ]
        .iter()
        .map(|(t, v)| Point::single(*t, *v, "Count"))
        .collect()
    }

    #[test]
    fn aggregates_points_into_periods() {
        let pts = points();
        let b = aggregate(pts.iter(), 0.0, 180.0, 60.0, None);
        assert_eq!(b.keys().copied().collect::<Vec<_>>(), [0, 60, 120]);
        assert_eq!(b[&0].stat("Sum"), Some(4.0));
        assert_eq!(b[&0].stat("Average"), Some(2.0));
        assert_eq!(b[&60].stat("Maximum"), Some(20.0));
        assert_eq!(b[&60].stat("Minimum"), Some(10.0));
        assert_eq!(b[&120].stat("SampleCount"), Some(1.0));
        let wide = aggregate(pts.iter(), 0.0, 120.0, 3600.0, None);
        assert_eq!(wide[&0].stat("Sum"), Some(34.0));
        assert!(aggregate(pts.iter(), 0.0, 180.0, 60.0, Some("Bytes")).is_empty());
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let mut b = Bucket::default();
        for v in 1..=100 {
            b.add(&Point::single(0.0, v as f64, "None"));
        }
        assert_eq!(b.stat("p50"), Some(50.0));
        assert_eq!(b.stat("p99"), Some(99.0));
        assert_eq!(b.stat("p100"), Some(100.0));
        assert_eq!(b.stat("bogus"), None);
    }

    #[test]
    fn metric_math_combines_series_and_scalars() {
        let mut vars = BTreeMap::new();
        vars.insert(
            "a".to_string(),
            Val::Series(BTreeMap::from([(0, 2.0), (60, 4.0)])),
        );
        vars.insert(
            "b".to_string(),
            Val::Series(BTreeMap::from([(0, 1.0), (120, 9.0)])),
        );
        let Val::Series(s) = eval("(a + b) * 10", &vars).unwrap() else {
            panic!()
        };
        assert_eq!(s, BTreeMap::from([(0, 30.0)]));
        let Val::Series(s) = eval("a / 2 - 1", &vars).unwrap() else {
            panic!()
        };
        assert_eq!(s, BTreeMap::from([(0, 0.0), (60, 1.0)]));
        let Val::Series(s) = eval("SUM([a, b])", &vars).unwrap() else {
            panic!()
        };
        assert_eq!(s, BTreeMap::from([(0, 3.0), (60, 4.0), (120, 9.0)]));
        let Val::Series(s) = eval("MAX(METRICS())", &vars).unwrap() else {
            panic!()
        };
        assert_eq!(s[&0], 2.0);
        assert!(eval("a +", &vars).is_err());
        assert!(eval("nope * 2", &vars).is_err());
    }
}
