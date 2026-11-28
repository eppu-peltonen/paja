// Paja Desk's window: one screen with the repository's lanes, a side panel (next, held, out)
// and the activity strip, all read from the core's commands (`work_list`, `repos`, `pool_status`,
// `pool_events`, `work_run`) and the event log. It keeps nothing of its own beyond the picked
// repository, the open tab, the open run view and the answers being typed; every refresh reads
// the log again. What is computed from the core's JSON lives in view.js.
"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const V = window.View;

const REFRESH_EVERY_MS = 2000;
const ACTIVITY_SHOWN = 80;

function byId(id) {
  return document.getElementById(id);
}

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

// What Desk holds: the picked repository, the open tab, the held item and run view open in the
// panel, and the last read of the core.
const ui = { repo: null, tab: "next", held: null, run: null, closedOpen: false };
let data = { repos: [], items: [], status: null, events: [] };

// ---- Errors -----------------------------------------------------------------------------------

// One line under the bar: the core's text. A refresh's error goes with the next good refresh; an
// action's stays until the next action works or it is clicked.
let refreshError = null;
let actionError = null;

function paintError() {
  const box = byId("error");
  const text = refreshError ?? actionError;
  box.textContent = text ?? "";
  box.hidden = !text;
}

function messageOf(err) {
  return typeof err === "string" ? err : err?.message ?? String(err);
}

function showError(err) {
  const text = messageOf(err);
  actionError = text.startsWith("error:") ? text : `error: ${text}`;
  paintError();
}

byId("error").addEventListener("click", () => {
  actionError = null;
  paintError();
});

// A message in the panel that stays until dismissed, whatever the queue does meanwhile.
const NOTICES = 20;
function notice(text) {
  const close = el("button", { type: "button", className: "quiet" }, "Dismiss");
  const entry = el("li", {}, [el("span", {}, text), close]);
  close.addEventListener("click", () => entry.remove());
  const list = byId("notices");
  list.append(entry);
  while (list.children.length > NOTICES) list.firstElementChild.remove();
}

// ---- Links and tags ---------------------------------------------------------------------------

function openUrl(url) {
  window.__TAURI__.opener.openUrl(url).catch(showError);
}

// A middle click on a link would make the webview try to open it itself: send it to the opener.
document.addEventListener("auxclick", (event) => {
  const link = event.target.closest?.("a[href]");
  if (!link) return;
  event.preventDefault();
  if (event.button === 1) openUrl(link.href);
});

function issueUrl(number) {
  const item = data.items.find((i) => i.repo === ui.repo && i.number === number);
  return item ? item.url : `https://github.com/${ui.repo}/issues/${number}`;
}

// `#n` in the item's colour, linking to its issue.
function tagLink(number, url = issueUrl(number)) {
  const link = el("a", { href: url, className: "tag" }, `#${number}`);
  link.style.setProperty("--tag", V.colourTag(number));
  link.addEventListener("click", (event) => {
    event.preventDefault();
    openUrl(url);
  });
  return link;
}

function tagText(number) {
  const tag = el("span", { className: "tag" }, `#${number}`);
  tag.style.setProperty("--tag", V.colourTag(number));
  return tag;
}

// ---- Refresh ----------------------------------------------------------------------------------

// Each refresh has a number; an older one that finishes late doesn't draw over a newer one.
let latest = 0;

function failed(result) {
  return result && !Array.isArray(result) && result.error;
}

async function refresh() {
  const mine = ++latest;
  const [items, repos] = await Promise.all([invoke("work_list"), invoke("repos")]);
  if (mine !== latest) return;
  const bad = failed(items) || failed(repos);
  if (bad) {
    refreshError = `error: ${bad}`;
    paintError();
    return;
  }
  const names = [...new Set([...repos.map((r) => r.repo), ...items.map((i) => i.repo)])].filter(Boolean).sort();
  if (!names.includes(ui.repo)) ui.repo = names[0] ?? null;
  let status = null;
  let events = [];
  if (ui.repo) {
    const repo = ui.repo;
    [status, events] = await Promise.all([
      invoke("pool_status", { repo }),
      invoke("pool_events", { repo, limit: ACTIVITY_SHOWN }),
    ]);
    if (mine !== latest || repo !== ui.repo) return;
    if (failed(status)) {
      refreshError = `error: ${status.error}`;
      paintError();
      return;
    }
    if (!Array.isArray(events)) events = [];
  }
  refreshError = null;
  data = { repos, items, status, events, names };
  paintError();
  render();
}

function render() {
  renderBar();
  renderLanes();
  renderPanel(false);
  renderActivity();
  if (byId("repos").open) renderRepoList();
}

// ---- Top bar ----------------------------------------------------------------------------------

function repoEntry() {
  return data.repos.find((r) => r.repo === ui.repo);
}

function renderBar() {
  const status = data.status;
  const pick = byId("repo-pick");
  const names = data.names ?? [];
  pick.hidden = names.length < 2;
  pick.replaceChildren(...names.map((n) => el("option", { value: n }, n)));
  if (ui.repo) pick.value = ui.repo;

  const running = status?.state === "running";
  byId("state-dot").className = running ? "dot running" : "dot";
  byId("state-word").textContent = running ? "running" : "stopped";
  const up = byId("fig-up");
  if (running && status.started_ms != null) {
    up.dataset.since = String(status.started_ms);
    up.textContent = V.duration(Date.now() - status.started_ms);
  } else {
    delete up.dataset.since;
    up.textContent = "-";
  }
  byId("fig-landed").textContent = String(status?.landed_since_start ?? 0);
  byId("fig-ready").textContent = String(status?.ready ?? 0);
  byId("fig-spend").textContent = `${V.tokens(status?.tokens)} · ${V.usd(status?.spent_usd)}`;

  const held = status?.held ?? 0;
  byId("held-count").textContent = String(held);
  byId("held-button").classList.toggle("hot", held > 0);
  const badge = byId("tab-held-count");
  badge.textContent = String(held);
  badge.hidden = held === 0;

  const entry = repoEntry();
  const runs = (entry && entry.runs) || {};
  byId("start").hidden = !status || running || !!runs.desk || !!runs.terminal || !(entry && entry.checkout);
  // Stop is there for any run Desk started, a single item's included.
  byId("stop").hidden = !runs.desk;
  byId("stop-now").hidden = !runs.desk;
}

byId("repo-pick").addEventListener("change", () => {
  ui.repo = byId("repo-pick").value;
  ui.held = null;
  ui.run = null;
  refresh().catch(showError);
});

byId("start").addEventListener("click", async () => {
  const lanes = data.status?.target ?? 1;
  if (lanes < 1) {
    showError("The pool has 0 lanes: add a lane with the + card, then Start.");
    return;
  }
  const result = await invoke("run_start", { repo: ui.repo, every: true, workers: lanes }).catch((err) => ({ error: String(err) }));
  settle(result);
});

for (const [id, now] of [["stop", false], ["stop-now", true]]) {
  byId(id).addEventListener("click", async () => {
    const result = await invoke("run_stop", { repo: ui.repo, now }).catch((err) => ({ error: String(err) }));
    settle(result);
  });
}

// An action's reply: its error on the line under the bar, else the error goes; then read again.
function settle(result) {
  if (result && result.error) showError(`error: ${result.error}`);
  else actionError = null;
  paintError();
  refresh().catch(showError);
}

byId("held-button").addEventListener("click", showHeld);

function showHeld() {
  ui.tab = "held";
  ui.run = null;
  ui.held = null;
  renderPanel(true);
}

// ---- Lanes ------------------------------------------------------------------------------------

function clock(since) {
  return el("span", { className: "clock num", "data-since": String(since) }, V.duration(Date.now() - since));
}

function laneCard(card) {
  const node = el("div", { className: card.kind === "idle" ? "card idle" : "card" });
  const minus = el("button", { type: "button", className: "minus", title: "Remove a lane" }, "−");
  minus.addEventListener("click", (event) => {
    event.stopPropagation();
    changeLanes(-1);
  });
  if (card.kind === "idle") {
    node.append(el("div", { className: "head" }, el("span", {}, "idle")), minus);
    return node;
  }
  const lane = card.lane;
  node.style.setProperty("--tag", V.colourTag(lane.number));
  node.append(
    el("div", { className: "head" }, [tagLink(lane.number), el("span", { className: "title", title: lane.title }, lane.title)]),
    minus,
  );
  const where = [lane.stage, lane.step && lane.step !== lane.stage ? lane.step : null].filter(Boolean).join(" · ");
  const stage = el("div", { className: "row" }, [
    el("span", {}, where),
    el("span", {}, [card.merging ? el("span", { className: "flag" }, "merging ") : "", V.roundLabel(lane.round)]),
  ]);
  node.append(stage);
  if (lane.stage === "review" && (lane.reviews || []).length) {
    node.append(
      el("div", { className: "row" }, [
        el("span", { className: "muted" }, "reviewers"),
        el("span", { className: "marks mono" }, lane.reviews.map((v) => el("span", { className: `mark-${v}`, title: v }, V.verdictMark(v)))),
      ]),
    );
  }
  node.append(
    el("div", { className: "row" }, [
      el("span", {}, ["step ", clock(lane.since_ms)]),
      el("span", {}, ["lane ", clock(lane.admitted_ms)]),
    ]),
    el("div", { className: "row" }, [
      el("span", { className: "num" }, `${V.tokens(lane.tokens)} tokens`),
      el("span", { className: "num" }, V.usd(lane.spent_usd)),
    ]),
    el("div", { className: "files mono" }, V.files(lane.paths).text || "no files named"),
  );
  node.addEventListener("click", (event) => {
    if (event.target.closest("a, button")) return;
    openRun(lane.number);
  });
  return node;
}

function renderLanes() {
  const box = byId("lanes");
  if (!data.status) {
    box.replaceChildren(el("p", { className: "muted" }, "No repositories yet. Use Repositories to add a checkout."));
    return;
  }
  const skeleton = el("button", { type: "button", className: "skeleton", title: "Add a lane (+)" }, "+");
  skeleton.addEventListener("click", () => changeLanes(1));
  // The core lets every busy lane finish and closes whichever frees first, so the note is the
  // pool's, not a card's.
  const shrink = V.shrinkBy(data.status);
  const note = shrink > 0
    ? [el("p", { className: "flag shrink-note" }, `Pool shrinking by ${shrink}: busy lanes finish their item, then ${shrink === 1 ? "one closes" : `${shrink} close`}.`)]
    : [];
  box.replaceChildren(...note, ...V.laneCards(data.status).map(laneCard), skeleton);
}

// Add or remove a lane: the lane count the pool keeps, one more or less. A running pool picks it
// up on its next tick, a stopped one starts with it. The core's refusal is shown as it comes.
async function changeLanes(delta) {
  if (!data.status) return;
  const target = data.status.target ?? 1;
  const next = V.lanesAfter(target, delta) ?? target + delta;
  if (next < 0) {
    showError(`error: a pool runs 0 to ${V.MAX_LANES} lanes; there is no lane left to remove`);
    return;
  }
  const result = await invoke("pool_lanes", { repo: ui.repo, lanes: next }).catch((err) => ({ error: String(err) }));
  if (result && !result.error && data.status) data.status.target = result.lanes;
  settle(result);
  if (data.status) renderLanes();
}

// ---- Side panel -------------------------------------------------------------------------------

for (const tab of document.querySelectorAll(".tab")) {
  tab.addEventListener("click", () => {
    ui.tab = tab.dataset.tab;
    ui.run = null;
    ui.held = null;
    renderPanel(true);
  });
}

// What the person has typed, by item, so a refresh or an error never loses it.
const drafts = new Map();

function renderPanel(force) {
  const body = byId("panel-body");
  // A refresh leaves the panel alone while an answer is being typed in it.
  if (!force && document.activeElement?.tagName === "TEXTAREA" && body.contains(document.activeElement)) return;
  for (const tab of document.querySelectorAll(".tab")) tab.classList.toggle("active", !ui.run && tab.dataset.tab === ui.tab);
  const status = data.status;
  if (!status) {
    body.replaceChildren();
    return;
  }
  if (ui.run != null) body.replaceChildren(...runPanel(ui.run));
  else if (ui.tab === "next") body.replaceChildren(...nextPanel(status));
  else if (ui.tab === "held") body.replaceChildren(...heldPanel());
  else body.replaceChildren(...outPanel(status));
}

function nextPanel(status) {
  const { ready, other } = V.splitWaiting(status.waiting);
  const parts = [el("h3", {}, `Ready (${ready.length})`)];
  parts.push(
    el("ul", {}, ready.length
      ? ready.map((w) => el("li", {}, [
          el("div", { className: "line" }, [tagLink(w.number), el("span", { className: "grow", title: w.title }, w.title)]),
          el("div", { className: "sub mono" }, V.files(w.paths).text || "files not planned yet"),
        ]))
      : [el("li", { className: "muted" }, "Nothing ready.")]),
  );
  if (other.length) {
    parts.push(el("h3", {}, `Waiting (${other.length})`));
    parts.push(
      el("ul", {}, other.map((w) => el("li", { className: "dim" }, [
        el("div", { className: "line" }, [tagLink(w.number), el("span", { className: "grow", title: w.title }, w.title)]),
        el("div", { className: "sub" }, w.why),
      ]))),
    );
  }
  return parts;
}

function outPanel(status) {
  if (!status.out.length) return [el("p", { className: "muted" }, "Nothing has left a lane yet.")];
  const now = Date.now();
  return [
    el("ul", {}, status.out.map((exit) => el("li", {}, [
      el("div", { className: "line" }, [tagLink(exit.number), el("span", { className: "grow", title: exit.title }, exit.title)]),
      el("div", { className: "sub" }, [`${exit.outcome} `, el("span", { className: "num" }, `${V.duration(now - exit.ts_ms)} ago`)]),
    ]))),
  ];
}

function heldRow(item) {
  const row = el("li", { className: "clickable" }, [
    el("div", { className: "line" }, [tagLink(item.number, item.url), el("span", { className: "grow", title: item.title }, item.title)]),
    el("div", { className: "sub" }, V.firstLine(item.note) || "waiting for your answer"),
  ]);
  row.addEventListener("click", (event) => {
    if (event.target.closest("a")) return;
    ui.held = item.number;
    renderPanel(true);
  });
  return row;
}

function heldPanel() {
  const mine = data.items.filter((i) => i.repo === ui.repo);
  const { open, closed } = V.heldItems(mine);
  if (ui.held != null) {
    const item = open.concat(closed).find((i) => i.number === ui.held);
    if (item) return heldDetail(item);
    ui.held = null;
  }
  const parts = [];
  parts.push(el("ul", {}, open.length ? open.map(heldRow) : [el("li", { className: "muted" }, "Nothing is waiting for you.")]));
  if (closed.length) {
    const group = el("details", {}, [el("summary", {}, `closed issues (${closed.length})`), el("ul", {}, closed.map(heldRow))]);
    group.open = ui.closedOpen;
    group.addEventListener("toggle", () => {
      ui.closedOpen = group.open;
    });
    parts.push(group);
  }
  return parts;
}

function heldDetail(item) {
  const back = el("button", { type: "button", className: "quiet" }, "Back");
  back.addEventListener("click", () => {
    ui.held = null;
    renderPanel(true);
  });
  const parts = [
    el("div", { className: "line" }, [back, tagLink(item.number, item.url), el("strong", { className: "grow" }, item.title)]),
  ];
  if (!item.open) parts.push(el("p", { className: "muted" }, "Its issue is closed."));
  const last = (item.answers || []).at(-1);
  if (last && last.by === "arbiter") parts.push(el("p", { className: "sub" }, `answered by arbiter: ${last.answer}`));
  if (item.note) parts.push(el("div", { className: "question" }, item.note));
  parts.push(answerForm(item));
  return parts;
}

function answerForm(item) {
  const key = `${item.repo}#${item.number}`;
  const text = el("textarea", { rows: "5", placeholder: "Your answer" });
  text.value = drafts.get(key) ?? "";
  text.addEventListener("input", () => drafts.set(key, text.value));
  const send = el("button", { type: "button" }, "Send answer");
  send.addEventListener("click", async () => {
    if (!text.value.trim()) {
      notice(`${key}: write an answer first.`);
      return;
    }
    send.disabled = true;
    const result = await invoke("work_answer", { repo: item.repo, number: item.number, text: text.value }).catch((err) => ({ error: String(err) }));
    send.disabled = false;
    if (result && result.error) {
      // A notice outlives the refresh that may rebuild this item.
      notice(`${key}: error: ${result.error}`);
      return;
    }
    drafts.delete(key);
    ui.held = null;
    refresh().catch(showError);
  });
  return el("div", { className: "answer" }, [
    text,
    el("div", { className: "actions" }, [send]),
    el("div", { className: "muted" }, ["Or answer with ", el("code", {}, `paja work answer ${item.number} "<your answer>"`)]),
  ]);
}

// ---- Run view (in place of the panel) ---------------------------------------------------------

// What the open run view last read, so a refresh shows it until the new read arrives.
let lastRun = null;

function openRun(number) {
  ui.run = number;
  lastRun = null;
  renderPanel(true);
}

function closeRun() {
  ui.run = null;
  renderPanel(true);
}

function stepName(step) {
  const who = step.reviewer ? `${step.role} (reviewer ${step.reviewer})` : step.role;
  return step.model ? `${who} on ${step.model}` : who;
}

function spent(step) {
  const parts = [];
  if (step.input_tokens != null) {
    const read = (step.input_tokens ?? 0) + (step.cache_read_input_tokens ?? 0) + (step.cache_creation_input_tokens ?? 0);
    parts.push(`${V.tokens(read)} tokens in, ${V.tokens(step.output_tokens ?? 0)} out`);
  }
  if (step.cost_usd != null) parts.push(V.usd(step.cost_usd));
  if (step.num_turns != null) parts.push(`${step.num_turns} turns`);
  if (step.duration_ms != null) parts.push(V.duration(step.duration_ms));
  return parts.length ? parts.join(", ") : "spend unknown";
}

function runPanel(number) {
  const item = data.items.find((i) => i.repo === ui.repo && i.number === number);
  const close = el("button", { type: "button", className: "quiet" }, "Close");
  close.addEventListener("click", closeRun);
  const box = el("div", { className: "run" });
  if (lastRun) box.replaceChildren(...runParts(lastRun));
  else box.append(el("p", { className: "muted" }, "Reading the run..."));
  const repo = ui.repo;
  invoke("work_run", { repo, number })
    .then((run) => {
      if (run && run.error) throw new Error(run.error);
      if (ui.run !== number || ui.repo !== repo) return;
      lastRun = run;
      box.replaceChildren(...runParts(run));
    })
    .catch((err) => box.replaceChildren(el("p", { className: "error" }, `error: ${messageOf(err)}`)));
  return [
    el("div", { className: "line" }, [close, tagLink(number), el("strong", { className: "grow" }, item ? item.title : "")]),
    box,
  ];
}

function runParts(run) {
  if (!run) return [el("p", { className: "muted" }, "No run has started this item yet.")];
  const parts = [];
  if (run.state === "interrupted") {
    parts.push(el("p", { className: "warning-inline" }, "Interrupted: no paja run is working on it now. Start paja run in its checkout to go on from here."));
  }
  if (run.state === "claimed") {
    parts.push(el("p", { className: "muted" }, "Claimed by hand since its last run, so paja run leaves it alone. Set it back to queued for a run to take it."));
    return parts;
  }
  // An interrupted run's step isn't running, so it gets no clock.
  const now = !run.step ? []
    : run.state !== "running" ? [`, stopped in ${stepName(run.step)}`]
    : [`, ${stepName(run.step)} for `, clock(run.step.started_ms)];
  parts.push(el("p", {}, [el("strong", {}, `Round ${run.round} of ${run.max_rounds}`), ...now]));
  if (run.env) parts.push(el("p", { className: "muted" }, `Environment ${run.env.name}: ${run.env.path} (${run.env.branch})`));
  if (run.pr) {
    const link = el("a", { href: run.pr }, run.pr);
    link.addEventListener("click", (event) => {
      event.preventDefault();
      openUrl(run.pr);
    });
    parts.push(el("p", {}, ["Pull request ", link]));
  }
  for (const round of run.rounds) {
    const steps = round.steps.map((s) => el("li", {}, `${stepName({ ...s, role: s.role ?? s.step })}: ${spent(s)}`));
    parts.push(el("div", { className: "round" }, [
      el("div", {}, [el("strong", {}, `Round ${round.round}: ${round.verdict}`), round.detail ? ` - ${round.detail}` : ""]),
      el("ul", {}, steps),
    ]));
  }
  return parts;
}

// ---- Activity ---------------------------------------------------------------------------------

function renderActivity() {
  const box = byId("activity-lines");
  const stuck = box.scrollTop + box.clientHeight >= box.scrollHeight - 8;
  box.replaceChildren(
    ...V.activityLines(data.events).map((line) =>
      el("div", { className: "event" }, [
        el("span", { className: "when num" }, new Date(line.ts_ms).toLocaleTimeString()),
        el("span", { className: "what" }, line.what),
        el("span", { className: "who" }, line.number != null ? tagText(line.number) : ""),
        el("span", { className: "detail mono", title: line.detail }, line.detail),
      ]),
    ),
  );
  if (stuck) box.scrollTop = box.scrollHeight;
}

// ---- Clocks -----------------------------------------------------------------------------------

setInterval(() => {
  for (const node of document.querySelectorAll(".clock, #fig-up[data-since]")) {
    node.textContent = V.duration(Date.now() - Number(node.dataset.since));
  }
}, 1000);

// ---- Dialogs ----------------------------------------------------------------------------------

function dialogOpen() {
  return !!document.querySelector("dialog[open]");
}

// Escape (the dialog's own) or a click outside closes a dialog; the close prompt only closes on
// Escape or its Cancel.
for (const id of ["repos", "filing"]) {
  const dialog = byId(id);
  dialog.addEventListener("click", (event) => {
    if (event.target !== dialog) return;
    const box = dialog.getBoundingClientRect();
    const inside = event.clientX >= box.left && event.clientX <= box.right && event.clientY >= box.top && event.clientY <= box.bottom;
    if (!inside) dialog.close();
  });
}

// "New work": edit, then review what will be filed, then file it.
function showFiling(step) {
  byId("file-edit").hidden = step !== "edit";
  byId("file-confirm").hidden = step !== "confirm";
}

function openNewWork() {
  const select = byId("file-repo");
  const chosen = select.value || ui.repo;
  select.replaceChildren(...(data.names ?? []).map((r) => el("option", { value: r }, r)));
  if ((data.names ?? []).includes(chosen)) select.value = chosen;
  byId("file-result").hidden = true;
  showFiling("edit");
  if (!byId("filing").open) byId("filing").showModal();
}

byId("new-work").addEventListener("click", openNewWork);
byId("file-cancel").addEventListener("click", () => byId("filing").close());

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
  const link = el("a", { href: filed.url }, filed.url);
  link.addEventListener("click", (event) => {
    event.preventDefault();
    openUrl(filed.url);
  });
  result.replaceChildren("Filed ", link);
  byId("file-title").value = "";
  byId("file-body").value = "";
  showFiling("edit");
});

// Repositories and the runs Desk starts in them.
function runLine(repo) {
  const runs = repo.runs || {};
  if (runs.desk) {
    const kind = runs.every ? "keeps running" : "runs one item";
    return `Paja Desk ${kind} here, started ${new Date(runs.started_ms).toLocaleTimeString()}.`;
  }
  if (runs.terminal) return `A paja run started elsewhere is working on #${runs.terminal}.`;
  return "Not running.";
}

const repoNotes = new Map();

function repoRow(repo) {
  const buttons = [];
  const act = (label, call, quiet) => {
    const button = el("button", { type: "button", className: quiet ? "quiet" : "" }, label);
    button.addEventListener("click", async () => {
      button.disabled = true;
      const result = await call().catch((err) => ({ error: String(err) }));
      button.disabled = false;
      if (result && result.error) repoNotes.set(repo.repo, `error: ${result.error}`);
      else if (result && result.checks) repoNotes.set(repo.repo, doctorText(result));
      else repoNotes.delete(repo.repo);
      refresh().catch(showError);
    });
    buttons.push(button);
  };
  const runs = repo.runs || {};
  if (runs.desk) {
    act("Stop after this step", () => invoke("run_stop", { repo: repo.repo, now: false }));
    act("Stop now", () => invoke("run_stop", { repo: repo.repo, now: true }), true);
  } else if (!runs.terminal && repo.checkout) {
    act("Run one item", () => invoke("run_start", { repo: repo.repo, every: false }));
    act("Keep running", () => invoke("run_start", { repo: repo.repo, every: true }));
  }
  if (repo.checkout) act("Check", () => invoke("repo_doctor", { repo: repo.repo }), true);
  const children = [
    el("div", { className: "row" }, [el("strong", {}, repo.repo), el("span", { className: "muted" }, repo.checkout ?? "checkout unknown: add it below")]),
    ...(repo.checkout_error ? [el("div", { className: "muted" }, "not confirmed with gh: " + repo.checkout_error)] : []),
    el("div", {}, runLine(repo)),
    el("div", { className: "actions" }, buttons),
  ];
  const note = repoNotes.get(repo.repo);
  if (note) children.push(el("pre", { className: note.startsWith("error") ? "error" : "" }, note));
  if (runs.output) children.push(el("pre", { className: "output" }, runs.output));
  return el("li", {}, children);
}

function doctorText(report) {
  const lines = report.checks.map((c) => `${c.status === "ok" ? "ok  " : c.status === "warning" ? "warn" : "FAIL"} ${c.name}: ${c.detail}${c.fix ? ` (${c.fix})` : ""}`);
  lines.unshift(report.ok ? "Ready to run." : "A check failed; runs won't start until it is fixed.");
  return lines.join("\n");
}

function renderRepoList() {
  byId("repo-list").replaceChildren(...data.repos.map(repoRow));
}

byId("show-repos").addEventListener("click", () => {
  renderRepoList();
  if (!byId("repos").open) byId("repos").showModal();
});

byId("repo-add").addEventListener("click", async () => {
  const path = byId("repo-path").value.trim();
  const out = byId("repo-result");
  if (!path) return;
  const result = await invoke("repo_add", { path }).catch((err) => ({ error: String(err) }));
  out.hidden = false;
  out.className = result && result.error ? "error" : "muted";
  out.textContent = result && result.error ? `error: ${result.error}` : `Added; ${(result.imported || []).length} new items.`;
  if (!result.error) byId("repo-path").value = "";
  refresh().catch(showError);
});

// Closing while Desk's runs are going asks what happens to them.
listen("paja://close-asked", (event) => {
  byId("closing-repos").textContent = (event.payload || []).join(", ");
  if (!byId("closing").open) byId("closing").showModal();
});
byId("close-leave").addEventListener("click", () => invoke("close_desk", { stop: false }).catch(showError));
// Desk waits for its runs to end (ending a stuck one after the grace period) before it closes.
byId("close-stop").addEventListener("click", () => {
  for (const id of ["close-leave", "close-stop", "close-cancel"]) byId(id).disabled = true;
  byId("close-stop").textContent = "Stopping the runs...";
  byId("closing-status").hidden = false;
  invoke("close_desk", { stop: true }).catch((err) => {
    for (const id of ["close-leave", "close-stop", "close-cancel"]) byId(id).disabled = false;
    byId("close-stop").textContent = "Stop them now and close";
    byId("closing-status").hidden = true;
    showError(err);
  });
});
byId("close-cancel").addEventListener("click", () => byId("closing").close());

// ---- Keyboard ---------------------------------------------------------------------------------

// n: new work, +: add a lane, h: held, Escape: close the run view (a dialog closes itself). None
// of them does anything while a text box has focus.
document.addEventListener("keydown", (event) => {
  if (event.ctrlKey || event.metaKey || event.altKey) return;
  const typing = event.target.matches?.("input, textarea, select, [contenteditable]");
  if (typing || dialogOpen()) return;
  if (event.key === "Escape" && ui.run != null) closeRun();
  else if (event.key === "n") {
    event.preventDefault();
    openNewWork();
  } else if (event.key === "+") changeLanes(1);
  else if (event.key === "h") showHeld();
});

// ---- About and start --------------------------------------------------------------------------

// Desk's version and the `paja` it found, with a warning when they differ.
async function showAbout() {
  const about = await invoke("about");
  if (about.error) throw new Error(about.error);
  const pajaText = about.paja ? `paja ${about.paja} (${about.paja_path})` : "no paja found next to Desk or on PATH";
  byId("about").textContent = `Paja Desk ${about.desk} · ${pajaText}`;
  const warning = byId("versions");
  if (!about.paja) {
    warning.textContent = "Desk found no paja program. Install Paja (or Desk's installer, which includes it) so runs and the plugin work.";
    warning.hidden = false;
  } else if (!about.matches) {
    warning.textContent = `Desk is ${about.desk} but paja is ${about.paja}. Install the same release of both.`;
    warning.hidden = false;
  }
}

listen("paja://events-changed", () => refresh().catch(showError));
setInterval(() => refresh().catch(showError), REFRESH_EVERY_MS);
showAbout().catch(showError);
refresh().catch(showError);
