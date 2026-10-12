//! Finding a connection manager to talk to.
//!
//! Steam has no fixed address for its servers. A client asks a public
//! directory for the current ones and connects to one: each answer names an
//! endpoint, the datacentre it is in, and how loaded it is. The directory is
//! plain HTTPS and needs no account, so it is asked through the same route the
//! account will use - an account sent through Tor does not tell Valve its real
//! address on the way to choosing a server.

use anyhow::{bail, Context, Result};
use rand::seq::SliceRandom;
use serde::Deserialize;
use std::time::Duration;

const DIRECTORY: &str = "https://api.steampowered.com/ISteamDirectory/GetCMListForConnect/v1/";

/// One connection manager, as the directory describes it.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Cm {
    /// `host:port`, the address of its WebSocket.
    pub endpoint: String,
    /// The datacentre it is in.
    #[serde(default)]
    pub dc: String,
    /// How the directory describes its kind: `websockets` for these.
    #[serde(default, rename = "type")]
    pub kind: String,
    /// Connections, roughly, as a percentage of what it takes.
    #[serde(default)]
    pub load: f64,
    /// The same weighted by the datacentre's other servers; what to rank by.
    #[serde(default)]
    pub wtd_load: f64,
}

impl Cm {
    /// Where its WebSocket is.
    pub fn url(&self) -> String {
        format!("wss://{}/cmsocket/", self.endpoint)
    }
}

#[derive(Deserialize)]
struct Directory {
    response: Inner,
}

#[derive(Deserialize)]
struct Inner {
    #[serde(default)]
    serverlist: Vec<Cm>,
    #[serde(default)]
    success: Option<bool>,
    #[serde(default)]
    message: String,
}

/// Reads the directory's answer.
pub fn parse(json: &str) -> Result<Vec<Cm>> {
    let answer: Directory = serde_json::from_str(json).context("reading Steam's server list")?;
    if answer.response.success == Some(false) {
        bail!("Steam's server list said no{}", if answer.response.message.is_empty() { String::new() } else { format!(": {}", answer.response.message) });
    }
    let servers: Vec<Cm> = answer.response.serverlist.into_iter().filter(|cm| cm.kind.is_empty() || cm.kind == "websockets").collect();
    if servers.is_empty() {
        bail!("Steam's server list had no servers in it");
    }
    Ok(servers)
}

/// The order to try them in: the least loaded first.
///
/// The best few are shuffled among themselves. Everybody who ranks by load
/// would otherwise pick the very same server, which is how the quietest one
/// stops being quiet, and a client that retries after a failure would walk
/// the same list from the same start every time.
pub fn ranked(mut servers: Vec<Cm>) -> Vec<Cm> {
    servers.sort_by(|a, b| a.wtd_load.partial_cmp(&b.wtd_load).unwrap_or(std::cmp::Ordering::Equal).then(a.load.partial_cmp(&b.load).unwrap_or(std::cmp::Ordering::Equal)));
    let head = servers.len().min(4);
    servers[..head].shuffle(&mut rand::thread_rng());
    servers
}

/// Asks the directory, as `key` is routed.
pub async fn discover(key: &str, cell: u32) -> Result<Vec<Cm>> {
    let router = crate::net::route::router();
    let http = router.client_if("steam", router.routed(key), |b| b.timeout(Duration::from_secs(20)).user_agent(concat!("moho/", env!("CARGO_PKG_VERSION"))));
    let body = http
        .get(DIRECTORY)
        .query(&[("cellid", cell.to_string()), ("cmtype", "websockets".to_string()), ("realm", "steamglobal".to_string()), ("maxcount", "20".to_string())])
        .send()
        .await
        .context("asking Steam for a server")?
        .error_for_status()
        .context("Steam's server list was refused")?
        .text()
        .await
        .context("reading Steam's server list")?;
    Ok(ranked(parse(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As the directory answered when this was written.
    const REAL: &str = r#"{"response":{"serverlist":[
        {"endpoint":"cmp1-iad1.steamserver.net:443","legacy_endpoint":"cmp1-iad1.steamserver.net:443","type":"websockets","dc":"iad1","realm":"steamglobal","load":9,"wtd_load":0},
        {"endpoint":"cmp2-iad1.steamserver.net:443","legacy_endpoint":"cmp2-iad1.steamserver.net:443","type":"websockets","dc":"iad1","realm":"steamglobal","load":9,"wtd_load":3.76304769515991211}
    ],"success":true,"message":""}}"#;

    #[test]
    fn the_directorys_answer_is_read() {
        let servers = parse(REAL).unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].endpoint, "cmp1-iad1.steamserver.net:443");
        assert_eq!(servers[0].dc, "iad1");
        assert_eq!(servers[1].wtd_load, 3.76304769515991211);
        assert_eq!(servers[0].url(), "wss://cmp1-iad1.steamserver.net:443/cmsocket/");
    }

    #[test]
    fn a_refusal_or_an_empty_list_is_an_error_that_says_so() {
        let refused = parse(r#"{"response":{"serverlist":[],"success":false,"message":"rate limited"}}"#).unwrap_err();
        assert!(format!("{refused:#}").contains("rate limited"));
        assert!(parse(r#"{"response":{"serverlist":[],"success":true}}"#).is_err());
        assert!(parse("<html>not json</html>").is_err());
        // Only the kind this client speaks counts.
        let other = r#"{"response":{"serverlist":[{"endpoint":"1.2.3.4:27017","type":"netfilter"}],"success":true}}"#;
        assert!(parse(other).is_err());
    }

    #[test]
    fn the_least_loaded_come_first_and_all_of_them_are_kept() {
        let cm = |name: &str, wtd: f64| Cm { endpoint: name.into(), dc: String::new(), kind: "websockets".into(), load: 0.0, wtd_load: wtd };
        let list: Vec<Cm> = (0..10).map(|i| cm(&format!("s{i}"), (10 - i) as f64)).collect();
        let order = ranked(list);
        assert_eq!(order.len(), 10);
        // The four best, in some order, then the rest strictly by load.
        let mut best: Vec<&str> = order[..4].iter().map(|c| c.endpoint.as_str()).collect();
        best.sort();
        assert_eq!(best, ["s6", "s7", "s8", "s9"]);
        let rest: Vec<&str> = order[4..].iter().map(|c| c.endpoint.as_str()).collect();
        assert_eq!(rest, ["s5", "s4", "s3", "s2", "s1", "s0"]);
    }

    #[test]
    fn who_goes_first_among_the_best_is_not_always_the_same() {
        let cm = |name: &str| Cm { endpoint: name.into(), dc: String::new(), kind: "websockets".into(), load: 0.0, wtd_load: 0.0 };
        let firsts: std::collections::HashSet<String> = (0..200).map(|_| ranked((0..4).map(|i| cm(&format!("s{i}"))).collect())[0].endpoint.clone()).collect();
        assert!(firsts.len() > 1, "always {firsts:?}");
    }
}
