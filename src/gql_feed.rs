// Reddit's GraphQL API in place of the JSON API, which is blocked for anonymous
// tokens since 2026-10-28 (see CLAUDE.md): subreddit feeds (also r/popular,
// r/all and combined feeds like r/a+b), posts by ID, subreddit and user info,
// user posts and comments, and search.
//
// `client::json` routes the matching JSON API paths here. Responses are
// converted into the JSON API's (listings of `t3` posts, `t1` comments, `t5`
// subreddits), so handlers and templates work unchanged. Paging cursors are
// fullnames (`t3_<id>`, `t1_<id>`), as in the JSON API; GraphQL takes them
// base64-encoded.

// Listings, about, wiki and search via the Android app's persisted queries.
// Adapted from polak14/redlib@29f18352 (gql_feed.rs). Same hashes as Nachtalb/redlib.
use crate::gql::is_id;
use crate::gql::{awards, content_html, flair_parts, query, secure_media, stored_url, timestamp, MAX_CURSOR_LEN};
use crate::utils::{format_url, param};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use log::debug;
use regex::Regex;
use serde_json::{json as json_value, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::LazyLock;

// Persisted query IDs from the Android app (see CLAUDE.md)
const SUBREDDIT_FEED_HASH: &str = "97046e2a48050f10a507db354a8da60955ffccb586dcf63163bdfc6610d5cf7a";
const POPULAR_FEED_HASH: &str = "728c0f9a4d12c17b8cffe07669bb1a8770b0f2ec0c84e040485f8fad2d9c417f";
const ALL_FEED_HASH: &str = "0e9c0045dd244e934efd0ea4fefc00cb0b4d91523e2f09c8af075856cb59b937";
const POSTS_BY_IDS_HASH: &str = "0e471495bcba33554b6c540b3cf1cae23dd5432a8b741dddc7fd6717dc2520a7";
const SUBREDDIT_INFO_HASH: &str = "6b9c1679e69097e1c6df364adc11183afe6e2b6545dd1c0c1cc8f7490448c3e5";
const USER_HASH: &str = "4822704f40348a38e37626b62f7f2a4e4af88aa5ab90f271a0c539db7ab2ac2f";
const WIKI_HASH: &str = "32f9314683f033a76812f772bbba771958760286c5cd28c558071a3ff4eb231f";
// The only operation with a post's duplicates (`otherDiscussions`)
const DUPLICATES_HASH: &str = "bce3523d2f221bc90447c79b131eeff478eb3fa98d5cdd19a6ded37c2c046b87";
const USER_POSTS_HASH: &str = "78a213c02a340db4a707d4bcd375ce34f733b75893379603acb4ec20388fafe6";
const USER_COMMENTS_HASH: &str = "e1b37f521fb40af2120946b70e7fbf6a3d924db2abaf28ff2164d4cfacca28db";
const COMMENTS_BY_IDS_HASH: &str = "51ab9f2e0eb87b6dac9aac02012ba7046049e516ec55ec442abfed0f19c6f311";
const SEARCH_POSTS_HASH: &str = "8610e33521c90caa8d12576800c18b708ce0d19df3dfac8e2aad55d9860e4bf9";
const SEARCH_SUBREDDITS_HASH: &str = "a3333069c1ead3ceec6d9bf4da4185ff584f69d0ba8b586e545b32ea62170932";
const PAGE_SIZE: usize = 25;
// r/all returns ~7 posts on its first page and ~25 after, whatever is asked for
const ALL_MAX_REQUESTS: usize = 3;

static SUBREDDIT_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/r/([^/]+)/(\w+)\.json$").unwrap());
static USER_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/user/([^/]+)/(\w+)\.json$").unwrap());
static WIKI_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/r/([^/]+)/wiki/(.+)\.json$").unwrap());
static DUPLICATES_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:/r/[^/]+)?/duplicates/(\w+)").unwrap());

const FEED_SORTS: [&str; 6] = ["hot", "new", "top", "rising", "controversial", "best"];
const USER_SORTS: [&str; 2] = ["new", "hot"];
const SEARCH_SORTS: [&str; 5] = ["relevance", "hot", "top", "new", "comments"];
const RANGES: [&str; 6] = ["hour", "day", "week", "month", "year", "all"];

/// Subreddit and user names: letters, digits, `_` (and `-` in user names)
fn is_name(name: &str, dash: bool) -> bool {
	!name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || (dash && b == b'-'))
}

/// A sort or time range from a path, checked so Reddit's errors don't stand in for ours
fn choice(value: &str, allowed: &[&str], what: &str) -> Result<String, String> {
	let value = value.to_lowercase();
	if allowed.contains(&value.as_str()) {
		Ok(value)
	} else {
		Err(format!("Unsupported {what}: {value}"))
	}
}

fn range(path: &str) -> Result<Option<String>, String> {
	param(path, "t").filter(|t| !t.is_empty()).map(|t| choice(&t, &RANGES, "time range")).transpose()
}

fn after(path: &str) -> Result<String, String> {
	let after = param(path, "after").unwrap_or_default();
	if after.len() > MAX_CURSOR_LEN {
		return Err("Invalid cursor".to_string());
	}
	Ok(after)
}

/// Answers the JSON API paths that GraphQL covers, or `None` for other paths.
pub async fn route(path: &str, _quarantine: bool) -> Option<Result<Value, String>> {
	let route = path.split('?').next().unwrap_or_default();
	if let Some(ids) = route.strip_prefix("/by_id/") {
		let ids: Vec<String> = ids.trim_end_matches(".json").split(',').map(str::to_string).collect();
		if !ids.iter().all(|id| id.strip_prefix("t3_").is_some_and(is_id)) {
			return Some(Err("Invalid post ID".to_string()));
		}
		return Some(by_id(ids).await.map(|posts| listing(posts, "")));
	}
	if route == "/search.json" {
		return Some(search(path, None).await);
	}
	if route == "/subreddits/search.json" {
		return Some(search_subreddits(path).await);
	}
	if let Some(caps) = WIKI_PATH.captures(route) {
		if !is_name(&caps[1], false) {
			return Some(Err("Invalid subreddit name".to_string()));
		}
		return Some(wiki(&caps[1], &caps[2]).await);
	}
	if let Some(caps) = DUPLICATES_PATH.captures(route) {
		if !is_id(&caps[1]) {
			return Some(Err("Invalid post ID".to_string()));
		}
		return Some(duplicates(&caps[1], &param(path, "sort").unwrap_or_default()).await);
	}

	if let Some(caps) = USER_PATH.captures(route) {
		let name = &caps[1];
		if !is_name(name, true) {
			return Some(Err("Invalid user name".to_string()));
		}
		let sort = match user_sort(&param(path, "sort").unwrap_or_default()) {
			Ok(sort) => sort,
			Err(msg) => return Some(Err(msg)),
		};
		let feed = || Feed::new(path, &sort, &USER_SORTS);
		return Some(match &caps[2] {
			"about" => user(name).await,
			"submitted" => async { feed()?.user_posts(name, &after(path)?).await }.await.map(|(posts, after)| listing(posts, &after)),
			"comments" => async { feed()?.user_comments(name, &after(path)?).await }
				.await
				.map(|(comments, after)| listing(comments, &after)),
			"overview" => async { feed()?.overview(name).await }.await.map(|(items, after)| listing(items, &after)),
			_ => return None,
		});
	}

	let caps = SUBREDDIT_PATH.captures(route)?;
	let subs = caps[1].replace("%2B", "+").replace("%2b", "+");
	if !subs.split('+').filter(|s| !s.is_empty()).all(|s| is_name(s, false)) {
		return Some(Err("Invalid subreddit name".to_string()));
	}
	let result = match &caps[2] {
		"about" => subreddit(&subs).await,
		"search" => search(path, Some(&subs)).await,
		sort => feed(path, sort, &subs).await,
	};
	// Keep "gated" and "quarantined" verbatim. The subreddit handler turns those
	// into the opt-in page. GraphQL does not return the listing after opt-in.
	Some(result)
}

async fn feed(path: &str, sort: &str, subs: &str) -> Result<Value, String> {
	let feed = Feed::new(path, sort, &FEED_SORTS)?;
	let result = match subs.to_lowercase().as_str() {
		"popular" => feed.popular().await,
		"all" => feed.all().await,
		_ if subs.contains('+') => feed.combined(subs.split('+').filter(|s| !s.is_empty()).map(str::to_lowercase).collect()).await,
		_ => feed.subreddit(subs, &feed.after).await,
	};
	result.map(|(posts, after)| listing(posts, &after))
}

fn listing(posts: Vec<Value>, after: &str) -> Value {
	json_value!({ "kind": "Listing", "data": { "children": posts, "after": if after.is_empty() { Value::Null } else { after.into() } } })
}

#[derive(Clone)]
struct Feed {
	sort: String,
	range: Option<String>,
	/// Fullname of the last post of the previous page, or the cursors of a combined feed
	after: String,
}

impl Feed {
	/// A feed with `sort` (one of `sorts`) and the time range and cursor of a JSON API path
	fn new(path: &str, sort: &str, sorts: &[&str]) -> Result<Self, String> {
		Ok(Self {
			sort: choice(sort, sorts, "sort")?.to_uppercase(),
			range: range(path)?.map(|t| t.to_uppercase()),
			after: after(path)?,
		})
	}

	fn variables(&self, after: &str) -> Value {
		let mut variables = json_value!({ "sort": self.sort, "pageSize": PAGE_SIZE });
		if let Some(range) = &self.range {
			variables["range"] = range.as_str().into();
		}
		if !after.is_empty() {
			variables["after"] = to_cursor(after).into();
		}
		variables
	}

	async fn subreddit(&self, name: &str, after: &str) -> Result<(Vec<Value>, String), String> {
		let mut variables = self.variables(after);
		variables["subredditName"] = name.into();
		variables["includeSubredditInPosts"] = true.into();
		let response = query("SubredditFeedElements", SUBREDDIT_FEED_HASH, &variables, "graphql").await?;
		let feed = &response["data"]["postFeed"];
		match feed["__typename"].as_str() {
			Some("Subreddit") => Ok(page(&feed["elements"], |node| node)),
			// The feed doesn't say why; the info does
			Some("UnavailableSubreddit") => Err(subreddit(name).await.err().unwrap_or_else(|| format!("r/{name} is unavailable"))),
			_ => Err(format!("r/{name} doesn't exist")),
		}
	}

	async fn popular(&self) -> Result<(Vec<Value>, String), String> {
		let response = query("PopularFeedElements", POPULAR_FEED_HASH, &self.variables(&self.after), "graphql").await?;
		Ok(page(&response["data"]["popular"]["elements"], |node| &node["postInfo"]))
	}

	/// r/all only gives post IDs, without ads' posts; the posts come from `PostsByIds`.
	async fn all(&self) -> Result<(Vec<Value>, String), String> {
		let mut variables = json_value!({ "sort": self.sort });
		if let Some(range) = &self.range {
			variables["time"] = range.as_str().into();
		}
		let mut after = self.after.clone();
		let mut ids = Vec::new();
		for _ in 0..ALL_MAX_REQUESTS {
			if !after.is_empty() {
				variables["after"] = to_cursor(&after).into();
			}
			let response = query("AllFeedSdui", ALL_FEED_HASH, &variables, "graphql").await?;
			let elements = &response["data"]["allV3"]["elements"];
			for node in elements["edges"].as_array().into_iter().flatten().map(|edge| &edge["node"]) {
				let is_ad = node["cells"].as_array().into_iter().flatten().any(|cell| cell["__typename"] == "AdMetadataCell");
				if let Some(id) = node["groupId"].as_str().filter(|id| id.starts_with("t3_") && !is_ad) {
					ids.push(id.to_string());
				}
			}
			after = elements["pageInfo"]["endCursor"].as_str().map(from_cursor).unwrap_or_default();
			if ids.len() >= PAGE_SIZE || after.is_empty() {
				break;
			}
		}
		Ok((by_id(ids).await?, after))
	}

	/// Merges the feeds of several subreddits, like the JSON API's `r/a+b`, which
	/// GraphQL lacks. Each page takes posts from the front of each subreddit's
	/// feed, so the cursors (`sub:t3_<id>,…`) say how far each one got.
	async fn combined(&self, subs: Vec<String>) -> Result<(Vec<Value>, String), String> {
		let cursors = self.cursors();
		let tasks: Vec<_> = subs
			.iter()
			.map(|sub| {
				let (feed, sub) = (self.clone(), sub.clone());
				let after = cursors.get(&sub).cloned().unwrap_or_default();
				tokio::spawn(async move { feed.subreddit(&sub, &after).await })
			})
			.collect();
		let mut feeds = Vec::new();
		let mut error = None;
		for (sub, task) in subs.iter().zip(tasks) {
			match task.await.map_err(|e| e.to_string()).and_then(|r| r) {
				// Stickied posts belong to the subreddit's own page
				Ok((posts, _)) => feeds.push((sub.clone(), posts.into_iter().filter(|p| p["data"]["stickied"] != true).collect())),
				Err(e) => {
					debug!("Skipping r/{sub} in combined feed: {e}");
					error.get_or_insert(e);
				}
			}
		}
		if feeds.is_empty() {
			return Err(error.unwrap_or_else(|| "No posts found".to_string()));
		}
		Ok(self.merge(feeds, cursors))
	}

	/// Cursors of a merged feed: `key:<fullname>,…`
	fn cursors(&self) -> HashMap<String, String> {
		self
			.after
			.split(',')
			.filter_map(|c| c.split_once(':'))
			.map(|(key, after)| (key.to_lowercase(), after.to_string()))
			.collect()
	}

	/// Merges feeds into one page, taking items from the front of each feed, so
	/// each feed's cursor says how far it got. Returns the page and its cursors.
	fn merge(&self, feeds: Vec<(String, Vec<Value>)>, mut cursors: HashMap<String, String>) -> (Vec<Value>, String) {
		let keys: Vec<String> = feeds.iter().map(|(key, _)| key.clone()).collect();
		let mut feeds: Vec<(String, VecDeque<(usize, Value)>)> = feeds.into_iter().map(|(key, items)| (key, items.into_iter().enumerate().collect())).collect();
		let sizes: Vec<usize> = feeds.iter().map(|(_, posts)| posts.len()).collect();
		let mut posts = Vec::new();
		while posts.len() < PAGE_SIZE {
			let best = feeds
				.iter()
				.enumerate()
				.filter_map(|(i, (_, posts))| posts.front().map(|(index, post)| (i, rank(&self.sort, post, *index, sizes[i]))))
				.max_by(|a, b| a.1.total_cmp(&b.1));
			let Some((i, _)) = best else { break };
			let (sub, feed) = &mut feeds[i];
			if let Some((_, post)) = feed.pop_front() {
				cursors.insert(sub.clone(), post["data"]["name"].as_str().unwrap_or_default().to_string());
				posts.push(post);
			}
		}

		let after = if posts.is_empty() {
			String::new()
		} else {
			keys
				.iter()
				.filter_map(|key| cursors.get(key).map(|after| format!("{key}:{after}")))
				.collect::<Vec<_>>()
				.join(",")
		};
		(posts, after)
	}

	async fn user_posts(&self, name: &str, after: &str) -> Result<(Vec<Value>, String), String> {
		let mut variables = self.variables(after);
		variables["username"] = name.into();
		variables["includeSubredditInPosts"] = true.into();
		// The persisted query pages only when `after` is present, including null
		// on the first page. Omitting it returns one page and no cursor.
		if after.is_empty() {
			variables["after"] = Value::Null;
		}
		let response = query("UserSubmittedPosts", USER_POSTS_HASH, &variables, "graphql").await?;
		let feed = &response["data"]["postFeed"];
		if feed.is_null() {
			return Err(format!("u/{name} doesn't exist"));
		}
		let (posts, _) = page(&feed["posts"], |node| node);
		let after = posts_after(&posts, &feed["posts"]);
		Ok((posts, after))
	}

	/// The comments list only has previews; full comments come from `LiveCommentsByIds`.
	async fn user_comments(&self, name: &str, after: &str) -> Result<(Vec<Value>, String), String> {
		let mut variables = self.variables(after);
		variables["name"] = name.into();
		let response = query("SubmittedComments", USER_COMMENTS_HASH, &variables, "graphql").await?;
		let user = &response["data"]["redditorInfoByName"];
		if user.is_null() {
			return Err(format!("u/{name} doesn't exist"));
		}
		let comments = &user["comments"];
		let previews: Vec<&Value> = comments["edges"]
			.as_array()
			.into_iter()
			.flatten()
			.map(|edge| &edge["node"])
			.filter(|node| node["id"].is_string())
			.collect();
		let after = next_cursor(comments);
		if previews.is_empty() {
			return Ok((Vec::new(), after));
		}

		let ids: Vec<&Value> = previews.iter().map(|node| &node["id"]).collect();
		let response = query(
			"LiveCommentsByIds",
			COMMENTS_BY_IDS_HASH,
			&json_value!({ "ids": ids, "includePremiumAvatarTreatment": false }),
			"graphql",
		)
		.await?;
		let full = response["data"]["commentsByIds"].as_array().cloned().unwrap_or_default();
		let comments = previews
			.iter()
			.filter_map(|preview| full.iter().find(|c| c["id"] == preview["id"]).map(|c| convert_comment(c, &preview["postInfo"]["title"])))
			.collect();
		Ok((comments, after))
	}

	/// Posts and comments, newest first. Each side keeps its own cursor
	/// (`posts:<fullname>,comments:<fullname>`). `-` means that side is finished.
	async fn overview(&self, name: &str) -> Result<(Vec<Value>, String), String> {
		let cursors = self.cursors();
		let cursor = |key: &str| cursors.get(key).cloned().unwrap_or_default();
		let post_cursor = cursor("posts");
		let comment_cursor = cursor("comments");
		let posts = async {
			if post_cursor == "-" {
				Ok((Vec::new(), "-".to_string()))
			} else {
				self.user_posts(name, &post_cursor).await
			}
		};
		let comments = async {
			if comment_cursor == "-" {
				Ok((Vec::new(), "-".to_string()))
			} else {
				self.user_comments(name, &comment_cursor).await
			}
		};
		let (posts, comments) = futures_lite::future::zip(posts, comments).await;
		let (posts, post_after) = posts?;
		let (comments, comment_after) = comments?;
		Ok(merge_by_date(posts, &post_after, comments, &comment_after))
	}
}

/// The next page of a user post listing. `hasNextPage: false` is the end of the
/// feed. The last post id is only a fallback when Reddit omits `pageInfo`.
fn posts_after(posts: &[Value], connection: &Value) -> String {
	if connection.get("pageInfo").is_some() {
		return next_cursor(connection);
	}
	if posts.len() + 1 >= PAGE_SIZE {
		if let Some(name) = posts.last().and_then(|post| post["data"]["name"].as_str()).filter(|name| is_fullname(name)) {
			return name.to_string();
		}
	}
	String::new()
}

/// User post queries return no edges for top and controversial.
fn user_sort(sort: &str) -> Result<String, String> {
	let sort = if sort.is_empty() { "new" } else { sort };
	if USER_SORTS.contains(&sort.to_ascii_lowercase().as_str()) {
		Ok(sort.to_string())
	} else {
		Err(format!("User listings only support hot and new. {sort} is not available."))
	}
}

fn created_utc(item: &Value) -> f64 {
	item["data"]["created_utc"].as_f64().unwrap_or_default()
}

/// Merges one page of posts and one page of comments in date order. Items older
/// than the last item of a side that still has another page wait, so a busy
/// comment page cannot skip every post.
fn merge_by_date(posts: Vec<Value>, post_after: &str, comments: Vec<Value>, comment_after: &str) -> (Vec<Value>, String) {
	let post_more = !post_after.is_empty() && post_after != "-";
	let comment_more = !comment_after.is_empty() && comment_after != "-";
	let mut cutoff = f64::MIN;
	if post_more {
		cutoff = cutoff.max(posts.last().map(created_utc).unwrap_or(f64::MIN));
	}
	if comment_more {
		cutoff = cutoff.max(comments.last().map(created_utc).unwrap_or(f64::MIN));
	}
	let keep = |items: &[Value]| -> Vec<Value> { items.iter().filter(|item| created_utc(item) >= cutoff).cloned().collect() };
	let kept_posts = keep(&posts);
	let kept_comments = keep(&comments);
	let side = |kept: &[Value], page: &[Value], after: &str| -> String {
		if kept.len() == page.len() && (after.is_empty() || after == "-") {
			"-".to_string()
		} else if let Some(name) = kept.last().and_then(|item| item["data"]["name"].as_str()).filter(|name| !name.is_empty()) {
			name.to_string()
		} else {
			after.to_string()
		}
	};
	let next_posts = side(&kept_posts, &posts, post_after);
	let next_comments = side(&kept_comments, &comments, comment_after);
	let mut children = kept_posts;
	children.extend(kept_comments);
	children.sort_by(|a, b| created_utc(b).total_cmp(&created_utc(a)));
	let after = if next_posts == "-" && next_comments == "-" {
		String::new()
	} else {
		format!("posts:{next_posts},comments:{next_comments}")
	};
	(children, after)
}

/// The next page's cursor of a connection, or empty on the last page
fn next_cursor(connection: &Value) -> String {
	let has_next = connection["pageInfo"]["hasNextPage"] != false;
	connection["pageInfo"]["endCursor"].as_str().filter(|_| has_next).map(from_cursor).unwrap_or_default()
}

/// Order of posts in a combined feed: higher goes first. Each subreddit's feed
/// keeps its own order; sorts without a formula interleave proportionally.
fn rank(sort: &str, post: &Value, index: usize, size: usize) -> f64 {
	let data = &post["data"];
	let score = data["score"].as_f64().unwrap_or_default();
	let created = data["created_utc"].as_f64().unwrap_or_default();
	match sort {
		"NEW" => created,
		"TOP" => score,
		// Reddit's hot ranking
		"HOT" => score.signum() * score.abs().max(1.0).log10() + (created - 1_134_028_003.0) / 45000.0,
		_ => 1.0 - index as f64 / size as f64,
	}
}

/// Posts and the next page's cursor of a feed's `elements`. `post` picks the
/// post from a node; nodes that aren't posts (recommendations) are skipped.
fn page<'a>(elements: &'a Value, post: impl Fn(&'a Value) -> &'a Value) -> (Vec<Value>, String) {
	let posts = elements["edges"]
		.as_array()
		.into_iter()
		.flatten()
		.map(|edge| post(&edge["node"]))
		.filter(|node| node["id"].as_str().is_some_and(|id| id.starts_with("t3_")))
		.map(convert)
		.collect();
	(posts, next_cursor(elements))
}

async fn by_id(ids: Vec<String>) -> Result<Vec<Value>, String> {
	if ids.is_empty() {
		return Ok(Vec::new());
	}
	let variables = json_value!({ "ids": ids, "includePostContentPostHint": true, "includePostContentThumbnailEnabled": true });
	let response = query("PostsByIds", POSTS_BY_IDS_HASH, &variables, "graphql").await?;
	Ok(
		response["data"]["postsInfoByIds"]
			.as_array()
			.into_iter()
			.flatten()
			.filter(|node| node["id"].is_string())
			.map(convert)
			.collect(),
	)
}

/// Why a subreddit is unavailable, as the JSON API's errors that handlers expect
fn unavailable(sub: &Value, name: &str) -> String {
	match sub["forbiddenReason"].as_str().map(str::to_lowercase).as_deref() {
		Some(reason @ ("gated" | "quarantined" | "private" | "banned")) => reason.to_string(),
		_ => format!("r/{name} is unavailable"),
	}
}

/// Subreddit info as the JSON API's `about`
async fn subreddit(name: &str) -> Result<Value, String> {
	let variables = json_value!({ "subredditName": name, "includeDevvitData": false, "includeRecapFields": false });
	let response = query("SubredditInfoByName", SUBREDDIT_INFO_HASH, &variables, "graphql").await?;
	let sub = &response["data"]["subredditInfoByName"];
	match sub["__typename"].as_str() {
		Some("Subreddit") => {}
		Some("UnavailableSubreddit") => return Err(unavailable(sub, name)),
		_ => return Err(format!("r/{name} doesn't exist")),
	}
	Ok(subreddit_value(sub))
}

fn subreddit_value(sub: &Value) -> Value {
	json_value!({
		"kind": "t5",
		"data": {
			"display_name": sub["name"],
			"title": sub["title"],
			"public_description": sub["publicDescriptionText"],
			"description_html": format!("<div class=\"md\">{}</div>", content_html(&sub["description"])),
			"subscribers": sub["subscribersCount"].as_f64().map(|n| n as u64),
			"accounts_active": sub["communityStats"]["weeklyActiveUsersCount"],
			"community_icon": sub["styles"]["icon"],
			"icon_img": sub["styles"]["legacyIcon"]["url"],
			"wiki_enabled": sub["wiki"]["indexPage"]["status"] == "VALID",
			"over18": sub["isNsfw"] == true,
		}
	})
}

/// User info as the JSON API's `about`
async fn user(name: &str) -> Result<Value, String> {
	let response = query("UserProfileDetails", USER_HASH, &json_value!({ "username": name }), "graphql").await?;
	let user = &response["data"]["redditorInfoByName"];
	if user["__typename"] != "Redditor" {
		return Err(format!("u/{name} doesn't exist"));
	}
	let profile = &user["profileInfo"];
	Ok(json_value!({
		"kind": "t2",
		"data": {
			"name": user["name"],
			"created": timestamp(&profile["createdAt"]),
			"total_karma": user["karma"]["total"],
			"subreddit": {
				"title": profile["title"],
				"icon_img": user["icon"]["url"],
				"banner_img": profile["styles"]["profileBanner"],
				"public_description": profile["publicDescriptionText"],
				"over_18": profile["isNsfw"] == true,
			}
		}
	}))
}

/// A wiki page as the JSON API's, without `content_html` if it doesn't exist
async fn wiki(sub: &str, page: &str) -> Result<Value, String> {
	let variables = json_value!({
		"subredditName": sub,
		"pageName": page,
		"includePage": true,
		"includePageContent": true,
		"includeMediaFragment": true,
		"includeIsRevisable": false,
		"includeRevisionsCount": false,
		"includePageTitle": true,
	});
	let response = query("SubredditWiki", WIKI_HASH, &variables, "graphql").await?;
	let info = &response["data"]["subredditInfoByName"];
	match info["__typename"].as_str() {
		Some("Subreddit") => {}
		Some("UnavailableSubreddit") => return Err(subreddit(sub).await.err().unwrap_or_else(|| format!("r/{sub} is unavailable"))),
		_ => return Err(format!("r/{sub} doesn't exist")),
	}
	let chosen = if page.eq_ignore_ascii_case("index") { wiki_page(info) } else { &info["wiki"]["page"] };
	let html = wiki_html(chosen);
	Ok(json_value!({ "kind": "wikipage", "data": { "content_html": html } }))
}

/// Index lives on `page` for some subs and on `indexPage` for others.
fn wiki_page(info: &Value) -> &Value {
	let page = &info["wiki"]["page"];
	if wiki_html(page).is_some() {
		page
	} else {
		&info["wiki"]["indexPage"]
	}
}

/// Render whenever the page has body text. `status == VALID` is not required:
/// readable wikis (r/rust's index among them) come back as another status with
/// the same content. A missing page has neither.
fn wiki_html(page: &Value) -> Option<String> {
	let status = page["status"].as_str().unwrap_or("").to_ascii_uppercase();
	if matches!(status.as_str(), "PAGE_NOT_FOUND" | "WIKI_DISABLED" | "FORBIDDEN" | "PAGE_FORBIDDEN" | "UNKNOWN") {
		return None;
	}
	let html = content_html(&page["content"]);
	if html.is_empty() {
		return None;
	}
	Some(format!("<div class=\"md wiki\">{html}</div>"))
}

async fn duplicate_ids(id: &str) -> Result<Vec<String>, String> {
	let response = query("CreatorStats", DUPLICATES_HASH, &json_value!({ "postId": format!("t3_{id}") }), "graphql").await?;
	let others = response["data"]["postInfoById"]["otherDiscussions"]["edges"].as_array().cloned().unwrap_or_default();
	Ok(others.iter().filter_map(|edge| edge["node"]["id"].as_str().map(str::to_string)).collect())
}

/// A post and its duplicates (other posts of the same link), as the JSON API's
/// `[post, duplicates]`. Duplicates come in one page, sorted here.
async fn duplicates(id: &str, sort: &str) -> Result<Value, String> {
	// Only a few fields each; the full posts come from `PostsByIds`
	let mut ids = vec![format!("t3_{id}")];
	ids.extend(duplicate_ids(id).await?);
	let mut posts = by_id(ids).await?;
	if posts.first().map(|post| post["data"]["id"] != id).unwrap_or(true) {
		return Err("Post not found".to_string());
	}
	let mut duplicates = posts.split_off(1);
	posts[0]["data"]["num_duplicates"] = duplicates.len().into();
	let key = if sort == "new" { "created_utc" } else { "num_comments" };
	duplicates.sort_by(|a, b| b["data"][key].as_f64().unwrap_or_default().total_cmp(&a["data"][key].as_f64().unwrap_or_default()));
	Ok(json_value!([listing(posts, ""), listing(duplicates, "")]))
}

/// Post search, everywhere or (`restrict_sr`) in subreddit `sub`
async fn search(path: &str, sub: Option<&str>) -> Result<Value, String> {
	let text = param(path, "q").unwrap_or_default();
	let restrict = param(path, "restrict_sr").is_some_and(|r| !r.is_empty());
	let text = match sub {
		Some(sub) if restrict => format!("subreddit:{sub} {text}"),
		_ => text,
	};
	let range = range(path)?.unwrap_or_else(|| "all".to_string());
	let sort = choice(
		&param(path, "sort").filter(|s| !s.is_empty()).unwrap_or_else(|| "relevance".to_string()),
		&SEARCH_SORTS,
		"sort",
	)?;
	let nsfw = if param(path, "include_over_18").is_some() { "1" } else { "0" };
	let mut variables = json_value!({
		"query": text,
		"sort": sort.to_uppercase(),
		"filters": [{ "key": "time_range", "value": range }, { "key": "nsfw", "value": nsfw }],
		"pageSize": PAGE_SIZE,
		"productSurface": "android",
	});
	let after = after(path)?;
	if !after.is_empty() {
		variables["afterCursor"] = after.into();
	}
	let response = query("SearchPosts", SEARCH_POSTS_HASH, &variables, "graphql").await?;
	let (posts, after) = page(&response["data"]["search"]["general"]["posts"], |node| node);
	Ok(listing(posts, &after))
}

/// Subreddit search as the JSON API's listing of `t5` items
async fn search_subreddits(path: &str) -> Result<Value, String> {
	let limit = param(path, "limit").and_then(|l| l.parse::<u32>().ok()).unwrap_or(3);
	let variables = json_value!({ "query": param(path, "q").unwrap_or_default(), "pageSize": limit, "productSurface": "android" });
	let response = query("SearchCommunities", SEARCH_SUBREDDITS_HASH, &variables, "graphql").await?;
	let subreddits = response["data"]["search"]["general"]["communities"]["edges"]
		.as_array()
		.into_iter()
		.flatten()
		.map(|edge| &edge["node"])
		.filter(|sub| sub["name"].is_string())
		.map(|sub| {
			json_value!({
				"kind": "t5",
				"data": {
					"display_name": sub["name"],
					"url": sub["path"],
					"community_icon": sub["styles"]["icon"],
					"icon_img": sub["styles"]["legacyIcon"]["url"],
					"public_description": sub["publicDescriptionText"],
					"subscribers": sub["subscribersCount"],
				}
			})
		})
		.collect();
	Ok(listing(subreddits, ""))
}

/// Converts a GraphQL comment, from `LiveCommentsByIds`, to an item of a user's
/// comments in the JSON API.
fn convert_comment(node: &Value, link_title: &Value) -> Value {
	let fullname = node["id"].as_str().unwrap_or_default();
	let content = &node["content"];
	let permalink = node["permalink"].as_str().unwrap_or_default();
	// `/r/<sub>/comments/…`, or `/user/<name>/comments/…` for profile posts
	let mut segments = permalink.split('/').skip(1);
	let subreddit = match (segments.next(), segments.next()) {
		(Some("r"), Some(sub)) => sub.to_string(),
		(Some("user"), Some(name)) => format!("u_{name}"),
		_ => String::new(),
	};
	let author = &node["authorInfo"];
	json_value!({
		"kind": "t1",
		"data": {
			"id": fullname.strip_prefix("t1_").unwrap_or(fullname),
			"name": fullname,
			"author": author["name"].as_str().or(author["attributes"]["redditHandleInfo"]["username"].as_str()).unwrap_or("[deleted]"),
			"body": content["markdown"],
			"body_html": format!("<div class=\"md\">{}</div>", content_html(content)),
			"score": node["score"].as_i64().unwrap_or_default(),
			"created_utc": timestamp(&node["createdAt"]).unwrap_or_default(),
			"permalink": permalink,
			"link_title": link_title,
			"subreddit": subreddit,
			"distinguished": node["distinguishedAs"].as_str().map(str::to_lowercase),
			"stickied": node["isStickied"] == true,
		}
	})
}

fn is_fullname(cursor: &str) -> bool {
	cursor.starts_with("t3_") || cursor.starts_with("t1_")
}

/// GraphQL's cursors are base64 fullnames, except search's (offsets), which
/// stay as they are
fn to_cursor(after: &str) -> String {
	if is_fullname(after) {
		STANDARD.encode(after)
	} else {
		after.to_string()
	}
}

fn from_cursor(cursor: &str) -> String {
	STANDARD
		.decode(cursor)
		.ok()
		.and_then(|c| String::from_utf8(c).ok())
		.filter(|c| is_fullname(c))
		.unwrap_or_else(|| cursor.to_string())
}

/// Converts a GraphQL post to an item of the JSON API.
fn convert(node: &Value) -> Value {
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
		"hide_score": node["isScoreHidden"] == true,
		"upvote_ratio": node["upvoteRatio"],
		"created_utc": timestamp(&node["createdAt"]).unwrap_or_default(),
		"edited": timestamp(&node["editedAt"]).map_or(Value::Bool(false), Value::from),
		"num_comments": node["commentCount"].as_i64().unwrap_or_default(),
		"over_18": node["isNsfw"] == true,
		"spoiler": node["isSpoiler"] == true,
		"stickied": node["isStickied"] == true,
		"locked": node["isLocked"] == true,
		"archived": node["isArchived"] == true,
		"permalink": node["permalink"],
		"url": stored_url(&node["url"]),
		"url_overridden_by_dest": if is_self { Value::Null } else { stored_url(&node["url"]) },
		"domain": domain,
		"is_self": is_self,
		"is_reddit_media_domain": domain == "i.redd.it" || domain == "v.redd.it",
		// SELF, IMAGE, HOSTED_VIDEO, RICH_VIDEO, LINK as self, image, hosted:video, …
		"post_hint": node["postHint"].as_str().map(|hint| hint.to_lowercase().replace('_', ":")),
		"thumbnail": format_url(node["thumbnail"]["url"].as_str().unwrap_or_default()),
		"thumbnail_width": node["thumbnail"]["dimensions"]["width"],
		"thumbnail_height": node["thumbnail"]["dimensions"]["height"],
		"all_awardings": awards(&node["awardings"]),
	});

	// The comment sort to use when none is chosen, like CONFIDENCE or QA
	data["suggested_sort"] = node["suggestedCommentSort"].as_str().map(str::to_lowercase).into();

	let media = &node["media"];
	let still = &media["still"]["source"];
	if still.is_object() {
		let mut image = json_value!({ "source": source(still), "variants": {} });
		let mp4 = &media["animated"]["mp4_source"];
		if mp4.is_object() {
			image["variants"]["mp4"] = json_value!({ "source": source(mp4) });
		}
		data["preview"] = json_value!({ "images": [image] });
	}
	if let Some(video) = secure_media(node) {
		data["secure_media"] = video;
	}

	if let Some(items) = node["gallery"]["items"].as_array() {
		let items: Vec<&Value> = items.iter().filter(|item| item["media"]["id"].is_string()).collect();
		data["is_gallery"] = true.into();
		data["gallery_data"] = json_value!({ "items": items.iter().map(|item| json_value!({ "media_id": item["media"]["id"], "caption": item["caption"], "outbound_url": item["outboundUrl"] })).collect::<Vec<_>>() });
		data["media_metadata"] = items
			.iter()
			.filter_map(|item| {
				let media = &item["media"];
				let url = format_url(media["url"].as_str().unwrap_or_default());
				let image = json_value!({ "u": url, "gif": url, "x": media["width"], "y": media["height"] });
				Some((media["id"].as_str()?.to_string(), json_value!({ "m": media["mimetype"], "s": image })))
			})
			.collect::<serde_json::Map<_, _>>()
			.into();
	}

	let poll = &node["poll"];
	if poll.is_object() {
		let options: Vec<Value> = poll["options"]
			.as_array()
			.into_iter()
			.flatten()
			.map(|option| json_value!({ "id": option["id"], "text": option["text"], "vote_count": option["voteCount"] }))
			.collect();
		data["poll_data"] = json_value!({
			"options": options,
			"total_vote_count": poll["totalVoteCount"],
			// In milliseconds, like the JSON API's
			"voting_end_timestamp": timestamp(&poll["votingEndsAt"]).map(|t| t * 1000.0),
		});
	}

	let crosspost = &node["crosspostRoot"]["post"];
	if crosspost["id"].is_string() {
		data["crosspost_parent_list"] = json_value!([convert(crosspost)["data"]]);
	}

	json_value!({ "kind": "t3", "data": data })
}

fn source(media: &Value) -> Value {
	json_value!({
		"url": format_url(media["url"].as_str().unwrap_or_default()),
		"width": media["dimensions"]["width"],
		"height": media["dimensions"]["height"],
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_cursor() {
		assert_eq!(to_cursor("t3_1xhmntk"), "dDNfMXhobW50aw==");
		assert_eq!(from_cursor("dDNfMXhobW50aw=="), "t3_1xhmntk");
		// Anything else passes through
		assert_eq!(from_cursor("abc"), "abc");
		assert_eq!(to_cursor("abc"), "abc");
	}

	#[test]
	fn test_checks() {
		assert_eq!(choice("Top", &FEED_SORTS, "sort"), Ok("top".to_string()));
		assert!(choice("TOP\"}", &FEED_SORTS, "sort").is_err());
		assert_eq!(range("?t=week"), Ok(Some("week".to_string())));
		assert_eq!(range("?t="), Ok(None));
		assert!(range("?t=decade").is_err());
		assert!(after(&format!("?after={}", "a".repeat(MAX_CURSOR_LEN + 1))).is_err());
		assert!(is_name("rust", false) && is_name("u_spez", false) && is_name("some-user", true));
		assert!(!is_name("some-user", false) && !is_name("../x", true) && !is_name("", false));
	}

	#[test]
	fn test_rank() {
		let post = |score: i64, created: f64| json_value!({ "data": { "score": score, "created_utc": created } });
		// A day newer outweighs 10 times the score
		assert!(rank("HOT", &post(100, 1_800_000_000.0 + 86400.0), 0, 1) > rank("HOT", &post(1000, 1_800_000_000.0), 0, 1));
		assert!(rank("HOT", &post(-10, 1_800_000_000.0), 0, 1) < rank("HOT", &post(0, 1_800_000_000.0), 0, 1));
		assert!(rank("RISING", &post(0, 0.0), 1, 4) < rank("RISING", &post(0, 0.0), 0, 2));
	}

	#[test]
	fn test_feed_sorts_time_range_and_after_cursor() {
		for sort in ["hot", "new", "top", "rising", "controversial", "best"] {
			let feed = Feed::new(&format!("/r/rust/{sort}.json"), sort, &FEED_SORTS).unwrap();
			assert_eq!(feed.sort, sort.to_uppercase());
			assert_eq!(feed.variables("")["sort"], sort.to_uppercase());
		}
		let feed = Feed::new("/r/rust/top.json?t=week&after=t3_abc&raw_json=1", "top", &FEED_SORTS).unwrap();
		let vars = feed.variables(&feed.after);
		assert_eq!(vars["sort"], "TOP");
		assert_eq!(vars["range"], "WEEK");
		assert_eq!(vars["after"], to_cursor("t3_abc"));
		assert_eq!(vars["pageSize"], PAGE_SIZE);
		let with_before = Feed::new("/r/rust/hot.json?before=t3_zzz", "hot", &FEED_SORTS).unwrap();
		assert!(with_before.variables("").get("before").is_none());
		assert!(SUBREDDIT_PATH.captures("/r/popular/hot.json").is_some());
		assert!(SUBREDDIT_PATH.captures("/r/all/new.json").is_some());
		assert_eq!(&SUBREDDIT_PATH.captures("/r/rust/controversial.json").unwrap()[1], "rust");
	}

	#[test]
	fn test_listing_page_keeps_children_and_after() {
		let elements = json_value!({
			"edges": [{
				"node": {
					"id": "t3_abc",
					"title": "Hello",
					"subreddit": {"name": "rust"},
					"authorInfo": {"name": "ada"},
					"isSelfPost": true,
					"permalink": "/r/rust/comments/abc/hello/",
					"url": "https://www.reddit.com/r/rust/comments/abc/hello/",
					"domain": "self.rust",
					"score": 4,
					"commentCount": 0,
					"createdAt": "2024-01-02T03:04:05.507000+0000",
					"awardings": [],
					"authorFlair": {},
					"flair": {}
				}
			}],
			"pageInfo": {"hasNextPage": true, "endCursor": to_cursor("t3_abc")}
		});
		let (posts, after) = page(&elements, |node| node);
		let listing = listing(posts, &after);
		assert_eq!(listing["data"]["children"][0]["kind"], "t3");
		assert_eq!(listing["data"]["children"][0]["data"]["title"], "Hello");
		assert_eq!(listing["data"]["children"][0]["data"]["subreddit"], "rust");
		assert_eq!(listing["data"]["after"], "t3_abc");
		assert!(listing["data"]["children"][0]["data"]["created_utc"].as_f64().unwrap_or_default() > 1_700_000_000.0);
	}

	fn sample_user_edges(count: usize) -> Vec<Value> {
		(0..count)
			.map(|n| {
				json_value!({
					"node": {
						"id": format!("t3_{n}"),
						"title": "p",
						"subreddit": {"name": "rust"},
						"authorInfo": {"name": "spez"},
						"isSelfPost": true,
						"permalink": "/r/rust/comments/x/p/",
						"url": "https://www.reddit.com/r/rust/comments/x/p/",
						"domain": "self.rust",
						"score": 1,
						"commentCount": 0,
						"createdAt": "2024-06-01T00:00:00+0000",
						"awardings": [],
						"authorFlair": {},
						"flair": {}
					}
				})
			})
			.collect()
	}

	#[test]
	fn test_user_posts_stop_when_has_next_page_is_false() {
		let connection = json_value!({ "edges": sample_user_edges(24), "pageInfo": { "hasNextPage": false } });
		let (posts, _) = page(&connection, |node| node);
		assert_eq!(posts.len(), 24);
		assert_eq!(posts_after(&posts, &connection), "");
	}

	#[test]
	fn test_user_posts_without_page_info_use_the_last_id() {
		let connection = json_value!({ "edges": sample_user_edges(24) });
		let (posts, _) = page(&connection, |node| node);
		assert_eq!(posts_after(&posts, &connection), "t3_23");
	}

	#[test]
	fn test_user_sort_rejects_top_and_controversial() {
		assert_eq!(user_sort(""), Ok("new".to_string()));
		assert_eq!(user_sort("HOT"), Ok("HOT".to_string()));
		assert!(user_sort("top").unwrap_err().contains("hot and new"));
		assert!(user_sort("controversial").unwrap_err().contains("not available"));
	}

	#[test]
	fn test_overview_merges_posts_and_comments_by_date() {
		let item = |name: &str, created: f64| json_value!({"kind": "t3", "data": {"name": name, "created_utc": created}});
		let (children, after) = merge_by_date(
			vec![item("t3_new", 300.0), item("t3_mid", 100.0)],
			"t3_mid",
			vec![item("t1_a", 200.0), item("t1_b", 50.0)],
			"t1_b",
		);
		let names: Vec<&str> = children.iter().filter_map(|item| item["data"]["name"].as_str()).collect();
		assert_eq!(names, ["t3_new", "t1_a", "t3_mid"]);
		assert!(after.starts_with("posts:t3_mid,comments:"));
	}

	#[test]
	fn test_combined_feed_interleaves_real_dates() {
		let node = |id: &str, created: &str| {
			convert(&json_value!({
				"id": id,
				"title": id,
				"subreddit": {"name": "rust"},
				"authorInfo": {"name": "ada"},
				"isSelfPost": true,
				"permalink": "/r/rust/comments/x/t/",
				"url": "https://www.reddit.com/r/rust/comments/x/t/",
				"domain": "self.rust",
				"score": 1,
				"commentCount": 0,
				"createdAt": created,
				"awardings": [],
				"authorFlair": {},
				"flair": {}
			}))
		};
		let feed = Feed::new("/r/rust+golang/new.json", "new", &FEED_SORTS).unwrap();
		let (posts, _) = feed.merge(
			vec![
				("rust".into(), vec![node("t3_r1", "2024-06-03T00:00:00+0000"), node("t3_r2", "2024-06-01T00:00:00+0000")]),
				("golang".into(), vec![node("t3_g1", "2024-06-02T00:00:00+0000")]),
			],
			HashMap::new(),
		);
		assert_eq!(posts[0]["data"]["name"], "t3_r1");
		assert_eq!(posts[1]["data"]["name"], "t3_g1");
		assert_eq!(posts[2]["data"]["name"], "t3_r2");
	}

	#[test]
	fn test_wiki_renders_without_valid_status_and_hides_missing_pages() {
		let readable = json_value!({
			"status": "MAY_REVISE",
			"content": {"markdown": "hello wiki", "richtext": null}
		});
		let html = wiki_html(&readable).unwrap();
		assert!(html.contains("hello wiki"));
		assert!(wiki_html(&json_value!({"status": "PAGE_NOT_FOUND", "content": {"markdown": ""}})).is_none());
		let info = json_value!({
			"wiki": {
				"page": {"status": "PAGE_NOT_FOUND"},
				"indexPage": {"status": "PUBLIC", "content": {"markdown": "index body"}}
			}
		});
		assert!(wiki_html(wiki_page(&info)).unwrap().contains("index body"));
	}

	fn assert_local_media(value: &Value) {
		match value {
			Value::String(url) if url.starts_with("http://") || url.starts_with("https://") => panic!("external media url {url}"),
			Value::Array(items) => items.iter().for_each(assert_local_media),
			Value::Object(map) => map.values().for_each(assert_local_media),
			_ => {}
		}
	}

	#[test]
	fn test_video_prefers_proxied_dash_and_packaged_media_is_local() {
		let base = |media| {
			json_value!({
				"id": "t3_vid",
				"title": "clip",
				"isSelfPost": false,
				"domain": "v.redd.it",
				"authorInfo": {"name": "ada"},
				"subreddit": {"name": "videos"},
				"permalink": "/r/videos/comments/vid/clip/",
				"createdAt": "2024-06-01T00:00:00+0000",
				"thumbnail": {"url": "https://external-preview.redd.it/thumb.jpg?auto=webp&s=1", "dimensions": {"width": 140, "height": 140}},
				"awardings": [],
				"authorFlair": {},
				"flair": {},
				"media": media
			})
		};
		let with_dash = base(json_value!({
			"still": {"source": {"url": "https://preview.redd.it/poster.jpg", "dimensions": {"width": 1280, "height": 720}}},
			"streaming": {
				"dashUrl": "https://v.redd.it/abc/DASHPlaylist.mpd",
				"hlsUrl": "https://v.redd.it/abc/HLSPlaylist.m3u8?f=hd",
				"isGif": false,
				"dimensions": {"width": 1280, "height": 720}
			},
			"packagedMedia": {"muxedMp4s": {"recommended": {"url": "https://packaged-media.redd.it/abc/muxed.mp4?e=1&s=sig"}}}
		}));
		let post = convert(&with_dash);
		let video = &post["data"]["secure_media"]["reddit_video"];
		assert_eq!(video["fallback_url"], "/vid/abc/720.mp4");
		assert_eq!(video["hls_url"], "/hls/abc/HLSPlaylist.m3u8?f=hd");
		assert_eq!(post["data"]["thumbnail"], "/preview/external-pre/thumb.jpg?auto=webp&s=1");
		assert_eq!(post["data"]["preview"]["images"][0]["source"]["url"], "/preview/pre/poster.jpg");
		assert_local_media(&post["data"]["secure_media"]);
		assert_local_media(&post["data"]["preview"]);
		assert_local_media(&post["data"]["thumbnail"]);

		let only_packaged = base(json_value!({
			"packagedMedia": {"muxedMp4s": {"recommended": {"url": "https://packaged-media.redd.it/m/abc/muxed.mp4?e=1700000000&s=sig"}}}
		}));
		let post = convert(&only_packaged);
		assert_eq!(post["data"]["secure_media"]["reddit_video"]["fallback_url"], "/pkg/m/abc/muxed.mp4?e=1700000000&s=sig");
	}

	#[test]
	fn test_about_shape() {
		let sub = json_value!({
			"__typename": "Subreddit",
			"name": "rust",
			"title": "The Rust Programming Language",
			"publicDescriptionText": "A language",
			"description": {"markdown": "sidebar"},
			"subscribersCount": 12.0,
			"communityStats": {"weeklyActiveUsersCount": 3},
			"styles": {"icon": "https://i.redd.it/icon.png", "legacyIcon": {"url": ""}},
			"wiki": {"indexPage": {"status": "VALID"}},
			"isNsfw": false
		});
		let about = subreddit_value(&sub);
		assert_eq!(about["kind"], "t5");
		assert_eq!(about["data"]["display_name"], "rust");
		assert_eq!(about["data"]["title"], "The Rust Programming Language");
		assert!(about["data"]["description_html"].as_str().unwrap().contains("sidebar"));
		assert_eq!(about["data"]["subscribers"], 12);
		assert_eq!(about["data"]["wiki_enabled"], true);
	}

	#[test]
	fn test_combined_home_feed_merges_by_new() {
		let feed = Feed::new("/r/aaa+bbb/new.json", "new", &FEED_SORTS).unwrap();
		let post = |name: &str, created: f64| json_value!({"kind": "t3", "data": {"name": name, "score": 1, "created_utc": created}});
		let (posts, after) = feed.merge(
			vec![("aaa".into(), vec![post("t3_a1", 30.0), post("t3_a2", 10.0)]), ("bbb".into(), vec![post("t3_b1", 20.0)])],
			HashMap::new(),
		);
		assert_eq!(posts[0]["data"]["name"], "t3_a1");
		assert_eq!(posts[1]["data"]["name"], "t3_b1");
		assert!(after.contains("aaa:t3_"));
		assert!(after.contains("bbb:t3_"));
	}
}
