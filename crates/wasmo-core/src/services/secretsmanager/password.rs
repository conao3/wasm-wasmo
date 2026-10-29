use super::*;

const LOWERCASE: &str = "abcdefghijklmnopqrstuvwxyz";
const UPPERCASE: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const DIGITS: &str = "0123456789";
const PUNCTUATION: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~";

fn below(ctx: &mut Ctx<'_>, n: usize) -> usize {
    let n = n as u32;
    let zone = u32::MAX - u32::MAX % n;
    loop {
        let mut b = [0u8; 4];
        ctx.random_bytes(&mut b);
        let x = u32::from_le_bytes(b);
        if x < zone {
            return (x % n) as usize;
        }
    }
}

pub(super) fn arn_suffix(ctx: &mut Ctx<'_>) -> String {
    const ALNUM: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    (0..6)
        .map(|_| ALNUM[below(ctx, ALNUM.len())] as char)
        .collect()
}

pub(super) fn random_password(ctx: &mut Ctx<'_>, input: &Value) -> Result<String, AwsError> {
    let length = input.i64_field("PasswordLength").unwrap_or(32);
    if !(1..=4096).contains(&length) {
        return Err(validation(format!(
            "Value '{length}' at 'passwordLength' failed to satisfy constraint: Member must have value between 1 and 4096"
        )));
    }
    let length = length as usize;
    let exclude = input.str_field("ExcludeCharacters").unwrap_or_default();
    let mut classes: Vec<Vec<char>> = [
        ("ExcludeLowercase", LOWERCASE),
        ("ExcludeUppercase", UPPERCASE),
        ("ExcludeNumbers", DIGITS),
        ("ExcludePunctuation", PUNCTUATION),
    ]
    .into_iter()
    .filter(|(key, _)| flag(input, key) != Some(true))
    .map(|(_, set)| set.chars().filter(|c| !exclude.contains(*c)).collect())
    .filter(|set: &Vec<char>| !set.is_empty())
    .collect();
    let mut pool: Vec<char> = classes.concat();
    if flag(input, "IncludeSpace") == Some(true) && !exclude.contains(' ') {
        pool.push(' ');
    }
    if pool.is_empty() {
        return Err(invalid_param(
            "The password can't be generated because all characters are excluded.",
        ));
    }
    if flag(input, "RequireEachIncludedType") == Some(false) {
        classes.clear();
    }
    if classes.len() > length {
        return Err(invalid_param(
            "The password length is too short to include every required character type.",
        ));
    }
    let mut chars: Vec<char> = classes
        .iter()
        .map(|set| set[below(ctx, set.len())])
        .collect();
    while chars.len() < length {
        chars.push(pool[below(ctx, pool.len())]);
    }
    for i in (1..chars.len()).rev() {
        let j = below(ctx, i + 1);
        chars.swap(i, j);
    }
    Ok(chars.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::FixedEnv;

    fn env() -> FixedEnv {
        FixedEnv {
            now_ms: 0.0,
            seed: 7,
        }
    }

    #[test]
    fn password_respects_classes_and_exclusions() {
        let mut env = env();
        let mut ctx = Ctx::new("a", "r", "e", &mut env);
        for _ in 0..50 {
            let p = random_password(&mut ctx, &json!({ "PasswordLength": 4 })).unwrap();
            assert_eq!(p.len(), 4);
            assert!(p.chars().any(|c| c.is_ascii_lowercase()), "{p}");
            assert!(p.chars().any(|c| c.is_ascii_uppercase()), "{p}");
            assert!(p.chars().any(|c| c.is_ascii_digit()), "{p}");
            assert!(p.chars().any(|c| PUNCTUATION.contains(c)), "{p}");
        }
        let p = random_password(
            &mut ctx,
            &json!({
                "PasswordLength": 200,
                "ExcludePunctuation": true,
                "ExcludeUppercase": true,
                "ExcludeCharacters": "abc0",
            }),
        )
        .unwrap();
        assert!(
            p.chars()
                .all(|c| (c.is_ascii_lowercase() || c.is_ascii_digit()) && !"abc0".contains(c)),
            "{p}"
        );
        let p = random_password(
            &mut ctx,
            &json!({ "PasswordLength": 300, "IncludeSpace": true, "ExcludeLowercase": true, "ExcludeUppercase": true, "ExcludeNumbers": true }),
        )
        .unwrap();
        assert!(p.contains(' '));
        assert!(p.chars().all(|c| c == ' ' || PUNCTUATION.contains(c)));
    }

    #[test]
    fn password_rejects_impossible_requests() {
        let mut env = env();
        let mut ctx = Ctx::new("a", "r", "e", &mut env);
        let code = |ctx: &mut Ctx<'_>, input: Value| random_password(ctx, &input).unwrap_err().code;
        assert_eq!(
            code(&mut ctx, json!({ "PasswordLength": 3 })),
            "InvalidParameterException"
        );
        assert_eq!(
            code(&mut ctx, json!({ "PasswordLength": 0 })),
            "ValidationException"
        );
        assert_eq!(
            code(
                &mut ctx,
                json!({ "ExcludeLowercase": true, "ExcludeUppercase": true, "ExcludeNumbers": true, "ExcludePunctuation": true })
            ),
            "InvalidParameterException"
        );
        let p = random_password(
            &mut ctx,
            &json!({ "PasswordLength": 1, "RequireEachIncludedType": false }),
        )
        .unwrap();
        assert_eq!(p.len(), 1);
    }
}
