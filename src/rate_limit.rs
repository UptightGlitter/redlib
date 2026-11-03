//! Turn a Reddit 429 into a message and an HTTP status. No global limiter:
//! each response is described on its own, and the next request is always sent.

const NUMBERED_PREFIX: &str = "Reddit is rate limiting this instance, try again in ";
const GENERIC: &str = "Reddit is rate limiting this instance. Try again in a moment.";

/// Seconds from `retry-after` or `x-ratelimit-reset`, if either is a number.
/// `retry-after` wins. A date or an empty value is ignored.
pub(crate) fn retry_seconds_from_headers(retry_after: Option<&str>, reset: Option<&str>) -> Option<u64> {
	parse_seconds(retry_after).or_else(|| parse_seconds(reset)).map(|seconds| seconds.max(1))
}

fn parse_seconds(value: Option<&str>) -> Option<u64> {
	let value = value?.trim();
	if value.is_empty() {
		return None;
	}
	let seconds = value.parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0)?;
	Some(seconds.ceil() as u64)
}

/// Visitor-facing text, and the `Retry-After` value when Reddit named a wait.
pub(crate) fn rate_limit_error(retry_after: Option<&str>, reset: Option<&str>) -> (String, Option<u64>) {
	match retry_seconds_from_headers(retry_after, reset) {
		Some(seconds) => (format!("{NUMBERED_PREFIX}{seconds} seconds"), Some(seconds)),
		None => (GENERIC.to_string(), None),
	}
}

pub(crate) fn is_rate_limit(msg: &str) -> bool {
	msg.starts_with("Reddit is rate limiting this instance")
}

pub(crate) fn retry_seconds(msg: &str) -> Option<u64> {
	let rest = msg.strip_prefix(NUMBERED_PREFIX)?;
	rest.split_whitespace().next()?.parse().ok()
}

/// Status and `Retry-After` for an error string. Rate-limit text is 429; everything else is 404.
pub(crate) fn http_status(msg: &str) -> (u16, Option<u64>) {
	if is_rate_limit(msg) {
		(429, retry_seconds(msg))
	} else {
		(404, None)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_rate_limit_error_uses_headers_when_present() {
		let (msg, retry_after) = rate_limit_error(Some("9"), Some("40"));
		assert_eq!(msg, "Reddit is rate limiting this instance, try again in 9 seconds");
		assert_eq!(retry_after, Some(9));
		assert_eq!(http_status(&msg), (429, Some(9)));

		let (from_reset, retry_after) = rate_limit_error(None, Some("8.2"));
		assert_eq!(from_reset, "Reddit is rate limiting this instance, try again in 9 seconds");
		assert_eq!(retry_after, Some(9));
	}

	#[test]
	fn test_rate_limit_error_without_headers_has_no_number() {
		let (msg, retry_after) = rate_limit_error(None, Some("Wed, 21 Oct 2015 07:28:00 GMT"));
		assert_eq!(msg, GENERIC);
		assert_eq!(retry_after, None);
		assert!(is_rate_limit(&msg));
		assert_eq!(http_status(&msg), (429, None));
		assert_eq!(http_status("Post not found"), (404, None));
	}
}
