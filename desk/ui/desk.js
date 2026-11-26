// Paja Desk's window: reads the queue through the core's `work_list` command and shows it
// grouped by what it needs. It keeps nothing of its own; every refresh reads the log again.
"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// The groups, in the order they are shown. Only the last few landed items are shown: the
// newest issues, since items carry no landing time.
const LANDED_SHOWN = 5;
const GROUPS = [
  { name: "Waiting on you", className: "waiting", test: (i) => i.status === "escalated" },
  { name: "Active", test: (i) => i.status === "active" },
  { name: "Queued", test: (i) => i.status === "queued" },
  { name: "Landed recently", test: (i) => i.status === "landed", last: LANDED_SHOWN },
];

function el(tag, props = {}, children = []) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === "className") node.className = value;
    else node.setAttribute(key, value);
  }
  for (const child of [].concat(children)) {
    node.append(typeof child === "string" ? document.createTextNode(child) : child);
  }
  return node;
}

function issueLink(item) {
  const link = el("a", { href: item.url }, `#${item.number}`);
  link.addEventListener("click", (event) => {
    event.preventDefault();
    window.__TAURI__.opener.openUrl(item.url).catch(showError);
  });
  return link;
}

function itemRow(item) {
  const row = el("div", { className: "row" }, [
    el("span", { className: "number" }, [issueLink(item)]),
    el("span", { className: "title" }, item.title),
    el("span", { className: "status" }, item.open ? item.status : `${item.status} (issue closed)`),
  ]);
  const children = [row];
  if (item.status === "escalated") {
    if (item.note) children.push(el("div", { className: "note" }, item.note));
    children.push(
      el("div", { className: "muted" }, [
        "Answer with ",
        el("code", {}, `paja work answer ${item.number} "<your answer>"`),
      ]),
    );
  }
  return el("li", {}, children);
}

function render(items) {
  const queue = document.getElementById("queue");
  queue.replaceChildren();
  const repos = [...new Set(items.map((i) => i.repo))].sort();
  if (repos.length === 0) {
    queue.append(el("p", { className: "muted" }, "No work items yet. Run `paja work sync` in a repository's checkout."));
  }
  for (const repo of repos) {
    queue.append(el("h3", {}, repo || "(no repository)"));
    const mine = items.filter((i) => i.repo === repo);
    for (const group of GROUPS) {
      let shown = mine.filter(group.test);
      if (group.last) shown = shown.slice(-group.last).reverse();
      if (shown.length === 0) continue;
      queue.append(el("h2", {}, `${group.name} (${shown.length})`));
      queue.append(el("ul", { className: group.className ?? "" }, shown.map(itemRow)));
    }
  }
}

function showError(err) {
  const box = document.getElementById("error");
  box.textContent = typeof err === "string" ? err : err?.message ?? String(err);
  box.hidden = false;
}

async function refresh() {
  const result = await invoke("work_list");
  const box = document.getElementById("error");
  if (result && !Array.isArray(result) && result.error) {
    showError(`error: ${result.error}`);
    return;
  }
  box.hidden = true;
  render(result);
  document.getElementById("updated").textContent = `updated ${new Date().toLocaleTimeString()}`;
}

listen("paja://events-changed", () => refresh().catch(showError));
refresh().catch(showError);
