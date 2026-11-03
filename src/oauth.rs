use crate::{
	client::{CLIENT, OAUTH_CLIENT, OAUTH_IS_ROLLING_OVER, OAUTH_RATELIMIT_REMAINING},
	oauth_resources::ANDROID_APP_VERSION_LIST,
};
use base64::{engine::general_purpose, Engine as _};
use log::{error, info, trace};
use serde_json::json;
use std::{collections::HashMap, sync::atomic::Ordering, time::Duration};
use tegen::tegen::TextGenerator;
use tokio::time::{error::Elapsed, timeout};

const REDDIT_ANDROID_OAUTH_CLIENT_ID: &str = "ohXpoqrZYub1kg";

const AUTH_ENDPOINT: &str = "https://www.reddit.com";

const OAUTH_TIMEOUT: Duration = Duration::from_secs(5);

// Response from OAuth backend authentication
#[derive(Debug, Clone)]
pub struct OauthResponse {
	pub token: String,
	pub expires_in: u64,
	pub additional_headers: HashMap<String, String>,
}

// Trait for OAuth backend implementations
trait OauthBackend: Send + Sync {
	fn authenticate(&mut self) -> impl std::future::Future<Output = Result<OauthResponse, AuthError>> + Send;
	fn user_agent(&self) -> &str;
	fn get_headers(&self) -> HashMap<String, String>;
}

// OAuth backend implementations
#[derive(Debug, Clone)]
pub(crate) enum OauthBackendImpl {
	MobileSpoof(MobileSpoofAuth),
	GenericWeb(GenericWebAuth),
}

impl OauthBackend for OauthBackendImpl {
	async fn authenticate(&mut self) -> Result<OauthResponse, AuthError> {
		match self {
			OauthBackendImpl::MobileSpoof(backend) => backend.authenticate().await,
			OauthBackendImpl::GenericWeb(backend) => backend.authenticate().await,
		}
	}

	fn user_agent(&self) -> &str {
		match self {
			OauthBackendImpl::MobileSpoof(backend) => backend.user_agent(),
			OauthBackendImpl::GenericWeb(backend) => backend.user_agent(),
		}
	}

	fn get_headers(&self) -> HashMap<String, String> {
		match self {
			OauthBackendImpl::MobileSpoof(backend) => backend.get_headers(),
			OauthBackendImpl::GenericWeb(backend) => backend.get_headers(),
		}
	}
}

// Spoofed client for Android devices
#[derive(Debug, Clone)]
pub struct Oauth {
	pub(crate) headers_map: HashMap<String, String>,
	expires_in: u64,
	pub(crate) backend: OauthBackendImpl,
	has_token: bool,
}

/// Delay after a failed attempt. 1s, 2s, 4s, then capped at 8s.
pub(crate) fn retry_delay(failed_attempt: u32) -> Duration {
	let shift = failed_attempt.saturating_sub(1).min(3);
	Duration::from_secs(1u64 << shift)
}

/// How long the refresh task waits. Never underflows when the token is missing.
pub(crate) fn refresh_wait(expires_in: u64) -> u64 {
	expires_in.saturating_sub(120).max(30)
}

impl Oauth {
	pub(crate) const AUTH_ATTEMPTS: u32 = 5;

	pub(crate) fn placeholder() -> Self {
		let backend = OauthBackendImpl::MobileSpoof(MobileSpoofAuth::new());
		let headers_map = backend.get_headers();
		Self {
			headers_map,
			expires_in: 0,
			backend,
			has_token: false,
		}
	}

	pub(crate) fn has_token(&self) -> bool {
		self.has_token
	}

	/// A single token request. `prefer_generic` selects the web fallback backend.
	pub(crate) async fn authenticate_once(prefer_generic: bool) -> Result<Self, String> {
		let backend = if prefer_generic {
			OauthBackendImpl::GenericWeb(GenericWebAuth::new())
		} else {
			OauthBackendImpl::MobileSpoof(MobileSpoofAuth::new())
		};
		match Self::new_with_timeout_with_backend(backend).await {
			Ok(Ok(oauth)) => Ok(oauth),
			Ok(Err(err)) => Err(err.public_message()),
			Err(_) => Err("Reddit token request timed out".to_string()),
		}
	}

	/// Bounded startup attempts. Does not loop forever and does not exit the process.
	pub(crate) async fn authenticate_bounded() -> Result<Self, String> {
		let mut last = String::from("Reddit token request failed");
		for attempt in 1..=Self::AUTH_ATTEMPTS {
			match Self::authenticate_once(attempt > 3).await {
				Ok(oauth) => {
					info!("[✅] Successfully created OAuth client");
					return Ok(oauth);
				}
				Err(err) => {
					last = err;
					error!("[⛔] OAuth attempt {attempt}/{} failed: {last}", Self::AUTH_ATTEMPTS);
					if attempt == Self::AUTH_ATTEMPTS {
						break;
					}
					tokio::time::sleep(retry_delay(attempt)).await;
				}
			}
		}
		let via = crate::client::proxy_for_log().unwrap_or_else(|| "direct".to_string());
		Err(format!("OAuth token unavailable after {} attempts via {via}: {last}", Self::AUTH_ATTEMPTS))
	}

	async fn new_with_timeout_with_backend(mut backend: OauthBackendImpl) -> Result<Result<Self, AuthError>, Elapsed> {
		timeout(OAUTH_TIMEOUT, async move {
			let response = backend.authenticate().await?;

			// Build headers_map from backend headers + Authorization header
			let mut headers_map = backend.get_headers();
			headers_map.insert("Authorization".to_owned(), format!("Bearer {}", response.token));
			headers_map.extend(response.additional_headers);

			Ok(Self {
				headers_map,
				expires_in: response.expires_in,
				backend,
				has_token: true,
			})
		})
		.await
	}

	pub fn user_agent(&self) -> &str {
		self.backend.user_agent()
	}
}

#[derive(Debug)]
enum AuthError {
	Wreq(wreq::Error),
	SerdeDeserialize(serde_json::Error),
	Field(&'static str),
}

impl From<wreq::Error> for AuthError {
	fn from(err: wreq::Error) -> Self {
		AuthError::Wreq(err)
	}
}

impl From<serde_json::Error> for AuthError {
	fn from(err: serde_json::Error) -> Self {
		AuthError::SerdeDeserialize(err)
	}
}

impl AuthError {
	fn public_message(&self) -> String {
		match self {
			AuthError::Wreq(err) => redact_credentials(&err.to_string()),
			AuthError::SerdeDeserialize(err) => format!("Reddit returned a token response that was not JSON ({err})"),
			AuthError::Field(field) => format!("Reddit token response missing {field}"),
		}
	}
}

static BACKGROUND_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Start the bounded initial login and the periodic refresh. Safe to call more than once.
pub fn spawn_background() {
	if BACKGROUND_STARTED.swap(true, Ordering::SeqCst) {
		return;
	}
	let Ok(handle) = tokio::runtime::Handle::try_current() else {
		BACKGROUND_STARTED.store(false, Ordering::SeqCst);
		return;
	};
	handle.spawn(initial_auth());
	handle.spawn(token_daemon());
}

async fn initial_auth() {
	if OAUTH_IS_ROLLING_OVER.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
		return;
	}
	match Oauth::authenticate_bounded().await {
		Ok(client) => {
			OAUTH_CLIENT.swap(client.into());
			OAUTH_RATELIMIT_REMAINING.store(99, Ordering::SeqCst);
		}
		Err(err) => error!("{err}"),
	}
	OAUTH_IS_ROLLING_OVER.store(false, Ordering::SeqCst);
}

pub async fn token_daemon() {
	loop {
		let client = OAUTH_CLIENT.load_full();
		if !client.has_token() {
			// Idle until a request or the startup task stores a token. No network here.
			tokio::time::sleep(Duration::from_secs(30)).await;
			continue;
		}
		let duration = Duration::from_secs(refresh_wait(client.expires_in));
		info!("[⏳] Waiting for {duration:?} before refreshing OAuth token...");
		tokio::time::sleep(duration).await;
		info!("Refreshing OAuth token...");
		force_refresh_token().await;
	}
}

/// One renewal at a time. A request that finds no token either waits for the
/// in-flight attempt or performs a single attempt of its own.
pub async fn prepare_token() -> Result<(), String> {
	spawn_background();
	if OAUTH_CLIENT.load().has_token() {
		return Ok(());
	}
	if OAUTH_IS_ROLLING_OVER.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
		for _ in 0..180 {
			if !OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst) || OAUTH_CLIENT.load().has_token() {
				break;
			}
			tokio::time::sleep(Duration::from_millis(250)).await;
		}
	} else {
		match Oauth::authenticate_once(false).await {
			Ok(client) => {
				OAUTH_CLIENT.swap(client.into());
				OAUTH_RATELIMIT_REMAINING.store(99, Ordering::SeqCst);
			}
			Err(err) => error!("{err}"),
		}
		OAUTH_IS_ROLLING_OVER.store(false, Ordering::SeqCst);
	}
	if OAUTH_CLIENT.load().has_token() {
		Ok(())
	} else {
		Err(unreachable_message())
	}
}

pub fn unreachable_message() -> String {
	unreachable_message_for(crate::client::proxy_for_log())
}

pub(crate) fn unreachable_message_for(proxy: Option<String>) -> String {
	match proxy {
		Some(proxy) => format!("Reddit unreachable / proxy failed ({proxy})"),
		None => "Reddit unreachable / proxy failed".to_string(),
	}
}

pub async fn force_refresh_token() {
	if OAUTH_IS_ROLLING_OVER.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
		trace!("Skipping refresh token roll over, already in progress");
		return;
	}

	trace!("Rolling over refresh token. Current rate limit: {}", OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst));
	match Oauth::authenticate_once(false).await {
		Ok(client) => {
			OAUTH_CLIENT.swap(client.into());
			OAUTH_RATELIMIT_REMAINING.store(99, Ordering::SeqCst);
		}
		Err(err) => error!("OAuth refresh failed: {err}"),
	}
	OAUTH_IS_ROLLING_OVER.store(false, Ordering::SeqCst);
}

pub(crate) fn oauth_success_message(expires_in: u64) -> String {
	format!("[✅] Success - retrieved OAuth token, expires in {expires_in}s")
}

/// Drop userinfo and bearer values before anything is written to a log or an error page.
pub(crate) fn header_names_for_log(headers: &wreq::header::HeaderMap) -> String {
	headers
		.keys()
		.map(|name| {
			let name = name.as_str();
			if name.eq_ignore_ascii_case("authorization")
				|| name.eq_ignore_ascii_case("x-reddit-loid")
				|| name.eq_ignore_ascii_case("x-reddit-session")
				|| name.eq_ignore_ascii_case("cookie")
				|| name.eq_ignore_ascii_case("set-cookie")
			{
				format!("{name}: [REDACTED]")
			} else {
				name.to_string()
			}
		})
		.collect::<Vec<_>>()
		.join(", ")
}

fn trace_response_headers(label: &str, headers: &wreq::header::HeaderMap) {
	trace!("{label}: {}", header_names_for_log(headers));
}

pub fn redact_credentials(input: &str) -> String {
	let mut out = String::with_capacity(input.len());
	let bytes = input.as_bytes();
	let mut index = 0;
	while index < bytes.len() {
		if bytes[index..].starts_with(b"://") {
			out.push_str("://");
			index += 3;
			let rest = &input[index..];
			if let Some(at) = rest.find('@') {
				let userinfo = &rest[..at];
				if !userinfo.is_empty() && !userinfo.contains('/') && !userinfo.chars().any(char::is_whitespace) {
					let host = &rest[at + 1..];
					let end = host.find(|ch: char| ch.is_whitespace() || ch == '/' || ch == '"' || ch == '\'').unwrap_or(host.len());
					out.push_str(&host[..end]);
					index += at + 1 + end;
					continue;
				}
			}
		}
		let lower = input[index..].to_ascii_lowercase();
		if lower.starts_with("bearer ") {
			out.push_str("Bearer [REDACTED]");
			index += "bearer ".len();
			while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
				index += 1;
			}
			continue;
		}
		let ch = input[index..].chars().next().unwrap_or('\u{FFFD}');
		out.push(ch);
		index += ch.len_utf8();
	}
	out
}

#[derive(Debug, Clone, Default)]
struct Device {
	oauth_id: String,
	initial_headers: HashMap<String, String>,
	headers: HashMap<String, String>,
	user_agent: String,
}

// MobileSpoofAuth backend - spoofs an Android mobile device
#[derive(Debug, Clone)]
pub struct MobileSpoofAuth {
	device: Device,
	additional_headers: HashMap<String, String>,
}

impl MobileSpoofAuth {
	fn new() -> Self {
		Self {
			device: Device::new(),
			additional_headers: HashMap::new(),
		}
	}
}

impl OauthBackend for MobileSpoofAuth {
	async fn authenticate(&mut self) -> Result<OauthResponse, AuthError> {
		// Construct URL for OAuth token
		let url = format!("{AUTH_ENDPOINT}/auth/v2/oauth/access-token/loid");
		let mut builder = CLIENT.post(&url);

		// Add headers from spoofed client
		for (key, value) in &self.device.initial_headers {
			builder = builder.header(key, value);
		}
		// Set up HTTP Basic Auth - basically just the const OAuth ID's with no password,
		// Base64-encoded. https://en.wikipedia.org/wiki/Basic_access_authentication
		// This could be constant, but I don't think it's worth it. OAuth ID's can change
		// over time and we want to be flexible.
		let auth = general_purpose::STANDARD.encode(format!("{}:", self.device.oauth_id));
		builder = builder.header("Authorization", format!("Basic {auth}"));

		// Set JSON body. I couldn't tell you what this means. But that's what the client sends
		let json = json!({
				"scopes": ["*","email", "pii"]
		});

		trace!("Sending token request to {url}...");

		// Send request
		let resp = builder.json(&json).send().await?;

		trace!("Received response with status {} and length {:?}", resp.status(), resp.headers().get("content-length"));
		trace_response_headers("OAuth response headers", resp.headers());

		// Parse headers - loid header _should_ be saved sent on subsequent token refreshes.
		// Technically it's not needed, but it's easy for Reddit API to check for this.
		// It's some kind of header that uniquely identifies the device.
		// Not worried about the privacy implications, since this is randomly changed
		// and really only as privacy-concerning as the OAuth token itself.
		if let Some(header) = resp.headers().get("x-reddit-loid") {
			let header_val: &wreq::header::HeaderValue = header;
			self.additional_headers.insert("x-reddit-loid".to_owned(), header_val.to_str().unwrap().to_string());
		}

		// Same with x-reddit-session
		if let Some(header) = resp.headers().get("x-reddit-session") {
			let header_val: &wreq::header::HeaderValue = header;
			self.additional_headers.insert("x-reddit-session".to_owned(), header_val.to_str().unwrap().to_string());
		}

		trace!("Serializing response...");

		// Serialize response
		let json: serde_json::Value = resp.json().await?;

		trace!("Accessing relevant fields...");

		// Save token and expiry
		let token = json
			.get("access_token")
			.ok_or_else(|| AuthError::Field("access_token"))?
			.as_str()
			.ok_or_else(|| AuthError::Field("access_token: as_str"))?
			.to_string();
		let expires_in = json
			.get("expires_in")
			.ok_or_else(|| AuthError::Field("expires_in"))?
			.as_u64()
			.ok_or_else(|| AuthError::Field("expires_in: as_u64"))?;

		info!("{}", oauth_success_message(expires_in));

		Ok(OauthResponse {
			token,
			expires_in,
			additional_headers: self.additional_headers.clone(),
		})
	}

	fn user_agent(&self) -> &str {
		&self.device.user_agent
	}

	fn get_headers(&self) -> HashMap<String, String> {
		let mut headers = self.device.headers.clone();
		headers.extend(self.additional_headers.clone());
		headers
	}
}

// GenericWebAuth backend - simple web-based authentication
#[derive(Debug, Clone)]
pub struct GenericWebAuth {
	device_id: String,
	user_agent: String,
	additional_headers: HashMap<String, String>,
}

impl GenericWebAuth {
	fn new() -> Self {
		// Generate random 20-character alphanumeric device_id
		let device_id: String = (0..20)
			.map(|_| {
				let idx = fastrand::usize(..62);
				let chars = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
				chars[idx] as char
			})
			.collect();

		info!("[🔄] Using GenericWebAuth with device_id: \"{device_id}\"");

		Self {
			device_id,
			user_agent: fake_user_agent::get_rua().to_owned(),
			additional_headers: HashMap::new(),
		}
	}
}

impl OauthBackend for GenericWebAuth {
	async fn authenticate(&mut self) -> Result<OauthResponse, AuthError> {
		// Construct URL for OAuth token
		let url = "https://www.reddit.com/api/v1/access_token";
		let mut builder = CLIENT.post(url);

		// Add minimal headers
		builder = builder.header("Host", "www.reddit.com");
		builder = builder.header("User-Agent", &self.user_agent);
		builder = builder.header("Accept", "*/*");
		builder = builder.header("Accept-Language", "en-US,en;q=0.5");
		// builder = builder.header("Accept-Encoding", "gzip, deflate, br, zstd");
		builder = builder.header("Authorization", "Basic M1hmQkpXbGlIdnFBQ25YcmZJWWxMdzo=");
		builder = builder.header("Content-Type", "application/x-www-form-urlencoded");
		builder = builder.header("Sec-GPC", "1");
		builder = builder.header("Connection", "keep-alive");

		// Set up form body
		let body_str = format!("grant_type=https%3A%2F%2Foauth.reddit.com%2Fgrants%2Finstalled_client&device_id={}", self.device_id);

		trace!("Sending GenericWebAuth token request to {url}...");

		// Send request
		let resp: wreq::Response = builder.body(body_str).send().await?;

		trace!("Received response with status {} and length {:?}", resp.status(), resp.headers().get("content-length"));
		trace_response_headers("GenericWebAuth response headers", resp.headers());

		// Parse headers - loid header _should_ be saved sent on subsequent token refreshes.
		// Technically it's not needed, but it's easy for Reddit API to check for this.
		// It's some kind of header that uniquely identifies the device.
		// Not worried about the privacy implications, since this is randomly changed
		// and really only as privacy-concerning as the OAuth token itself.
		if let Some(header) = resp.headers().get("x-reddit-loid") {
			let header_val: &wreq::header::HeaderValue = header;
			self.additional_headers.insert("x-reddit-loid".to_owned(), header_val.to_str().unwrap().to_string());
		}

		// Same with x-reddit-session
		if let Some(header) = resp.headers().get("x-reddit-session") {
			let header_val: &wreq::header::HeaderValue = header;
			self.additional_headers.insert("x-reddit-session".to_owned(), header_val.to_str().unwrap().to_string());
		}

		trace!("Serializing GenericWebAuth response...");

		// Serialize response
		let json: serde_json::Value = resp.json().await?;

		trace!("Accessing relevant fields...");

		// Parse response - access_token, token_type, device_id, expires_in, scope
		let token = json
			.get("access_token")
			.ok_or_else(|| AuthError::Field("access_token"))?
			.as_str()
			.ok_or_else(|| AuthError::Field("access_token: as_str"))?
			.to_string();
		let expires_in = json
			.get("expires_in")
			.ok_or_else(|| AuthError::Field("expires_in"))?
			.as_u64()
			.ok_or_else(|| AuthError::Field("expires_in: as_u64"))?;

		info!("{}", oauth_success_message(expires_in));

		// Insert a few necessary headers
		self.additional_headers.insert("Origin".to_owned(), "https://www.reddit.com".to_owned());
		self.additional_headers.insert("User-Agent".to_owned(), self.user_agent.to_owned());

		Ok(OauthResponse {
			token,
			expires_in,
			additional_headers: self.additional_headers.clone(),
		})
	}

	fn user_agent(&self) -> &str {
		&self.user_agent
	}

	fn get_headers(&self) -> HashMap<String, String> {
		self.additional_headers.clone()
	}
}

impl Device {
	fn android() -> Self {
		// Generate uuid
		let uuid = uuid::Uuid::new_v4().to_string();

		// Generate random user-agent
		let android_app_version = choose(ANDROID_APP_VERSION_LIST).to_string();
		let android_version = fastrand::u8(9..=14);

		let android_user_agent = format!("Reddit/{android_app_version}/Android {android_version}");

		let qos = fastrand::u32(1000..=100_000);
		let qos: f32 = qos as f32 / 1000.0;
		let qos = format!("{qos:.3}");

		let codecs = TextGenerator::new().generate("available-codecs=video/avc, video/hevc{, video/x-vnd.on2.vp9|}");

		// Android device headers
		let headers: HashMap<String, String> = HashMap::from([
			("User-Agent".into(), android_user_agent.clone()),
			("x-reddit-retry".into(), "algo=no-retries".into()),
			("x-reddit-compression".into(), "1".into()),
			("x-reddit-qos".into(), qos),
			("x-reddit-media-codecs".into(), codecs),
			("Content-Type".into(), "application/json; charset=UTF-8".into()),
			("client-vendor-id".into(), uuid.clone()),
			("X-Reddit-Device-Id".into(), uuid.clone()),
		]);

		info!("[🔄] Spoofing Android client with headers: {headers:?}, uuid: \"{uuid}\", and OAuth ID \"{REDDIT_ANDROID_OAUTH_CLIENT_ID}\"");

		Self {
			oauth_id: REDDIT_ANDROID_OAUTH_CLIENT_ID.to_string(),
			headers: headers.clone(),
			initial_headers: headers,
			user_agent: android_user_agent,
		}
	}
	fn new() -> Self {
		// See https://github.com/redlib-org/redlib/issues/8
		Self::android()
	}
}

fn choose<T: Copy>(list: &[T]) -> T {
	*fastrand::choose_multiple(list.iter(), 1)[0]
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test(flavor = "multi_thread")]
	async fn test_mobile_spoof_backend() {
		// Test MobileSpoofAuth backend specifically
		let mut backend = MobileSpoofAuth::new();
		let response = backend.authenticate().await;
		assert!(response.is_ok());
		let response = response.unwrap();
		assert!(!response.token.is_empty());
		assert!(response.expires_in > 0);
		assert!(!backend.user_agent().is_empty());
		assert!(!backend.get_headers().is_empty());
	}

	#[tokio::test(flavor = "multi_thread")]
	#[ignore = "todolol"]
	async fn test_generic_web_backend() {
		// Test GenericWebAuth backend specifically
		let mut backend = GenericWebAuth::new();
		let response = backend.authenticate().await;
		assert!(response.is_ok());
		let response = response.unwrap();
		assert!(!response.token.is_empty());
		assert!(response.expires_in > 0);
		assert!(!backend.user_agent().is_empty());
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_oauth_client() {
		prepare_token().await.expect("OAuth token");
		assert!(OAUTH_CLIENT.load_full().headers_map.contains_key("Authorization"));
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_oauth_client_refresh() {
		force_refresh_token().await;
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_oauth_token_exists() {
		prepare_token().await.expect("OAuth token");
		let client = OAUTH_CLIENT.load_full();
		let auth_header = client.headers_map.get("Authorization").unwrap();
		assert!(auth_header.starts_with("Bearer "));
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_oauth_headers_len() {
		assert!(OAUTH_CLIENT.load_full().headers_map.len() >= 3);
	}

	#[test]
	fn test_creating_device() {
		Device::new();
	}

	#[test]
	fn test_creating_backends() {
		// Test that both backends can be created
		MobileSpoofAuth::new();
		GenericWebAuth::new();
	}

	#[test]
	fn test_retry_is_bounded_and_capped() {
		assert_eq!(Oauth::AUTH_ATTEMPTS, 5);
		assert_eq!(retry_delay(1).as_secs(), 1);
		assert_eq!(retry_delay(2).as_secs(), 2);
		assert_eq!(retry_delay(3).as_secs(), 4);
		assert_eq!(retry_delay(4).as_secs(), 8);
		assert_eq!(retry_delay(9).as_secs(), 8);
		assert_eq!(refresh_wait(0), 30);
		assert_eq!(refresh_wait(10), 30);
		assert_eq!(refresh_wait(200), 80);
	}

	#[test]
	fn test_logs_do_not_include_tokens_or_proxy_passwords() {
		let message = oauth_success_message(86399);
		assert!(message.contains("86399"));
		assert!(!message.contains("eyJ"));
		assert!(!message.contains("token \""));
		let mut headers = wreq::header::HeaderMap::new();
		headers.insert("x-reddit-loid", "secret-loid".parse().unwrap());
		headers.insert("x-reddit-session", "secret-session".parse().unwrap());
		headers.insert(wreq::header::CONTENT_LENGTH, "12".parse().unwrap());
		let names = header_names_for_log(&headers);
		assert!(names.contains("x-reddit-loid: [REDACTED]"));
		assert!(names.contains("x-reddit-session: [REDACTED]"));
		assert!(names.contains("content-length"));
		assert!(!names.contains("secret-loid"));
		assert!(!names.contains("secret-session"));
		let redacted = redact_credentials("connect socks5h://user:s3cret-pass@127.0.0.1:1080 failed, bearer eyJabc.def");
		assert!(redacted.contains("socks5h://127.0.0.1:1080"));
		assert!(!redacted.contains("s3cret-pass"));
		assert!(!redacted.contains("user:"));
		assert!(!redacted.contains("eyJabc"));
		assert_eq!(
			unreachable_message_for(Some("socks5h://proxy.example:1080".into())),
			"Reddit unreachable / proxy failed (socks5h://proxy.example:1080)"
		);
	}
}
