"use strict";

const state = {
  snap: null,
  seq: null,
  showStale: false,
  showClosed: false,
  mailReady: false,
  activity: [],
  next: null,
  appended: false,
  timer: null,
};

const $ = (id) => document.getElementById(id);

function esc(value) {
  return String(value ?? "").replace(/[&<>"']/g, (ch) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  }[ch]));
}

function rel(iso, now) {
  if (!iso) return "never";
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return iso;
  const delta = Math.round((now - t) / 1000);
  const future = delta < 0;
  const abs = Math.abs(delta);
  let text = "just now";
  if (abs >= 45 && abs < 3600) text = Math.round(abs / 60) + "m";
  else if (abs >= 3600 && abs < 86400) text = Math.round(abs / 3600) + "h";
  else if (abs >= 86400) text = Math.round(abs / 86400) + "d";
  else return future ? "moments" : "just now";
  return future ? "in " + text : text + " ago";
}

function until(iso, now) {
  if (!iso) return "";
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return "";
  if (t <= now) return "expired";
  return rel(iso, now);
}

function toast(text) {
  const el = $("toast");
  el.textContent = text;
  el.classList.add("on");
  clearTimeout(toast._t);
  toast._t = setTimeout(() => el.classList.remove("on"), 2800);
}

function setLive(mode, label) {
  const dot = $("live-dot");
  dot.className = "dot " + (mode === "live" ? "live" : mode === "poll" ? "poll" : "down");
  $("live-state").textContent = label;
}

async function getJson(url) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(res.status + " " + url);
  return res.json();
}

async function refresh() {
  const snap = await getJson("/api/dash");
  state.snap = snap;
  state.seq = snap.seq;
  render(snap);
  if (!state.appended) loadActivity(false);
}

function render(snap) {
  const now = Date.now();
  const node = snap.node;
  $("node-meta").innerHTML =
    "<b>" + esc(node.display_name) + "</b><br>" +
    esc(node.role) + " · " + esc(node.fingerprint.slice(0, 16));
  document.title = "Another · " + node.display_name;

  const banner = $("banner");
  if (!snap.access.bodies || !snap.access.actions) {
    banner.hidden = false;
    banner.textContent = snap.access.bodies
      ? "Read-only."
      : "Read-only. Message bodies are omitted because this server requires a token. A tunnel could be in front of loopback. Actions stay off.";
  } else {
    banner.hidden = true;
  }
  $("timeline-note").textContent = snap.access.bodies
    ? "Search the local record by agent, project, kind and time. Bodies of messages written on this node are included."
    : "Search summaries by agent, project, kind and time. Message bodies are not on this server.";

  $("boot").hidden = true;
  $("summary").innerHTML = summaryHtml(snap, now);
  renderAgents(snap, now);
  $("holds-body").innerHTML = holdsHtml(snap, now);
  renderRequests(snap, now);
  $("mail-body").innerHTML = mailHtml(snap, now);
  $("resources-body").innerHTML = resourcesHtml(snap, now);
  $("peers-body").innerHTML = peersHtml(snap, now);
  fillFilters(snap);
  if (location.hash.length > 1) {
    const target = document.querySelector(location.hash);
    if (target) target.scrollIntoView();
  }
}

function countBits(summary) {
  const counts = summary.counts || {};
  const total = Object.values(counts).reduce((sum, n) => sum + n, 0);
  const parts = Object.entries(counts)
    .filter(([, n]) => n > 0)
    .map(([k, n]) => n + " " + k)
    .join(" · ");
  return { total, parts: parts || "quiet" };
}

function summaryHtml(snap, now) {
  const hour = countBits(snap.hour);
  const day = countBits(snap.day);
  const c = snap.counts;
  const expiring = snap.holds.filter((h) => {
    const t = Date.parse(h.expires_at);
    return !Number.isNaN(t) && t - now < 30 * 60 * 1000 && t > now;
  }).length;
  const attention = [];
  if (c.conflicts) attention.push(c.conflicts + " conflicting hold" + (c.conflicts === 1 ? "" : "s"));
  if (c.remote_warnings) attention.push(c.remote_warnings + " cross-node clash" + (c.remote_warnings === 1 ? "" : "es"));
  if (c.requests_stale) attention.push(c.requests_stale + " stale request" + (c.requests_stale === 1 ? "" : "s"));
  if (expiring) attention.push(expiring + " hold" + (expiring === 1 ? "" : "s") + " ending soon");
  const highlights = (snap.hour.highlights || []).slice(-3).reverse();
  return (
    '<div class="tiles">' +
    tile("Last hour", hour.total, hour.parts) +
    tile("Last day", day.total, day.parts) +
    tile("Right now", c.live + " live", c.idle + " idle · " + c.holds + " holds · " + c.resources_live + " leases") +
    tile("Attention", attention.length ? attention.length : "Clear", attention.join(" · ") || "Nothing waiting on you") +
    "</div>" +
    (highlights.length
      ? '<ul class="highlights">' + highlights.map((h) => "<li>" + esc(h) + "</li>").join("") + "</ul>"
      : "")
  );
}

function tile(k, n, s) {
  return '<article class="tile"><div class="k">' + esc(k) + '</div><div class="n">' + esc(n) +
    '</div><div class="s">' + esc(s) + "</div></article>";
}

function renderAgents(snap, now) {
  const stale = snap.agents.filter((a) => a.stale);
  const shown = snap.agents.filter((a) => state.showStale || !a.stale);
  const btn = $("show-stale");
  if (stale.length) {
    btn.hidden = false;
    btn.textContent = state.showStale ? "Hide " + stale.length + " stale" : "Show " + stale.length + " stale";
  } else {
    btn.hidden = true;
  }
  if (!shown.length) {
    $("agents-body").innerHTML = '<p class="empty">No agents yet. <span class="path">another agent register</span> adds the first one.</p>';
    return;
  }
  const groups = new Map();
  for (const agent of shown) {
    const key = agent.presence === "live" ? "Live" : agent.presence === "idle" ? "Idle" : "Away";
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(agent);
  }
  let html = "";
  for (const [name, agents] of groups) {
    html += '<div class="group"><h3>' + esc(name) + "<span>" + agents.length + "</span></h3><div class=\"cards\">";
    for (const agent of agents) {
      const unread = agent.unread ? agent.unread + " unread" : "no unread";
      const where = agent.project || "no project";
      const node = agent.node ? " · " + agent.node : "";
      html +=
        '<article class="card ' + esc(agent.program) + '">' +
        '<span class="presence ' + esc(agent.presence) + '">' + esc(agent.presence) + "</span>" +
        '<div class="who">' + esc(agent.address) + "</div>" +
        '<div class="meta">' + esc(programName(agent.program)) + " · " + esc(where) + esc(node) + "</div>" +
        '<div class="meta">' + esc(unread) + " · seen " + esc(rel(agent.last_seen_at, now)) + "</div>" +
        (agent.description ? '<div class="sub">' + esc(agent.description) + "</div>" : "") +
        "</article>";
    }
    html += "</div></div>";
  }
  $("agents-body").innerHTML = html;
}

function programName(program) {
  return ({ claude: "Claude", codex: "Codex", grok: "Grok", cursor: "Cursor", grok_bot: "Grok Bot", other: "Other" })[program] || program;
}

function holdsHtml(snap, now) {
  if (!snap.holds.length) {
    return '<p class="empty">No live holds. Agents take them with <span class="path">hold_paths</span> before they edit.</p>';
  }
  const blocks = [];
  if (snap.counts.conflicts) {
    blocks.push('<p class="empty">Two agents hold overlapping paths. The leases are advisory. This is the clash, not a lock.</p>');
  }
  blocks.push('<div class="stack">');
  for (const hold of snap.holds) {
    const conflict = hold.conflicts.length || hold.remote_warnings.length;
    const left = until(hold.expires_at, now);
    const tags = [
      hold.exclusive ? "exclusive" : "shared",
      hold.project,
      left === "expired" ? "expired" : "lapses " + left,
    ];
    if (hold.conflicts.length) tags.push("conflict");
    if (hold.remote_warnings.length) tags.push("other node");
    let note = "";
    if (hold.conflicts.length) {
      note = hold.conflicts.map((c) => c.agent + " holds " + c.pattern).join("; ");
    } else if (hold.remote_warnings.length) {
      note = hold.remote_warnings.map((c) => (c.node || "remote") + " · " + c.agent + " holds " + c.pattern).join("; ");
    } else if (hold.reason) {
      note = hold.reason;
    }
    blocks.push(
      '<article class="row' + (conflict ? " conflict" : "") + '">' +
      '<div class="top"><span class="path">' + esc(hold.pattern) + "</span><span class=\"sub\">" + esc(hold.agent) + "</span></div>" +
      '<div class="tags">' + tags.map((t) => '<span class="tag' + (t === "conflict" || t === "other node" ? " bad" : "") + '">' + esc(t) + "</span>").join("") + "</div>" +
      (note ? '<div class="sub">' + esc(note) + "</div>" : "") +
      actionButton(snap, hold.releasable, "release-hold", hold.hold_id, "Release hold") +
      "</article>"
    );
  }
  blocks.push("</div>");
  return blocks.join("");
}

function renderRequests(snap, now) {
  const closed = snap.requests.filter((r) => r.status === "done" || r.status === "cancelled");
  const btn = $("show-closed");
  if (closed.length) {
    btn.hidden = false;
    btn.textContent = state.showClosed ? "Hide closed" : "Show " + closed.length + " closed";
  } else {
    btn.hidden = true;
  }
  const rows = snap.requests.filter((r) => state.showClosed || (r.status !== "done" && r.status !== "cancelled"));
  if (!rows.length) {
    $("requests-body").innerHTML = '<p class="empty">No open requests.</p>';
    return;
  }
  const html = ['<div class="stack">'];
  for (const req of rows) {
    const to = (req.to || []).join(", ") || "-";
    const due = req.due_at ? "due " + rel(req.due_at, now) : "no due time";
    html.push(
      '<article class="row">' +
      '<div class="top"><span>' + esc(req.title) + '</span><span class="tag ' + statusClass(req.status) + '">' + esc(req.status) + "</span></div>" +
      '<div class="sub">' + esc(req.created_by) + " → " + esc(to) + " · " + esc(due) +
      (req.owner ? " · owner " + esc(req.owner) : "") + "</div>" +
      actionButton(snap, req.expirable, "expire-request", req.request_id, "Expire") +
      "</article>"
    );
  }
  html.push("</div>");
  $("requests-body").innerHTML = html.join("");
}

function statusClass(status) {
  if (status === "done") return "good";
  if (status === "stale" || status === "cancelled") return "bad";
  if (status === "claimed") return "info";
  return "warn";
}

function actionButton(snap, enabled, action, id, label) {
  if (!snap.access.actions || !enabled) return "";
  return '<div class="actions"><button type="button" class="btn" data-action="' + esc(action) +
    '" data-id="' + esc(id) + '">' + esc(label) + "</button></div>";
}

function mailHtml(snap, now) {
  if (!snap.threads.length) {
    return '<p class="empty">The inbox is empty.</p>';
  }
  const opened = new Set(
    [...document.querySelectorAll("#mail-body details[open]")].map((el) => el.dataset.id)
  );
  const firstPaint = !state.mailReady;
  state.mailReady = true;
  return snap.threads.map((thread, index) => {
    const people = thread.participants.join(", ");
    const open = (firstPaint && index === 0) || opened.has(thread.thread_id) ? " open" : "";
    const messages = thread.messages.map((msg) => {
      const to = msg.to.join(", ") || "-";
      const cc = msg.cc.length ? " · cc " + msg.cc.join(", ") : "";
      let body;
      if (msg.body != null) body = "<pre>" + esc(msg.body) + "</pre>";
      else if (msg.body_redacted) body = '<p class="redacted">Body omitted.</p>';
      else body = "";
      return (
        '<div class="msg"><div class="from"><b>' + esc(msg.from) + "</b> → " + esc(to) + esc(cc) +
        " · " + esc(rel(msg.created_at, now)) + "</div>" + body + "</div>"
      );
    }).join("");
    const earlier = thread.truncated ? '<p class="redacted">Earlier messages in this thread are omitted.</p>' : "";
    return (
      '<details class="thread" data-id="' + esc(thread.thread_id) + '"' + open + '><summary><div><div class="subj">' + esc(thread.subject) +
      '</div><div class="sub">' + esc(people) + " · " + esc(rel(thread.last_at, now)) +
      "</div></div><span class=\"tag\">" + thread.messages.length + "</span></summary><div class=\"body-list\">" +
      earlier + messages + "</div></details>"
    );
  }).join("");
}

function resourcesHtml(snap, now) {
  if (!snap.resources.length) {
    return '<p class="empty">No registered resources. Agents should find one before starting another.</p>';
  }
  return '<div class="stack">' + snap.resources.map((res) => {
    const leases = (res.leases || []).map((lease) =>
      lease.role + " " + lease.agent + " · " + until(lease.expires_at, now)
    ).join(" · ");
    const where = [res.endpoint, res.port ? "port " + res.port : "", res.purpose || ""].filter(Boolean).join(" · ");
    const node = res.node ? " · " + res.node : "";
    return (
      '<article class="row"><div class="top"><span class="path">' + esc(res.kind) + " " + esc(res.name) +
      '</span><span class="tag ' + (res.status === "live" ? "good" : "warn") + '">' + esc(res.status) +
      "</span></div><div class=\"sub\">" + esc(where) + esc(node) +
      (res.owner ? " · owner " + esc(res.owner) : "") + "</div>" +
      (leases ? '<div class="sub">' + esc(leases) + "</div>" : "") +
      (res.reachable === false ? '<div class="sub">Not reachable from this machine.</div>' : "") +
      "</article>"
    );
  }).join("") + "</div>";
}

function peersHtml(snap) {
  if (!snap.peers.length) {
    return '<p class="empty">No paired nodes. This machine is a single node. <span class="path">another pair invite</span> on a hub starts federation.</p>';
  }
  return '<div class="stack">' + snap.peers.map((peer) => {
    const perms = (peer.perms || []).join(", ") || "no permissions";
    const sync = peer.last_sync_at ? "synced " + rel(peer.last_sync_at, Date.now()) : "not synced yet";
    return (
      '<article class="peer"><div class="top"><span class="who">' + esc(peer.display_name) +
      '</span><span class="tag info">' + esc(peer.role) + "</span></div>" +
      '<div class="perms">' + esc(perms) + " · " + esc(sync) + "</div>" +
      '<div class="sub path">' + esc(peer.fingerprint) + "</div></article>"
    );
  }).join("") + "</div>";
}

function fillFilters(snap) {
  fillSelect($("f-agent"), snap.agents.map((a) => a.address), "All agents");
  const projects = [];
  const seen = new Set();
  for (const project of snap.projects) {
    if (!seen.has(project.slug)) {
      seen.add(project.slug);
      projects.push(project.slug);
    }
  }
  fillSelect($("f-project"), projects, "All projects");
}

function fillSelect(select, values, first) {
  const current = select.value;
  const opts = ['<option value="">' + esc(first) + "</option>"];
  for (const value of values) opts.push('<option value="' + esc(value) + '">' + esc(value) + "</option>");
  select.innerHTML = opts.join("");
  if ([...select.options].some((o) => o.value === current)) select.value = current;
}

function activityQuery(before) {
  const url = new URL("/api/dash/activity", location.origin);
  const q = $("q").value.trim();
  if (q) url.searchParams.set("q", q);
  if ($("f-agent").value) url.searchParams.set("agent", $("f-agent").value);
  if ($("f-project").value) url.searchParams.set("project", $("f-project").value);
  if ($("f-kind").value) url.searchParams.set("kind", $("f-kind").value);
  const since = $("f-since").value;
  if (since && since !== "all") url.searchParams.set("since", since);
  if (before) url.searchParams.set("before", before);
  url.searchParams.set("limit", "80");
  return url.pathname + url.search;
}

async function loadActivity(append) {
  const before = append ? state.next : null;
  if (append && !before) return;
  const page = await getJson(activityQuery(before));
  state.activity = append ? state.activity.concat(page.items) : page.items;
  state.next = page.next_before;
  state.appended = append;
  renderTimeline(page.scanned_limit);
}

function renderTimeline(scanned) {
  const now = Date.now();
  const list = $("timeline-list");
  if (!state.activity.length) {
    list.innerHTML = '<p class="empty">Nothing in this window.</p>';
  } else {
    list.innerHTML = '<div class="timeline">' + state.activity.map((item) => {
      let extra = "";
      if (item.body) extra = '<div class="extra">' + esc(item.body) + "</div>";
      else if (item.body_redacted) extra = '<div class="extra redacted">Body omitted.</div>';
      const project = item.project ? " · " + item.project : "";
      return (
        '<article class="event"><time datetime="' + esc(item.ts) + '">' + esc(rel(item.ts, now)) +
        '</time><div><div><span class="kind">' + esc(item.category) + "</span>" + esc(item.actor) +
        esc(project) + '</div><div class="sum">' + esc(item.summary) + "</div>" + extra + "</div></article>"
      );
    }).join("") + "</div>";
  }
  const more = $("timeline-more");
  more.hidden = !state.next;
  if (scanned) more.textContent = "Older (search window was capped)";
  else more.textContent = "Older";
}

async function confirmAction(title, body, okLabel, url) {
  $("confirm-title").textContent = title;
  $("confirm-body").textContent = body;
  $("confirm-ok").textContent = okLabel;
  const dialog = $("confirm");
  if (typeof dialog.showModal === "function") {
    dialog.showModal();
    const choice = await new Promise((resolve) => {
      dialog.addEventListener("close", () => resolve(dialog.returnValue), { once: true });
    });
    if (choice !== "ok") return;
  } else if (!window.confirm(title + "\n\n" + body)) {
    return;
  }
  const res = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json", "x-another-confirm": "1" },
    body: JSON.stringify({ confirm: true }),
  });
  if (!res.ok) {
    const err = await res.json().catch(() => ({}));
    toast((err.error && err.error.message) || "Could not apply that");
    return;
  }
  toast(okLabel + " done");
  state.appended = false;
  await refresh();
}

document.body.addEventListener("click", (event) => {
  const button = event.target.closest("[data-action]");
  if (!button || !state.snap) return;
  const id = button.getAttribute("data-id");
  const action = button.getAttribute("data-action");
  if (action === "release-hold") {
    const hold = state.snap.holds.find((h) => h.hold_id === id);
    const who = hold ? hold.agent + " · " + hold.pattern : id;
    confirmAction(
      "Release this hold?",
      who + ". This only runs on this machine, and only when that owner is away or stale.",
      "Release hold",
      "/api/dash/holds/" + encodeURIComponent(id) + "/release"
    );
  } else if (action === "expire-request") {
    const req = state.snap.requests.find((r) => r.request_id === id);
    confirmAction(
      "Expire this request?",
      (req ? req.title : id) + " will be marked stale. It can still be claimed. This is local only.",
      "Expire",
      "/api/dash/requests/" + encodeURIComponent(id) + "/expire"
    );
  }
});

$("show-stale").addEventListener("click", () => {
  state.showStale = !state.showStale;
  if (state.snap) renderAgents(state.snap, Date.now());
});
$("show-closed").addEventListener("click", () => {
  state.showClosed = !state.showClosed;
  if (state.snap) renderRequests(state.snap, Date.now());
});

let searchTimer = null;
function filtersChanged() {
  state.appended = false;
  loadActivity(false).catch((err) => toast(String(err.message || err)));
}
$("q").addEventListener("input", () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(filtersChanged, 220);
});
for (const id of ["f-agent", "f-project", "f-kind", "f-since"]) {
  $(id).addEventListener("change", filtersChanged);
}
$("timeline-more").addEventListener("click", () => {
  loadActivity(true).catch((err) => toast(String(err.message || err)));
});

function watch() {
  if (!window.EventSource) {
    setLive("poll", "polling");
    state.timer = setInterval(() => poll(), 3000);
    return;
  }
  const source = new EventSource("/api/dash/stream");
  source.addEventListener("revision", (event) => {
    setLive("live", "live");
    let seq = null;
    try { seq = JSON.parse(event.data).seq; } catch (_) { return; }
    if (state.seq != null && seq !== state.seq && !state.appended) {
      refresh().catch((err) => toast(String(err.message || err)));
    } else if (state.seq == null) {
      state.seq = seq;
    }
  });
  source.onerror = () => {
    source.close();
    setLive("down", "reconnecting");
    setTimeout(watch, 3000);
  };
}

async function poll() {
  try {
    const rev = await getJson("/api/dash/revision");
    setLive("poll", "polling");
    if (state.seq != null && rev.seq !== state.seq && !state.appended) await refresh();
  } catch (_) {
    setLive("down", "offline");
  }
}

refresh()
  .then(() => watch())
  .catch((err) => {
    $("boot").textContent = "Could not read the dashboard. " + (err.message || err);
    setLive("down", "offline");
  });
