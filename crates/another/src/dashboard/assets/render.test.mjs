// Renders the dashboard page twice. The boot message used to live inside
// #summary, so the second render threw and live updates never landed.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const dir = dirname(fileURLToPath(import.meta.url));
const jsdomUrl = process.env.DASH_UI_NODE_MODULES
  ? pathToFileURL(join(process.env.DASH_UI_NODE_MODULES, "jsdom/lib/api.js")).href
  : "jsdom";
const { JSDOM } = await import(jsdomUrl);

const html = readFileSync(join(dir, "index.html"), "utf8");
const script = readFileSync(join(dir, "app.js"), "utf8");

function page(hash) {
  const dom = new JSDOM(html, {
    url: "http://127.0.0.1:7466/ui" + hash,
    runScripts: "dangerously",
  });
  dom.window.__ANOTHER_DASH_TEST = true;
  dom.window.eval(script);
  return dom;
}

function snap(title, status) {
  return {
    node: {
      display_name: "laptop",
      role: "home",
      fingerprint: "abcdabcdabcdabcdffff",
    },
    access: { bodies: true, actions: true, mode: "local" },
    hour: { counts: { mail: 1 }, highlights: ["alice sent a note"] },
    day: { counts: { mail: 2 }, highlights: [] },
    counts: {
      live: 1,
      idle: 0,
      holds: 1,
      resources_live: 0,
      conflicts: 0,
      remote_warnings: 0,
      requests_stale: status === "stale" ? 1 : 0,
    },
    agents: [],
    holds: [
      {
        hold_id: "h1",
        agent: "alice@shop",
        pattern: "src/main.rs",
        project: "shop",
        exclusive: true,
        expires_at: new Date(Date.now() + 600000).toISOString(),
        reason: "editing",
        releasable: false,
        conflicts: [],
        remote_warnings: [],
      },
    ],
    requests: [
      {
        request_id: "r1",
        title,
        status,
        created_by: "alice@shop",
        to: ["bob@shop"],
        due_at: null,
        owner: null,
        expirable: status !== "stale",
      },
    ],
    threads: [],
    resources: [],
    peers: [],
    projects: [],
  };
}

function assert(cond, message) {
  if (!cond) {
    throw new Error(message);
  }
}

const first = page("");
const render = first.window.__anotherDash.render;
render(snap("Cut over the billing rewrite", "open"));
const boot = first.window.document.getElementById("boot");
assert(boot, "boot element was removed by the first render");
assert(boot.hidden, "boot message should be hidden after the first render");
render(snap("Nightly fixture drift", "stale"));
const requests = first.window.document.getElementById("requests-body").textContent;
assert(requests.includes("Nightly fixture drift"), "second render did not update the request");
assert(
  !requests.includes("Cut over the billing rewrite"),
  "second render left the previous request in place"
);
assert(
  first.window.document.getElementById("boot"),
  "boot element disappeared on the second render"
);

const hashed = page("#1x");
hashed.window.__anotherDash.render(snap("Still here", "open"));
assert(
  hashed.window.document.body.textContent.includes("Still here"),
  "a hash that is not a selector aborted render"
);

console.log("dashboard render test ok");
