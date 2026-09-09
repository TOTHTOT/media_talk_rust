(function () {
  "use strict";

  async function fetchJson(url, options) {
    const r = await fetch(url, options);
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

  function selectDevice(d, el) {
    markActive(el);
    console.log("selected device", d);
  }

  // The backend republishes every session on the in-process webrtcsink
  // signalling server (ws://<host>:8443) with meta.name = session id.
  // One GstWebRTCAPI instance is reused across profile switches; only
  // the consumer session is swapped.
  const api = new GstWebRTCAPI({
    meta: { name: "web-" + Date.now() },
    signalingServerUrl: "ws://" + location.hostname + ":8443",
  });

  let current = null; // { sessionId, consumer }

  function stopCurrent() {
    if (!current) return;
    const { sessionId, consumer } = current;
    current = null;
    if (consumer) {
      try { consumer.close(); } catch (e) { /* already closed */ }
    }
    const video = document.getElementById("player");
    video.pause();
    video.srcObject = null;
    // Tell the backend to tear the camera pipeline down.
    fetch(`/api/sessions/${sessionId}`, { method: "DELETE" }).catch(() => {});
  }

  async function selectProfile(d, p, el) {
    markActive(el);
    stopCurrent();
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
    const entry = { sessionId, consumer: null };
    current = entry;

    const attach = (producer) => {
      if (current !== entry || entry.consumer) return;
      if (!producer.meta || producer.meta.name !== sessionId) return;
      const consumer = api.createConsumerSession(producer.id);
      entry.consumer = consumer;
      consumer.addEventListener("streamsChanged", () => {
        if (current !== entry) return;
        const streams = consumer.streams;
        if (streams.length > 0) {
          video.srcObject = streams[0];
          video.play().catch(() => {});
        }
      });
      consumer.addEventListener("error", (e) => console.error("consumer error", e.message, e.error));
      consumer.addEventListener("closed", () => {
        if (entry.consumer === consumer) entry.consumer = null;
      });
      consumer.connect();
    };

    api.registerPeerListener({
      producerAdded: attach,
      producerRemoved: (p) => {
        // Pipeline restart (reconnect) drops the producer; the matching
        // producerAdded re-attaches automatically.
        if (p.meta && p.meta.name === sessionId && entry.consumer) {
          try { entry.consumer.close(); } catch (e) { /* ignore */ }
          entry.consumer = null;
        }
      },
    });
    // The producer may already be registered if the pipeline started fast.
    for (const producer of api.getAvailableProducers()) attach(producer);
  }

  window.addEventListener("beforeunload", stopCurrent);

  refreshDevices();
})();
