(function () {
	document.documentElement.classList.add("js");

	var nav = document.querySelector("nav.tabbar");
	if (nav && window.matchMedia) {
		var mq = window.matchMedia("(max-width: 600px)");
		var reduce = window.matchMedia("(prefers-reduced-motion: reduce)");
		var anchor = window.scrollY || 0;
		var ticking = false;
		var THRESHOLD = 10;
		function menuOpen() {
			var feeds = document.getElementById("feeds");
			return !!(feeds && feeds.open);
		}
		function show() {
			nav.classList.remove("nav-hidden");
		}
		function update() {
			ticking = false;
			if (!mq.matches) {
				show();
				anchor = window.scrollY || 0;
				return;
			}
			var y = window.scrollY || 0;
			var doc = document.documentElement;
			var nearTop = y < 48;
			var atEnd = y + window.innerHeight >= doc.scrollHeight - 8;
			if (nearTop || atEnd || menuOpen()) {
				show();
				anchor = y;
				return;
			}
			var dy = y - anchor;
			if (dy > THRESHOLD) {
				nav.classList.add("nav-hidden");
				anchor = y;
			} else if (dy < -THRESHOLD) {
				show();
				anchor = y;
			}
		}
		window.addEventListener("scroll", function () {
			if (ticking) return;
			ticking = true;
			requestAnimationFrame(update);
		}, { passive: true });
		var feeds = document.getElementById("feeds");
		if (feeds) {
			document.body.classList.toggle("feeds-open", feeds.open);
			feeds.addEventListener("toggle", function () {
				document.body.classList.toggle("feeds-open", feeds.open);
				if (!feeds.open) return;
				if (!reduce.matches) nav.classList.add("nav-instant");
				show();
				anchor = window.scrollY || 0;
				requestAnimationFrame(function () { nav.classList.remove("nav-instant"); });
			});
		}
	}

	var settingsForm = document.querySelector("#settings form");
	var savebar = settingsForm && settingsForm.querySelector(".savebar");
	if (settingsForm && savebar) {
		function snap() {
			var parts = [];
			var fields = settingsForm.querySelectorAll("input, select, textarea");
			for (var i = 0; i < fields.length; i++) {
				var el = fields[i];
				if (el.type === "submit" || el.type === "button" || el.type === "hidden") continue;
				if (el.type === "checkbox" || el.type === "radio") parts.push(el.name + "=" + (el.checked ? "1" : "0"));
				else parts.push(el.name + "=" + el.value);
			}
			return parts.join("&");
		}
		var initial = snap();
		function syncSave() {
			savebar.classList.toggle("is-dirty", snap() !== initial);
		}
		settingsForm.addEventListener("input", syncSave);
		settingsForm.addEventListener("change", syncSave);
	}

	var RECENT_KEY = "redlib-recent-searches";
	function readRecent() {
		try {
			var list = JSON.parse(localStorage.getItem(RECENT_KEY) || "[]");
			return Array.isArray(list) ? list.filter(function (item) { return typeof item === "string" && item.trim(); }).slice(0, 8) : [];
		} catch (e) {
			return [];
		}
	}
	function writeRecent(list) {
		localStorage.setItem(RECENT_KEY, JSON.stringify(list.slice(0, 8)));
	}
	document.addEventListener("submit", function (event) {
		var form = event.target;
		if (!form || form.id !== "search_sort" && form.id !== "searchbox") return;
		var input = form.querySelector("input[name=q]");
		if (!input) return;
		var q = input.value.trim();
		if (!q) return;
		var list = readRecent().filter(function (item) { return item !== q; });
		list.unshift(q);
		writeRecent(list);
	}, true);
	var recentBox = document.getElementById("recent_searches");
	var recentList = document.getElementById("recent_list");
	var emptyNote = document.getElementById("search_empty");
	var home = document.getElementById("search_home");
	function renderRecent() {
		if (!recentBox || !recentList) return;
		var list = readRecent();
		recentList.textContent = "";
		list.forEach(function (q) {
			var link = document.createElement("a");
			link.className = "glass-chip";
			link.href = "/search?q=" + encodeURIComponent(q);
			link.textContent = q;
			recentList.appendChild(link);
		});
		recentBox.hidden = list.length === 0;
		if (emptyNote && home) {
			var hasSubs = home.getAttribute("data-has-subs") === "1";
			emptyNote.hidden = hasSubs || list.length > 0;
		}
	}
	if (recentBox) {
		renderRecent();
		var clearBtn = document.getElementById("clear_recent");
		if (clearBtn) clearBtn.addEventListener("click", function () {
			localStorage.removeItem(RECENT_KEY);
			renderRecent();
		});
	}

	var galleries = document.querySelectorAll(".gallery-card[data-count]");
	for (var g = 0; g < galleries.length; g++) {
		(function (card) {
			var track = card.querySelector(".gallery-track");
			var badge = card.querySelector(".gallery-count");
			var prev = card.querySelector(".gallery-nav.prev");
			var next = card.querySelector(".gallery-nav.next");
			var total = parseInt(card.getAttribute("data-count"), 10) || 1;
			if (!track) return;
			function current() {
				var w = track.clientWidth || 1;
				return Math.max(0, Math.min(total - 1, Math.round(track.scrollLeft / w)));
			}
			var pending = false;
			function sync() {
				pending = false;
				var i = current();
				if (badge) badge.textContent = (i + 1) + "/" + total;
				if (prev) prev.hidden = i <= 0;
				if (next) next.hidden = i >= total - 1;
			}
			track.addEventListener("scroll", function () {
				if (pending) return;
				pending = true;
				requestAnimationFrame(sync);
			}, { passive: true });
			function step(dir) {
				track.scrollBy({ left: dir * (track.clientWidth || 0), behavior: "smooth" });
			}
			if (prev) prev.addEventListener("click", function () { step(-1); });
			if (next) next.addEventListener("click", function () { step(1); });
			sync();
		})(galleries[g]);
	}

	var sel = document.getElementById("theme");
	if (!sel) return;
	var names = ["indigo", "abismo", "grafite", "floresta", "aurora", "dark", "light", "dracula", "system", "black", "nord"];
	for (var i = 0; i < sel.options.length; i++) names.push(sel.options[i].value);
	sel.addEventListener("change", function () {
		names.forEach(function (name) { document.body.classList.remove(name); });
		if (sel.value && sel.value !== "system") document.body.classList.add(sel.value);
	});
})();
