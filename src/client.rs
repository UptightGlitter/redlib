use crate::dbg_msg;
use crate::oauth::{force_refresh_token, Oauth, OauthBackendImpl};
use crate::server::RequestExt;
use crate::utils::{format_url, Post};
use arc_swap::ArcSwap;
use cached::proc_macro::cached;
use futures_lite::{future::Boxed, FutureExt};
use hyper::{body::Buf, header, Body, Request as HyperRequest, Response as HyperResponse};
use log::{error, info, trace, warn};
use percent_encoding::{percent_encode, CONTROLS};
use serde_json::Value;
use std::borrow::Cow;
use std::io::Read;
use std::result::Result;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicU16};
use std::sync::LazyLock;
use wreq::dns::{Addrs, Name, Resolve, Resolving};
use wreq::redirect::Policy;
use wreq::{header as wreq_header, Client as WreqClient, Method, Response as WreqResponse};
use wreq_util::{Emulation, Platform, Profile};

const REDDIT_URL_BASE: &str = "https://oauth.reddit.com";
const REDDIT_URL_BASE_HOST: &str = "oauth.reddit.com";

const REDDIT_SHORT_URL_BASE: &str = "https://redd.it";
const REDDIT_SHORT_URL_BASE_HOST: &str = "redd.it";

const ALTERNATIVE_REDDIT_URL_BASE: &str = "https://www.reddit.com";
const ALTERNATIVE_REDDIT_URL_BASE_HOST: &str = "www.reddit.com";
const ALTERNATIVE_REDDIT_DNS_HOST: &str = "dualstack.reddit.map.fastly.net";

pub static CLIENT: LazyLock<WreqClient> = LazyLock::new(build_client);

pub static OAUTH_CLIENT: LazyLock<ArcSwap<Oauth>> = LazyLock::new(|| {
	// The listener must not wait on a token. Background auth yields once so this
	// lock is finished before the task touches `OAUTH_CLIENT`.
	if let Ok(handle) = tokio::runtime::Handle::try_current() {
		handle.spawn(async {
			tokio::task::yield_now().await;
			crate::oauth::spawn_background();
		});
	}
	ArcSwap::new(crate::oauth::Oauth::placeholder().into())
});

pub static OAUTH_RATELIMIT_REMAINING: AtomicU16 = AtomicU16::new(99);

pub static OAUTH_IS_ROLLING_OVER: AtomicBool = AtomicBool::new(false);

const URL_PAIRS: [(&str, &str); 2] = [
	(ALTERNATIVE_REDDIT_URL_BASE, ALTERNATIVE_REDDIT_URL_BASE_HOST),
	(REDDIT_SHORT_URL_BASE, REDDIT_SHORT_URL_BASE_HOST),
];

#[derive(Debug)]
struct RedditDnsResolver;

// Keep the original request host for TLS/SNI and HTTP while resolving its
// addresses through Fastly's dual-stack alias.
fn reddit_dns_name(name: &str) -> &str {
	if name.eq_ignore_ascii_case(ALTERNATIVE_REDDIT_URL_BASE_HOST) {
		ALTERNATIVE_REDDIT_DNS_HOST
	} else {
		name
	}
}

impl Resolve for RedditDnsResolver {
	fn resolve(&self, name: Name) -> Resolving {
		let dns_name = reddit_dns_name(name.as_str()).to_owned();

		Box::pin(async move {
			tokio::net::lookup_host((dns_name, 0))
				.await
				.map(|addresses| Box::new(addresses) as Addrs)
				.map_err(Into::into)
		})
	}
}

pub fn build_client() -> WreqClient {
	// Keeping this list short to aid in privacy.
	// The more emulations, the more unique a fingerprint each instance has.
	// But some emulations should increase evasiveness.
	let emulation = [Profile::Chrome145, Profile::Firefox147];
	let emulation_platform = [Platform::Android, Platform::Windows];

	let rand = fastrand::usize(..);
	let emulation = Emulation::builder()
		.profile(emulation[rand % emulation.len()])
		.platform(emulation_platform[rand % emulation_platform.len()])
		.build();

	info!("Building Wreq client with random emulation");
	let mut builder = WreqClient::builder().emulation(emulation).redirect(Policy::none()).dns_resolver(RedditDnsResolver);

	match configured_reddit_proxy() {
		Ok(Some(proxy)) => builder = builder.proxy(proxy),
		Ok(None) => {}
		Err(msg) => {
			// Logger may not be installed yet; stderr is the startup signal.
			eprintln!("{msg}");
			error!("{msg}");
			std::process::exit(1);
		}
	}

	builder.build().expect("Should always be able to build a client")
}

/// `REDLIB_PROXY` applies to every request on the shared Reddit client, which is
/// also what OAuth token requests use. Unset or blank leaves the client untouched,
/// including wreq's own `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` handling.
fn configured_reddit_proxy() -> Result<Option<wreq::Proxy>, String> {
	let Ok(raw) = std::env::var("REDLIB_PROXY") else {
		return Ok(None);
	};
	let raw = raw.trim();
	if raw.is_empty() {
		return Ok(None);
	}

	let proxy = parse_reddit_proxy(raw)?;
	if let Some(target) = proxy_log_target(raw) {
		info!("Reddit API client using proxy {target}");
	}
	Ok(Some(proxy))
}

fn parse_reddit_proxy(raw: &str) -> Result<wreq::Proxy, String> {
	let Some(url) = url::Url::parse(raw).ok() else {
		return Err("REDLIB_PROXY is set but is not a valid proxy URL. Use socks5, socks5h, http, or https.".to_string());
	};
	let target = proxy_log_target(raw).unwrap_or_else(|| url.scheme().to_string());
	match url.scheme() {
		"socks5" | "socks5h" | "http" | "https" => {}
		_ => {
			return Err(format!("REDLIB_PROXY scheme is not supported ({target}). Use socks5, socks5h, http, or https."));
		}
	}

	// Drop the client error: its Display can echo the raw URL, including userinfo.
	wreq::Proxy::all(raw).map_err(|_| format!("REDLIB_PROXY could not be used ({target}). Use socks5, socks5h, http, or https."))
}

pub(crate) fn proxy_for_log() -> Option<String> {
	let raw = std::env::var("REDLIB_PROXY").ok()?;
	let raw = raw.trim();
	if raw.is_empty() {
		return None;
	}
	proxy_log_target(raw)
}

/// scheme://host:port only. Never include userinfo.
fn proxy_log_target(raw: &str) -> Option<String> {
	let url = url::Url::parse(raw).ok()?;
	let host = match url.host()? {
		url::Host::Ipv6(ip) => format!("[{ip}]"),
		_ => url.host_str()?.to_string(),
	};
	Some(match url.port() {
		Some(port) => format!("{}://{host}:{port}", url.scheme()),
		None => format!("{}://{host}", url.scheme()),
	})
}

/// Gets the canonical path for a resource on Reddit. This is accomplished by
/// making a `HEAD` request to Reddit at the path given in `path`.
///
/// This function returns `Ok(Some(path))`, where `path`'s value is identical
/// to that of the value of the argument `path`, if Reddit responds to our
/// `HEAD` request with a 2xx-family HTTP code. It will also return an
/// `Ok(Some(String))` if Reddit responds to our `HEAD` request with a
/// `Location` header in the response, and the HTTP code is in the 3xx-family;
/// the `String` will contain the path as reported in `Location`. The return
/// value is `Ok(None)` if Reddit responded with a 3xx, but did not provide a
/// `Location` header. An `Err(String)` is returned if Reddit responds with a
/// 429, or if we were unable to decode the value in the `Location` header.
#[cached(size = 1024, time = 600, result = true)]
#[async_recursion::async_recursion]
pub async fn canonical_path(path: String, tries: i8) -> Result<Option<String>, String> {
	if tries == 0 {
		return Ok(None);
	}

	// for each URL pair, try the HEAD request
	let res = {
		// for url base and host in URL_PAIRS, try reddit_short_head(path.clone(), true, url_base, url_base_host) and if it succeeds, set res. else, res = None
		let mut res = None;
		for (url_base, url_base_host) in URL_PAIRS {
			res = reddit_short_head(path.clone(), true, url_base, url_base_host).await.ok();
			if let Some(res) = &res {
				if !res.status().is_client_error() {
					break;
				}
			}
		}
		res
	};

	let res = res.ok_or_else(|| "Unable to make HEAD request to Reddit.".to_string())?;
	let status = res.status().as_u16();
	let policy_error = res.headers().get(wreq_header::RETRY_AFTER).is_some();

	match status {
		// If Reddit responds with a 2xx, then the path is already canonical.
		200..=299 => Ok(Some(path)),

		// If Reddit responds with a 301, then the path is redirected.
		301 => match res.headers().get(wreq_header::LOCATION) {
			Some(val) => {
				let Ok(original) = val.to_str() else {
					return Err("Unable to decode Location header.".to_string());
				};

				// We need to strip the .json suffix from the original path.
				// In addition, we want to remove share parameters.
				// Cut it off here instead of letting it propagate all the way
				// to main.rs
				let stripped_uri = original.strip_suffix(".json").unwrap_or(original).split('?').next().unwrap_or_default();

				// The reason why we now have to format_url, is because the new OAuth
				// endpoints seem to return full paths, instead of relative paths.
				// So we need to strip the .json suffix from the original path, and
				// also remove all Reddit domain parts with format_url.
				// Otherwise, it will literally redirect to Reddit.com.
				let uri = comments_logical_path(&format_url(stripped_uri));

				// Decrement tries and try again
				canonical_path(uri, tries - 1).await
			}
			None => Ok(None),
		},

		// If Reddit responds with anything other than 3xx (except for the 2xx and 301
		// as above), return a None.
		300..=399 => Ok(None),

		// Rate limiting
		429 => Err("Too many requests.".to_string()),

		// Special condition rate limiting - https://github.com/redlib-org/redlib/issues/229
		403 if policy_error => Err("Too many requests.".to_string()),

		_ => Ok(
			res
				.headers()
				.get(wreq_header::LOCATION)
				.map(|val| percent_encode(val.as_bytes(), CONTROLS).to_string().trim_start_matches(REDDIT_URL_BASE).to_string()),
		),
	}
}

pub async fn proxy(req: HyperRequest<Body>, format: &str) -> Result<HyperResponse<Body>, String> {
	let mut url = format!("{format}?{}", req.uri().query().unwrap_or_default());

	// For each parameter in request
	for (name, value) in &req.params() {
		// Fill the parameter value in the url
		url = url.replace(&format!("{{{name}}}"), value);
	}

	// First parameter is target URL (mandatory).
	let wreq_uri = wreq::Uri::try_from(url).map_err(|_| "Couldn't parse URL".to_string())?;

	let mut builder = CLIENT.get(wreq_uri);

	// Copy useful headers from original request
	for &key in &["Range", "If-Modified-Since", "Cache-Control"] {
		if let Some(value) = req.headers().get(key) {
			builder = builder.header(key, value.as_bytes());
		}
	}

	// Add User-Agent header of the currently spoofed device
	{
		let client = OAUTH_CLIENT.load_full();
		builder = builder.header("User-Agent", client.user_agent());
	}

	// This is needed or Reddit will redirect us to a /media landing page that just renders the image.
	builder = builder.header(wreq_header::ACCEPT, "*/*");

	builder
		.send()
		.await
		.map(|mut res| {
			let headers = res.headers_mut();

			let mut rm = |key: &str| headers.remove(key);

			rm("access-control-expose-headers");
			rm("server");
			rm("vary");
			rm("etag");
			rm("x-cdn");
			rm("x-cdn-client-region");
			rm("x-cdn-name");
			rm("x-cdn-server-region");
			rm("x-reddit-cdn");
			rm("x-reddit-video-features");
			rm("Nel");
			rm("Report-To");

			res.into_hyper_response()
		})
		.map_err(|e| e.to_string())
}

/// Makes a GET request to Reddit at `path`. By default, this will honor HTTP
/// 3xx codes Reddit returns and will automatically redirect.
fn reddit_get(path: String, quarantine: bool) -> Boxed<Result<WreqResponse, String>> {
	request(&Method::GET, path, true, quarantine, REDDIT_URL_BASE, REDDIT_URL_BASE_HOST)
}

/// Makes a HEAD request to Reddit at `path, using the short URL base. This will not follow redirects.
fn reddit_short_head(path: String, quarantine: bool, base_path: &'static str, host: &'static str) -> Boxed<Result<WreqResponse, String>> {
	request(&Method::HEAD, path, false, quarantine, base_path, host)
}

// /// Makes a HEAD request to Reddit at `path`. This will not follow redirects.
// fn reddit_head(path: String, quarantine: bool) -> Boxed<Result<Response<Body>, String>> {
// 	request(&Method::HEAD, path, false, quarantine, false)
// }
// Unused - reddit_head is only ever called in the context of a short URL

/// Makes a request to Reddit. If `redirect` is `true`, `request_with_redirect`
/// will recurse on the URL that Reddit provides in the Location HTTP header
/// in its response.
fn request(method: &'static Method, path: String, redirect: bool, quarantine: bool, base_path: &'static str, host: &'static str) -> Boxed<Result<WreqResponse, String>> {
	// Keep the logical path unchanged for caches and diagnostics. Reddit blocks
	// literal anonymous comments routes, so only rewrite the outgoing URL.
	let transport_path = comments_transport_path(&path);
	let url = format!("{base_path}{transport_path}");

	let mut headers: Vec<(String, String)> = vec![
		("Host".into(), host.into()),
		(
			"Cookie".into(),
			if quarantine {
				"_options=%7B%22pref_quarantine_optin%22%3A%20true%2C%20%22pref_gated_sr_optin%22%3A%20true%7D".into()
			} else {
				"".into()
			},
		),
	];

	{
		let client = OAUTH_CLIENT.load_full();
		for (key, value) in client.headers_map.clone() {
			headers.push((key, value));
		}
	}

	// shuffle headers: https://github.com/redlib-org/redlib/issues/324
	fastrand::shuffle(&mut headers);

	let mut builder = CLIENT.request(method.clone(), &url);

	for (key, value) in headers {
		builder = builder.header(key, value);
	}

	async move {
		match builder.send().await {
			Ok(response) => {
				// Reddit may respond with a 3xx. Decide whether or not to
				// redirect based on caller params.
				if response.status().is_redirection() {
					if !redirect {
						return Ok(response);
					};
					let location_header = response.headers().get(wreq::header::LOCATION);
					if location_header.and_then(|h| h.to_str().ok()) == Some(ALTERNATIVE_REDDIT_URL_BASE) {
						return Err("Reddit response was invalid".to_string());
					}
					return request(
						method,
						location_header
							.map(|val| {
								// We need to make adjustments to the URI
								// we get back from Reddit. Namely, we
								// must:
								//
								//     1. Remove the authority (e.g.
								//     https://www.reddit.com) that may be
								//     present, so that we recurse on the
								//     path (and query parameters) as
								//     required.
								//
								//     2. Percent-encode the path.
								let new_path = percent_encode(val.as_bytes(), CONTROLS)
									.to_string()
									.trim_start_matches(REDDIT_URL_BASE)
									.trim_start_matches(ALTERNATIVE_REDDIT_URL_BASE)
									.to_string();
								format!("{new_path}{}raw_json=1", if new_path.contains('?') { "&" } else { "?" })
							})
							.unwrap_or_default()
							.to_string(),
						true,
						quarantine,
						base_path,
						host,
					)
					.await;
				};

				Ok(response)
			}
			Err(e) => {
				dbg_msg!("{method} {REDDIT_URL_BASE}{path}: {}", e);

				Err(e.to_string())
			}
		}
	}
	.boxed()
}

/// Encode one separator in comments API routes to avoid Reddit's anonymous
/// request block while leaving every other path and the query string intact.
fn comments_transport_path(path: &str) -> Cow<'_, str> {
	let base = path.split('?').next().unwrap_or(path);
	let (separator, tail) = if let Some(tail) = base.strip_prefix("/comments/") {
		("/comments".len(), tail)
	} else if let Some(scoped) = base.strip_prefix("/r/") {
		let Some((subreddit, rest)) = scoped.split_once('/') else {
			return Cow::Borrowed(path);
		};
		let Some(tail) = rest.strip_prefix("comments/") else {
			return Cow::Borrowed(path);
		};
		if subreddit.is_empty() {
			return Cow::Borrowed(path);
		}
		("/r/".len() + subreddit.len(), tail)
	} else {
		return Cow::Borrowed(path);
	};

	let tail = tail.strip_suffix(".json").unwrap_or(tail).trim_end_matches('/');
	let mut segments = tail.split('/');
	let Some(post_id) = segments.next() else {
		return Cow::Borrowed(path);
	};
	let _slug = segments.next();
	let comment_id = segments.next();

	if post_id.is_empty() || comment_id.is_some_and(str::is_empty) || segments.next().is_some() {
		return Cow::Borrowed(path);
	}

	Cow::Owned(format!("{}%2F{}", &path[..separator], &path[separator + 1..]))
}

/// Remove the comments-route separator encoding if Reddit reflects our
/// transport-only rewrite in a redirect.
fn comments_logical_path(path: &str) -> String {
	let path = path.replace("/comments%2F", "/comments/").replace("/comments%2f", "/comments/");

	let Some(scoped) = path.strip_prefix("/r/") else {
		return path;
	};
	let Some((subreddit, tail)) = scoped.split_once("%2Fcomments/").or_else(|| scoped.split_once("%2fcomments/")) else {
		return path;
	};
	if subreddit.is_empty() || subreddit.contains('/') {
		return path;
	}
	format!("/r/{subreddit}/comments/{tail}")
}

/// User-facing error for a body that is not Reddit's JSON API document.
/// Uses the logical `path` (not the transport encoding) and never request headers.
pub(crate) fn non_json_response_message(status: wreq::StatusCode, content_type: &str, body: &[u8], path: &str) -> String {
	let text = String::from_utf8_lossy(body);
	let visible = collapse_whitespace(&strip_markup(&text));
	let redacted = redact_secrets(&visible).replace('|', "/");
	let lower = redacted.to_ascii_lowercase();
	let lead = if lower.contains("blocked by network security") {
		"Reddit blocked this request (network security)"
	} else if lower.contains("network policy") {
		"Reddit blocked this request (network policy)"
	} else if status.is_server_error() {
		"Reddit is having issues, check if there's an outage"
	} else {
		"Failed to parse page JSON data"
	};
	let ctype = if content_type.is_empty() { "(none)" } else { content_type.trim() };
	let snippet = truncate_chars(&redacted, 200);
	format!("{lead}. HTTP {status}, content-type {ctype}: {snippet} | {path}")
}

fn strip_markup(input: &str) -> String {
	let lower = input.to_ascii_lowercase();
	let bytes = input.as_bytes();
	let mut out = String::with_capacity(input.len());
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] == b'<' {
			let rest = &lower[i..];
			if let Some(end_tag) = if rest.starts_with("<script") {
				Some("</script>")
			} else if rest.starts_with("<style") {
				Some("</style>")
			} else {
				None
			} {
				if let Some(end) = rest.find(end_tag) {
					i += end + end_tag.len();
					out.push(' ');
					continue;
				}
			}
			if let Some(end) = input[i..].find('>') {
				i += end + 1;
				out.push(' ');
				continue;
			}
		}
		let ch = input[i..].chars().next().unwrap_or('\u{FFFD}');
		out.push(ch);
		i += ch.len_utf8();
	}
	out
}

fn collapse_whitespace(input: &str) -> String {
	let mut out = String::with_capacity(input.len());
	let mut pending_space = false;
	for ch in input.chars() {
		if ch.is_whitespace() {
			pending_space = !out.is_empty();
			continue;
		}
		if pending_space {
			out.push(' ');
			pending_space = false;
		}
		out.push(ch);
	}
	out
}

fn redact_secrets(input: &str) -> String {
	let chars: Vec<char> = input.chars().collect();
	let lower: Vec<char> = input.to_ascii_lowercase().chars().collect();
	let mut out = String::with_capacity(input.len());
	let mut i = 0;
	while i < chars.len() {
		if starts_with_chars(&lower, i, "bearer ") {
			out.push_str("Bearer ");
			i += "bearer ".chars().count();
			while i < chars.len() && !chars[i].is_whitespace() {
				i += 1;
			}
			out.push_str("[REDACTED]");
			continue;
		}
		if starts_with_chars(&lower, i, "access_token") {
			out.push_str("access_token");
			i += "access_token".chars().count();
			// JSON keys are quoted (`"access_token": "value"`).
			if i < chars.len() && (chars[i] == '"' || chars[i] == '\'') {
				out.push(chars[i]);
				i += 1;
			}
			while i < chars.len() && matches!(chars[i], ' ' | '\t' | ':' | '=') {
				out.push(chars[i]);
				i += 1;
			}
			if i < chars.len() && (chars[i] == '"' || chars[i] == '\'') {
				let quote = chars[i];
				out.push(quote);
				i += 1;
				while i < chars.len() && chars[i] != quote {
					i += 1;
				}
				out.push_str("[REDACTED]");
				if i < chars.len() && chars[i] == quote {
					out.push(quote);
					i += 1;
				}
			} else {
				while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '&' {
					i += 1;
				}
				out.push_str("[REDACTED]");
			}
			continue;
		}
		out.push(chars[i]);
		i += 1;
	}
	out
}

fn starts_with_chars(haystack: &[char], at: usize, needle: &str) -> bool {
	let needle: Vec<char> = needle.chars().collect();
	haystack[at..].starts_with(&needle)
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
	let count = input.chars().count();
	if count <= max_chars {
		return input.to_string();
	}
	let keep = max_chars.saturating_sub(1);
	let mut out: String = input.chars().take(keep).collect();
	out.push('…');
	out
}

/// Make a request to a Reddit API and parse the JSON response.
pub async fn json(path: String, quarantine: bool) -> Result<Value, String> {
	if let Err(message) = crate::oauth::prepare_token().await {
		return Err(format!("{message} | {path}"));
	}
	json_cached(path, quarantine).await
}

#[cached(size = 100, time = 30, result = true)]
async fn json_cached(path: String, quarantine: bool) -> Result<Value, String> {
	json_uncached(path, quarantine).await
}

/// Uncached fetch. GraphQL comment loading calls this for `/by_id` so it does not re-enter the cache.
pub(crate) async fn json_uncached(path: String, quarantine: bool) -> Result<Value, String> {
	// `/comments/` JSON is blocked for anonymous tokens. Listings stay on the JSON API.
	if path.contains("/comments/") {
		return Box::pin(crate::gql::thread(&path, quarantine)).await;
	}
	// Listings, about, wiki and search go straight to GraphQL. Trying the JSON
	// route first would 403 on every page and log that block as an error.
	if let Some(result) = crate::gql_feed::route(&path, quarantine).await {
		return result;
	}

	// Closure to quickly build errors
	let err = |msg: &str, e: String, path: String| -> Result<Value, String> {
		// eprintln!("{} - {}: {}", url, msg, e);
		Err(format!("{msg}: {e} | {path}"))
	};

	// First, handle rolling over the OAUTH_CLIENT if need be.
	let current_rate_limit = OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst);
	let is_rolling_over = OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst);
	if current_rate_limit < 10 && !is_rolling_over {
		warn!("Rate limit {current_rate_limit} is low. Spawning force_refresh_token()");
		tokio::spawn(force_refresh_token());
	}
	#[allow(deprecated)] // try_update is newer than the project's MSRV.
	let _ = OAUTH_RATELIMIT_REMAINING.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| remaining.checked_sub(1));

	// Fetch the url...
	match reddit_get(path.clone(), quarantine).await {
		Ok(response) => {
			let status = response.status();
			let content_type = response
				.headers()
				.get(wreq_header::CONTENT_TYPE)
				.and_then(|value| value.to_str().ok())
				.unwrap_or("")
				.to_string();

			let reset: Option<String> = if let (Some(remaining), Some(reset), Some(used)) = (
				response.headers().get("x-ratelimit-remaining").and_then(|val| val.to_str().ok().map(|s| s.to_string())),
				response.headers().get("x-ratelimit-reset").and_then(|val| val.to_str().ok().map(|s| s.to_string())),
				response.headers().get("x-ratelimit-used").and_then(|val| val.to_str().ok().map(|s| s.to_string())),
			) {
				trace!(
					"Ratelimit remaining: Header says {remaining}, we have {current_rate_limit}. Resets in {reset}. Rollover: {}. Ratelimit used: {used}",
					if is_rolling_over { "yes" } else { "no" },
				);

				// If can parse remaining as a float, round to a u16 and save
				if let Ok(val) = remaining.parse::<f32>() {
					OAUTH_RATELIMIT_REMAINING.store(val.round() as u16, Ordering::SeqCst);
				}

				Some(reset)
			} else {
				None
			};

			// asynchronously aggregate the chunks of the body
			match hyper::body::aggregate(response.into_hyper_response()).await {
				Ok(body) => {
					let has_remaining = body.has_remaining();

					if !has_remaining {
						// Rate limited, so spawn a force_refresh_token()
						tokio::spawn(force_refresh_token());
						return match reset {
							Some(val) => Err(format!(
								"Reddit rate limit exceeded. Try refreshing in a few seconds.\
								 Rate limit will reset in: {val}"
							)),
							None => Err("Reddit rate limit exceeded".to_string()),
						};
					}

					let mut raw = Vec::new();
					if let Err(e) = body.reader().read_to_end(&mut raw) {
						return err("Failed receiving body from Reddit", e.to_string(), path);
					}

					// Parse the response from Reddit as JSON. A non-JSON body (the HTML
					// 403 block page) must not refresh or rotate the OAuth token.
					match serde_json::from_slice(&raw) {
						Ok(value) => {
							let json: Value = value;

							// If user is suspended
							if let Some(data) = json.get("data") {
								if let Some(is_suspended) = data.get("is_suspended").and_then(Value::as_bool) {
									if is_suspended {
										return Err("suspended".into());
									}
								}
							}

							// If Reddit returned an error
							if json["error"].is_i64() {
								// OAuth token has expired; http status 401
								if json["message"] == "Unauthorized" {
									error!("Forcing a token refresh");
									let () = force_refresh_token().await;
									return Err("OAuth token has expired. Please refresh the page!".to_string());
								}

								// Handle quarantined
								if json["reason"] == "quarantined" {
									return Err("quarantined".into());
								}
								// Handle gated
								if json["reason"] == "gated" {
									return Err("gated".into());
								}
								// Handle private subs
								if json["reason"] == "private" {
									return Err("private".into());
								}
								// Handle banned subs
								if json["reason"] == "banned" {
									return Err("banned".into());
								}

								Err(format!("Reddit error {} \"{}\": {} | {path}", json["error"], json["reason"], json["message"]))
							} else {
								Ok(json)
							}
						}
						Err(e) => {
							let detail = non_json_response_message(status, &content_type, &raw, &path);
							error!("Got an invalid response from reddit {e}. {detail}");
							Err(detail)
						}
					}
				}
				Err(e) => err("Failed receiving body from Reddit", e.to_string(), path),
			}
		}
		Err(e) => {
			let e = crate::oauth::redact_credentials(&e);
			if e.to_ascii_lowercase().contains("proxy") || e.to_ascii_lowercase().contains("connect") || e.to_ascii_lowercase().contains("timed out") {
				Err(format!("{} | {path}", crate::oauth::unreachable_message()))
			} else {
				err("Couldn't send request to Reddit", e, path)
			}
		}
	}
}

async fn self_check(sub: &str) -> Result<(), String> {
	let query = format!("/r/{sub}/hot.json?&raw_json=1");

	match Post::fetch(&query, true).await {
		Ok(_) => Ok(()),
		Err(e) => Err(e),
	}
}

pub async fn rate_limit_check() -> Result<(), String> {
	if !OAUTH_CLIENT.load().has_token() {
		warn!("[⚠️] Skipping rate limit check because the OAuth token is not ready.");
		return Ok(());
	}
	// First, test the Oauth client: we can perform a rate limit check if the OAuth backend is MobileSpoof; if GenericWeb, we skip the check.
	if matches!(OAUTH_CLIENT.load().backend, OauthBackendImpl::GenericWeb(_)) {
		warn!("[⚠️] Cannot perform rate limit check, running as GenericWeb. Skipping check.");
		return Ok(());
	}

	// First, check a subreddit.
	self_check("reddit").await?;
	// This will reduce the rate limit to 99. Assert this check.
	if OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst) != 99 {
		return Err(format!("Rate limit check 1 failed: expected 99, got {}", OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst)));
	}
	// Now, we switch out the OAuth client.
	// This checks for the IP rate limit association.
	force_refresh_token().await;
	// Now, check a new sub to break cache.
	self_check("rust").await?;
	// Again, assert the rate limit check.
	if OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst) != 99 {
		return Err(format!("Rate limit check 2 failed: expected 99, got {}", OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst)));
	}

	Ok(())
}

trait IntoHyperResponse {
	fn into_hyper_response(self) -> HyperResponse<Body>;
}

impl IntoHyperResponse for WreqResponse {
	fn into_hyper_response(self) -> HyperResponse<Body> {
		let status = self.status();
		let version = self.version();

		let mut builder = HyperResponse::builder().status(status.as_u16()).version(match version {
			wreq::Version::HTTP_09 => hyper::Version::HTTP_09,
			wreq::Version::HTTP_10 => hyper::Version::HTTP_10,
			wreq::Version::HTTP_11 => hyper::Version::HTTP_11,
			wreq::Version::HTTP_2 => hyper::Version::HTTP_2,
			wreq::Version::HTTP_3 => hyper::Version::HTTP_3,
			_ => hyper::Version::HTTP_11,
		});

		for (name, value) in self.headers() {
			builder = builder.header(
				header::HeaderName::from_bytes(name.as_str().as_bytes()).unwrap(),
				header::HeaderValue::from_bytes(value.as_bytes()).unwrap(),
			);
		}

		builder.body(Body::wrap_stream(self.bytes_stream())).unwrap()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use {crate::config::get_setting, sealed_test::prelude::*};

	const POPULAR_URL: &str = "/r/popular/hot.json?&raw_json=1&geo_filter=GLOBAL";

	#[test]
	fn test_reddit_dns_name_uses_fastly_dualstack_alias() {
		assert_eq!(reddit_dns_name("www.reddit.com"), ALTERNATIVE_REDDIT_DNS_HOST);
		assert_eq!(reddit_dns_name("WWW.REDDIT.COM"), ALTERNATIVE_REDDIT_DNS_HOST);
		assert_eq!(reddit_dns_name("oauth.reddit.com"), "oauth.reddit.com");
	}

	#[test]
	fn test_comments_transport_path_encodes_scoped_and_short_routes() {
		for (path, expected) in [
			(
				"/r/rust/comments/18t5968/title/.json?sort=new&raw_json=1",
				"/r/rust%2Fcomments/18t5968/title/.json?sort=new&raw_json=1",
			),
			("/r/rust/comments/18t5968/_/kfbqlbc.json?context=3", "/r/rust%2Fcomments/18t5968/_/kfbqlbc.json?context=3"),
			("/comments/18t5968.json?raw_json=1", "/comments%2F18t5968.json?raw_json=1"),
			("/comments/18t5968/_/kfbqlbc.json?context=3", "/comments%2F18t5968/_/kfbqlbc.json?context=3"),
			("/r/comments/comments/18t5968.json", "/r/comments%2Fcomments/18t5968.json"),
		] {
			assert_eq!(comments_transport_path(path), expected);
			assert_eq!(comments_transport_path(expected), expected, "encoding must be idempotent");
		}
	}

	#[test]
	fn test_comments_transport_path_preserves_other_routes_and_queries() {
		for path in [
			"/r/popular/hot.json?after=t3_abc",
			"/r/rust/comments.json",
			"/r/rust/commentsXYZ/abc.json",
			"/r/rust/wiki/comments/abc.json",
			"/r//comments/abc.json",
			"/comments/.json",
			"/comments.json",
			"/user/example/comments.json",
			"/user/example/comments/abc.json",
			"/search.json?q=/r/rust/comments/abc",
			"https://example.com/r/rust/comments/abc.json",
			"//example.com/r/rust/comments/abc.json",
		] {
			assert!(matches!(comments_transport_path(path), Cow::Borrowed(_)), "{path}");
		}

		let path = "/r/rust/comments/abc.json?sort=new&context=3&q=a%2Bb&empty=&sort=top";
		assert_eq!(comments_transport_path(path), "/r/rust%2Fcomments/abc.json?sort=new&context=3&q=a%2Bb&empty=&sort=top");
	}

	#[test]
	fn test_non_json_error_names_network_block_and_strips_markup() {
		let body = br#"<body><style>.x{color:red}</style><div>You've been blocked by network security.</div><div>If you think you've been blocked by mistake.</div></body>"#;
		let msg = non_json_response_message(wreq::StatusCode::FORBIDDEN, "text/html", body, "/r/rust/comments/abc/.json?raw_json=1");
		assert!(msg.contains("Reddit blocked this request (network security)"));
		assert!(msg.contains("HTTP 403 Forbidden"));
		assert!(msg.contains("content-type text/html"));
		assert!(msg.contains("You've been blocked by network security. If you think you've been blocked by mistake."));
		assert!(msg.ends_with("| /r/rust/comments/abc/.json?raw_json=1"));
		assert!(!msg.contains("<div"));
		assert!(!msg.contains("color:red"));
	}

	#[test]
	fn test_non_json_error_names_network_policy() {
		let body = b"<html><title>Blocked</title><body>whoa there, pardner! Your request has been blocked due to a network policy.</body></html>";
		let msg = non_json_response_message(wreq::StatusCode::FORBIDDEN, "text/html", body, "/r/rust/hot.json");
		assert!(msg.contains("Reddit blocked this request (network policy)"));
		assert!(msg.contains("network policy"));
		assert!(!msg.contains("<title"));
	}

	#[test]
	fn test_non_json_error_redacts_tokens_collapses_whitespace_and_truncates() {
		let secret = "super-secret-token";
		let bearer = "abc.def.ghi";
		let body = format!("nope   Bearer {bearer}\n{{\"access_token\":\"{secret}\"}}{}", " word".repeat(80));
		let msg = non_json_response_message(wreq::StatusCode::BAD_GATEWAY, "", body.as_bytes(), "/r/x/hot.json");
		assert!(msg.contains("Reddit is having issues, check if there's an outage"));
		assert!(msg.contains("HTTP 502 Bad Gateway"));
		assert!(msg.contains("content-type (none)"));
		assert!(msg.contains("Bearer [REDACTED]"));
		assert!(msg.contains("access_token\":\"[REDACTED]\""));
		assert!(!msg.contains(secret));
		assert!(!msg.contains(bearer));
		assert!(!msg.contains("  "));
		assert!(msg.contains('…'));
		assert!(msg.ends_with("| /r/x/hot.json"));
	}

	#[test]
	fn test_proxy_log_target_hides_credentials() {
		assert_eq!(proxy_log_target("socks5h://user:s3cret-pass@127.0.0.1:1080").as_deref(), Some("socks5h://127.0.0.1:1080"));
		assert_eq!(proxy_log_target("http://alice:wonder@proxy.example:8080").as_deref(), Some("http://proxy.example:8080"));
		assert_eq!(proxy_log_target("https://proxy.example").as_deref(), Some("https://proxy.example"));
		let err = parse_reddit_proxy("ftp://user:s3cret-pass@evil.example:21").unwrap_err();
		assert!(err.contains("ftp://evil.example"), "{err}");
		assert!(!err.contains("s3cret-pass"));
		assert!(!err.contains("user:"));
		let bad = parse_reddit_proxy("not a url").unwrap_err();
		assert!(!bad.contains("not a url"));
		assert!(parse_reddit_proxy("socks5h://user:s3cret-pass@127.0.0.1:1080").is_ok());
		assert!(parse_reddit_proxy("socks5://127.0.0.1:1080").is_ok());
		assert!(parse_reddit_proxy("http://127.0.0.1:8080").is_ok());
		assert!(parse_reddit_proxy("https://proxy.example:8443").is_ok());
	}

	#[test]
	fn test_comments_logical_path_removes_transport_encoding() {
		for (path, expected) in [
			("/r/rust%2Fcomments/18t5968/title/kfbqlbc/", "/r/rust/comments/18t5968/title/kfbqlbc/"),
			("/r/rust%2fcomments/18t5968/title/kfbqlbc/", "/r/rust/comments/18t5968/title/kfbqlbc/"),
			("/comments%2F18t5968/title/", "/comments/18t5968/title/"),
			("/r/rust%2Fwiki/page", "/r/rust%2Fwiki/page"),
		] {
			assert_eq!(comments_logical_path(path), expected);
		}
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_rate_limit_check() {
		rate_limit_check().await.unwrap();
	}

	#[test]
	#[sealed_test(env = [("REDLIB_DEFAULT_SUBSCRIPTIONS", "rust")])]
	fn test_default_subscriptions() {
		tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async {
			let subscriptions = get_setting("REDLIB_DEFAULT_SUBSCRIPTIONS");
			assert!(subscriptions.is_some());

			// check rate limit
			rate_limit_check().await.unwrap();
		});
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_localization_popular() {
		// PopularFeedElements does not return the JSON API's geo_filter. The
		// listing itself still has to load.
		let val = json(POPULAR_URL.to_string(), false).await.unwrap();
		assert!(val["data"]["children"].as_array().is_some_and(|children| !children.is_empty()));
		assert!(val["data"].get("geo_filter").is_none());
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_obfuscated_share_link() {
		let share_link = "/r/rust/s/kPgq8WNHRK".into();
		// Correct link without share parameters
		let canonical_link = "/r/rust/comments/18t5968/why_use_tuple_struct_over_standard_struct/kfbqlbc/".into();
		assert_eq!(canonical_path(share_link, 3).await, Ok(Some(canonical_link)));
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_private_sub() {
		let link = json("/r/suicide/about.json?raw_json=1".into(), true).await;
		assert!(link.is_err());
		assert_eq!(link, Err("private".into()));
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_banned_sub() {
		let link = json("/r/aaa/about.json?raw_json=1".into(), true).await;
		assert!(link.is_err());
		assert_eq!(link, Err("banned".into()));
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_gated_sub() {
		// quarantine to false to specifically catch when we _don't_ catch it
		let link = json("/r/drugs/about.json?raw_json=1".into(), false).await;
		assert!(link.is_err());
		assert_eq!(link, Err("gated".into()));
	}
}
