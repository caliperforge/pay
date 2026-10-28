//! `pay catalog next` — print the first failing check on a PAY.md as one brief, or `green`.

use std::fmt;
use std::path::{Path, PathBuf};

use pay_core::client::fetch::RawResponse;
use pay_core::client::mpp::{self, Challenge};
use pay_core::skills::build::parse_frontmatter;
use pay_core::skills::openapi::{ResolvedEndpoint, effective_endpoints_relative_to};
use pay_core::skills::price_range_usd;
use pay_core::skills::probe::{
    Live, Transport, amount_to_usd, decimals_for, normalize_currency,
    parse_payment_required_header, probe_response,
};
use pay_kit::mpp::ChargeRequest;
use pay_kit::x402::exact::{SOLANA_DEVNET, SOLANA_MAINNET};
use pay_types::registry::ProviderFrontmatter;
use serde_json::Value as Json;
use serde_yml::Value;

const FORMAT_BOUNDS: [(&str, usize, usize); 3] = [
    ("summary", 24, 63),
    ("description", 64, 255),
    ("use_case", 32, 255),
];

#[derive(clap::Args)]
pub struct NextCommand {
    /// Provider `PAY.md` to check.
    pub path: PathBuf,

    /// Earlier copy of the listing; every endpoint it declares must remain.
    #[arg(long)]
    pub before: Option<PathBuf>,
}

impl NextCommand {
    pub fn run(self) -> pay_core::Result<()> {
        match next_brief(&self.path, self.before.as_deref(), &Live)? {
            Some(brief) => print!("{brief}"),
            None => println!("green"),
        }
        Ok(())
    }
}

struct Brief {
    check: &'static str,
    rule: String,
    current: String,
    replacement: String,
    stop: bool,
}

impl fmt::Display for Brief {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "check: {}", self.check)?;
        writeln!(f, "rule: {}", self.rule)?;
        writeln!(f, "current: {}", self.current)?;
        writeln!(f, "replacement: {}", self.replacement)?;
        if self.stop {
            writeln!(f, "stop: the cause is on the seller's server")?;
        }
        Ok(())
    }
}

/// Network slug and asked USD of one Solana offer.
type SolanaOffer = (String, Option<f64>);

struct Offer {
    current: String,
    listed: Option<f64>,
    solana: Vec<SolanaOffer>,
    tempo_only: bool,
}

fn next_brief(
    path: &Path,
    before: Option<&Path>,
    transport: &dyn Transport,
) -> pay_core::Result<Option<Brief>> {
    let yaml = read_frontmatter(path)?;
    let value: Value = serde_yml::from_str(&yaml)
        .map_err(|e| pay_core::Error::Config(format!("{}: {e}", path.display())))?;
    if let Some(brief) = format_brief(&value) {
        return Ok(Some(brief));
    }
    let (spec, endpoints) = resolve(path, &yaml)?;
    if let Some(before) = before {
        let held = endpoint_pairs(&endpoints);
        let dropped = endpoint_pairs(&resolve(before, &read_frontmatter(before)?)?.1)
            .into_iter()
            .find(|pair| !held.contains(pair));
        if let Some((method, path)) = dropped {
            let pair = format!("{method} {path}");
            return Ok(Some(Brief {
                check: "declared_less",
                rule: format!("--before declares {pair}"),
                replacement: format!("restore {pair}"),
                current: pair,
                stop: false,
            }));
        }
    }
    probe_brief(&spec.meta.service_url, &endpoints, transport)
}

fn format_brief(yaml: &Value) -> Option<Brief> {
    FORMAT_BOUNDS.iter().find_map(|&(field, low, high)| {
        let text = yaml.get(field).and_then(Value::as_str).unwrap_or("");
        let n = text.chars().count();
        (!(low..=high).contains(&n)).then(|| Brief {
            check: "format",
            rule: format!("{field} is {n} characters"),
            current: text.to_string(),
            replacement: format!("a {field} of {low}-{high} characters"),
            stop: false,
        })
    })
}

fn probe_brief(
    service_url: &str,
    endpoints: &[ResolvedEndpoint],
    transport: &dyn Transport,
) -> pay_core::Result<Option<Brief>> {
    let host = url_host(service_url)
        .ok_or_else(|| pay_core::Error::Config(format!("service_url {service_url} has no host")))?;
    if !transport.resolves(&host) {
        return Ok(Some(Brief {
            check: "offer",
            rule: format!("{host} does not resolve"),
            current: service_url.to_string(),
            replacement: "a service_url whose host resolves".into(),
            stop: false,
        }));
    }
    let mut offers = Vec::with_capacity(endpoints.len());
    for ep in endpoints {
        let url = format!(
            "{}/{}",
            service_url.trim_end_matches('/'),
            ep.spec.path.trim_start_matches('/')
        );
        let current = format!("{} {}", ep.spec.method.to_ascii_uppercase(), ep.spec.path);
        let raw = probe_response(transport, &ep.spec.method, &url, ep.body_example.as_deref());
        match read_offer(raw, &host) {
            Ok((solana, tempo_only)) => offers.push(Offer {
                current,
                listed: price_range_usd(&ep.spec.pricing).map(|(low, _)| low),
                solana,
                tempo_only,
            }),
            Err(rule) => {
                return Ok(Some(Brief {
                    check: "offer",
                    rule,
                    current,
                    replacement: "a 402 with its offer in the JSON body or an MPP challenge".into(),
                    stop: false,
                }));
            }
        }
    }
    Ok(offers
        .iter()
        .find_map(network_brief)
        .or_else(|| offers.iter().find_map(price_brief)))
}

fn read_offer(
    raw: pay_core::Result<RawResponse>,
    host: &str,
) -> Result<(Vec<SolanaOffer>, bool), String> {
    let raw = raw.map_err(|_| "unreachable".to_string())?;
    if raw.status != 402 {
        return Err(format!("HTTP {}", raw.status));
    }
    let with_accepts = |v: Option<Json>| v.filter(|v| v["accepts"].is_array());
    let body = with_accepts(serde_json::from_slice(&raw.body).ok());
    let header = with_accepts(parse_payment_required_header(&raw.headers));
    let challenges = mpp::parse_headers(&raw.headers);
    let envelope = body.as_ref().or(header.as_ref());
    if envelope.is_none() && challenges.is_empty() {
        return Err("402 without a readable offer".into());
    }
    if let Some(other) = envelope
        .into_iter()
        .flat_map(resource_hosts)
        .find(|h| h != host)
    {
        return Err(format!("payment resource points at {other}"));
    }
    if body.is_none() && header.is_some() && raw.content_type().is_some_and(|t| t.contains("json"))
    {
        return Err("HEADER_ONLY".into());
    }
    let mut solana: Vec<SolanaOffer> = envelope
        .into_iter()
        .flat_map(|e| e["accepts"].as_array().into_iter().flatten())
        .filter_map(x402_offer)
        .collect();
    let methods: Vec<&str> = challenges.iter().map(|c| c.method.as_str()).collect();
    solana.extend(
        challenges
            .iter()
            .filter(|c| c.method.as_str() == "solana")
            .filter_map(mpp_offer),
    );
    let tempo_only = !methods.contains(&"solana") && methods.contains(&"tempo");
    Ok((solana, tempo_only))
}

fn resource_hosts(envelope: &Json) -> impl Iterator<Item = String> + '_ {
    let accepts = envelope["accepts"].as_array().into_iter().flatten();
    std::iter::once(&envelope["resource"]["url"])
        .chain(accepts.map(|a| &a["resource"]))
        .filter_map(Json::as_str)
        .filter_map(url_host)
}

fn x402_offer(accept: &Json) -> Option<SolanaOffer> {
    let network = match accept["network"].as_str()? {
        SOLANA_MAINNET | "solana" | "mainnet-beta" | "solana-mainnet" => "mainnet",
        SOLANA_DEVNET | "solana-devnet" | "devnet" => "devnet",
        _ => return None,
    };
    let asked = accept["amount"]
        .as_str()
        .or(accept["maxAmountRequired"].as_str())
        .and_then(|amount| usd(amount, accept["asset"].as_str().unwrap_or("")));
    Some((network.to_string(), asked))
}

fn mpp_offer(challenge: &Challenge) -> Option<SolanaOffer> {
    let request: Option<ChargeRequest> = challenge.request.decode().ok();
    let asked = request.and_then(|r| usd(&r.amount, &r.currency));
    Some((mpp::challenge_network(challenge, None).ok()?, asked))
}

fn usd(amount: &str, asset: &str) -> Option<f64> {
    amount_to_usd(amount, decimals_for(&normalize_currency(asset)))
}

fn network_brief(offer: &Offer) -> Option<Brief> {
    let on = |net: &str| offer.solana.iter().any(|(n, _)| n == net);
    let rule = match (on("mainnet"), on("devnet")) {
        (false, true) => "devnet only",
        (false, false) => "no Solana mainnet offer",
        (true, true) => "also advertises devnet",
        (true, false) if offer.tempo_only => "MPP offered on Tempo, not Solana",
        (true, false) => return None,
    };
    Some(Brief {
        check: "network",
        rule: rule.into(),
        current: offer.current.clone(),
        replacement: "Solana mainnet offers only".into(),
        stop: true,
    })
}

fn price_brief(offer: &Offer) -> Option<Brief> {
    let mut asked: Vec<f64> = offer
        .solana
        .iter()
        .filter(|(n, _)| n == "mainnet")
        .filter_map(|(_, usd)| *usd)
        .collect();
    asked.sort_by(f64::total_cmp);
    asked.dedup();
    let (rule, current, replacement, stop) = match (asked.as_slice(), offer.listed) {
        (a, _) if a.contains(&0.0) => (
            "asks for $0".into(),
            offer.current.clone(),
            "a price above $0".into(),
            true,
        ),
        (a, _) if a.len() > 1 => (
            format!(
                "inconsistent prices across offers [{}]",
                a.iter().map(|&q| g(q)).collect::<Vec<_>>().join(", ")
            ),
            offer.current.clone(),
            "one price across offers".into(),
            true,
        ),
        (&[q], Some(d)) if q != d => {
            let (current, replacement) = if q > d { (q, d) } else { (d, q) };
            (
                format!("price listed ${} vs asked ${}", g(d), g(q)),
                format!("${}", g(current)),
                format!("${}", g(replacement)),
                q > d,
            )
        }
        _ => return None,
    };
    Some(Brief {
        check: "price",
        rule,
        current,
        replacement,
        stop,
    })
}

/// Python's `format(n, "g")`.
fn g(n: f64) -> String {
    let sci = format!("{n:.5e}");
    let (mantissa, exp) = sci.split_once('e').expect("`{:e}` writes an exponent");
    let exp: i32 = exp.parse().expect("`{:e}` writes an integer exponent");
    let trim = |s: String| {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    };
    if (-4..6).contains(&exp) {
        trim(format!("{n:.*}", (5 - exp) as usize))
    } else {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", trim(mantissa.to_string()), exp.abs())
    }
}

fn url_host(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()?
        .host_str()
        .map(str::to_string)
}

fn read_frontmatter(path: &Path) -> pay_core::Result<String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| pay_core::Error::Config(format!("read {}: {e}", path.display())))?;
    Ok(parse_frontmatter(&text)?.0)
}

fn resolve(
    path: &Path,
    yaml: &str,
) -> pay_core::Result<(ProviderFrontmatter, Vec<ResolvedEndpoint>)> {
    let spec: ProviderFrontmatter = serde_yml::from_str(yaml)
        .map_err(|e| pay_core::Error::Config(format!("{}: {e}", path.display())))?;
    let endpoints = effective_endpoints_relative_to(&spec, path.parent())?;
    Ok((spec, endpoints))
}

fn endpoint_pairs(endpoints: &[ResolvedEndpoint]) -> Vec<(String, String)> {
    endpoints
        .iter()
        .map(|r| (r.spec.method.to_ascii_uppercase(), r.spec.path.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use base64::Engine;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    use pay_core::skills::probe::Recorded;
    use serde_json::json;

    use super::*;

    const SUMMARY: &str = "Paid data feed for one call";
    const PAID: &str = "[{method: GET, path: /paid, description: Fetch the paid resource}]";
    const URL: &str = "https://paid.example.com/paid";
    const USDC: &str = pay_types::stablecoin_mints::USDC_MAINNET;
    const STOP: &str = "stop: the cause is on the seller's server\n";

    fn listing(
        summary: &str,
        description: &str,
        use_case: Option<&str>,
        endpoints: &str,
    ) -> String {
        let use_case = use_case.map_or(String::new(), |u| format!("use_case: {u}\n"));
        format!(
            "---\nname: paid\nsummary: {summary}\ndescription: {description}\n{use_case}\
             category: data\nservice_url: https://paid.example.com\nendpoints: {endpoints}\n---\n"
        )
    }

    fn valid(summary: &str, endpoints: &str) -> String {
        listing(summary, &"d".repeat(64), Some(&"u".repeat(32)), endpoints)
    }

    fn priced(listed: f64) -> String {
        format!(
            "[{{method: GET, path: /paid, description: Fetch the paid resource, \
             pricing: {{dimensions: [{{tiers: [{{price_usd: {listed}}}]}}]}}}}]"
        )
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn brief(path: &Path, before: Option<&Path>, transport: &dyn Transport) -> String {
        next_brief(path, before, transport)
            .unwrap()
            .unwrap()
            .to_string()
    }

    fn is_green(path: &Path, before: Option<&Path>, transport: &dyn Transport) -> bool {
        next_brief(path, before, transport).unwrap().is_none()
    }

    fn recorded(status: u16, headers: &[(&str, String)], body: &str) -> Recorded {
        let raw = RawResponse {
            url: URL.into(),
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            body: body.into(),
        };
        Recorded(HashMap::from([(("GET".into(), URL.into()), raw)]))
    }

    fn nothing() -> Recorded {
        Recorded(HashMap::new())
    }

    fn accept(network: &str, amount: &str) -> Json {
        json!({"scheme": "exact", "network": network, "amount": amount, "asset": USDC, "payTo": "Payee"})
    }

    fn x402(accepts: &[Json]) -> String {
        json!({"x402Version": 2, "accepts": accepts}).to_string()
    }

    fn json_402(body: &str) -> Recorded {
        recorded(402, &[("content-type", "application/json".into())], body)
    }

    fn paying() -> Recorded {
        json_402(&x402(&[accept(SOLANA_MAINNET, "10000")]))
    }

    fn mpp(method: &str, network: &str) -> String {
        let request = json!({
            "amount": "10000",
            "currency": USDC,
            "recipient": "Payee",
            "methodDetails": {"network": network}
        });
        let request = URL_SAFE_NO_PAD.encode(request.to_string());
        format!(
            "Payment id=\"test\", realm=\"test\", method=\"{method}\", intent=\"charge\", request=\"{request}\""
        )
    }

    #[test]
    fn format_fail_names_summary() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "PAY.md", &valid("Pays per call", PAID));
        assert_eq!(
            brief(&path, None, &nothing()),
            "check: format\nrule: summary is 13 characters\ncurrent: Pays per call\n\
             replacement: a summary of 24-63 characters\n"
        );
    }

    #[test]
    fn format_checks_description_then_use_case() {
        let dir = tempfile::tempdir().unwrap();
        let short = write(
            dir.path(),
            "short.md",
            &listing(SUMMARY, "Too short", None, PAID),
        );
        assert!(
            brief(&short, None, &nothing())
                .starts_with("check: format\nrule: description is 9 characters\n")
        );

        let absent = write(
            dir.path(),
            "absent.md",
            &listing(SUMMARY, &"d".repeat(64), None, PAID),
        );
        assert_eq!(
            brief(&absent, None, &nothing()),
            "check: format\nrule: use_case is 0 characters\ncurrent: \n\
             replacement: a use_case of 32-255 characters\n"
        );
    }

    #[test]
    fn format_counts_code_points() {
        let dir = tempfile::tempdir().unwrap();
        let wide = write(dir.path(), "wide.md", &valid(&"é".repeat(24), PAID));
        assert!(is_green(&wide, None, &paying()));

        let long = write(dir.path(), "long.md", &valid(&"é".repeat(64), PAID));
        assert!(
            brief(&long, None, &nothing())
                .starts_with("check: format\nrule: summary is 64 characters\n")
        );

        let under = write(dir.path(), "under.md", &valid(&"a".repeat(23), PAID));
        assert!(
            brief(&under, None, &nothing())
                .starts_with("check: format\nrule: summary is 23 characters\n")
        );

        let at = write(dir.path(), "at.md", &valid(&"a".repeat(24), PAID));
        assert!(is_green(&at, None, &paying()));
    }

    #[test]
    fn declared_less_names_dropped_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let before = write(dir.path(), "no-usdc.md", &valid(SUMMARY, PAID));
        let path = write(dir.path(), "PAY.md", &valid(SUMMARY, "[]"));
        assert_eq!(
            brief(&path, Some(&before), &nothing()),
            "check: declared_less\nrule: --before declares GET /paid\ncurrent: GET /paid\n\
             replacement: restore GET /paid\n"
        );

        let faulty = write(dir.path(), "faulty.md", &valid("Pays per call", "[]"));
        assert!(brief(&faulty, Some(&before), &nothing()).starts_with("check: format\n"));
    }

    #[test]
    fn valid_listing_is_green() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "PAY.md", &valid(SUMMARY, PAID));
        assert!(is_green(&path, None, &paying()));

        let before = write(dir.path(), "before.md", &valid(SUMMARY, PAID));
        assert!(is_green(&path, Some(&before), &paying()));
    }

    #[test]
    fn unreadable_listing_is_err() {
        let dir = tempfile::tempdir().unwrap();
        assert!(next_brief(&dir.path().join("missing.md"), None, &nothing()).is_err());

        let open = write(dir.path(), "open.md", "---\nname: paid\nsummary: Paid\n");
        assert!(next_brief(&open, None, &nothing()).is_err());
    }

    #[test]
    fn unresolved_host_fails_offer() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "PAY.md", &valid(&"a".repeat(24), PAID));
        assert_eq!(
            brief(&path, None, &nothing()),
            "check: offer\nrule: paid.example.com does not resolve\n\
             current: https://paid.example.com\nreplacement: a service_url whose host resolves\n"
        );
    }

    #[test]
    fn header_only_offer_fails_offer() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "PAY.md", &valid(SUMMARY, PAID));
        let header = (
            "payment-required",
            STANDARD.encode(x402(&[accept(SOLANA_MAINNET, "10000")])),
        );
        let json = recorded(
            402,
            &[header.clone(), ("content-type", "application/json".into())],
            r#"{"error":"payment required"}"#,
        );
        assert!(brief(&path, None, &json).starts_with("check: offer\nrule: HEADER_ONLY\n"));
        assert!(is_green(&path, None, &recorded(402, &[header], "")));
    }

    #[test]
    fn unreadable_offer_fails_offer() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "PAY.md", &valid(SUMMARY, PAID));
        assert!(
            brief(&path, None, &recorded(404, &[], ""))
                .starts_with("check: offer\nrule: HTTP 404\n")
        );
        assert!(
            brief(&path, None, &json_402("{}"))
                .starts_with("check: offer\nrule: 402 without a readable offer\n")
        );
    }

    #[test]
    fn devnet_only_stops() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "PAY.md", &valid(SUMMARY, PAID));
        let body = json!({"x402Version": 1, "accepts": [{
            "scheme": "exact",
            "network": "solana-devnet",
            "maxAmountRequired": "10000",
            "asset": USDC,
            "payTo": "Payee",
            "resource": URL
        }]});
        let devnet = recorded(
            402,
            &[
                ("content-type", "application/json".into()),
                ("www-authenticate", mpp("solana", "devnet")),
            ],
            &body.to_string(),
        );
        assert_eq!(
            brief(&path, None, &devnet),
            format!(
                "check: network\nrule: devnet only\ncurrent: GET /paid\n\
                 replacement: Solana mainnet offers only\n{STOP}"
            )
        );
    }

    #[test]
    fn network_names_devnet_and_tempo() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "PAY.md", &valid(SUMMARY, PAID));
        let both = json_402(&x402(&[
            accept(SOLANA_MAINNET, "10000"),
            accept(SOLANA_DEVNET, "10000"),
        ]));
        assert!(
            brief(&path, None, &both).starts_with("check: network\nrule: also advertises devnet\n")
        );

        let tempo = recorded(
            402,
            &[
                ("content-type", "application/json".into()),
                ("www-authenticate", mpp("tempo", "mainnet")),
            ],
            &x402(&[accept(SOLANA_MAINNET, "10000")]),
        );
        assert!(
            brief(&path, None, &tempo)
                .starts_with("check: network\nrule: MPP offered on Tempo, not Solana\n")
        );
    }

    #[test]
    fn asked_above_listed_stops() {
        let dir = tempfile::tempdir().unwrap();
        let offer =
            json!({"scheme": "exact", "network": SOLANA_MAINNET, "amount": "10000", "asset": USDC});
        let header = recorded(
            402,
            &[("payment-required", STANDARD.encode(x402(&[offer])))],
            "",
        );
        let low = write(dir.path(), "low.md", &valid(SUMMARY, &priced(0.001)));
        assert_eq!(
            brief(&low, None, &header),
            format!(
                "check: price\nrule: price listed $0.001 vs asked $0.01\ncurrent: $0.01\n\
                 replacement: $0.001\n{STOP}"
            )
        );

        let even = write(dir.path(), "even.md", &valid(SUMMARY, &priced(0.01)));
        assert!(is_green(&even, None, &header));
    }

    #[test]
    fn asked_below_listed_does_not_stop() {
        let dir = tempfile::tempdir().unwrap();
        let high = write(dir.path(), "PAY.md", &valid(SUMMARY, &priced(0.02)));
        assert_eq!(
            brief(&high, None, &paying()),
            "check: price\nrule: price listed $0.02 vs asked $0.01\ncurrent: $0.02\n\
             replacement: $0.01\n"
        );
    }

    #[test]
    fn price_names_zero_and_inconsistent_offers() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "PAY.md", &valid(SUMMARY, PAID));
        let zero = brief(
            &path,
            None,
            &json_402(&x402(&[accept(SOLANA_MAINNET, "0")])),
        );
        assert!(zero.starts_with("check: price\nrule: asks for $0\n") && zero.ends_with(STOP));

        let mixed = brief(
            &path,
            None,
            &json_402(&x402(&[
                accept(SOLANA_MAINNET, "20000"),
                accept(SOLANA_MAINNET, "10000"),
            ])),
        );
        assert!(
            mixed.starts_with(
                "check: price\nrule: inconsistent prices across offers [0.01, 0.02]\n"
            ) && mixed.ends_with(STOP)
        );
    }

    #[test]
    fn g_matches_python_general_format() {
        assert_eq!(
            [0.001, 0.01, 0.00001, 1234567.0].map(g),
            ["0.001", "0.01", "1e-05", "1.23457e+06"]
        );
    }
}
