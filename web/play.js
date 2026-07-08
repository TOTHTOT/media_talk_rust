(function () {
  "use strict";

  async function fetchJson(url) {
    const r = await fetch(url);
    if (!r.ok) throw new Error(`HTTP ${r.status} for ${url}`);
    return r.json();
  }

  async function refreshDevices() {
    const list = document.getElementById("device-list");
    list.innerHTML = '<div class="empty">加载中…</div>';
    let devices = [];
    try {
      devices = await fetchJson("/api/devices");
    } catch (e) {
      list.innerHTML = `<div class="empty">无法加载设备列表: ${e.message}</div>`;
      return;
    }
    if (!devices || devices.length === 0) {
      list.innerHTML = '<div class="empty">未发现摄像头</div>';
      return;
    }
    list.innerHTML = "";
    for (const d of devices) {
      const div = document.createElement("div");
      div.className = "device";
      div.innerHTML = `<div><strong>${d.address}</strong></div><div class="profile">${d.profiles.length} 个 profile / 鉴权: ${d.auth_status}</div>`;
      div.onclick = () => selectDevice(d, div);
      list.appendChild(div);
      for (const p of d.profiles || []) {
        const pdiv = document.createElement("div");
        pdiv.className = "profile device";
        pdiv.textContent = `${p.profile_id} · ${p.codec} ${p.width}x${p.height} @ ${p.fps}`;
        pdiv.onclick = (ev) => {
          ev.stopPropagation();
          selectProfile(d, p, pdiv);
        };
        list.appendChild(pdiv);
      }
    }
  }

  function markActive(el) {
    for (const e of document.querySelectorAll(".device.active")) e.classList.remove("active");
    el.classList.add("active");
  }

  async function selectDevice(d, el) {
    markActive(el);
    console.log("selected device", d);
  }

  async function selectProfile(d, p, el) {
    markActive(el);
    let resp;
    try {
      resp = await fetchJson("/api/sessions", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ device_id: d.id, profile_id: p.profile_id }),
      });
    } catch (e) {
      console.error("create session failed", e);
      return;
    }
    openStream(resp.session_id);
  }

  function openStream(sessionId) {
    const video = document.getElementById("player");
    video.src = "";
    const ms = new MediaSource();
    video.src = URL.createObjectURL(ms);
    const sb = ms.addSourceBuffer('video/mp4; codecs="avc1.42E01E"');
    const ws = new WebSocket((location.protocol === "https:" ? "wss://" : "ws://") + location.host + "/ws/" + sessionId);
    ws.binaryType = "arraybuffer";
    let queue = [];
    let feeding = false;
    function feed() {
      if (feeding || queue.length === 0) return;
      feeding = true;
      const next = queue.shift();
      sb.appendBuffer(next);
    }
    sb.addEventListener("updateend", () => { feeding = false; feed(); });
    sb.addEventListener("error", (e) => console.error("source buffer error", e));
    ws.onmessage = (ev) => {
      if (ev.data instanceof ArrayBuffer) {
        queue.push(ev.data);
        if (ms.readyState === "open") feed();
      }
    };
    ws.onerror = (e) => console.error("ws error", e);
    ms.addEventListener("sourceopen", () => {
      feed();
    });
  }

  refreshDevices();
})();