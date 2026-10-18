//! Fetch host calls against a fake network: DNS answers and responses are
//! scripted, and nothing leaves the process.
use super::*;
use crate::network::{Net, Pending};
use std::{collections::HashMap, net::SocketAddr};

/// A scripted response: status, headers and body.
type Scripted = (u16, Vec<(&'static str, String)>, Vec<u8>);

#[derive(Default)]
struct FakeNet {
    dns: HashMap<String, Vec<std::net::IpAddr>>,
    routes: HashMap<String, Scripted>,
    /// Every host resolved and URL requested, in order.
    seen: Mutex<Vec<String>>,
}
impl Net for FakeNet {
    fn resolve<'a>(&'a self, host: &'a str) -> Pending<'a, std::io::Result<Vec<SocketAddr>>> {
        self.seen.lock().unwrap().push(format!("resolve {host}"));
        let answers = self
            .dns
            .get(host)
            .map(|ips| ips.iter().map(|ip| SocketAddr::new(*ip, 443)).collect());
        Box::pin(async move {
            answers.ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
        })
    }
    fn get<'a>(
        &'a self,
        url: &'a url::Url,
        _host: &'a str,
        answers: &'a [SocketAddr],
    ) -> Pending<'a, Result<reqwest::Response, reqwest::Error>> {
        assert!(
            answers
                .iter()
                .all(|a| crate::network::public_address(a.ip()))
        );
        self.seen.lock().unwrap().push(format!("get {url}"));
        let (status, headers, body) =
            self.routes
                .get(url.as_str())
                .cloned()
                .unwrap_or((404, vec![], vec![]));
        Box::pin(async move {
            let mut response = http::Response::builder().status(status);
            for (name, value) in headers {
                response = response.header(name, value);
            }
            Ok(reqwest::Response::from(response.body(body).unwrap()))
        })
    }
}

fn host() -> String {
    format!("{}.example", uuid::Uuid::new_v4().simple())
}
fn public() -> std::net::IpAddr {
    // TEST-NET addresses are reserved; this one is globally routable and
    // only ever handed to the fake.
    "8.8.8.8".parse().unwrap()
}
/// Fetch host calls for an extension declaring `hosts`, granted `network`
/// when `granted`.
fn fetch_calls(
    store: &Arc<Mutex<Store>>,
    net: FakeNet,
    hosts: &[String],
    granted: bool,
) -> (HostCalls, Arc<FakeNet>) {
    let grant: &[Capability] = if granted { &[Capability::Network] } else { &[] };
    let mut calls = host_calls(store, None, grant);
    if let Some(plugin) = store
        .lock()
        .unwrap()
        .state
        .plugins
        .iter_mut()
        .find(|p| p.id == calls.extension)
    {
        plugin.granted = grant.iter().copied().collect();
    }
    let net = Arc::new(net);
    calls.hosts = hosts.to_vec();
    calls.net = Arc::clone(&net) as Arc<dyn Net>;
    (calls, net)
}
fn fetch(url: &str) -> HostRequest {
    HostRequest::Fetch { url: url.into() }
}

#[tokio::test]
async fn fetch_needs_the_grant_and_a_declared_host() {
    let (_dir, store) = store();
    let (allowed, other) = (host(), host());
    let mut net = FakeNet::default();
    net.dns.insert(allowed.clone(), vec![public()]);
    net.dns.insert(other.clone(), vec![public()]);
    let (mut denied, seen) = fetch_calls(&store, net, std::slice::from_ref(&allowed), false);
    assert_eq!(
        code(denied.handle(fetch(&format!("https://{allowed}/"))).await),
        Some(HostErrorCode::Denied)
    );
    assert!(seen.seen.lock().unwrap().is_empty());

    let mut net = FakeNet::default();
    net.dns.insert(other.clone(), vec![public()]);
    let (mut granted, seen) = fetch_calls(&store, net, &[allowed], true);
    assert_eq!(
        code(granted.handle(fetch(&format!("https://{other}/"))).await),
        Some(HostErrorCode::Denied)
    );
    // Refused before any lookup or connection.
    assert!(seen.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn fetch_refuses_private_addresses_insecure_urls_and_undeclared_redirects() {
    let (_dir, store) = store();
    let (internal, start, elsewhere, rebinding) = (host(), host(), host(), host());
    let mut net = FakeNet::default();
    net.dns
        .insert(internal.clone(), vec!["10.0.0.1".parse().unwrap()]);
    net.dns.insert(start.clone(), vec![public()]);
    net.dns.insert(
        rebinding.clone(),
        vec![public(), "127.0.0.1".parse().unwrap()],
    );
    net.dns.insert(elsewhere.clone(), vec![public()]);
    let location = |to: &str| vec![("location", format!("https://{to}/next"))];
    net.routes.insert(
        format!("https://{start}/out"),
        (302, location(&elsewhere), vec![]),
    );
    net.routes.insert(
        format!("https://{start}/in"),
        (301, location(&internal), vec![]),
    );
    let declared = [internal.clone(), start.clone(), rebinding.clone()];
    let (mut calls, seen) = fetch_calls(&store, net, &declared, true);
    for (url, expected) in [
        (format!("https://{internal}/"), HostErrorCode::Denied),
        (format!("https://{rebinding}/"), HostErrorCode::Denied),
        (format!("https://{start}/out"), HostErrorCode::Denied),
        (format!("https://{start}/in"), HostErrorCode::Denied),
        (format!("http://{start}/"), HostErrorCode::Invalid),
        (
            format!("https://user:pass@{start}/"),
            HostErrorCode::Invalid,
        ),
        (format!("https://{start}:8443/"), HostErrorCode::Invalid),
        ("not a url".to_string(), HostErrorCode::Invalid),
    ] {
        assert_eq!(
            code(calls.handle(fetch(&url)).await),
            Some(expected),
            "{url}"
        );
    }
    let seen = seen.seen.lock().unwrap();
    assert!(!seen.iter().any(|s| s.contains(&elsewhere)), "{seen:?}");
    assert!(
        !seen
            .iter()
            .any(|s| s.starts_with("get") && (s.contains(&internal) || s.contains(&rebinding))),
        "{seen:?}"
    );
}

#[tokio::test]
async fn fetch_returns_any_status_and_bounds_the_body() {
    let (_dir, store) = store();
    let site = host();
    let body = uuid::Uuid::new_v4().to_string().into_bytes();
    let big = vec![b'x'; crate::extension_host::fetch::MAX_FETCH + 1];
    let mut net = FakeNet::default();
    net.dns.insert(site.clone(), vec![public()]);
    let text = vec![("content-type", "text/plain; charset=utf-8".to_string())];
    net.routes.insert(
        format!("https://{site}/a"),
        (302, vec![("location", "/b".into())], vec![]),
    );
    net.routes.insert(
        format!("https://{site}/b"),
        (404, text.clone(), body.clone()),
    );
    net.routes
        .insert(format!("https://{site}/big"), (200, vec![], big.clone()));
    // Not a redirect, despite the 3xx code: there is nothing to follow.
    net.routes
        .insert(format!("https://{site}/same"), (304, vec![], vec![]));
    let (mut calls, _) = fetch_calls(
        &store,
        net,
        &[a206_extensions::manifest::ANY_HOST.into()],
        true,
    );
    assert_eq!(
        calls.handle(fetch(&format!("https://{site}/a"))).await,
        HostReply::Fetched(a206_extensions::abi::FetchResponse {
            status: 404,
            content_type: Some(text[0].1.clone()),
            body: body.into(),
            truncated: false,
            redirected: true,
        })
    );
    let HostReply::Fetched(cut) = calls.handle(fetch(&format!("https://{site}/big"))).await else {
        panic!("fetch failed");
    };
    assert!(cut.truncated);
    assert_eq!(cut.body.len(), crate::extension_host::fetch::MAX_FETCH);
    assert_eq!(cut.content_type, None);
    assert!(!cut.redirected);
    let HostReply::Fetched(unchanged) = calls.handle(fetch(&format!("https://{site}/same"))).await
    else {
        panic!("fetch failed");
    };
    assert_eq!(unchanged.status, 304);
}
