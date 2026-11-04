(function (root) {
  "use strict";

  function lower(value) {
    return typeof value === "string" ? value.toLowerCase() : "";
  }

  var MOBILE = /android|iphone|ipad|ipod|cros|playstation|xbox/;

  function detectOs(info) {
    var hinted = lower(info.uaPlatform);
    if (hinted) {
      if (hinted === "macos") return "darwin";
      if (hinted === "windows") return "windows";
      if (hinted === "linux") return "linux";
      return null;
    }
    var ua = lower(info.userAgent);
    if (MOBILE.test(ua)) return null;
    var platform = lower(info.platform);
    if (/^mac/.test(platform)) {
      // iPadOS in desktop mode reports MacIntel; only it has a touch screen.
      return (info.maxTouchPoints || 0) > 1 ? null : "darwin";
    }
    if (/^win/.test(platform)) return "windows";
    if (/linux/.test(platform)) return "linux";
    if (/macintosh|mac os x/.test(ua)) return "darwin";
    if (/windows/.test(ua)) return "windows";
    if (/linux|x11/.test(ua)) return "linux";
    return null;
  }

  function detectArch(os, info) {
    var hinted = lower(info.uaArch);
    if (hinted) {
      if (info.uaBitness === "32") return null;
      if (hinted === "arm") return "aarch64";
      if (hinted === "x86") return "x86_64";
      return null;
    }
    if (os === "darwin") {
      // Every Mac browser freezes its UA at "Intel Mac OS X" and its platform
      // at "MacIntel", so only the GPU tells Apple Silicon from Intel.
      var gpu = lower(info.webglRenderer);
      if (/apple (m\d|gpu)/.test(gpu)) return "aarch64";
      if (/intel|amd|radeon|nvidia/.test(gpu)) return "x86_64";
      return null;
    }
    var text = lower(info.platform) + " " + lower(info.userAgent);
    if (/aarch64|arm64|armv8/.test(text)) return "aarch64";
    if (/x86_64|x86-64|amd64|win64|wow64|\bx64\b/.test(text)) return "x86_64";
    return null;
  }

  var OS_LABELS = { darwin: "macOS", windows: "Windows", linux: "Linux" };
  var ARCH_LABELS = {
    darwin: { aarch64: "Apple Silicon (aarch64)", x86_64: "Intel (x86_64)" },
    windows: { aarch64: "ARM64 (aarch64)", x86_64: "x64 (x86_64)" },
    linux: { aarch64: "ARM64 (aarch64)", x86_64: "x64 (x86_64)" }
  };

  function detect(info) {
    var os = detectOs(info || {});
    var arch = os ? detectArch(os, info) : null;
    var label = null;
    if (os) {
      label = OS_LABELS[os] + " · " + (arch ? ARCH_LABELS[os][arch] : "architecture unknown");
    }
    return { os: os, arch: arch, recommended: Boolean(os && arch), label: label };
  }

  function assetFor(result, rows) {
    if (!result || !result.recommended) return null;
    for (var i = 0; i < rows.length; i++) {
      if (rows[i].os === result.os && rows[i].arch === result.arch) return rows[i];
    }
    return null;
  }

  function webglRenderer(doc) {
    try {
      var gl = doc.createElement("canvas").getContext("webgl");
      var ext = gl && gl.getExtension("WEBGL_debug_renderer_info");
      return ext ? gl.getParameter(ext.UNMASKED_RENDERER_WEBGL) : "";
    } catch (_error) {
      return "";
    }
  }

  function gather(nav, doc) {
    var hints = nav.userAgentData;
    var info = {
      uaPlatform: hints && hints.platform,
      platform: nav.platform,
      userAgent: nav.userAgent,
      maxTouchPoints: nav.maxTouchPoints,
      webglRenderer: webglRenderer(doc)
    };
    if (!hints || !hints.getHighEntropyValues) return Promise.resolve(info);
    return hints.getHighEntropyValues(["architecture", "bitness"]).then(
      function (values) {
        info.uaArch = values.architecture;
        info.uaBitness = values.bitness;
        return info;
      },
      function () { return info; }
    );
  }

  function show(doc, row, note) {
    var button = doc.getElementById("recommended");
    var detected = doc.getElementById("detected");
    detected.textContent = note;
    if (!row) {
      button.hidden = true;
      button.removeAttribute("href");
      return;
    }
    button.href = row.url;
    button.textContent = "Download " + row.filename;
    button.hidden = false;
  }

  function start(nav, doc) {
    var select = doc.getElementById("platform-select");
    var rows = [];
    for (var i = 0; i < select.options.length; i++) {
      var option = select.options[i];
      if (!option.value) continue;
      rows.push({ os: option.dataset.os, arch: option.dataset.arch, url: option.value,
        filename: option.dataset.filename, label: option.textContent });
    }
    select.addEventListener("change", function () {
      var row = rows.filter(function (item) { return item.url === select.value; })[0];
      show(doc, row || null, row ? "Selected: " + row.label : "Choose your platform below.");
    });
    gather(nav, doc).then(function (info) {
      var result = detect(info);
      var row = assetFor(result, rows);
      if (row) {
        select.value = row.url;
        show(doc, row, "Detected: " + result.label + ". Not right? Pick another below.");
      } else if (result.label) {
        show(doc, null, "Detected: " + result.label + ". Choose your download below.");
      } else {
        show(doc, null, "Could not detect a supported system. Choose your download below.");
      }
    });
    var copy = doc.getElementById("copy-one-liner");
    copy.addEventListener("click", function () {
      var text = doc.getElementById("one-liner").textContent;
      var done = function () { copy.textContent = "Copied"; };
      if (nav.clipboard && nav.clipboard.writeText) {
        nav.clipboard.writeText(text).then(done, function () { fallbackCopy(doc, text, done); });
      } else {
        fallbackCopy(doc, text, done);
      }
    });
  }

  function fallbackCopy(doc, text, done) {
    var area = doc.createElement("textarea");
    area.value = text;
    doc.body.appendChild(area);
    area.select();
    try { if (doc.execCommand("copy")) done(); } catch (_error) { /* leave text selectable */ }
    doc.body.removeChild(area);
  }

  var api = { detect: detect, assetFor: assetFor };
  if (typeof module === "object" && module.exports) {
    module.exports = api;
  } else if (root && root.document) {
    root.cludDownload = api;
    if (root.document.readyState === "loading") {
      root.document.addEventListener("DOMContentLoaded", function () { start(root.navigator, root.document); });
    } else {
      start(root.navigator, root.document);
    }
  }
})(typeof window === "undefined" ? null : window);
