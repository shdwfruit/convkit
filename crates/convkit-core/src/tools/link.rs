//! `conv link`: links without their tracking, and out of the redirect
//! wrappers mail and chat apps put around them.
//!
//! Everything here works on the text of the link, offline. The one list of
//! what counts as tracking is `TRACKING` (and `SITE_TRACKING` for names
//! that only mean tracking on one site); the wrappers are in `WRAPPERS`.
//! A short link (t.co, bit.ly) doesn't say where it goes, so only
//! `fetch_location`, behind `conv link --follow`, can find out.

use std::borrow::Cow;

/// Query parameters that are tracking on any site. A name ending in `*` is
/// a prefix.
pub const TRACKING: &[&str] = &[
    // Campaign tags: Google Analytics, Matomo, Piwik, HubSpot ads.
    "utm_*",
    "mtm_*",
    "pk_*",
    "hsa_*",
    // Click IDs from ad networks.
    "fbclid",
    "gclid",
    "gclsrc",
    "dclid",
    "gbraid",
    "wbraid",
    "gad_source",
    "gad_campaignid",
    "msclkid",
    "yclid",
    "twclid",
    "ttclid",
    "li_fat_id",
    "epik",
    "rb_clickid",
    "s_kwcid",
    "ef_id",
    "srsltid",
    "_gl",
    // Mail and marketing tools: Mailchimp, HubSpot, Marketo, Klaviyo,
    // Oracle, Vero, Drip.
    "mc_cid",
    "mc_eid",
    "_hsenc",
    "_hsmi",
    "__hstc",
    "__hssc",
    "__hsfp",
    "hsctatracking",
    "mkt_tok",
    "_kx",
    "oly_anon_id",
    "oly_enc_id",
    "vero_id",
    "vero_conv",
    "__s",
    "wickedid",
    "_openstat",
    // Instagram's share ID, on links to anywhere.
    "igshid",
    "igsh",
    // Facebook share parameters, on links to anywhere.
    "fb_action_ids",
    "fb_action_types",
    "fb_source",
    "fb_ref",
    "action_object_map",
    "action_type_map",
    "action_ref_map",
];

/// Parameters that are tracking only on these sites, where a general name
/// like `si` or `ref` is a share or referral tag rather than part of the
/// page.
pub const SITE_TRACKING: &[(&[&str], &[&str])] = &[
    (&["youtube.com", "youtu.be"], &["si", "feature", "pp"]),
    (&["spotify.com"], &["si", "nd"]),
    (&["twitter.com", "x.com"], &["s", "t", "ref_src", "ref_url"]),
    (
        &["facebook.com"],
        &[
            "mibextid", "rdid", "sfnsn", "__tn__", "__cft__*", "__xts__*",
        ],
    ),
    (
        &["tiktok.com"],
        &[
            "is_from_webapp",
            "sender_device",
            "is_copy_url",
            "share_app_id",
            "share_link_id",
            "tt_from",
            "u_code",
            "_r",
            "_t",
        ],
    ),
    (
        &["linkedin.com"],
        &[
            "trk",
            "trackingid",
            "lipi",
            "refid",
            "midtoken",
            "midsig",
            "trkemail",
        ],
    ),
    (
        &["reddit.com"],
        &["share_id", "rdt", "ref", "ref_source", "ref_campaign"],
    ),
    (
        &["amazon.*"],
        &[
            "ref",
            "ref_",
            "pd_rd_*",
            "pf_rd_*",
            "content-id",
            "qid",
            "sr",
            "sprefix",
            "crid",
            "dib",
            "dib_tag",
            "_encoding",
            "linkcode",
            "linkid",
        ],
    ),
    (
        &["ebay.*"],
        &[
            "_trkparms",
            "_trksid",
            "amdata",
            "mkevt",
            "mkcid",
            "mkrid",
            "campid",
            "toolid",
            "customid",
        ],
    ),
    (
        &["aliexpress.com", "aliexpress.us"],
        &[
            "spm",
            "scm",
            "algo_pvid",
            "algo_exp_id",
            "aff_platform",
            "aff_trace_key",
            "aff_fcid",
            "sk",
            "pdp_npi",
            "pdp_ext_f",
            "gatewayadapt",
        ],
    ),
    (
        &["google.*"],
        &[
            "ved", "ei", "oq", "aqs", "gs_lcp", "gs_lcrp", "gs_lp", "sourceid", "ie", "sxsrf",
            "sca_esv", "sca_upv", "iflsig", "uact", "bih", "biw", "rlz", "sclient",
        ],
    ),
    (
        &["bing.com"],
        &[
            "form", "cvid", "qs", "sk", "sp", "sc", "ghsh", "ghacc", "ghpl",
        ],
    ),
];

/// How a wrapper carries the link it wraps.
#[derive(Debug, Clone, Copy)]
enum Carry {
    /// In a query parameter, percent-encoded.
    Param(&'static str),
    /// Proofpoint's v2 encoding of the `u` parameter: `-` for `%`, `_` for
    /// `/`.
    ProofpointV2,
    /// Proofpoint v3: the link between `__` and `__;` in the path.
    ProofpointV3,
    /// Bing's `u` parameter: `a1` and the link in base64url.
    Bing,
    /// Google AMP: the host and path after `/amp/s/`.
    Amp,
}

struct Wrapper {
    name: &'static str,
    hosts: &'static [&'static str],
    path: &'static str,
    carry: Carry,
}

/// Redirect wrappers that carry the real link inside their own, so they
/// can be undone offline.
const WRAPPERS: &[Wrapper] = &[
    Wrapper {
        name: "a Google redirect",
        hosts: &["google.*"],
        path: "/url",
        carry: Carry::Param("q"),
    },
    Wrapper {
        name: "a Google redirect",
        hosts: &["google.*"],
        path: "/url",
        carry: Carry::Param("url"),
    },
    Wrapper {
        name: "a Google AMP page",
        hosts: &["google.*"],
        path: "/amp/s/",
        carry: Carry::Amp,
    },
    Wrapper {
        name: "a Facebook redirect",
        hosts: &["l.facebook.com", "lm.facebook.com", "l.messenger.com"],
        path: "/l.php",
        carry: Carry::Param("u"),
    },
    Wrapper {
        name: "an Instagram redirect",
        hosts: &["l.instagram.com"],
        path: "/",
        carry: Carry::Param("u"),
    },
    Wrapper {
        name: "Outlook Safe Links",
        hosts: &["safelinks.protection.outlook.com"],
        path: "/",
        carry: Carry::Param("url"),
    },
    Wrapper {
        name: "Proofpoint URL Defense",
        hosts: &["urldefense.proofpoint.com"],
        path: "/v2/url",
        carry: Carry::ProofpointV2,
    },
    Wrapper {
        name: "Proofpoint URL Defense",
        hosts: &["urldefense.com"],
        path: "/v3/__",
        carry: Carry::ProofpointV3,
    },
    Wrapper {
        name: "a YouTube redirect",
        hosts: &["youtube.com"],
        path: "/redirect",
        carry: Carry::Param("q"),
    },
    Wrapper {
        name: "a Slack redirect",
        hosts: &["slack-redirect.slack.com"],
        path: "/",
        carry: Carry::Param("url"),
    },
    Wrapper {
        name: "a Steam link filter",
        hosts: &["steamcommunity.com"],
        path: "/linkfilter",
        carry: Carry::Param("url"),
    },
    Wrapper {
        name: "a Steam link filter",
        hosts: &["steamcommunity.com"],
        path: "/linkfilter",
        carry: Carry::Param("u"),
    },
    Wrapper {
        name: "a DuckDuckGo redirect",
        hosts: &["duckduckgo.com"],
        path: "/l/",
        carry: Carry::Param("uddg"),
    },
    Wrapper {
        name: "a Bing redirect",
        hosts: &["bing.com"],
        path: "/ck/a",
        carry: Carry::Bing,
    },
    Wrapper {
        name: "a Reddit redirect",
        hosts: &["out.reddit.com"],
        path: "/",
        carry: Carry::Param("url"),
    },
    Wrapper {
        name: "a LinkedIn redirect",
        hosts: &["linkedin.com"],
        path: "/redir/redirect",
        carry: Carry::Param("url"),
    },
    Wrapper {
        name: "a Tumblr redirect",
        hosts: &["t.umblr.com"],
        path: "/redirect",
        carry: Carry::Param("z"),
    },
    Wrapper {
        name: "a VK redirect",
        hosts: &["vk.com", "away.vk.com"],
        path: "/away.php",
        carry: Carry::Param("to"),
    },
    Wrapper {
        name: "a SoundCloud redirect",
        hosts: &["exit.sc"],
        path: "/",
        carry: Carry::Param("url"),
    },
];

/// Hosts whose links are short codes for a link kept on their server, and
/// tracking redirects that work the same way. Only following them, with
/// the network, says where they go.
pub const SHORT_LINK_HOSTS: &[&str] = &[
    "t.co",
    "bit.ly",
    "bitly.com",
    "tinyurl.com",
    "goo.gl",
    "ow.ly",
    "buff.ly",
    "is.gd",
    "v.gd",
    "rb.gy",
    "cutt.ly",
    "shorturl.at",
    "t.ly",
    "tiny.cc",
    "rebrand.ly",
    "bl.ink",
    "lnkd.in",
    "fb.me",
    "amzn.to",
    "amzn.eu",
    "a.co",
    "spoti.fi",
    "spotify.link",
    "apple.co",
    "g.co",
    "forms.gle",
    "maps.app.goo.gl",
    "page.link",
    "aka.ms",
    "redd.it",
    "vm.tiktok.com",
    "vt.tiktok.com",
    "pin.it",
    "on.soundcloud.com",
    "trib.al",
    "dlvr.it",
    "ift.tt",
    "wp.me",
    "flip.it",
    "list-manage.com",
];

/// A link split into the parts cleaning touches, borrowing the text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Parts<'a> {
    /// `https://user@host:port`, as written.
    origin: &'a str,
    /// The host alone, lowercase, without `www.`.
    host: String,
    path: &'a str,
    query: Option<&'a str>,
    fragment: Option<&'a str>,
}

fn split(url: &str) -> Option<Parts<'_>> {
    let scheme_end = url.find("://")?;
    let scheme = &url[..scheme_end];
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let after = scheme_end + 3;
    let auth_len = url[after..]
        .find(['/', '\\', '?', '#'])
        .unwrap_or(url.len() - after);
    let authority = &url[after..after + auth_len];
    let host = authority.rsplit('@').next().unwrap_or("");
    let host = if host.starts_with('[') {
        host.split(']')
            .next()
            .map(|h| format!("{h}]"))
            .unwrap_or_default()
    } else {
        host.split(':').next().unwrap_or("").to_string()
    };
    if host.is_empty() {
        return None;
    }
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    let rest = &url[after + auth_len..];
    let (before_frag, fragment) = match rest.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (rest, None),
    };
    let (path, query) = match before_frag.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (before_frag, None),
    };
    Some(Parts {
        origin: &url[..after + auth_len],
        host,
        path,
        query,
        fragment,
    })
}

/// Whether `host` is `pattern` or under it: `google.*` matches any Google
/// country domain, and `amazon.com` matches `smile.amazon.com`.
fn host_is(host: &str, pattern: &str) -> bool {
    if let Some(name) = pattern.strip_suffix(".*") {
        // name.TLD or name.co.TLD, with any subdomain in front.
        let labels: Vec<&str> = host.split('.').collect();
        return labels.iter().enumerate().any(|(i, l)| {
            *l == name && {
                let tail = &labels[i + 1..];
                // google.de, google.co.uk, google.com.au: no other second label.
                match tail {
                    [tld] => (2..=3).contains(&tld.len()),
                    [sld, cc] => matches!(*sld, "co" | "com") && cc.len() == 2,
                    _ => false,
                }
            }
        });
    }
    host == pattern || host.ends_with(&format!(".{pattern}"))
}

fn name_matches(name: &str, pattern: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

/// Whether the parameter `name` is tracking on `host`.
pub fn is_tracking(host: &str, name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    TRACKING.iter().any(|p| name_matches(&name, p))
        || SITE_TRACKING.iter().any(|(hosts, names)| {
            hosts.iter().any(|h| host_is(host, h)) && names.iter().any(|p| name_matches(&name, p))
        })
}

/// Whether `url` is a short link that only its server can expand.
pub fn is_short(url: &str) -> bool {
    split(url).is_some_and(|p| {
        let has_code = !(p.path.is_empty() || p.path == "/");
        let shortener = SHORT_LINK_HOSTS.iter().any(|h| host_is(&p.host, h));
        // Reddit's share links: /r/NAME/s/CODE.
        let reddit_share = host_is(&p.host, "reddit.com") && p.path.contains("/s/");
        (shortener && has_code) || reddit_share
    })
}

/// What cleaning one link did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleaned {
    pub input: String,
    pub output: String,
    /// The tracking parameters taken out, by name.
    pub removed: Vec<String>,
    /// The wrappers undone, outermost first: "a Google redirect".
    pub unwrapped: Vec<String>,
    /// Whether what is left is a short link only the network can expand.
    pub short: bool,
    /// Every link `--follow` went through, the first one first; empty when
    /// nothing was followed.
    pub followed: Vec<String>,
}

impl Cleaned {
    pub fn changed(&self) -> bool {
        self.input != self.output
    }
}

/// Undoes every wrapper around `url`, then takes out its tracking.
pub fn clean(url: &str) -> Result<Cleaned, String> {
    let input = url.trim();
    if split(input).is_none() {
        let hint = if !input.contains("://") && input.contains('.') && !input.contains(' ') {
            format!("; write it as https://{input}")
        } else {
            String::new()
        };
        return Err(format!(
            "{} isn't a link conv can clean: it reads http and https links{hint}",
            quoted(input)
        ));
    }
    let mut current = input.to_string();
    let mut unwrapped = Vec::new();
    // A wrapper inside a wrapper (Safe Links around a Google redirect) is
    // undone too; the bound only stops a link that wraps itself.
    for _ in 0..8 {
        match unwrap_once(&current) {
            Some((name, inner)) => {
                unwrapped.push(name.to_string());
                current = inner;
            }
            None => break,
        }
    }
    let (mut output, mut removed) = strip_tracking(&current);
    if split(&output).is_some_and(|p| host_is(&p.host, "amazon.*")) {
        let stripped = strip_amazon_ref(&output);
        if stripped != output {
            removed.push("ref (in the path)".to_string());
            output = stripped;
        }
    }
    let short = is_short(&output);
    Ok(Cleaned {
        input: input.to_string(),
        output,
        removed,
        unwrapped,
        short,
        followed: Vec::new(),
    })
}

fn quoted(s: &str) -> String {
    if s.chars().count() > 60 {
        format!("{:?}", s.chars().take(57).collect::<String>() + "...")
    } else {
        format!("{s:?}")
    }
}

/// The value of `name` in a query string, percent-decoded.
fn param<'a>(query: &'a str, name: &str) -> Option<Cow<'a, str>> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (k == name).then(|| decode(v))
    })
}

fn decode(s: &str) -> Cow<'_, str> {
    // `+` is a space in a query string.
    if s.contains('+') {
        let spaced = s.replace('+', " ");
        return Cow::Owned(
            percent_encoding::percent_decode_str(&spaced)
                .decode_utf8_lossy()
                .into_owned(),
        );
    }
    percent_encoding::percent_decode_str(s).decode_utf8_lossy()
}

fn is_link(s: &str) -> bool {
    split(s).is_some()
}

fn unwrap_once(url: &str) -> Option<(&'static str, String)> {
    let p = split(url)?;
    for w in WRAPPERS {
        if !w.hosts.iter().any(|h| host_is(&p.host, h)) {
            continue;
        }
        let path_ok = match w.path {
            "/" => true,
            prefix if prefix.ends_with('/') || prefix.ends_with("__") => p.path.starts_with(prefix),
            exact => p.path == exact || p.path == format!("{exact}/"),
        };
        if !path_ok {
            continue;
        }
        let inner: Option<String> = match w.carry {
            Carry::Param(name) => p.query.and_then(|q| param(q, name)).map(Cow::into_owned),
            Carry::ProofpointV2 => p.query.and_then(|q| {
                let raw = q.split('&').find_map(|pair| pair.strip_prefix("u="))?;
                let swapped = raw.replace('-', "%").replace('_', "/");
                Some(decode(&swapped).into_owned())
            }),
            Carry::ProofpointV3 => {
                let after = url.split_once("/v3/__")?.1;
                let link = after.split_once("__;")?.0;
                // Characters Proofpoint replaced with * are only in its
                // encoded tail; such a link stays wrapped.
                (!link.contains('*')).then(|| link.to_string())
            }
            Carry::Bing => p.query.and_then(|q| {
                let v = param(q, "u")?;
                let b64 = v.strip_prefix("a1")?;
                use base64::Engine;
                let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(b64.trim_end_matches('='))
                    .ok()?;
                String::from_utf8(bytes).ok()
            }),
            Carry::Amp => p.path.strip_prefix("/amp/s/").map(|rest| match p.query {
                Some(q) => format!("https://{rest}?{q}"),
                None => format!("https://{rest}"),
            }),
        };
        if let Some(inner) = inner.filter(|i| is_link(i) && !i.chars().any(char::is_control)) {
            return Some((w.name, inner));
        }
    }
    None
}

/// Takes tracking parameters out of the query and out of a fragment
/// written like one, leaving everything else exactly as it was.
fn strip_tracking(url: &str) -> (String, Vec<String>) {
    let Some(p) = split(url) else {
        return (url.to_string(), vec![]);
    };
    let mut removed = Vec::new();
    let mut keep = |part: &str| -> Option<String> {
        let mut kept = Vec::new();
        let mut dropped_any = false;
        for pair in part.split('&') {
            let key = pair.split_once('=').map_or(pair, |(k, _)| k);
            let key = decode(key);
            // Bing Maps uses `sp` for the saved places on the map.
            let bing_maps =
                key == "sp" && host_is(&p.host, "bing.com") && p.path.starts_with("/maps");
            if !key.is_empty() && !bing_maps && is_tracking(&p.host, &key) {
                removed.push(key.chars().filter(|c| !c.is_control()).collect());
                dropped_any = true;
            } else {
                kept.push(pair);
            }
        }
        if !dropped_any {
            return Some(part.to_string());
        }
        let kept: Vec<&str> = kept.into_iter().filter(|k| !k.is_empty()).collect();
        (!kept.is_empty()).then(|| kept.join("&"))
    };
    let query = p.query.and_then(&mut keep);
    // A fragment only when it reads as parameters: `#section` stays.
    let fragment = match p.fragment {
        Some(f) if f.contains('=') && !f.starts_with(":~:") => keep(f),
        other => other.map(str::to_string),
    };
    let mut out = format!("{}{}", p.origin, p.path);
    if let Some(q) = query {
        out.push('?');
        out.push_str(&q);
    }
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(&f);
    }
    (out, removed)
}

/// Amazon puts tracking in the path too: `/dp/B0.../ref=sr_1_1`.
fn strip_amazon_ref(url: &str) -> String {
    let Some(p) = split(url) else {
        return url.to_string();
    };
    let segments: Vec<&str> = p
        .path
        .split('/')
        .filter(|s| !s.starts_with("ref="))
        .collect();
    let path = segments.join("/");
    let rest = &url[p.origin.len() + p.path.len()..];
    format!("{}{path}{rest}", p.origin)
}

/// Finds every http and https link in `text` and cleans it in place,
/// leaving the text around it as it was.
pub fn clean_text(text: &str) -> (String, Vec<Cleaned>) {
    let mut out = String::with_capacity(text.len());
    let mut found = Vec::new();
    let mut rest = text;
    // Lowercased once; the same byte offsets as `text`.
    let lower = text.to_ascii_lowercase();
    while let Some(start) = find_link(&lower[text.len() - rest.len()..]) {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let len = link_len(tail);
        let link = &tail[..len];
        match clean(link) {
            Ok(c) => {
                out.push_str(&c.output);
                found.push(c);
            }
            Err(_) => out.push_str(link),
        }
        rest = &tail[len..];
    }
    out.push_str(rest);
    (out, found)
}

fn find_link(s: &str) -> Option<usize> {
    s.match_indices("http").map(|(i, _)| i).find(|&i| {
        let after = &s[i + 4..];
        after.starts_with("://") || after.starts_with("s://")
    })
}

/// How much of `s` is the link: up to whitespace or a character links
/// don't hold, less punctuation that ends the sentence around it, and a
/// closing bracket that wasn't opened inside it.
fn link_len(s: &str) -> usize {
    let mut end = s
        .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`' | '{' | '}' | '|'))
        .unwrap_or(s.len());
    loop {
        let link = &s[..end];
        let Some(last) = link.chars().last() else {
            break;
        };
        let unbalanced = |open: char, close: char| {
            last == close && link.matches(open).count() < link.matches(close).count()
        };
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*')
            || unbalanced('(', ')')
            || unbalanced('[', ']')
        {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    end
}

/// Where `location` points, read against the link it came from.
pub fn resolve(base: &str, location: &str) -> String {
    if is_link(location) {
        return location.to_string();
    }
    let Some(p) = split(base) else {
        return location.to_string();
    };
    let scheme = &base[..base.find("://").unwrap_or(0)];
    if let Some(rest) = location.strip_prefix("//") {
        return format!("{scheme}://{rest}");
    }
    if location.starts_with('/') {
        return format!("{}{location}", p.origin);
    }
    if location.starts_with('?') {
        return format!("{}{}{location}", p.origin, p.path);
    }
    let dir = match p.path.rfind('/') {
        Some(i) => &p.path[..=i],
        None => "/",
    };
    format!("{}{dir}{location}", p.origin)
}

/// One request: where a link redirects to, if it does. A HEAD request
/// with redirects off, no cookies (this build of ureq has no cookie
/// store) and nothing read from the body; a server that refuses HEAD gets
/// a GET whose body is dropped unread.
pub fn fetch_location(url: &str) -> Result<Option<String>, String> {
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(concat!("conv/", env!("CARGO_PKG_VERSION")))
        .build();
    let answer = |r: Result<ureq::Response, ureq::Error>| match r {
        Ok(resp) => Ok((resp.status(), resp.header("location").map(str::to_string))),
        Err(ureq::Error::Status(code, resp)) => {
            Ok((code, resp.header("location").map(str::to_string)))
        }
        Err(ureq::Error::Transport(t)) => Err(t.to_string()),
    };
    let (status, location) = answer(agent.head(url).call())?;
    let (status, location) = if matches!(status, 400 | 403 | 405 | 501) && location.is_none() {
        answer(agent.get(url).call())?
    } else {
        (status, location)
    };
    if (300..400).contains(&status) {
        Ok(location)
    } else if status >= 400 {
        Err(format!("the server answered {status}"))
    } else {
        Ok(None)
    }
}

/// Follows redirects from `url` with `fetch`, up to `max` hops, and
/// returns every link along the way, `url` first.
pub fn follow(
    url: &str,
    max: usize,
    fetch: &mut dyn FnMut(&str) -> Result<Option<String>, String>,
) -> Result<Vec<String>, String> {
    let mut hops = vec![url.to_string()];
    for _ in 0..max {
        let current = hops.last().expect("never empty").clone();
        match fetch(&current)? {
            Some(location) => {
                let next = resolve(&current, &location);
                if hops.contains(&next) {
                    return Err(format!("{next} redirects in a loop"));
                }
                hops.push(next);
            }
            None => return Ok(hops),
        }
    }
    Err(format!(
        "{url} redirected more than {max} times; stopped there"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(url: &str) -> String {
        clean(url).unwrap().output
    }

    #[test]
    fn utm_and_click_ids_go_and_the_rest_stays() {
        let c =
            clean("https://example.com/a?utm_source=x&id=7&UTM_Medium=y&fbclid=abc#top").unwrap();
        assert_eq!(c.output, "https://example.com/a?id=7#top");
        assert_eq!(c.removed, ["utm_source", "UTM_Medium", "fbclid"]);
        assert_eq!(out("https://example.com/?gclid=1"), "https://example.com/");
        assert_eq!(
            out("https://example.com/p?mc_eid=1&mc_cid=2"),
            "https://example.com/p"
        );
    }

    #[test]
    fn a_clean_link_is_left_exactly_as_it_was() {
        for url in [
            "https://example.com/a%20b?q=caf%C3%A9&x=1+2#frag",
            "http://user@example.com:8080/path;params?a&b=&c",
            "https://example.com/?si=keep",
        ] {
            let c = clean(url).unwrap();
            assert_eq!(c.output, url);
            assert!(!c.changed());
        }
    }

    #[test]
    fn site_parameters_only_go_on_their_site() {
        assert_eq!(
            out("https://youtu.be/dQw4w9WgXcQ?si=abc&t=42"),
            "https://youtu.be/dQw4w9WgXcQ?t=42"
        );
        assert_eq!(
            out("https://www.youtube.com/watch?v=dQw4w9WgXcQ&feature=share&si=x"),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
        assert_eq!(
            out("https://open.spotify.com/track/123?si=abcdef"),
            "https://open.spotify.com/track/123"
        );
        assert_eq!(
            out("https://x.com/a/status/1?s=20&t=xyz"),
            "https://x.com/a/status/1"
        );
        assert_eq!(
            out("https://www.instagram.com/p/abc/?igsh=MWQ1ZGUxMzBkMA=="),
            "https://www.instagram.com/p/abc/"
        );
        // `t` is a time on YouTube, and stays.
        assert_eq!(
            out("https://youtube.com/watch?v=1&t=30"),
            "https://youtube.com/watch?v=1&t=30"
        );
    }

    #[test]
    fn amazon_loses_its_path_ref_too() {
        assert_eq!(
            out("https://www.amazon.com/Some-Thing/dp/B0ABC123/ref=sr_1_3?keywords=x&qid=1&sr=8-3"),
            "https://www.amazon.com/Some-Thing/dp/B0ABC123?keywords=x"
        );
        assert_eq!(
            out("https://www.amazon.co.uk/dp/B0ABC123/?ref_=ast_sto_dp&th=1"),
            "https://www.amazon.co.uk/dp/B0ABC123/?th=1"
        );
    }

    #[test]
    fn tracking_in_a_fragment_goes_and_anchors_stay() {
        assert_eq!(
            out("https://example.com/post#utm_source=feed&utm_medium=rss"),
            "https://example.com/post"
        );
        assert_eq!(
            out("https://example.com/post#comments"),
            "https://example.com/post#comments"
        );
        assert_eq!(
            out("https://example.com/post#:~:text=hello=world"),
            "https://example.com/post#:~:text=hello=world"
        );
    }

    #[test]
    fn redirect_wrappers_are_undone() {
        let c = clean("https://www.google.com/url?sa=t&url=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1%26utm_source%3Dg&usg=AOv").unwrap();
        assert_eq!(c.output, "https://example.com/a?b=1");
        assert_eq!(c.unwrapped, ["a Google redirect"]);
        assert_eq!(
            out("https://l.facebook.com/l.php?u=https%3A%2F%2Fexample.org%2F%3Ffbclid%3Dx&h=AT0"),
            "https://example.org/"
        );
        assert_eq!(
            out("https://nam12.safelinks.protection.outlook.com/?url=https%3A%2F%2Fexample.com%2Fdoc&data=05%7C01&reserved=0"),
            "https://example.com/doc"
        );
        assert_eq!(
            out("https://urldefense.proofpoint.com/v2/url?u=https-3A__example.com_path-3Fa-3D1&d=DwMF&c=x"),
            "https://example.com/path?a=1"
        );
        assert_eq!(
            out("https://urldefense.com/v3/__https://example.com/page__;!!ABC!def$"),
            "https://example.com/page"
        );
        assert_eq!(
            out("https://www.youtube.com/redirect?event=video&q=https%3A%2F%2Fexample.com&v=1"),
            "https://example.com"
        );
        assert_eq!(
            out("https://duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F&rut=abc"),
            "https://example.com/"
        );
        assert_eq!(
            out("https://www.google.com/amp/s/example.com/news/story.amp"),
            "https://example.com/news/story.amp"
        );
    }

    #[test]
    fn a_bing_redirect_is_base64() {
        use base64::Engine;
        let target =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("https://example.com/x");
        let url = format!("https://www.bing.com/ck/a?!&&p=abc&u=a1{target}&ntb=1");
        assert_eq!(out(&url), "https://example.com/x");
    }

    #[test]
    fn nested_wrappers_unwrap_all_the_way() {
        let google =
            "https://www.google.com/url?q=https%3A%2F%2Fexample.com%2F%3Futm_source%3Dmail";
        let safe = format!(
            "https://eur01.safelinks.protection.outlook.com/?url={}",
            percent_encoding::utf8_percent_encode(google, percent_encoding::NON_ALPHANUMERIC)
        );
        let c = clean(&safe).unwrap();
        assert_eq!(c.output, "https://example.com/");
        assert_eq!(c.unwrapped, ["Outlook Safe Links", "a Google redirect"]);
        assert_eq!(c.removed, ["utm_source"]);
    }

    #[test]
    fn a_google_search_is_not_a_redirect() {
        assert_eq!(
            out("https://www.google.com/search?q=rust+lang&oq=rust&sourceid=chrome&ie=UTF-8"),
            "https://www.google.com/search?q=rust+lang"
        );
        // q= that isn't a link stays.
        assert_eq!(
            out("https://www.google.com/url?q=hello"),
            "https://www.google.com/url?q=hello"
        );
    }

    #[test]
    fn unwrapping_refuses_control_characters_and_backslash_hosts() {
        let c =
            clean("https://www.google.com/url?q=https%3A%2F%2Fexample.com%2F%1B%5D0%3Btitle%07")
                .unwrap();
        assert!(!c.output.contains('\x1b') && c.unwrapped.is_empty());
        let c = clean("https://example.com/?utm_%1B%5D=1").unwrap();
        assert!(c.removed.iter().all(|k| !k.contains('\x1b')));
        let c = clean("https://evil.example\\@www.google.com/url?q=https://bank.example/login")
            .unwrap();
        assert!(c.unwrapped.is_empty());
        assert!(!host_is("google.abc.io", "google.*"));
        assert!(host_is("google.com.au", "google.*"));
    }

    #[test]
    fn amp_keeps_its_query_and_bing_maps_keeps_sp() {
        assert_eq!(
            clean("https://www.google.com/amp/s/www.example.com/story.php?id=123")
                .unwrap()
                .output,
            "https://www.example.com/story.php?id=123"
        );
        assert_eq!(
            clean("https://www.bing.com/maps?cp=1&sp=aN.x")
                .unwrap()
                .output,
            "https://www.bing.com/maps?cp=1&sp=aN.x"
        );
        assert_eq!(
            resolve("https://a.com/x/file.php?a=1", "?page=2"),
            "https://a.com/x/file.php?page=2"
        );
    }

    #[test]
    fn a_text_of_many_links_is_cleaned_in_linear_time() {
        let text = "http://a.com/x?utm_source=z ".repeat(30_000);
        assert_eq!(clean_text(&text).1.len(), 30_000);
    }

    #[test]
    fn short_links_are_named() {
        assert!(clean("https://t.co/abc123").unwrap().short);
        assert!(clean("https://bit.ly/3xyz").unwrap().short);
        assert!(
            clean("https://www.reddit.com/r/rust/s/AbCdEf")
                .unwrap()
                .short
        );
        assert!(!clean("https://bit.ly/").unwrap().short);
        assert!(!clean("https://example.com/t.co").unwrap().short);
    }

    #[test]
    fn hosts_match_by_label_not_by_substring() {
        assert!(host_is("google.co.uk", "google.*"));
        assert!(host_is("news.google.com", "google.*"));
        assert!(!host_is("google.example.org.evil", "google.*"));
        assert!(!host_is("notgoogle.com", "google.*"));
        assert!(host_is("m.youtube.com", "youtube.com"));
        assert!(!host_is("notyoutube.com", "youtube.com"));
    }

    #[test]
    fn non_links_are_refused() {
        assert!(clean("example.com").unwrap_err().contains("http and https"));
        assert!(clean("ftp://example.com/").is_err());
        assert!(clean("https://").is_err());
    }

    #[test]
    fn links_in_text_are_cleaned_in_place() {
        let text = "See https://example.com/a?utm_source=x. And (https://youtu.be/1?si=2), done!\nhttp://example.org/?id=1&fbclid=z";
        let (cleaned, found) = clean_text(text);
        assert_eq!(
            cleaned,
            "See https://example.com/a. And (https://youtu.be/1), done!\nhttp://example.org/?id=1"
        );
        assert_eq!(found.len(), 3);
        let (same, found) = clean_text("no links here");
        assert_eq!(same, "no links here");
        assert!(found.is_empty());
    }

    #[test]
    fn wikipedia_parentheses_stay_in_the_link() {
        let (cleaned, _) = clean_text("https://en.wikipedia.org/wiki/Rust_(programming_language)");
        assert_eq!(
            cleaned,
            "https://en.wikipedia.org/wiki/Rust_(programming_language)"
        );
    }

    #[test]
    fn locations_resolve_against_the_link() {
        assert_eq!(
            resolve("https://t.co/abc", "https://x.com/a"),
            "https://x.com/a"
        );
        assert_eq!(resolve("https://a.com/x/y", "/z"), "https://a.com/z");
        assert_eq!(resolve("https://a.com/x/y", "z"), "https://a.com/x/z");
        assert_eq!(resolve("https://a.com/x", "//b.com/c"), "https://b.com/c");
    }

    #[test]
    fn following_stops_at_the_end_or_a_loop() {
        let mut fetch = |u: &str| -> Result<Option<String>, String> {
            Ok(match u {
                "https://t.co/a" => Some("https://bit.ly/b".to_string()),
                "https://bit.ly/b" => Some("/final?utm_source=x".to_string()),
                _ => None,
            })
        };
        let hops = follow("https://t.co/a", 10, &mut fetch).unwrap();
        assert_eq!(
            hops,
            [
                "https://t.co/a",
                "https://bit.ly/b",
                "https://bit.ly/final?utm_source=x"
            ]
        );
        let mut looping = |u: &str| -> Result<Option<String>, String> {
            Ok(Some(
                if u.ends_with('a') {
                    "https://x.com/b"
                } else {
                    "https://x.com/a"
                }
                .to_string(),
            ))
        };
        assert!(follow("https://x.com/a", 10, &mut looping)
            .unwrap_err()
            .contains("loop"));
    }
}
