//! `conv jwt`: what a JSON Web Token says, when it expires, and whether an
//! HMAC signature is right. The token never leaves this process.

use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::DecodePaddingMode;
use base64::{alphabet, Engine};
use hmac::{Hmac, Mac};
use jiff::Timestamp;
use serde_json::{Map, Value};
use sha2::{Sha256, Sha384, Sha512};

/// A decoded token.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub header: Map<String, Value>,
    pub claims: Map<String, Value>,
    /// The header's `alg`, if it has one.
    pub alg: Option<String>,
    /// `iat`, `nbf` and `exp`, in that order, where present and numbers.
    pub times: Vec<(&'static str, Timestamp)>,
    /// What is odd about the token without making it unreadable.
    pub warnings: Vec<String>,
    signing_input: String,
    signature: Vec<u8>,
}

/// Where a token stands against the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// `exp` has passed.
    Expired(Timestamp),
    /// `nbf` hasn't come yet.
    NotYet(Timestamp),
    /// Valid now, until `exp`.
    ValidUntil(Timestamp),
    /// No `exp`: it never expires by itself.
    NoExpiry,
}

/// What checking the signature found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signature {
    Valid,
    Invalid,
}

const TIME_CLAIMS: [&str; 3] = ["iat", "nbf", "exp"];

/// Decodes `s`, which may carry a `Bearer ` prefix or quotes around it.
pub fn decode(s: &str) -> Result<Token, String> {
    let s = s.trim().trim_matches(|c| c == '"' || c == '\'').trim();
    let s = match s.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("bearer ") => s[7..].trim(),
        _ => s,
    };
    if s.is_empty() {
        return Err("there is no token to read".to_string());
    }
    let parts: Vec<&str> = s.split('.').collect();
    match parts.len() {
        3 => {}
        5 => {
            return Err(
                "this is an encrypted token (JWE, five parts): its claims can't be read without the key, and conv reads signed tokens (JWS, three parts)"
                    .to_string(),
            )
        }
        n => {
            return Err(format!(
                "this isn't a JWT: a JWT is three parts joined by dots (header.claims.signature), and this has {n}"
            ))
        }
    }
    let header = object(parts[0], "header", 1)?;
    let claims = object(parts[1], "claims", 2)?;
    let signature =
        segment(parts[2]).map_err(|e| format!("the signature (part 3) isn't base64url: {e}"))?;
    let alg = header
        .get("alg")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut warnings = Vec::new();
    if parts.iter().take(2).any(|p| p.contains(['+', '/', '='])) {
        warnings.push(
            "the token uses standard base64 or padding where a JWT has base64url without it; some libraries will refuse it"
                .to_string(),
        );
    }
    let mut times = Vec::new();
    for name in TIME_CLAIMS {
        let Some(v) = claims.get(name) else { continue };
        let Some(secs) = v.as_f64() else {
            warnings.push(format!(
                "{name} is {v}, not a number of seconds, so it can't be read as a time"
            ));
            continue;
        };
        // Seconds are the standard; a value this large is milliseconds by
        // mistake, which most libraries would read as thousands of years
        // from now.
        if secs.abs() >= 1e11 {
            warnings.push(format!(
                "{name} is {v}, which looks like milliseconds; a JWT counts seconds, so to a library this is tens of thousands of years away"
            ));
        }
        match Timestamp::from_nanosecond((secs * 1e9) as i128) {
            Ok(ts) => times.push((name, ts)),
            Err(_) => warnings.push(format!("{name} is {v}, which is past any date")),
        }
    }
    Ok(Token {
        header,
        claims,
        alg,
        times,
        warnings,
        signing_input: format!("{}.{}", parts[0], parts[1]),
        signature,
    })
}

/// base64url, as JWTs carry it; the standard alphabet and padding are
/// read too, since some issuers send them.
fn segment(s: &str) -> Result<Vec<u8>, String> {
    let lenient =
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent);
    let alphabet = if s.contains(['+', '/']) {
        &alphabet::STANDARD
    } else {
        &alphabet::URL_SAFE
    };
    GeneralPurpose::new(alphabet, lenient)
        .decode(s)
        .map_err(|e| match e {
            base64::DecodeError::InvalidByte(i, b) => {
                format!("{:?} at character {} doesn't belong", b as char, i + 1)
            }
            base64::DecodeError::InvalidLength(_) => {
                "its length can't be base64, so it was probably cut short".to_string()
            }
            base64::DecodeError::InvalidLastSymbol(..) => {
                "it ends in a character that leaves bits over, so it was probably cut short"
                    .to_string()
            }
            base64::DecodeError::InvalidPadding => "its padding is wrong".to_string(),
        })
}

fn object(part: &str, what: &str, n: usize) -> Result<Map<String, Value>, String> {
    let bytes = segment(part).map_err(|e| format!("the {what} (part {n}) isn't base64url: {e}"))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        if what == "claims" {
            "the claims (part 2) aren't JSON; if the header says cty JWT, they are another token inside this one"
                .to_string()
        } else {
            format!("the {what} (part {n}) isn't JSON")
        }
    })?;
    match value {
        Value::Object(map) => Ok(map),
        other => Err(format!(
            "the {what} (part {n}) is JSON but not an object: {}",
            serde_json::to_string(&other).unwrap_or_default()
        )),
    }
}

impl Token {
    pub fn time(&self, name: &str) -> Option<Timestamp> {
        self.times.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
    }

    /// Where the token stands at `now`, with no leeway.
    pub fn status(&self, now: Timestamp) -> Status {
        if let Some(exp) = self.time("exp") {
            if now >= exp {
                return Status::Expired(exp);
            }
        }
        if let Some(nbf) = self.time("nbf") {
            if now < nbf {
                return Status::NotYet(nbf);
            }
        }
        match self.time("exp") {
            Some(exp) => Status::ValidUntil(exp),
            None => Status::NoExpiry,
        }
    }

    /// Checks an HMAC signature (HS256, HS384, HS512) with `secret`. Any
    /// other algorithm is an error that says why it can't be checked here.
    pub fn verify(&self, secret: &[u8]) -> Result<Signature, String> {
        let alg = self
            .alg
            .as_deref()
            .ok_or("the header names no alg, so there is no way to check the signature")?;
        let input = self.signing_input.as_bytes();
        let ok = match alg {
            "HS256" => mac::<Hmac<Sha256>>(secret, input, &self.signature),
            "HS384" => mac::<Hmac<Sha384>>(secret, input, &self.signature),
            "HS512" => mac::<Hmac<Sha512>>(secret, input, &self.signature),
            "none" | "None" | "NONE" => {
                return Err(
                    "the token is unsigned (alg none), so there is no signature to check, and nothing should accept it"
                        .to_string(),
                )
            }
            other if other.starts_with("RS")
                || other.starts_with("PS")
                || other.starts_with("ES")
                || other == "EdDSA" =>
            {
                return Err(format!(
                    "the token is signed with {other}, which is checked with the issuer's public key; conv checks HMAC signatures (HS256, HS384, HS512)"
                ))
            }
            other => return Err(format!("conv doesn't know the alg {other:?}")),
        };
        Ok(if ok {
            Signature::Valid
        } else {
            Signature::Invalid
        })
    }

    /// Whether this token's signature is one a secret checks.
    pub fn is_hmac(&self) -> bool {
        matches!(self.alg.as_deref(), Some("HS256" | "HS384" | "HS512"))
    }
}

/// Compares in constant time, through the hmac crate's own check.
fn mac<M: Mac + hmac::digest::KeyInit>(secret: &[u8], input: &[u8], signature: &[u8]) -> bool {
    let Ok(mut m) = <M as hmac::digest::KeyInit>::new_from_slice(secret) else {
        return false;
    };
    m.update(input);
    m.verify_slice(signature).is_ok()
}

/// A signed token, for tests elsewhere.
#[cfg(test)]
pub(crate) fn sign_hs256(header: &str, claims: &str, secret: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header),
        URL_SAFE_NO_PAD.encode(claims)
    );
    let mut m = <Hmac<Sha256> as hmac::digest::KeyInit>::new_from_slice(secret).unwrap();
    m.update(input.as_bytes());
    let sig = URL_SAFE_NO_PAD.encode(m.finalize().into_bytes());
    format!("{input}.{sig}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// jwt.io's own example token, signed with "your-256-bit-secret".
    const JWT_IO: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";

    #[test]
    fn decodes_the_jwt_io_example() {
        let t = decode(JWT_IO).unwrap();
        assert_eq!(t.alg.as_deref(), Some("HS256"));
        assert_eq!(t.claims["name"], "John Doe");
        assert_eq!(t.time("iat").unwrap().to_string(), "2018-01-18T01:30:22Z");
        assert_eq!(t.status(Timestamp::now()), Status::NoExpiry);
        assert!(t.warnings.is_empty());
    }

    #[test]
    fn verifies_hmac_with_the_right_secret_only() {
        let t = decode(JWT_IO).unwrap();
        assert_eq!(t.verify(b"your-256-bit-secret"), Ok(Signature::Valid));
        assert_eq!(t.verify(b"wrong"), Ok(Signature::Invalid));
        let t = decode(&sign_hs256(r#"{"alg":"HS256"}"#, r#"{"a":1}"#, b"")).unwrap();
        assert_eq!(t.verify(b""), Ok(Signature::Valid));
    }

    #[test]
    fn a_tampered_claim_fails_the_signature() {
        let parts: Vec<&str> = JWT_IO.split('.').collect();
        let forged = format!(
            "{}.{}.{}",
            parts[0],
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(r#"{"sub":"1234567890","name":"Admin","iat":1516239022}"#),
            parts[2]
        );
        let t = decode(&forged).unwrap();
        assert_eq!(t.claims["name"], "Admin");
        assert_eq!(t.verify(b"your-256-bit-secret"), Ok(Signature::Invalid));
    }

    #[test]
    fn bearer_prefixes_and_quotes_are_ignored() {
        assert!(decode(&format!("Bearer {JWT_IO}")).is_ok());
        assert!(decode(&format!("  \"{JWT_IO}\"\n")).is_ok());
    }

    #[test]
    fn expiry_and_not_before_against_the_clock() {
        let t = decode(&sign_hs256(
            r#"{"alg":"HS256"}"#,
            r#"{"nbf":1000,"exp":2000}"#,
            b"k",
        ))
        .unwrap();
        let at = |s| Timestamp::from_second(s).unwrap();
        assert_eq!(t.status(at(500)), Status::NotYet(at(1000)));
        assert_eq!(t.status(at(1500)), Status::ValidUntil(at(2000)));
        // Expired at the second itself.
        assert_eq!(t.status(at(2000)), Status::Expired(at(2000)));
    }

    #[test]
    fn malformed_tokens_say_what_is_wrong() {
        assert!(decode("").unwrap_err().contains("no token"));
        assert!(decode("abc").unwrap_err().contains("has 1"));
        assert!(decode("a.b").unwrap_err().contains("has 2"));
        assert!(decode("a.b.c.d.e").unwrap_err().contains("encrypted"));
        let e = decode("eyJhbGc!.e30.").unwrap_err();
        assert!(e.contains("header (part 1) isn't base64url"), "{e}");
        let e = decode("aGVsbG8.e30.").unwrap_err();
        assert!(e.contains("header (part 1) isn't JSON"), "{e}");
        let e = decode("e30.aGVsbG8.").unwrap_err();
        assert!(e.contains("claims (part 2) aren't JSON"), "{e}");
        let e = decode("e30.WzEsMl0.").unwrap_err();
        assert!(e.contains("not an object"), "{e}");
    }

    #[test]
    fn odd_times_are_warned_about() {
        let t = decode(&sign_hs256(
            r#"{"alg":"HS256"}"#,
            r#"{"exp":"tomorrow","iat":1700000000000}"#,
            b"k",
        ))
        .unwrap();
        assert!(t.warnings.iter().any(|w| w.contains("exp is \"tomorrow\"")));
        assert!(t.warnings.iter().any(|w| w.contains("milliseconds")));
        assert_eq!(t.time("exp"), None);
    }

    #[test]
    fn other_algorithms_say_why_they_cannot_be_checked() {
        let none = "eyJhbGciOiJub25lIn0.eyJhIjoxfQ.";
        let t = decode(none).unwrap();
        assert!(t.verify(b"x").unwrap_err().contains("unsigned"));
        let rs = format!(
            "{}.e30.c2ln",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256"}"#)
        );
        let e = decode(&rs).unwrap().verify(b"x").unwrap_err();
        assert!(e.contains("public key"), "{e}");
    }
}
