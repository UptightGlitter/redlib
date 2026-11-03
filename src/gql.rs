// Comment threads via Reddit's persisted `PostComments` query on gql-fed.reddit.com.
// Adapted from polak14/redlib@29f18352 and the comments portion of redlib-org/redlib#603.
// Anonymous OAuth clients receive a block page from the `/comments/` JSON route.

use crate::client::{json_uncached, non_json_response_message, CLIENT, OAUTH_CLIENT};
use crate::utils::{format_url, param};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::DateTime;
use log::debug;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::{json as json_value, Value};
use std::time::Duration;

const GQL_URL: &str = "https://gql-fed.reddit.com/";
const POST_COMMENTS_HASH: &str = "314f29c2494fd78c7844ef2c6b09acb4bef5b37ab50b4ddc01691d6d5c8433f8";
const COMMENT_COUNT: u32 = 200;
const MAX_DEPTH: u32 = 10;
const TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const MAX_CURSOR_LEN: usize = 8192;

/// Build the two listings `post.rs` already parses: the post, then the comments.
pub async fn thread(path: &str, quarantine: bool) -> Result<Value, String> {
	let (id, comment_id) = parse_path(path).ok_or_else(|| format!("Couldn't parse post path | {path}"))?;
	let requested = param(path, "sort").filter(|sort| !sort.is_empty());
	if let Some(sort) = requested.as_deref().filter(|sort| sort_type(sort).is_none()) {
		return Err(format!("Unsupported comment sort: {sort} | {path}"));
	}

	// `/by_id` is already served by `gql_feed::route` (PostsByIds). A missing
	// post comes back as an empty listing, which used to render a blank page.
	let post = json_uncached(format!("/by_id/t3_{id}.json?raw_json=1"), quarantine).await?;
	if !has_post(&post) {
		return Err("Post not found".to_string());
	}

	let sort = match requested {
		Some(sort) => sort,
		None => post["data"]["children"][0]["data"]["suggested_sort"]
			.as_str()
			.filter(|sort| sort_type(sort).is_some())
			.unwrap_or("confidence")
			.to_string(),
	};
	let comments = comments(&id, comment_id.as_deref(), path, &sort).await?;
	let mut listing = listing(comments);
	listing["data"]["sort"] = sort.into();
	Ok(json_value!([post, listing]))
}

/// A listing actually contains a post. An unknown id comes back with no children.
fn has_post(post: &Value) -> bool {
	post["data"]["children"].as_array().is_some_and(|children| {
		children.iter().any(|child| {
			let data = &child["data"];
			data["id"].as_str().is_some_and(is_id) || data["name"].as_str().is_some_and(|name| name.starts_with("t3_") && is_id(name.trim_start_matches("t3_")))
		})
	})
}

async fn comments(id: &str, comment_id: Option<&str>, path: &str, sort: &str) -> Result<Vec<Value>, String> {
	let variables = variables(id, comment_id, path, sort)?;
	let response = query("PostComments", POST_COMMENTS_HASH, &variables, path).await?;
	let trees = response["data"]["postInfoById"]["commentForest"]["trees"]
		.as_array()
		.cloned()
		.ok_or_else(|| graphql_failure("PostComments", wreq::StatusCode::OK, &response))?;
	let comments = nest(trees.iter().filter_map(|tree| item(tree, id)).collect());
	debug!("PostComments id=t3_{id} converted {} top-level items", comments.len());
	let context = context(path);
	match comment_id {
		Some(comment_id) if context > 0 && variables["after"].is_null() => {
			let (mut depth, mut branch) = focused_branch(&comments, comment_id).ok_or_else(|| format!("Comment not found | {path}"))?;
			while depth > context {
				branch = branch["data"]["replies"]["data"]["children"][0].take();
				depth -= 1;
			}
			Ok(vec![branch])
		}
		_ => Ok(comments),
	}
}

fn sort_type(sort: &str) -> Option<&'static str> {
	Some(match sort {
		"confidence" | "best" => "CONFIDENCE",
		"top" => "TOP",
		"new" => "NEW",
		"controversial" => "CONTROVERSIAL",
		"old" => "OLD",
		"qa" => "QA",
		_ => return None,
	})
}

fn context(path: &str) -> usize {
	param(path, "context").and_then(|value| value.parse().ok()).unwrap_or(0)
}

fn variables(id: &str, comment_id: Option<&str>, path: &str, sort: &str) -> Result<Value, String> {
	let mut variables = json_value!({
		"id": format!("t3_{id}"),
		"sortType": sort_type(sort).ok_or_else(|| format!("Unsupported comment sort: {sort}"))?,
		"count": COMMENT_COUNT,
		"maxDepth": MAX_DEPTH,
		"includeConsistencyToken": false,
		"includePremiumAvatarTreatment": false,
	});
	let cursor = param(path, "cursor");
	if let Some(cursor) = &cursor {
		let cursor = Some(cursor.as_str())
			.filter(|cursor| cursor.len() <= MAX_CURSOR_LEN)
			.and_then(|cursor| STANDARD.decode(cursor).ok())
			.and_then(|cursor| String::from_utf8(cursor).ok())
			.ok_or_else(|| format!("Invalid comments cursor | {path}"))?;
		variables["after"] = cursor.into();
	}
	if let Some(comment_id) = comment_id.filter(|_| cursor.is_none()) {
		variables["focusedCommentContext"] = json_value!({
			"commentId": format!("t1_{comment_id}"),
			"isFocusedThreadOnly": context(path) == 0,
		});
	}
	Ok(variables)
}

/// One persisted query. A block page uses the same non-JSON error as `client::json` and does not refresh the token.
pub(crate) async fn query(operation: &str, hash: &str, variables: &Value, path: &str) -> Result<Value, String> {
	let body = json_value!({
		"operationName": operation,
		"variables": variables,
		"extensions": { "persistedQuery": { "version": 1, "sha256Hash": hash } },
	});

	let mut response = Value::Null;
	for attempt in 0..2 {
		let mut builder = CLIENT.post(GQL_URL).timeout(TIMEOUT);
		for (key, value) in &OAUTH_CLIENT.load_full().headers_map {
			if !key.eq_ignore_ascii_case("content-type") {
				builder = builder.header(key, value);
			}
		}
		let res = builder.json(&body).send().await.map_err(|err| format!("{operation} request failed: {err} | {path}"))?;
		let status = res.status();
		let content_type = res
			.headers()
			.get(wreq::header::CONTENT_TYPE)
			.and_then(|value| value.to_str().ok())
			.unwrap_or("")
			.to_string();
		let post_id = variables.get("id").and_then(Value::as_str).unwrap_or("-");
		debug!("GraphQL {operation} id={post_id} HTTP {status}");
		let text = res.text().await.map_err(|err| format!("{operation} returned {status}, then failed: {err} | {path}"))?;
		response = match serde_json::from_str(&text) {
			Ok(response) => response,
			Err(_) => {
				debug!("GraphQL {operation} HTTP {status} was not JSON");
				return Err(non_json_response_message(status, &content_type, text.as_bytes(), path));
			}
		};
		if response["data"].is_null() {
			let failure = graphql_failure(operation, status, &response);
			debug!("GraphQL {operation} failed: {failure}");
			return Err(format!("{failure} | {path}"));
		}
		if response["errors"].is_null() || attempt == 1 {
			break;
		}
		debug!("GraphQL {operation} returned partial errors; retrying once");
	}
	Ok(response)
}

fn graphql_failure(operation: &str, status: wreq::StatusCode, response: &Value) -> String {
	let message = response["errors"]
		.as_array()
		.and_then(|errors| errors.first())
		.and_then(|error| error["message"].as_str())
		.unwrap_or("unknown GraphQL error");
	let lower = message.to_ascii_lowercase();
	if lower.contains("not_authenticated") || lower.contains("not authenticated") {
		return format!("Reddit GraphQL rejected {operation} (HTTP {status}): NOT_AUTHENTICATED");
	}
	if lower.contains("persistedquery") || lower.contains("persisted query") || lower.contains("query hash") {
		return format!("Reddit GraphQL rejected {operation} (HTTP {status}): persisted query hash is unknown or changed ({message})");
	}
	format!("Reddit GraphQL error in {operation} (HTTP {status}): {message}")
}

pub(crate) fn item(tree: &Value, post_id: &str) -> Option<(u64, Value)> {
	let depth = tree["depth"].as_u64()?;
	let post_fullname = format!("t3_{post_id}");
	let parent = tree["parentId"].as_str().filter(|parent| parent.starts_with("t1_")).unwrap_or(&post_fullname).to_string();

	if !tree["more"].is_null() {
		let more = &tree["more"];
		let count = more["count"].as_i64().unwrap_or_default();
		return Some((
			depth,
			if tree["parentId"].is_null() {
				let cursor = STANDARD.encode(more["cursor"].as_str()?);
				json_value!({ "kind": "more", "data": { "id": utf8_percent_encode(&cursor, NON_ALPHANUMERIC).to_string(), "count": count, "parent_id": post_fullname } })
			} else {
				json_value!({ "kind": "more", "data": { "id": "", "count": count, "parent_id": parent } })
			},
		));
	}

	let node = &tree["node"];
	let fullname = node["id"].as_str().unwrap_or_default();
	if node["__typename"] != "DeletedComment" && !fullname.strip_prefix("t1_").is_some_and(is_id) {
		return None;
	}
	let author = node["authorInfo"]["name"].as_str().unwrap_or("[unknown]");
	let content = &node["content"];
	let (author, body, body_html) = if node["__typename"] == "DeletedComment" {
		("[deleted]", "[deleted]", "<p>[deleted]</p>".to_string())
	} else if node["isAdminTakedown"] == true {
		(author, "[ Removed by Reddit ]", String::new())
	} else if node["isRemoved"] == true {
		("[deleted]", "[removed]", String::new())
	} else {
		(author, content["markdown"].as_str().unwrap_or_default(), content_html(content))
	};
	let flair = &node["authorFlair"];
	let (flair_type, flair_richtext) = flair_parts(flair);
	Some((
		depth,
		json_value!({
			"kind": "t1",
			"data": {
				"id": fullname.strip_prefix("t1_").unwrap_or(fullname),
				"name": fullname,
				"author": author,
				"body": body,
				"body_html": format!("<div class=\"md\">{body_html}</div>"),
				"created_utc": timestamp(&node["createdAt"]).unwrap_or_default(),
				"edited": timestamp(&node["editedAt"]).map_or(Value::Bool(false), Value::from),
				"score": node["score"].as_i64().unwrap_or_default(),
				"score_hidden": node["isScoreHidden"] == true,
				"stickied": node["isStickied"] == true,
				"distinguished": node["distinguishedAs"].as_str().map(str::to_lowercase),
				"author_flair_type": flair_type,
				"author_flair_richtext": flair_richtext,
				"author_flair_text": flair["text"],
				"author_flair_background_color": flair["template"]["backgroundColor"],
				"author_flair_text_color": flair["textColor"].as_str().map(str::to_lowercase),
				"all_awardings": awards(&node["awardings"]),
				"parent_id": parent,
				"replies": "",
			}
		}),
	))
}

pub(crate) fn flair_parts(flair: &Value) -> (&'static str, Value) {
	let richtext: Value = flair["richtext"].as_str().and_then(|richtext| serde_json::from_str(richtext).ok()).unwrap_or_default();
	let kind = if richtext.as_array().is_some_and(|richtext| !richtext.is_empty()) {
		"richtext"
	} else if flair["text"].as_str().is_some_and(|text| !text.is_empty()) {
		"text"
	} else {
		""
	};
	(kind, richtext)
}

pub(crate) fn awards(awardings: &Value) -> Vec<Value> {
	awardings
		.as_array()
		.into_iter()
		.flatten()
		.map(|award| {
			let icon = award["award"]["static_icon_16"]["url"]
				.as_str()
				.or(award["award"]["icon_16"]["url"].as_str())
				.map(format_url);
			json_value!({ "name": award["award"]["name"], "count": award["total"], "resized_icons": [{ "url": icon }] })
		})
		.collect()
}

/// Keep off-site links. Reddit media hosts become the local proxy path.
pub(crate) fn stored_url(url: &Value) -> Value {
	let Some(raw) = url.as_str() else {
		return url.clone();
	};
	let Ok(parsed) = url::Url::parse(raw) else {
		return url.clone();
	};
	let domain = parsed.domain().unwrap_or_default();
	let media = domain.ends_with("redd.it") || domain.ends_with("redditmedia.com") || domain.ends_with("redditstatic.com");
	if !media {
		return url.clone();
	}
	let formatted = format_url(raw);
	if formatted.starts_with('/') {
		Value::String(formatted)
	} else {
		url.clone()
	}
}

/// Reddit's Android API sends `2023-12-28T21:35:27.507000+0000` (no colon in the
/// offset). `parse_from_rfc3339` only accepts `+00:00`, which used to become
/// the Unix epoch and show every date as Jan 01 '70.
pub(crate) fn timestamp(value: &Value) -> Option<f64> {
	let text = value.as_str()?;
	let parsed = DateTime::parse_from_rfc3339(text)
		.or_else(|_| DateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f%z"))
		.or_else(|_| DateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%z"));
	parsed.ok().map(|time| time.timestamp() as f64)
}

pub(crate) fn nest(items: Vec<(u64, Value)>) -> Vec<Value> {
	let mut top = Vec::new();
	let mut open: Vec<(u64, Value)> = Vec::new();
	for (depth, item) in items {
		while open.last().is_some_and(|(open_depth, _)| *open_depth >= depth) {
			close(&mut open, &mut top);
		}
		open.push((depth, item));
	}
	while !open.is_empty() {
		close(&mut open, &mut top);
	}
	top
}

fn close(open: &mut Vec<(u64, Value)>, top: &mut Vec<Value>) {
	let Some((_, item)) = open.pop() else { return };
	let Some((_, parent)) = open.last_mut() else {
		top.push(item);
		return;
	};
	let replies = &mut parent["data"]["replies"];
	if !replies.is_object() {
		*replies = listing(Vec::new());
	}
	if let Some(children) = replies["data"]["children"].as_array_mut() {
		children.push(item);
	}
}

pub(crate) fn parse_path(path: &str) -> Option<(String, Option<String>)> {
	let path = path.split('?').next()?.strip_suffix(".json")?;
	let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
	let rest = match segments.as_slice() {
		["comments", rest @ ..] => rest,
		["r" | "user" | "u", owner, "comments", rest @ ..] if !owner.is_empty() => rest,
		_ => return None,
	};
	let (id, comment_id) = match rest {
		[id] | [id, _] => (*id, None),
		[id, _, comment_id] => (*id, Some(*comment_id).filter(|id| !id.is_empty())),
		_ => return None,
	};
	if !is_id(id) || comment_id.is_some_and(|id| !is_id(id)) {
		return None;
	}
	Some((id.to_string(), comment_id.map(str::to_string)))
}

pub(crate) fn is_id(id: &str) -> bool {
	!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn listing(children: Vec<Value>) -> Value {
	json_value!({ "kind": "Listing", "data": { "children": children } })
}

fn url_attr(value: Option<&str>) -> String {
	let value = value.unwrap_or_default().trim();
	if value.chars().any(char::is_control) {
		return String::new();
	}
	let local = (value.starts_with('/') && !value.starts_with("//") && !value.contains('\\')) || value.starts_with('#');
	let web = url::Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "http" | "https" | "mailto"));
	if !local && !web {
		return String::new();
	}
	htmlescape::encode_minimal(&format_url(value))
}

pub(crate) fn content_html(content: &Value) -> String {
	let richtext: Value = if content["richtext"].is_object() {
		content["richtext"].clone()
	} else {
		content["richtext"].as_str().and_then(|richtext| serde_json::from_str(richtext).ok()).unwrap_or_default()
	};
	if richtext["document"].is_array() {
		return render(&richtext["document"], &content["richtextMedia"]);
	}
	match content["markdown"].as_str().filter(|markdown| !markdown.is_empty()) {
		Some(markdown) => format!("<p>{}</p>", htmlescape::encode_minimal(markdown)),
		None => String::new(),
	}
}

pub(crate) fn render(nodes: &Value, media: &Value) -> String {
	nodes.as_array().into_iter().flatten().map(|node| element(node, media)).collect()
}

fn element(node: &Value, media: &Value) -> String {
	let children = || render(&node["c"], media);
	match node["e"].as_str().unwrap_or_default() {
		"par" => format!("<p>{}</p>", children()),
		"text" => formatted(node),
		"br" => "<br>".to_string(),
		"hr" => "<hr>".to_string(),
		"h" => {
			let level = node["l"].as_u64().unwrap_or(1).clamp(1, 6);
			format!("<h{level}>{}</h{level}>", children())
		}
		"blockquote" => format!("<blockquote>{}</blockquote>", children()),
		"list" => {
			let tag = if node["o"] == true { "ol" } else { "ul" };
			format!("<{tag}>{}</{tag}>", children())
		}
		"li" => format!("<li>{}</li>", children()),
		"code" => {
			let lines: Vec<&str> = node["c"].as_array().into_iter().flatten().map(|line| line["t"].as_str().unwrap_or_default()).collect();
			format!("<pre><code>{}</code></pre>", htmlescape::encode_minimal(&lines.join("\n")))
		}
		"spoilertext" => format!("<span class=\"md-spoiler-text\">{}</span>", children()),
		"link" => {
			let title = node["a"]
				.as_str()
				.map(|title| format!(" title=\"{}\"", htmlescape::encode_minimal(title)))
				.unwrap_or_default();
			format!("<a href=\"{}\"{title}>{}</a>", url_attr(node["u"].as_str()), formatted(node))
		}
		kind @ ("r/" | "u/") => {
			let name = htmlescape::encode_minimal(node["t"].as_str().unwrap_or_default());
			let slash = if node["l"] == true { "/" } else { "" };
			format!("<a href=\"/{kind}{name}\">{slash}{kind}{name}</a>")
		}
		"img" | "gif" => {
			let Some(item) = media.as_array().and_then(|items| items.iter().find(|item| item["id"] == node["id"])) else {
				return String::new();
			};
			let src = url_attr(item["url"].as_str());
			format!("<figure><a href=\"{src}\"><img src=\"{src}\" loading=\"lazy\"></a></figure>")
		}
		"table" => table(node, media),
		_ if node["c"].is_array() => children(),
		_ => htmlescape::encode_minimal(node["t"].as_str().unwrap_or_default()),
	}
}

const FORMATS: [(u64, &str); 6] = [(1, "strong"), (2, "em"), (8, "del"), (16, "sub"), (32, "sup"), (64, "code")];

fn formatted(node: &Value) -> String {
	let text: Vec<char> = node["t"].as_str().unwrap_or_default().chars().collect();
	let mut bits = vec![0u64; text.len()];
	for range in node["f"].as_array().into_iter().flatten() {
		let (Some(format), Some(start), Some(len)) = (range[0].as_u64(), range[1].as_u64(), range[2].as_u64()) else {
			continue;
		};
		for bits in bits.iter_mut().skip(start as usize).take(len as usize) {
			*bits |= format;
		}
	}
	let mut out = String::new();
	let mut start = 0;
	while start < text.len() {
		let end = (start..text.len()).find(|&index| bits[index] != bits[start]).unwrap_or(text.len());
		let tags: Vec<&str> = FORMATS.iter().filter(|(bit, _)| bits[start] & bit != 0).map(|(_, tag)| *tag).collect();
		for tag in &tags {
			out += &format!("<{tag}>");
		}
		out += &htmlescape::encode_minimal(&text[start..end].iter().collect::<String>());
		for tag in tags.iter().rev() {
			out += &format!("</{tag}>");
		}
		start = end;
	}
	out
}

fn table(node: &Value, media: &Value) -> String {
	let align = |header: Option<&Value>| match header.and_then(|header| header["a"].as_str()) {
		Some("L") => " align=\"left\"",
		Some("C") => " align=\"center\"",
		Some("R") => " align=\"right\"",
		_ => "",
	};
	let headers = node["h"].as_array().map(Vec::as_slice).unwrap_or_default();
	let mut out = "<table><thead><tr>".to_string();
	for header in headers {
		out += &format!("<th{}>{}</th>", align(Some(header)), render(&header["c"], media));
	}
	out += "</tr></thead><tbody>";
	for row in node["c"].as_array().into_iter().flatten() {
		out += "<tr>";
		for (index, cell) in row.as_array().into_iter().flatten().enumerate() {
			out += &format!("<td{}>{}</td>", align(headers.get(index)), render(&cell["c"], media));
		}
		out += "</tr>";
	}
	out + "</tbody></table>"
}

fn focused_branch(comments: &[Value], id: &str) -> Option<(usize, Value)> {
	for comment in comments {
		if comment["data"]["id"] == id {
			return Some((0, comment.clone()));
		}
		if let Some(replies) = comment["data"]["replies"]["data"]["children"].as_array() {
			if let Some((depth, branch)) = focused_branch(replies, id) {
				let mut parent = comment.clone();
				parent["data"]["replies"] = listing(vec![branch]);
				return Some((depth + 1, parent));
			}
		}
	}
	None
}

#[cfg(test)]
fn proxied_source(media: &Value) -> Value {
	json_value!({
		"url": format_url(media["url"].as_str().unwrap_or_default()),
		"width": media["dimensions"]["width"],
		"height": media["dimensions"]["height"],
	})
}

/// `t3` `secure_media`. Prefer a v.redd.it DASH file (`/vid/{id}/{size}.mp4`).
/// `packaged-media.redd.it` is only used when that id is missing, and then only
/// through `/pkg/`, which keeps the `e` and `s` signature query.
pub(crate) fn secure_media(node: &Value) -> Option<Value> {
	let media = &node["media"];
	let streaming = &media["streaming"];
	let packaged = media["packagedMedia"]["muxedMp4s"]["recommended"]["url"].as_str().or(media["download"]["url"].as_str());
	let fallback = vreddit_dash_mp4(node).or_else(|| packaged.map(str::to_string))?;
	let fallback = format_url(&fallback);
	if fallback.is_empty() || fallback.starts_with("http://") || fallback.starts_with("https://") {
		return None;
	}
	let hls = streaming["hlsUrl"]
		.as_str()
		.filter(|url| url.contains(".m3u8"))
		.map(format_url)
		.filter(|url| !(url.starts_with("http://") || url.starts_with("https://")));
	Some(json_value!({
		"reddit_video": {
			"fallback_url": fallback,
			"hls_url": hls,
			"is_gif": streaming["isGif"] == true,
		}
	}))
}

/// Old JSON shape: `https://v.redd.it/{id}/DASH_{height}.mp4?source=fallback`.
fn vreddit_dash_mp4(node: &Value) -> Option<String> {
	let streaming = &node["media"]["streaming"];
	let id = vreddit_id(streaming["dashUrl"].as_str()).or_else(|| vreddit_id(node["url"].as_str()))?;
	let width = streaming["dimensions"]["width"].as_u64().unwrap_or(0);
	let height = streaming["dimensions"]["height"].as_u64().unwrap_or(0);
	let edge = match (width, height) {
		(0, 0) => return None,
		(width, 0) => width,
		(0, height) => height,
		(width, height) => width.min(height),
	};
	let size = [1080u64, 720, 480, 360, 240].into_iter().find(|size| *size <= edge).unwrap_or(240);
	Some(format!("https://v.redd.it/{id}/DASH_{size}.mp4?source=fallback"))
}

fn vreddit_id(url: Option<&str>) -> Option<String> {
	let parsed = url::Url::parse(url?).ok()?;
	if parsed.domain() != Some("v.redd.it") {
		return None;
	}
	let id = parsed.path_segments()?.next()?;
	if id.is_empty() {
		None
	} else {
		Some(id.to_string())
	}
}

/// GraphQL post to one `t3` child, used when `/by_id` is the block page.
#[cfg(test)]
pub(crate) fn convert_post(node: &Value) -> Value {
	let fullname = node["id"].as_str().unwrap_or_default();
	let content = &node["content"];
	let (selftext, selftext_html) = if content.is_object() {
		(content["markdown"].clone(), format!("<div class=\"md\">{}</div>", content_html(content)))
	} else {
		(Value::Null, String::new())
	};
	let author = &node["authorInfo"];
	let author_flair = &node["authorFlair"];
	let (author_flair_type, author_flair_richtext) = flair_parts(author_flair);
	let flair = &node["flair"];
	let (link_flair_type, link_flair_richtext) = flair_parts(flair);
	let domain = node["domain"].as_str().unwrap_or_default();
	let is_self = node["isSelfPost"] == true;
	let mut data = json_value!({
		"id": fullname.strip_prefix("t3_").unwrap_or(fullname),
		"name": fullname,
		"title": node["title"].as_str().or(node["postTitle"].as_str()),
		"subreddit": node["subreddit"]["name"],
		"author": author["name"].as_str().or(author["displayName"].as_str()).unwrap_or("[deleted]"),
		"author_flair_type": author_flair_type,
		"author_flair_richtext": author_flair_richtext,
		"author_flair_text": author_flair["text"],
		"author_flair_background_color": author_flair["template"]["backgroundColor"],
		"author_flair_text_color": author_flair["textColor"].as_str().map(str::to_lowercase),
		"link_flair_type": link_flair_type,
		"link_flair_richtext": link_flair_richtext,
		"link_flair_text": flair["text"],
		"link_flair_background_color": flair["template"]["backgroundColor"],
		"link_flair_text_color": flair["textColor"].as_str().map(str::to_lowercase),
		"distinguished": node["distinguishedAs"].as_str().map(str::to_lowercase),
		"removed_by_category": node["removedByCategory"].as_str().map(str::to_lowercase),
		"selftext": selftext,
		"selftext_html": selftext_html,
		"score": node["score"].as_i64().unwrap_or_default(),
		"upvote_ratio": node["upvoteRatio"],
		"created_utc": timestamp(&node["createdAt"]).unwrap_or_default(),
		"edited": timestamp(&node["editedAt"]).map_or(Value::Bool(false), Value::from),
		"num_comments": node["commentCount"].as_i64().unwrap_or_default(),
		"over_18": node["isNsfw"] == true,
		"spoiler": node["isSpoiler"] == true,
		"stickied": node["isStickied"] == true,
		"permalink": node["permalink"],
		"url": stored_url(&node["url"]),
		"url_overridden_by_dest": if is_self { Value::Null } else { stored_url(&node["url"]) },
		"domain": domain,
		"is_self": is_self,
		"is_reddit_media_domain": domain == "i.redd.it" || domain == "v.redd.it",
		"post_hint": node["postHint"].as_str().map(|hint| hint.to_lowercase().replace('_', ":")),
		"thumbnail": format_url(node["thumbnail"]["url"].as_str().unwrap_or_default()),
		"thumbnail_width": node["thumbnail"]["dimensions"]["width"],
		"thumbnail_height": node["thumbnail"]["dimensions"]["height"],
		"all_awardings": awards(&node["awardings"]),
		"suggested_sort": node["suggestedCommentSort"].as_str().map(str::to_lowercase),
	});
	let media = &node["media"];
	let still = &media["still"]["source"];
	if still.is_object() {
		let mut image = json_value!({ "source": proxied_source(still), "variants": {} });
		let mp4 = &media["animated"]["mp4_source"];
		if mp4.is_object() {
			image["variants"]["mp4"] = json_value!({ "source": proxied_source(mp4) });
		}
		data["preview"] = json_value!({ "images": [image] });
	}
	if let Some(video) = secure_media(node) {
		data["secure_media"] = video;
	}
	if let Some(items) = node["gallery"]["items"].as_array() {
		let items: Vec<&Value> = items.iter().filter(|item| item["media"]["id"].is_string()).collect();
		data["is_gallery"] = true.into();
		data["gallery_data"] = json_value!({ "items": items.iter().map(|item| json_value!({ "media_id": item["media"]["id"], "caption": item["caption"] })).collect::<Vec<_>>() });
		data["media_metadata"] = items
			.iter()
			.filter_map(|item| {
				let media = &item["media"];
				Some((
					media["id"].as_str()?.to_string(),
					json_value!({ "m": media["mimetype"], "s": { "u": format_url(media["url"].as_str().unwrap_or_default()), "x": media["width"], "y": media["height"] } }),
				))
			})
			.collect::<serde_json::Map<_, _>>()
			.into();
	}
	json_value!({ "kind": "t3", "data": data })
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_parse_path_and_sort() {
		assert_eq!(parse_path("/r/rust/comments/18t5968/title/.json?sort=top&raw_json=1"), Some(("18t5968".into(), None)));
		assert_eq!(
			parse_path("/r/rust/comments/18t5968/title/kfbqlbc.json?context=2"),
			Some(("18t5968".into(), Some("kfbqlbc".into())))
		);
		assert_eq!(parse_path("/r/rust/hot.json"), None);
		assert_eq!(sort_type("confidence"), Some("CONFIDENCE"));
		assert_eq!(sort_type("nope"), None);
	}

	#[test]
	fn test_comment_forest_becomes_nested_listing() {
		let trees = json_value!([
			{
				"depth": 0,
				"parentId": null,
				"node": {
					"__typename": "Comment",
					"id": "t1_abc123",
					"authorInfo": {"name": "alice"},
					"content": {"markdown": "hello **world**", "richtext": null},
					"createdAt": "2024-01-02T03:04:05.000000+0000",
					"score": 3,
					"isScoreHidden": false,
					"isStickied": true,
					"distinguishedAs": "MODERATOR",
					"authorFlair": {"text": "dev", "richtext": "[]", "template": {"backgroundColor": "#fff"}, "textColor": "DARK"},
					"awardings": [{ "award": {"name": "Helpful", "static_icon_16": {"url": "https://i.redd.it/a.png"}}, "total": 2 }]
				}
			},
			{
				"depth": 1,
				"parentId": "t1_abc123",
				"node": {"__typename": "DeletedComment", "authorInfo": {}, "content": {}, "awardings": []}
			},
			{
				"depth": 1,
				"parentId": "t1_abc123",
				"more": {"count": 4, "cursor": "next"}
			}
		]);
		let comments = nest(trees.as_array().unwrap().iter().filter_map(|tree| item(tree, "18t5968")).collect());
		assert_eq!(comments.len(), 1);
		assert_eq!(comments[0]["kind"], "t1");
		assert_eq!(comments[0]["data"]["author"], "alice");
		assert_eq!(comments[0]["data"]["body"], "hello **world**");
		assert_eq!(comments[0]["data"]["author_flair_text"], "dev");
		assert_eq!(comments[0]["data"]["author_flair_text_color"], "dark");
		assert_eq!(comments[0]["data"]["all_awardings"][0]["name"], "Helpful");
		assert_eq!(comments[0]["data"]["distinguished"], "moderator");
		assert_eq!(comments[0]["data"]["parent_id"], "t3_18t5968");
		assert!(comments[0]["data"]["created_utc"].as_f64().unwrap_or_default() > 1_700_000_000.0);
		let replies = comments[0]["data"]["replies"]["data"]["children"].as_array().unwrap();
		assert_eq!(replies.len(), 2);
		assert_eq!(replies[0]["data"]["body"], "[deleted]");
		assert_eq!(replies[0]["data"]["author"], "[deleted]");
		assert_eq!(replies[1]["kind"], "more");
		assert_eq!(replies[1]["data"]["count"], 4);
	}

	#[test]
	fn test_post_conversion_keeps_image_and_removed_body() {
		let node = json_value!({
			"id": "t3_18t5968",
			"title": "A picture",
			"subreddit": {"name": "pics"},
			"authorInfo": {"name": "bob"},
			"isSelfPost": false,
			"domain": "i.redd.it",
			"url": "https://i.redd.it/x.png",
			"permalink": "/r/pics/comments/18t5968/a_picture/",
			"postHint": "IMAGE",
			"score": 10,
			"upvoteRatio": 0.9,
			"commentCount": 0,
			"createdAt": "2024-01-02T03:04:05.507000+0000",
			"editedAt": "2024-01-03T03:04:05+0000",
			"isNsfw": false,
			"removedByCategory": "moderator",
			"media": {"still": {"source": {"url": "https://preview.redd.it/x.png", "dimensions": {"width": 100, "height": 80}}}},
			"awardings": [],
			"authorFlair": {},
			"flair": {"text": "OC"}
		});
		let post = convert_post(&node);
		assert_eq!(post["kind"], "t3");
		assert_eq!(post["data"]["id"], "18t5968");
		assert_eq!(post["data"]["post_hint"], "image");
		assert_eq!(post["data"]["preview"]["images"][0]["source"]["url"], "/preview/pre/x.png");
		assert_eq!(post["data"]["removed_by_category"], "moderator");
		assert_eq!(post["data"]["link_flair_text"], "OC");
		assert_eq!(post["data"]["num_comments"], 0);
		assert!(post["data"]["created_utc"].as_f64().unwrap_or_default() > 1_700_000_000.0);
		assert!(post["data"]["edited"].as_f64().unwrap_or_default() > post["data"]["created_utc"].as_f64().unwrap_or_default());
	}

	#[test]
	fn test_timestamp_accepts_reddit_and_rfc3339() {
		let reddit = timestamp(&json_value!("2023-12-28T21:35:27.507000+0000")).unwrap();
		let rfc3339 = timestamp(&json_value!("2023-12-28T21:35:27.507000+00:00")).unwrap();
		assert_eq!(reddit, rfc3339);
		assert!(reddit > 1_700_000_000.0);
		let plain = timestamp(&json_value!("2024-01-02T03:04:05+0000")).unwrap();
		assert_eq!(plain, timestamp(&json_value!("2024-01-02T03:04:05+00:00")).unwrap());
		assert_eq!(plain, timestamp(&json_value!("2024-01-02T03:04:05Z")).unwrap());
		assert_eq!(timestamp(&json_value!("not-a-date")), None);
		assert_eq!(timestamp(&json_value!(null)), None);
	}

	#[test]
	fn test_missing_post_is_not_found() {
		assert!(!has_post(&json_value!({ "kind": "Listing", "data": { "children": [] } })));
		assert!(!has_post(&json_value!({ "kind": "Listing", "data": { "children": [{ "kind": "t3", "data": {} }] } })));
		assert!(has_post(&json_value!({ "data": { "children": [{ "data": { "id": "18t5968", "name": "t3_18t5968" } }] } })));
	}

	#[test]
	fn test_graphql_errors_are_specific_and_do_not_panic() {
		let missing = json_value!({});
		let message = graphql_failure("PostComments", wreq::StatusCode::OK, &missing);
		assert!(message.contains("unknown GraphQL error"));
		let auth = json_value!({"errors": [{"message": "NOT_AUTHENTICATED"}]});
		assert!(graphql_failure("PostComments", wreq::StatusCode::OK, &auth).contains("NOT_AUTHENTICATED"));
		let hash = json_value!({"errors": [{"message": "PersistedQueryNotFound"}]});
		assert!(graphql_failure("PostComments", wreq::StatusCode::OK, &hash).contains("hash"));
	}
}
