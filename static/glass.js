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
		function applyTheme() {
			var theme = document.getElementById("theme");
			if (!theme) return;
			var names = ["indigo", "abismo", "grafite", "floresta", "aurora", "dark", "light", "dracula", "system", "black", "nord"];
			for (var n = 0; n < theme.options.length; n++) names.push(theme.options[n].value);
			names.forEach(function (name) { document.body.classList.remove(name); });
			if (theme.value && theme.value !== "system") document.body.classList.add(theme.value);
		}
		function setControl(el, value) {
			if (el.type === "checkbox" || el.type === "radio") el.checked = value === "on";
			else el.value = value;
		}
		var restoreBtn = document.getElementById("restore_defaults");
		var cancelBtn = document.getElementById("cancel_changes");
		if (restoreBtn) restoreBtn.addEventListener("click", function () {
			var fields = settingsForm.querySelectorAll("[data-default]");
			for (var i = 0; i < fields.length; i++) setControl(fields[i], fields[i].getAttribute("data-default"));
			applyTheme();
			syncSave();
		});
		if (cancelBtn) cancelBtn.addEventListener("click", function () {
			settingsForm.reset();
			applyTheme();
			syncSave();
		});
		settingsForm.addEventListener("submit", function () {
			try { sessionStorage.setItem("redlib-salvo", "1"); } catch (e) {}
		});
		settingsForm.addEventListener("input", syncSave);
		settingsForm.addEventListener("change", syncSave);
	}

	try {
		if (sessionStorage.getItem("redlib-salvo") === "1") {
			sessionStorage.removeItem("redlib-salvo");
			var toast = document.createElement("div");
			toast.className = "salvo-toast";
			toast.setAttribute("role", "status");
			toast.setAttribute("aria-live", "polite");
			toast.innerHTML = "<strong>Salvo</strong><span>Preferências atualizadas</span>";
			document.body.appendChild(toast);
			var reduceMotion = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
			setTimeout(function () {
				if (reduceMotion) { toast.remove(); return; }
				toast.classList.add("hide");
				setTimeout(function () { toast.remove(); }, 200);
			}, 3000);
		}
	} catch (e) {}

	var skelTimer = 0;
	function hideSkeleton() {
		clearTimeout(skelTimer);
		skelTimer = 0;
		var sk = document.getElementById("sx-skel");
		if (sk) sk.remove();
		var hosts = document.querySelectorAll(".is-loading");
		for (var h = 0; h < hosts.length; h++) hosts[h].classList.remove("is-loading");
	}
	function viewerImageLink(a) {
		if (!a || a.hasAttribute("download") || a.target === "_blank") return false;
		if (a.classList.contains("post_media_image")) return true;
		var href = a.getAttribute("href") || "";
		return /^\/(img|preview|thumb)\//.test(href) && !!a.querySelector("img");
	}
	function showSkeleton() {
		if (document.getElementById("sx-skel") || document.body.classList.contains("image-open")) return;
		var host = document.getElementById("posts") || document.getElementById("column_one") || document.querySelector("main");
		if (!host) return;
		host.classList.add("is-loading");
		var el = document.createElement("div");
		el.id = "sx-skel";
		el.setAttribute("role", "status");
		el.setAttribute("aria-live", "polite");
		el.innerHTML = '<p class="sr-only">Loading</p>'
			+ '<div class="sx-skel-card"><i class="sm"></i><i class="lg"></i><i></i><i class="media"></i></div>'
			+ '<div class="sx-skel-card"><i class="sm"></i><i class="lg"></i><i class="media"></i></div>';
		host.appendChild(el);
		clearTimeout(skelTimer);
		skelTimer = setTimeout(hideSkeleton, 8000);
	}
	document.addEventListener("click", function (e) {
		if (e.defaultPrevented || e.button !== 0) return;
		if (e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
		if (document.body.classList.contains("image-open")) return;
		var node = e.target;
		var a = node && node.closest ? node.closest("a") : null;
		if (!a || !a.getAttribute("href") || viewerImageLink(a)) return;
		if (a.target && a.target !== "_self") return;
		if (a.hasAttribute("download")) return;
		var url;
		try { url = new URL(a.href, window.location.href); } catch (err) { return; }
		if (url.origin !== window.location.origin) return;
		if (url.pathname === window.location.pathname && url.search === window.location.search) return;
		showSkeleton();
	});
	window.addEventListener("pageshow", function () { hideSkeleton(); });
	window.addEventListener("popstate", hideSkeleton);
	window.redlibHideSkeleton = hideSkeleton;

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

	document.addEventListener("play", function (e) {
		var v = e.target;
		if (!v || v.tagName !== "VIDEO") return;
		var all = document.querySelectorAll("video");
		for (var i = 0; i < all.length; i++) {
			if (all[i] !== v && !all[i].paused) all[i].pause();
		}
	}, true);

	initImageViewer();
	initBackSwipe();

	var sel = document.getElementById("theme");
	if (sel) sel.addEventListener("change", function () {
		var names = ["indigo", "abismo", "grafite", "floresta", "aurora", "dark", "light", "dracula", "system", "black", "nord"];
		for (var i = 0; i < sel.options.length; i++) names.push(sel.options[i].value);
		names.forEach(function (name) { document.body.classList.remove(name); });
		if (sel.value && sel.value !== "system") document.body.classList.add(sel.value);
	});
})();

/* Edge back-swipe for the installed app (no browser back button in fullscreen).
   Threshold is the smaller of 80px and 30% of the viewport: a 390px phone triggers
   at 80px, and a narrower window never asks for a drag longer than a short thumb move. */
/* Image posts link to the proxied file (/img, /preview). That response is not
   an HTML page, so glass.js never loads and the edge swipe cannot run.
   Open those links in an overlay on this page instead. The href stays put
   for no-JS, long-press, and open-in-new-tab. */
function initImageViewer() {
	var overlay = null;
	var pushed = false;
	var reduce = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;

	function removeOverlay() {
		if (!overlay) return;
		overlay.remove();
		overlay = null;
		document.body.classList.remove("image-open");
	}
	function close() {
		if (pushed) {
			pushed = false;
			history.back();
			return;
		}
		removeOverlay();
	}
	function open(href, alt) {
		if (overlay) return;
		overlay = document.createElement("div");
		overlay.className = "image-viewer" + (reduce ? " reduced" : "");
		overlay.setAttribute("role", "dialog");
		overlay.setAttribute("aria-modal", "true");
		overlay.setAttribute("aria-label", alt || "Image");
		overlay.innerHTML = '<img alt="">';
		var img = overlay.querySelector("img");
		img.alt = alt || "Image";
		img.src = href;
		document.body.appendChild(overlay);
		document.body.classList.add("image-open");
		try {
			history.pushState({ redlibImage: 1 }, "");
			pushed = true;
		} catch (err) {
			pushed = false;
		}
	}
	function imageLink(node) {
		var a = node && node.closest ? node.closest("a") : null;
		if (!a || a.hasAttribute("download") || a.target === "_blank") return null;
		if (a.classList.contains("post_media_image")) return a;
		var href = a.getAttribute("href") || "";
		if (/^\/(img|preview|thumb)\//.test(href) && a.querySelector("img")) return a;
		return null;
	}
	document.addEventListener("click", function (e) {
		if (e.defaultPrevented || e.button !== 0) return;
		if (e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
		var a = imageLink(e.target);
		if (!a) return;
		e.preventDefault();
		var pic = a.querySelector("img");
		open(a.href, pic && pic.alt);
	});
	window.addEventListener("popstate", function () {
		pushed = false;
		removeOverlay();
		if (window.redlibHideSkeleton) window.redlibHideSkeleton();
	});
	document.addEventListener("keydown", function (e) {
		if (e.key === "Escape" && overlay) close();
	});
}

function initBackSwipe() {
	if (!window.matchMedia) return;
	var coarse = window.matchMedia("(pointer: coarse)");
	var narrow = window.matchMedia("(max-width: 899px)");
	var reduce = window.matchMedia("(prefers-reduced-motion: reduce)");
	function enabled() { return coarse.matches || narrow.matches; }
	function threshold() { return Math.min(80, window.innerWidth * 0.3); }

	var el = document.createElement("div");
	el.className = "swipe-back";
	el.setAttribute("aria-hidden", "true");
	el.innerHTML = '<div class="swipe-back-disc"><span class="swipe-back-ring"></span><svg class="swipe-back-arrow" viewBox="0 0 24 24"><path d="M14 6 8 12l6 6"/></svg></div>';
	document.body.appendChild(el);
	var disc = el.querySelector(".swipe-back-disc");

	var active = null;
	var raf = 0;
	var state = { x: -64, p: 0 };

	function paint() {
		raf = 0;
		el.style.transform = "translate3d(" + state.x + "px,-50%,0)";
		el.style.opacity = state.p <= 0 ? "0" : String(Math.min(1, 0.4 + state.p * 0.6));
		el.style.setProperty("--swipe-p", String(state.p));
		if (!reduce.matches) disc.style.transform = "scale(" + (0.72 + state.p * 0.28) + ")";
	}
	function schedule() {
		if (!raf) raf = requestAnimationFrame(paint);
	}
	function blocked(start) {
		var node = start && start.nodeType === 1 ? start : start && start.parentElement;
		while (node && node !== document.documentElement) {
			if (node.classList && (node.classList.contains("gallery-track") || node.classList.contains("gallery-card") || node.classList.contains("gallery-slide"))) return true;
			var tag = node.tagName;
			if (tag === "PRE" || tag === "CODE" || tag === "TEXTAREA" || tag === "INPUT" || tag === "SELECT") return true;
			if (node.id === "sort_options" || node.id === "listing_options") return true;
			var ox = window.getComputedStyle(node).overflowX;
			if ((ox === "auto" || ox === "scroll" || ox === "overlay") && node.scrollWidth > node.clientWidth + 1) return true;
			node = node.parentElement;
		}
		return false;
	}
	function goBack() {
		var viewer = document.querySelector(".image-viewer");
		if (viewer) {
			if (history.state && history.state.redlibImage) window.history.back();
			else {
				viewer.remove();
				document.body.classList.remove("image-open");
			}
			return;
		}
		if (window.history.length > 1) window.history.back();
		else window.location.href = "/";
	}

	document.addEventListener("touchstart", function (e) {
		if (!enabled() || !e.touches || e.touches.length !== 1) return;
		if (window.visualViewport && window.visualViewport.scale > 1.01) return;
		var t = e.touches[0];
		if (t.clientX > 30) return;
		if (blocked(e.target)) return;
		active = { id: t.identifier, x0: t.clientX, y0: t.clientY, dx: 0, dy: 0, decided: false, aborted: false };
		el.classList.remove("cancel", "pulse", "demo");
		state.p = 0;
		state.x = 0;
		schedule();
	}, { passive: true });

	document.addEventListener("touchmove", function (e) {
		if (!active || !e.touches) return;
		var t = null;
		for (var i = 0; i < e.touches.length; i++) if (e.touches[i].identifier === active.id) t = e.touches[i];
		if (!t) return;
		var dx = t.clientX - active.x0;
		var dy = t.clientY - active.y0;
		if (!active.decided && (Math.abs(dx) > 10 || Math.abs(dy) > 10)) {
			active.decided = true;
			if (Math.abs(dy) > Math.abs(dx)) {
				active.aborted = true;
				state.p = 0;
				state.x = -64;
				schedule();
				return;
			}
		}
		if (active.aborted || dx <= 0) return;
		var limit = threshold();
		var travel = Math.min(dx, limit);
		state.p = Math.min(1, dx / limit);
		state.x = travel * 0.9;
		active.dx = dx;
		schedule();
	}, { passive: true });

	function finish(e) {
		if (!active) return;
		var aborted = active.aborted;
		var dx = active.dx;
		var limit = threshold();
		var trigger = !aborted && dx >= limit;
		active = null;
		if (!trigger) {
			el.classList.add("cancel");
			state.p = 0;
			state.x = -64;
			schedule();
			return;
		}
		state.p = 1;
		schedule();
		el.classList.add("pulse");
		goBack();
	}
	document.addEventListener("touchend", finish, { passive: true });
	document.addEventListener("touchcancel", function () {
		if (!active) return;
		active.aborted = true;
		active.dx = 0;
		finish();
	}, { passive: true });
}
