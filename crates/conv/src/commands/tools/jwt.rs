//! `conv jwt`: what a JSON Web Token says, and whether its signature is
//! right. The token is read here and sent nowhere.

use convkit_core::tools::jwt::{self as j, Signature, Status, Token};
use convkit_core::tools::time as t;
use convkit_core::ConvError;
use jiff::Timestamp;
use serde_json::{json, Value};

use super::{Common, Done, Paint, Piped};
use crate::cli::Cli;

pub const ABOUT: &str = "\
Decodes a JSON Web Token: its header and claims, when it was issued and \
expires (iat, nbf, exp) in your time zone, and whether it has expired. \
The token is read here and sent nowhere.

--verify checks an HMAC signature (HS256, HS384, HS512): conv asks for \
the secret without showing it. --secret-env VAR reads the secret from an \
environment variable instead, for scripts. A token whose signature, or \
whose time, doesn't check out then exits 1.

A token signed with RS256, ES256 or another public-key algorithm needs \
the issuer's public key, which conv doesn't check.";

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// The token; a "Bearer " in front is fine. - reads it from what is
    /// piped in.
    pub token: Option<String>,
    /// Check the signature, asking for the secret without showing it.
    #[arg(long)]
    pub verify: bool,
    /// Check the signature with the secret in this environment variable.
    #[arg(long, value_name = "VAR", conflicts_with = "verify")]
    pub secret_env: Option<String>,
    /// Check the signature with this secret. It stays in your shell's
    /// history and other users can see it while conv runs, so --verify or
    /// --secret-env are better.
    #[arg(long, value_name = "SECRET", conflicts_with_all = ["verify", "secret_env"])]
    pub secret: Option<String>,
    #[command(flatten)]
    pub io: Common,
}

pub fn run(cli: &Cli, args: &Args) -> i32 {
    let text = match super::read(cli, args.token.as_deref(), &args.io, Piped::Read, "jwt")
        .and_then(|i| i.map(super::Input::text).transpose())
    {
        Ok(Some(t)) => t,
        Ok(None) => return super::fail(cli, &super::nothing("jwt", "a token")),
        Err(e) => return super::fail(cli, &e),
    };
    let token = match j::decode(&text) {
        Ok(t) => t,
        Err(e) => return super::fail(cli, &super::failed(e)),
    };
    for w in &token.warnings {
        super::warn(w);
    }
    let secret = match secret(args) {
        Ok(s) => s,
        Err(e) => return super::fail(cli, &e),
    };
    let signature = match &secret {
        Some(s) => match token.verify(s) {
            Ok(sig) => Some(sig),
            Err(e) => return super::fail(cli, &super::failed(e)),
        },
        None => None,
    };
    let now = Timestamp::now();
    let status = token.status(now);
    let human = human(&token, status, signature, now, Paint::stdout());
    // Checking says whether the token is good now: a right signature on an
    // expired token isn't.
    let bad_time = matches!(status, Status::Expired(_) | Status::NotYet(_));
    let code = match signature {
        Some(Signature::Invalid) => 1,
        Some(Signature::Valid) if bad_time => 1,
        _ => 0,
    };
    let claims = serde_json::to_string_pretty(&Value::Object(token.claims.clone())).unwrap();
    let json = json!({ "tokens": [json_of(&token, status, signature, now)] });
    super::finish(
        cli,
        args.io.copy,
        Done::new(human, Some(claims), json).code(code),
    )
}

/// The secret to check with, from wherever it was asked to come from.
fn secret(args: &Args) -> Result<Option<Vec<u8>>, ConvError> {
    if let Some(s) = &args.secret {
        super::warn(
            "a secret given on the command line stays in your shell's history, and other users can see it while conv runs; --verify asks for it instead, and --secret-env reads it from a variable",
        );
        return Ok(Some(s.as_bytes().to_vec()));
    }
    if let Some(var) = &args.secret_env {
        let value = std::env::var_os(var).ok_or_else(|| {
            super::usage(format!(
                "{var} isn't set, so there is no secret to check with"
            ))
        })?;
        return Ok(Some(os_bytes(value)));
    }
    if !args.verify {
        return Ok(None);
    }
    let term = console::Term::stderr();
    if !term.is_term() {
        return Err(super::usage(
            "--verify asks for the secret, and there is no terminal to ask on; use --secret-env VAR",
        ));
    }
    eprint!("secret (not shown): ");
    let s = term
        .read_secure_line()
        .map_err(|e| super::usage(format!("couldn't read the secret: {e}")))?;
    Ok(Some(s.into_bytes()))
}

#[cfg(unix)]
fn os_bytes(s: std::ffi::OsString) -> Vec<u8> {
    use std::os::unix::ffi::OsStringExt;
    s.into_vec()
}

#[cfg(not(unix))]
fn os_bytes(s: std::ffi::OsString) -> Vec<u8> {
    s.to_string_lossy().into_owned().into_bytes()
}

fn indent(json: &str) -> String {
    json.lines().map(|l| format!("  {l}\n")).collect()
}

fn when(at: Timestamp, now: Timestamp) -> String {
    let local = jiff::tz::TimeZone::system();
    format!("{} ({})", t::show(at, &local).human, t::relative(at, now))
}

fn human(
    token: &Token,
    status: Status,
    signature: Option<Signature>,
    now: Timestamp,
    paint: Paint,
) -> String {
    let mut out = String::new();
    out.push_str(&paint.bold("header"));
    out.push('\n');
    out.push_str(&indent(
        &serde_json::to_string_pretty(&Value::Object(token.header.clone())).unwrap(),
    ));
    out.push_str(&paint.bold("claims"));
    out.push('\n');
    out.push_str(&indent(
        &serde_json::to_string_pretty(&Value::Object(token.claims.clone())).unwrap(),
    ));
    out.push('\n');
    let mut rows: Vec<(String, String)> = Vec::new();
    if let Some(iat) = token.time("iat") {
        rows.push(("issued".to_string(), when(iat, now)));
    }
    if let Some(nbf) = token.time("nbf") {
        rows.push(("not before".to_string(), when(nbf, now)));
    }
    match status {
        Status::Expired(exp) => rows.push(("expired".to_string(), paint.red(&when(exp, now)))),
        Status::ValidUntil(exp) => rows.push(("expires".to_string(), when(exp, now))),
        Status::NotYet(_) | Status::NoExpiry => {
            if let Some(exp) = token.time("exp") {
                rows.push(("expires".to_string(), when(exp, now)));
            } else {
                rows.push(("expires".to_string(), "never: it has no exp".to_string()));
            }
        }
    }
    let status_text = match status {
        Status::Expired(_) => paint.red("expired"),
        Status::NotYet(_) => paint.yellow("not valid yet"),
        Status::ValidUntil(_) | Status::NoExpiry => paint.green("valid now"),
    };
    rows.push(("status".to_string(), status_text));
    // The header is the token's author's text: no raw escape codes to the terminal.
    let alg: String = token
        .alg
        .as_deref()
        .unwrap_or("none named")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    // The marks only where they are symbols; piped, the words say it.
    let mark = |m: String| {
        if paint.on {
            format!("{m} ")
        } else {
            String::new()
        }
    };
    let sig = match signature {
        Some(Signature::Valid) => {
            format!("{}valid ({alg}), checked with the secret", mark(paint.ok()))
        }
        Some(Signature::Invalid) => paint.red(&format!(
            "{}INVALID ({alg}): the secret doesn't match, or the token was changed",
            mark(paint.fail())
        )),
        None if token
            .alg
            .as_deref()
            .is_some_and(|a| a.eq_ignore_ascii_case("none")) =>
        {
            paint.yellow("none: the token is unsigned, and nothing should accept it")
        }
        None if token.is_hmac() => {
            format!("{alg}, not checked; --verify checks it with the secret")
        }
        None => format!("{alg}, not checked: that takes the issuer's public key"),
    };
    rows.push(("signature".to_string(), sig));
    out.push_str(&super::rows(&rows));
    out
}

fn json_of(token: &Token, status: Status, signature: Option<Signature>, now: Timestamp) -> Value {
    let mut times = serde_json::Map::new();
    for (name, at) in &token.times {
        times.insert(
            name.to_string(),
            json!({
                "unix": at.as_second(),
                "utc": at.to_string(),
                "relative": t::relative(*at, now),
            }),
        );
    }
    let (status_key, expired) = match status {
        Status::Expired(_) => ("expired", true),
        Status::NotYet(_) => ("not_yet_valid", false),
        Status::ValidUntil(_) | Status::NoExpiry => ("valid", false),
    };
    json!({
        "header": token.header,
        "claims": token.claims,
        "times": times,
        "status": status_key,
        "expired": expired,
        "signature": {
            "alg": token.alg,
            "checked": signature.is_some(),
            "valid": signature.map(|s| s == Signature::Valid),
        },
        "warnings": token.warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_alg_cannot_write_escape_codes() {
        let t = j::decode("eyJhbGciOiJcdTAwMWJdNTI7YztYXHUwMDA3In0.eyJhIjoxfQ.AA").unwrap();
        let now = Timestamp::now();
        let text = human(&t, t.status(now), None, now, Paint { on: false });
        assert!(!text.contains('\x1b') && !text.contains('\x07'));
    }
}
