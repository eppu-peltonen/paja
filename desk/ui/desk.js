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
    children.push(answerForm(item));
  }
  return el("li", {}, children);
}

// A message that stays until the person dismisses it, whatever the queue does meanwhile.
function notice(text) {
  const close = el("button", { type: "button", className: "quiet" }, "Dismiss");
  const entry = el("li", {}, [el("span", {}, text), close]);
  close.addEventListener("click", () => entry.remove());
  document.getElementById("notices").append(entry);
}

// What the person has typed, by item, so a refresh or an error never loses it.
const drafts = new Map();

function answerForm(item) {
  const key = `${item.repo}#${item.number}`;
  const text = el("textarea", { rows: "3", placeholder: "Your answer" });
  text.value = drafts.get(key) ?? "";
  text.addEventListener("input", () => drafts.set(key, text.value));
  const send = el("button", { type: "button" }, "Send answer");
  send.addEventListener("click", async () => {
    if (!text.value.trim()) {
      notice(`#${item.number}: write an answer first.`);
      return;
    }
    send.disabled = true;
    const result = await invoke("work_answer", {
      repo: item.repo,
      number: item.number,
      text: text.value,
    }).catch((err) => ({ error: String(err) }));
    send.disabled = false;
    if (result && result.error) {
      // A notice outlives the refresh that may rebuild this item (or, when the answer was
      // recorded but posting it failed, drop it from "waiting on you").
      notice(`#${item.number}: error: ${result.error}`);
      return;
    }
    drafts.delete(key);
    refresh().catch(showError);
  });
  return el("div", { className: "answer" }, [
    text,
    el("div", { className: "actions" }, [send]),
    el("div", { className: "muted" }, [
      "Or answer with ",
      el("code", {}, `paja work answer ${item.number} "<your answer>"`),
    ]),
  ]);
}

let knownRepos = [];

function render(items) {
  const queue = document.getElementById("queue");
  queue.replaceChildren();
  const repos = [...new Set(items.map((i) => i.repo))].sort();
  knownRepos = repos.filter((r) => r);
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

// Each refresh has a number; an older one that finishes late doesn't draw over a newer one.
let latest = 0;

async function refresh() {
  const mine = ++latest;
  const result = await invoke("work_list");
  if (mine !== latest) return;
  const box = document.getElementById("error");
  if (result && !Array.isArray(result) && result.error) {
    showError(`error: ${result.error}`);
    return;
  }
  box.hidden = true;
  render(result);
  document.getElementById("updated").textContent = `updated ${new Date().toLocaleTimeString()}`;
}

// The "New work" form: edit, then review what will be filed, then file it.
function byId(id) {
  return document.getElementById(id);
}

function showFiling(step) {
  byId("file-edit").hidden = step !== "edit";
  byId("file-confirm").hidden = step !== "confirm";
}

byId("new-work").addEventListener("click", () => {
  const select = byId("file-repo");
  const chosen = select.value;
  select.replaceChildren(...knownRepos.map((r) => el("option", { value: r }, r)));
  if (knownRepos.includes(chosen)) select.value = chosen;
  byId("filing").hidden = false;
  byId("file-result").hidden = true;
  showFiling("edit");
});

byId("file-cancel").addEventListener("click", () => {
  byId("filing").hidden = true;
});

byId("file-review").addEventListener("click", () => {
  const result = byId("file-result");
  if (!byId("file-repo").value || !byId("file-title").value.trim()) {
    result.className = "error";
    result.textContent = "Pick a repository and give the issue a title.";
    result.hidden = false;
    return;
  }
  result.hidden = true;
  byId("confirm-repo").textContent = byId("file-repo").value;
  byId("confirm-title").textContent = byId("file-title").value;
  byId("confirm-body").textContent = byId("file-body").value;
  showFiling("confirm");
});

byId("file-back").addEventListener("click", () => showFiling("edit"));

byId("file-send").addEventListener("click", async () => {
  const send = byId("file-send");
  const result = byId("file-result");
  send.disabled = true;
  const filed = await invoke("work_file", {
    repo: byId("file-repo").value,
    title: byId("file-title").value,
    body: byId("file-body").value,
  }).catch((err) => ({ error: String(err) }));
  send.disabled = false;
  result.hidden = false;
  if (filed && filed.error) {
    // The text stays in the form, to fix and send again.
    result.className = "error";
    result.textContent = `error: ${filed.error}`;
    showFiling("edit");
    return;
  }
  result.className = "";
  result.replaceChildren("Filed ", issueLink({ url: filed.url, number: filed.url.split("/").pop() }));
  byId("file-title").value = "";
  byId("file-body").value = "";
  showFiling("edit");
});

listen("paja://events-changed", () => refresh().catch(showError));
refresh().catch(showError);
