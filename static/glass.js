(function () {
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
			feeds.addEventListener("toggle", function () {
				if (!feeds.open) return;
				if (!reduce.matches) nav.classList.add("nav-instant");
				show();
				anchor = window.scrollY || 0;
				requestAnimationFrame(function () { nav.classList.remove("nav-instant"); });
			});
		}
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
