// VMherd website: progressive enhancement only (the pages work without it).
// 1. App Store badge fallback, 2. click-to-zoom lightbox, 3. the hero's broadcast animation.
(function () {
  "use strict";

  var reduced = window.matchMedia("(prefers-reduced-motion: reduce)");

  // ---------- 1. badge: show a text button if the official badge image is missing ----------
  document.querySelectorAll(".appstore img").forEach(function (img) {
    var missing = function () {
      img.parentNode.classList.add("no-badge");
    };
    if (img.complete && img.naturalWidth === 0) missing();
    else img.addEventListener("error", missing);
  });

  // ---------- 2. lightbox for screenshots ----------
  var shots = Array.prototype.slice.call(document.querySelectorAll("a.shot"));
  if (shots.length && typeof HTMLDialogElement === "function") {
    var dlg = document.createElement("dialog");
    dlg.className = "lightbox";
    dlg.setAttribute("aria-label", "Screenshot");
    dlg.innerHTML =
      '<img alt="">' +
      '<div class="lb-bar"><p></p>' +
      '<button type="button" class="btn" data-go="-1" aria-label="Previous screenshot">&larr;</button>' +
      '<button type="button" class="btn" data-go="1" aria-label="Next screenshot">&rarr;</button>' +
      '<button type="button" class="btn" data-close>Close</button></div>';
    document.body.appendChild(dlg);
    var big = dlg.querySelector("img");
    var cap = dlg.querySelector("p");
    var current = 0;
    var opener = null;

    var show = function (i) {
      current = (i + shots.length) % shots.length;
      var a = shots[current];
      var img = a.querySelector("img");
      big.src = (img && img.currentSrc) || a.href;
      big.alt = img ? img.alt : "";
      var fig = a.closest("figure");
      var fc = fig && fig.querySelector("figcaption");
      cap.textContent = fc ? fc.textContent : big.alt;
    };
    shots.forEach(function (a, i) {
      a.addEventListener("click", function (e) {
        if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return; // new tab etc.
        e.preventDefault();
        opener = a;
        show(i);
        dlg.showModal();
        dlg.querySelector("[data-close]").focus();
      });
    });
    dlg.addEventListener("click", function (e) {
      var t = e.target;
      if (t === dlg || t.hasAttribute("data-close")) dlg.close();
      else if (t.hasAttribute("data-go")) show(current + Number(t.getAttribute("data-go")));
      else if (t === big) dlg.close();
    });
    dlg.addEventListener("keydown", function (e) {
      if (e.key === "ArrowRight") show(current + 1);
      else if (e.key === "ArrowLeft") show(current - 1);
    });
    dlg.addEventListener("close", function () {
      if (opener) opener.focus();
    });
  }

  // ---------- 3. hero: one keystroke stream fanning out to many consoles ----------
  var herd = document.querySelector(".herd");
  if (!herd) return;
  var bar = herd.querySelector(".herd-bar");
  var typed = bar.querySelector(".typed");
  var promptEl = bar.querySelector(".prompt");
  var mode = herd.querySelector(".mode");
  var svg = herd.querySelector(".herd-wires");
  var tiles = Array.prototype.slice.call(herd.querySelectorAll(".tile"));
  var NS = "http://www.w3.org/2000/svg";

  // each tile: its VM's name + address (documentation ranges only)
  var vms = tiles.map(function (t) {
    return { name: t.getAttribute("data-name"), ip: t.getAttribute("data-ip"), screen: t.querySelector(".tile-screen") };
  });
  var wires = [];
  var lit = []; // indexes of the tiles whose wire is lit

  var visible = function (el) {
    return el.offsetParent !== null;
  };

  // wires: from the bottom of the broadcast bar to the top of every visible tile
  var drawWires = function () {
    var box = herd.getBoundingClientRect();
    var b = bar.getBoundingClientRect();
    svg.setAttribute("viewBox", "0 0 " + box.width + " " + box.height);
    while (svg.firstChild) svg.removeChild(svg.firstChild);
    wires = tiles.map(function (t) {
      if (!visible(t)) return null;
      var r = t.getBoundingClientRect();
      var x1 = b.left - box.left + b.width / 2;
      var y1 = b.bottom - box.top;
      var x2 = r.left - box.left + r.width / 2;
      var y2 = r.top - box.top;
      var d = "M" + x1 + " " + y1 + " C" + x1 + " " + (y1 + (y2 - y1) * 0.7) + " " + x2 + " " + (y2 - (y2 - y1) * 0.6) + " " + x2 + " " + y2;
      var base = document.createElementNS(NS, "path");
      base.setAttribute("d", d);
      var pulse = document.createElementNS(NS, "path");
      pulse.setAttribute("d", d);
      pulse.setAttribute("class", "pulse");
      svg.appendChild(base);
      svg.appendChild(pulse);
      return { base: base, pulse: pulse };
    });
    setWires(lit);
  };

  var fire = function (targets) {
    if (reduced.matches) return;
    targets.forEach(function (i) {
      var w = wires[i];
      if (!w) return;
      w.pulse.classList.remove("go");
      void w.pulse.getBoundingClientRect(); // restart the animation
      w.pulse.classList.add("go");
    });
  };

  var setWires = function (targets) {
    lit = targets;
    wires.forEach(function (w, i) {
      if (!w) return;
      var on = targets.indexOf(i) >= 0;
      w.base.classList.toggle("off", !on);
      w.pulse.classList.toggle("off", !on);
    });
  };

  // a console's text as lines; the last line is the one being typed into
  var screens = vms.map(function () {
    return [];
  });
  var render = function (i, caret) {
    var el = vms[i].screen;
    el.textContent = "";
    screens[i].forEach(function (ln, n) {
      var span = document.createElement("span");
      span.className = ln.c || "";
      span.textContent = ln.t;
      el.appendChild(span);
      if (n < screens[i].length - 1) el.appendChild(document.createTextNode("\n"));
    });
    if (caret) {
      var c = document.createElement("span");
      c.className = "caret";
      el.appendChild(c);
    }
    // keep the newest lines in view, like a terminal scrolling up
    if (el.scrollHeight > el.clientHeight + 1 && screens[i].length > 1) {
      screens[i].shift();
      render(i, caret);
    }
  };
  var prompt = function (i) {
    return "root@" + vms[i].name + ":~# ";
  };

  var all = vms.map(function (_, i) {
    return i;
  });
  var nosync = 4; // this tile's sync toggle is off: broadcast skips it
  var synced = all.filter(function (i) {
    return i !== nosync;
  });
  tiles[nosync].classList.add("nosync");

  var setMode = function (kind, label, promptText) {
    herd.classList.toggle("is-type", kind === "type");
    bar.className = "herd-bar " + (kind === "air" ? "" : kind);
    svg.setAttribute("class", "herd-wires " + (kind === "air" ? "" : kind));
    mode.className = "mode " + (kind === "air" ? "" : kind);
    mode.textContent = label;
    promptEl.textContent = promptText;
    typed.textContent = "";
  };

  var reset = function () {
    all.forEach(function (i) {
      screens[i] = [{ t: vms[i].name + " login: root", c: "m" }, { t: prompt(i), c: "g" }];
      tiles[i].classList.remove("solo");
      tiles[i].classList.toggle("hot", i !== nosync);
      render(i, true);
    });
  };

  // the storyboard: each step is [delay ms, fn]
  var steps = [];
  var at = function (ms, fn) {
    steps.push([ms, fn]);
  };
  var typeInto = function (text, targets, perVm, cps) {
    // perVm(i) returns the text a console receives (placeholders filled in); the bar shows `text`
    var gap = 1000 / (cps || 16);
    var longest = 0;
    targets.forEach(function (i) {
      longest = Math.max(longest, perVm(i).length);
    });
    var n = Math.max(text.length, longest);
    for (var k = 0; k < n; k++) {
      (function (k) {
        at(gap * (0.7 + Math.random() * 0.6), function () {
          if (k < text.length) typed.textContent = text.slice(0, k + 1);
          targets.forEach(function (i) {
            var s = perVm(i);
            if (k < s.length) {
              var last = screens[i][screens[i].length - 1];
              last.t += s[k];
              render(i, true);
            }
          });
          fire(targets);
        });
      })(k);
    }
  };
  var enter = function (targets, output) {
    at(420, function () {
      targets.forEach(function (i) {
        var lines = output ? output(i) : [];
        lines.forEach(function (l) {
          screens[i].push(l);
        });
        screens[i].push({ t: prompt(i), c: "g" });
        render(i, true);
      });
      fire(targets);
      typed.textContent = "";
    });
  };

  var same = function (s) {
    return function () {
      return s;
    };
  };

  // 1. broadcast: the same keys into every synced console
  at(0, function () {
    reset();
    setMode("air", "ON AIR", "● ON AIR → " + synced.length + " consoles › ");
    setWires(synced);
  });
  at(700, function () {});
  var cmd1 = "apt update && apt -y full-upgrade";
  typeInto(cmd1, synced, same(cmd1), 17);
  enter(synced, function () {
    return [{ t: "Reading package lists... Done", c: "m" }, { t: "All packages are up to date.", c: "m" }];
  });
  at(1600, function () {});

  // 2. type text: one line, a different value per VM
  at(10, function () {
    all.forEach(function (i) {
      tiles[i].classList.add("hot");
    });
    setMode("type", "TYPE TEXT", "TYPE › ");
    setWires(all);
  });
  at(600, function () {});
  var tpl = "echo {{name}} {{ipv4}}";
  typeInto(tpl, all, function (i) {
    return "echo " + vms[i].name + " " + vms[i].ip;
  }, 20);
  enter(all, function (i) {
    return [{ t: vms[i].name + " " + vms[i].ip, c: "" }];
  });
  at(1800, function () {});

  // 3. solo: click one console, only that VM gets the keys
  var solo = 1;
  at(10, function () {
    all.forEach(function (i) {
      tiles[i].classList.remove("hot");
    });
    tiles[solo].classList.add("solo");
    setMode("solo", "SOLO", "SOLO → " + vms[solo].name + " › ");
    setWires([solo]);
  });
  at(600, function () {});
  var cmd3 = "systemctl restart nginx";
  typeInto(cmd3, [solo], same(cmd3), 15);
  enter([solo]);
  at(2200, function () {});

  // play the storyboard; paused while off screen or in a background tab
  var idx = 0;
  var timer = null;
  var running = false;
  var tick = function () {
    timer = null;
    if (!running) return;
    var s = steps[idx];
    s[1]();
    idx = (idx + 1) % steps.length;
    timer = setTimeout(tick, steps[idx][0]);
  };
  var play = function () {
    if (running || reduced.matches) return;
    running = true;
    timer = setTimeout(tick, steps[idx][0]);
  };
  var pause = function () {
    running = false;
    if (timer) clearTimeout(timer);
    timer = null;
  };

  // reduced motion: one still frame of the broadcast, fully typed, no pulses
  var still = function () {
    pause();
    idx = 0;
    reset();
    setMode("air", "ON AIR", "● ON AIR → " + synced.length + " consoles › ");
    setWires(synced);
    typed.textContent = cmd1;
    synced.forEach(function (i) {
      screens[i][screens[i].length - 1].t += cmd1;
      render(i, true);
    });
  };

  drawWires();
  var onVisible = true;
  var sync = function () {
    if (reduced.matches) still();
    else if (onVisible && !document.hidden) play();
    else pause();
  };
  if ("IntersectionObserver" in window) {
    new IntersectionObserver(function (entries) {
      onVisible = entries[0].isIntersecting;
      sync();
    }).observe(herd);
  }
  document.addEventListener("visibilitychange", sync);
  if (reduced.addEventListener) reduced.addEventListener("change", sync);
  var rt = null;
  window.addEventListener("resize", function () {
    clearTimeout(rt);
    rt = setTimeout(drawWires, 120);
  });
  if (document.fonts && document.fonts.ready) document.fonts.ready.then(drawWires);
  if (reduced.matches) still();
  else {
    reset();
    sync();
  }
})();
