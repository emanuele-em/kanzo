"use strict";

// The board runs on browsers back to Safari 13.1, Chrome 80 and Firefox 78. These
// two are newer than that, and are all it needs on top.
if (!Element.prototype.replaceChildren) {
  Element.prototype.replaceChildren = function (...nodes) {
    this.textContent = "";
    this.append(...nodes);
  };
}
if (!HTMLFormElement.prototype.requestSubmit) {
  HTMLFormElement.prototype.requestSubmit = function () {
    if (this.reportValidity()) this.dispatchEvent(new Event("submit", { cancelable: true }));
  };
}

const $ = (sel, root = document) => root.querySelector(sel);
const STATUS_COLORS = ["#8a8a83", "#3b82f6", "#e8a317", "#2f9e44", "#ae3ec9", "#e8590c"];

const state = {
  project: "all",
  board: null,
  changes: null,
  task: null,
  dragging: null,
  dropped: false,
  layout: "",
  allDone: false,
  menu: null,
  net: null,
  guests: null,
  selected: new Set(),
  sweeping: false,
};

async function api(method, path, body) {
  const image = body instanceof Blob;
  const res = await fetch("/api" + path, {
    method,
    headers: body ? { "Content-Type": image ? body.type : "application/json" } : {},
    body: image || !body ? body : JSON.stringify(body),
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw Object.assign(new Error(data.error || res.statusText), { data });
  return data;
}

function plural(n, word) {
  return `${n} ${word}${n === 1 ? "" : "s"}`;
}

// Storage can be off, as in private windows.
function localStorageGet(k) {
  try { return localStorage.getItem(k); } catch { return null; }
}

function localStorageSet(k, v) {
  try { localStorage.setItem(k, v); } catch {}
}

function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k.startsWith("on")) node.addEventListener(k.slice(2), v);
    else if (k === "class") node.className = v;
    else node.setAttribute(k, v === true ? "" : v);
  }
  node.append(...children.flat().filter((c) => c != null && c !== false));
  return node;
}

// From Lucide, ISC licence.
const ICONS = {
  newProject: '<path d="M12 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"/><path d="M18.4 2.6a2.1 2.1 0 0 1 3 3L12 15l-4 1 1-4Z"/>',
  board: '<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M8 7v7M12 7v4M16 7v9"/>',
  folder: '<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>',
  search: '<circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/>',
  close: '<path d="M18 6 6 18M6 6l12 12"/>',
  more: '<circle cx="5" cy="12" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/>',
  plus: '<path d="M5 12h14M12 5v14"/>',
  list: '<path d="M4 6h16M4 12h16M4 18h16"/>',
  trash: '<path d="M3 6h18M8 6V4h8v2M19 6l-1 14H6L5 6M10 11v6M14 11v6"/>',
  check: '<path d="M20 6 9 17l-5-5"/>',
  copy: '<rect width="14" height="14" x="8" y="8" rx="2"/><path d="M4 16a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2"/>',
  human: '<circle cx="12" cy="8" r="4"/><path d="M5 21v-1a6 6 0 0 1 6-6h2a6 6 0 0 1 6 6v1"/>',
  url: '<path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/>',
  github: '<path d="M15 22v-4a4.8 4.8 0 0 0-1-3.5c3 0 6-2 6-5.5.1-1.3-.3-2.5-1-3.5.3-1.2.3-2.4 0-3.5 0 0-1 0-3 1.5a12 12 0 0 0-8 0C6 2 5 2 5 2c-.3 1.2-.3 2.4 0 3.5A5.4 5.4 0 0 0 4 9c0 3.5 3 5.5 6 5.5-.4.5-.7 1-.8 1.7-.2.6-.3 1.2-.2 1.8v4"/><path d="M9 18c-4.5 2-5-2-7-2"/>',
  sync: '<path d="M12 13v8"/><path d="M4 14.9A7 7 0 1 1 15.7 8h1.8a4.5 4.5 0 0 1 2.5 8.2"/><path d="m8 17 4-4 4 4"/>',
  collapse: '<path d="m13 17 5-5-5-5M6 17l5-5-5-5"/>',
  expand: '<path d="m11 17-5-5 5-5M18 17l-5-5 5-5"/>',
  rename: '<path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z"/>',
  update: '<circle cx="12" cy="12" r="9"/><path d="M12 7v9M8 12l4 4 4-4"/>',
  settings: '<path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/>',
  send: '<path d="M12 19V5M5 12l7-7 7 7"/>',
  info: '<circle cx="12" cy="12" r="9"/><path d="M12 16v-4M12 8h.01"/>',
  sidebar: '<rect x="3" y="4" width="18" height="16" rx="4"/><path d="M9 4v16"/>',
  news: '<path d="M12 3l2.6 5.6 6.1.7-4.5 4.2 1.2 6-5.4-3-5.4 3 1.2-6-4.5-4.2 6.1-.7z"/>',
  comment: '<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/>',
  share: '<circle cx="18" cy="5" r="3"/><circle cx="6" cy="12" r="3"/><circle cx="18" cy="19" r="3"/><path d="m8.6 13.5 6.8 4M15.4 6.5l-6.8 4"/>',
};

function icon(name, color) {
  const span = el("span", { class: "icon", style: color ? `color:${color}` : null });
  span.innerHTML = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${ICONS[name]}</svg>`;
  return span;
}

document.querySelectorAll("[data-icon]").forEach((n) => n.replaceWith(icon(n.dataset.icon)));

let toastTimer;
// The clipboard API needs a secure page; a board opened from another device over plain
// HTTP falls back.
async function copyKey(text) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const area = el("textarea", { style: "position:fixed;opacity:0" });
    area.value = text;
    document.body.append(area);
    area.select();
    document.execCommand("copy");
    area.remove();
  }
  toast(`Copied ${text}`);
}

function toast(message) {
  document.querySelector(".toast")?.remove();
  const t = el("div", { class: "toast" }, message);
  document.body.append(t);
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.remove(), 3500);
}

// The names match PROJECT_COLORS in store.rs.
const PROJECT_COLORS = {
  gray: "#8e8e93", red: "#ef4444", orange: "#f97316", yellow: "#eab308", green: "#22c55e",
  teal: "#14b8a6", blue: "#3b82f6", purple: "#a855f7", pink: "#ec4899",
};

function projectColorName(slug) {
  const picked = state.board?.projects.find((p) => p.slug === slug)?.color;
  if (PROJECT_COLORS[picked]) return picked;
  const names = Object.keys(PROJECT_COLORS).filter((n) => n !== "gray");
  let h = 0;
  for (const c of slug) h = (h * 31 + c.charCodeAt(0)) % 997;
  return names[h % names.length];
}

function projectColor(slug) {
  return PROJECT_COLORS[projectColorName(slug)];
}

function statusStep(status) {
  const { statuses } = state.board;
  const i = statuses.indexOf(status);
  return i < 0 ? 0 : i - statuses.indexOf("todo");
}

function statusColor(status) {
  const i = state.board ? statusStep(status) : 0;
  return STATUS_COLORS[Math.max(i, 0) % STATUS_COLORS.length];
}

function statusIcon(status) {
  const step = statusStep(status), steps = statusStep("done");
  const f = Math.max(step, 0) / steps;
  const color = statusColor(status);
  const dash = step < 0 ? ' stroke-dasharray="2.36 2.36"' : "";
  let inside = "";
  if (f >= 1) {
    inside = `<circle cx="7" cy="7" r="6" fill="${color}"/>` +
      '<path d="m4.4 7.2 1.8 1.8 3.4-3.6" fill="none" stroke="var(--bg)" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>';
  } else if (f > 0) {
    const r = 3.4, a = f * 2 * Math.PI;
    const x = (7 + r * Math.sin(a)).toFixed(2), y = (7 - r * Math.cos(a)).toFixed(2);
    inside = `<path d="M7 7L7 ${7 - r}A${r} ${r} 0 ${f > 0.5 ? 1 : 0} 1 ${x} ${y}Z" fill="${color}"/>`;
  }
  const span = el("span", { class: "status-icon" });
  span.innerHTML = `<svg viewBox="0 0 14 14" aria-hidden="true"><circle cx="7" cy="7" r="6" fill="none" stroke="${color}" stroke-width="1.5"${dash}/>${inside}</svg>`;
  return span;
}

const key = (t) => `${t.project}#${t.id}`;

function parseTime(ts) {
  return new Date(ts.replace(" ", "T"));
}

function ago(ts) {
  const s = (Date.now() - parseTime(ts)) / 1000;
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  if (s < 86400 * 7) return `${Math.floor(s / 86400)}d ago`;
  return parseTime(ts).toLocaleDateString();
}

function timeEl(ts) {
  return el("time", { datetime: ts, title: parseTime(ts).toLocaleString() }, ago(ts));
}

function escapeHtml(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

// Code spans, links and bare URLs are swapped for placeholders as soon as they are
// rendered, so later rules never see, or rewrite, the HTML they produced. The input
// is already HTML-escaped, so it holds no tags or raw quotes of its own.
function inline(s) {
  const saved = [];
  const keep = (html) => `\u0000${saved.push(html) - 1}\u0000`;
  const link = (url, text) => keep(`<a href="${url}" target="_blank" rel="noopener">${text}</a>`);
  s = s
    .replace(/\u0000/g, "")
    .replace(/`([^`]+)`/g, (_, code) => keep(`<code>${code}</code>`))
    .replace(/\[([^\]]+)\]\(((?:https?:\/\/|mailto:|file:\/\/\/)[^\s)\u0000]+)\)/g, (_, text, url) => link(url, emphasis(text)))
    .replace(/(^|[\s(])(https?:\/\/[^\s)\u0000]+)/g, (_, before, url) => before + link(url, url));
  const restore = (html) => html.replace(/\u0000(\d+)\u0000/g, (_, i) => restore(saved[i]));
  return restore(emphasis(s));
}

function emphasis(s) {
  return s
    .replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>")
    .replace(/(^|[^\w*])\*(?!\s)(.+?)\*(?!\w)/g, "$1<em>$2</em>")
    .replace(/(^|[^\w])_(?!\s)(.+?)_(?!\w)/g, "$1<em>$2</em>")
    .replace(/~~(.+?)~~/g, "<del>$1</del>");
}

function markdown(src) {
  const lines = escapeHtml(src.replace(/\r\n?/g, "\n")).split("\n");
  const out = [];
  let para = [];
  let list = null;
  const flushPara = () => { if (para.length) out.push(`<p>${inline(para.join("<br>"))}</p>`); para = []; };
  const flushList = () => {
    if (!list) return;
    out.push(`<${list.type}>${list.items.join("")}</${list.type}>`);
    list = null;
  };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    let m;
    if ((m = line.match(/^\s*```/))) {
      flushPara(); flushList();
      const code = [];
      while (++i < lines.length && !/^\s*```/.test(lines[i])) code.push(lines[i]);
      out.push(`<pre><code>${code.join("\n")}</code></pre>`);
    } else if ((m = line.match(/^(#{1,6})\s+(.*)$/))) {
      flushPara(); flushList();
      const n = Math.min(m[1].length, 4);
      out.push(`<h${n}>${inline(m[2])}</h${n}>`);
    } else if ((m = line.match(/^\s*(?:[-*+]|(\d+)[.)])\s+(.*)$/))) {
      flushPara();
      const type = m[1] ? "ol" : "ul";
      if (list && list.type !== type) flushList();
      list = list || { type, items: [] };
      const check = m[2].match(/^\[( |x|X)\]\s+(.*)$/);
      list.items.push(check
        ? `<li class="check"><input type="checkbox" disabled ${check[1] === " " ? "" : "checked"}> ${inline(check[2])}</li>`
        : `<li>${inline(m[2])}</li>`);
    } else if ((m = line.match(/^&gt;\s?(.*)$/))) {
      flushPara(); flushList();
      const quote = [m[1]];
      while (i + 1 < lines.length && (m = lines[i + 1].match(/^&gt;\s?(.*)$/))) { quote.push(m[1]); i++; }
      out.push(`<blockquote>${inline(quote.join("<br>"))}</blockquote>`);
    } else if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(line)) {
      flushPara(); flushList();
      out.push("<hr>");
    } else if (line.trim() === "") {
      flushPara(); flushList();
    } else if (list && /^\s{2,}\S/.test(line)) {
      list.items[list.items.length - 1] = list.items[list.items.length - 1].replace(/<\/li>$/, ` ${inline(line.trim())}</li>`);
    } else {
      flushList();
      para.push(line);
    }
  }
  flushPara(); flushList();
  return out.join("");
}

// Loads can overlap (a poll, a drop, an edit); only the newest one may render,
// so a slow older reply never undoes what is on screen.
let boardRequest = 0;

async function loadBoard() {
  const request = ++boardRequest;
  try {
    const done = state.allDone ? "&done=all" : "";
    const q = filterQuery();
    const board = await api("GET", "/board?project=" + encodeURIComponent(state.project) + done
                            + (q ? "&q=" + encodeURIComponent(q) : ""));
    if (request !== boardRequest) return;
    state.board = board;
    document.documentElement.dataset.hide = (board.hidden_properties || []).join(" ");
    state.changes = board.changes;
    $("#offline").hidden = true;
    render();
  } catch (e) {
    if (request !== boardRequest) return;
    if (state.project !== "all" && /no project/.test(e.message)) {
      setProject("all");
    } else {
      toast(e.message);
    }
  }
}

function updateHash() {
  const hash = "#" + state.project + (state.task ? "/" + state.task : "");
  if (location.hash !== hash) history.replaceState(null, "", hash);
}

function readHash() {
  let hash = location.hash.slice(1);
  try { hash = decodeURIComponent(hash); } catch {}
  const [slug, task] = hash.split("/");
  return { slug: slug || null, task: Number(task) || null };
}

function setProject(slug) {
  state.project = slug;
  localStorageSet("kanzo.project", slug);
  updateHash();
  state.layout = "";
  loadBoard();
}

function render() {
  renderNav();
  renderHeader();
  renderColumns();
}

function renderNav() {
  // Redrawing would take the field away from someone renaming a project.
  if (renaming && document.contains(renaming.input)) return;
  // Nor while dragging a project: it would cancel the drag.
  if (movingProject) return;
  const { projects } = state.board;
  const open = projects.reduce((n, p) => n + p.open, 0);
  const link = (slug, name, count, glyph) =>
    el("a", { href: "#" + slug, title: name, draggable: "false", class: state.project === slug ? "active" : null,
              onclick: (e) => { e.preventDefault(); setProject(slug); } },
      glyph,
      el("span", { class: "name" }, name),
      el("span", { class: "count" }, String(count)));
  const item = (p) => renaming && renaming.slug === p.slug
    ? el("div", { class: "item renaming" }, icon("folder", projectColor(p.slug)), renaming.input)
    : el("div", { class: "item" + (state.menu === p.slug ? " menu-open" : ""), draggable: "true", "data-slug": p.slug },
    link(p.slug, p.name, p.open, icon("folder", projectColor(p.slug))),
    el("button", { class: "more", title: "More", "aria-label": `More for ${p.name}`, "aria-haspopup": "menu",
                   onclick: (e) => toggleProjectMenu(p, e.currentTarget) }, icon("more")));
  $("#nav").replaceChildren(
    link("all", "All projects", open, icon("board")),
    ...(projects.length ? [el("div", { class: "label" }, "Projects")] : []),
    ...projects.map(item),
  );
}

// `state.menu` holds the slug, so the row keeps its highlight while polling redraws the sidebar.
function toggleProjectMenu(p, button) {
  if (!toggleMenu(p.slug)) return;
  button.closest(".item").classList.add("menu-open");
  const swatches = el("div", { class: "swatches", role: "group", "aria-label": "Color" });
  const drawSwatches = () => swatches.replaceChildren(...Object.entries(PROJECT_COLORS).map(([name, hex]) => {
    const current = projectColorName(p.slug) === name;
    return el("button", { class: "swatch", style: `background:${hex}`, title: name, "aria-label": name,
                          "aria-pressed": String(current), onclick: () => pickColor(p.slug, name).then(drawSwatches) },
      current ? icon("check") : null);
  }));
  drawSwatches();
  const repos = el("div", { class: "repos" });
  const drawRepos = (list, focus) => {
    const input = el("input", { class: "field", placeholder: "Link a repo: owner/repo", "aria-label": "Link a GitHub repo",
                                autocomplete: "off", spellcheck: "false" });
    input.addEventListener("keydown", async (e) => {
      if (e.key !== "Enter" || !input.value.trim()) return;
      const q = await changeRepo(p.slug, "POST", input.value);
      if (q) drawRepos(q.repos, true);
    });
    repos.replaceChildren(...list.map((r) => el("div", { class: "repo" }, el("span", { class: "name" }, r),
      el("button", { class: "unlink", title: `Unlink ${r}`, "aria-label": `Unlink ${r}`,
                     onclick: async () => { const q = await changeRepo(p.slug, "DELETE", r); if (q) drawRepos(q.repos); } },
        icon("close")))), input);
    if (focus) input.focus();
  };
  drawRepos(p.repos);
  const rename = el("button", { onclick: () => startRename(p) }, icon("rename"), "Rename");
  const remove = el("button", { class: "danger", onclick: () => { closeMenu(); deleteProject(p); } },
    icon("trash"), "Delete project");
  const menu = el("div", { class: "menu", role: "menu", tabindex: "-1" },
    el("div", { class: "label" }, "Color"), swatches, el("div", { class: "label" }, "Repos"), repos,
    el("div", { class: "sep" }), rename, remove);
  document.body.append(menu);
  const r = button.getBoundingClientRect();
  const below = r.bottom + 4 + menu.offsetHeight <= innerHeight;
  menu.style.top = `${below ? r.bottom + 4 : r.top - 4 - menu.offsetHeight}px`;
  menu.style.left = `${Math.max(8, Math.min(r.left, innerWidth - menu.offsetWidth - 8))}px`;
  // Focus the menu itself, so Tab reaches its buttons without one looking selected.
  menu.focus();
}

let renaming = null;
let movingProject = null;
const projectLine = el("div", { class: "drop-line" });
const projectAfter = (y) => [...$("#nav").querySelectorAll("[data-slug]")]
  .find((i) => { const r = i.getBoundingClientRect(); return y < r.top + r.height / 2; }) || null;

$("#nav").addEventListener("dragstart", (e) => {
  // Or it's text dragged out of the field renaming a project.
  movingProject = e.target.dataset?.slug;
  if (!movingProject) return;
  e.dataTransfer.effectAllowed = "move";
  e.dataTransfer.setData("text/plain", movingProject);
});
$("#nav").addEventListener("dragover", (e) => {
  if (!movingProject) return;
  e.preventDefault();
  $("#nav").insertBefore(projectLine, projectAfter(e.clientY));
});
$("#nav").addEventListener("dragleave", (e) => { if (!$("#nav").contains(e.relatedTarget)) projectLine.remove(); });
$("#nav").addEventListener("drop", async (e) => {
  if (!movingProject) return;
  e.preventDefault();
  const before = projectAfter(e.clientY)?.dataset.slug || null;
  try { await api("POST", `/projects/${encodeURIComponent(movingProject)}/move`, { before }); } catch (err) { toast(err.message); }
  loadBoard();
});
$("#nav").addEventListener("dragend", () => {
  movingProject = null;
  projectLine.remove();
  renderNav();
});

function startRename(p) {
  closeMenu();
  const input = el("input", { class: "field", "aria-label": `Rename ${p.name}`, spellcheck: "false", autocomplete: "off" });
  input.value = p.name;
  renaming = { slug: p.slug, input };
  let finished = false;
  const finish = async (save) => {
    if (finished) return;
    finished = true;
    const name = input.value.trim();
    renaming = null;
    if (save && name && name !== p.name) {
      try { await api("PATCH", `/projects/${encodeURIComponent(p.slug)}`, { name }); } catch (err) { toast(err.message); }
    }
    loadBoard();
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") { e.preventDefault(); finish(true); }
    else if (e.key === "Escape") { e.stopPropagation(); finish(false); }
  });
  input.addEventListener("blur", () => finish(true));
  renderNav();
  input.focus();
  input.select();
}

async function changeRepo(slug, method, repo) {
  try {
    const p = await api(method, `/projects/${encodeURIComponent(slug)}/repos`, { repo });
    loadBoard();
    return p;
  } catch (err) { toast(err.message); return null; }
}

async function pickColor(slug, color) {
  try {
    await api("PATCH", `/projects/${encodeURIComponent(slug)}`, { color });
    await loadBoard();
  } catch (err) { toast(err.message); }
}

function toggleMenu(name) {
  const wasOpen = state.menu === name;
  closeMenu();
  if (wasOpen) return false;
  state.menu = name;
  return true;
}

function closeMenu() {
  if (!state.menu) return;
  state.menu = null;
  document.querySelector(".menu")?.remove();
  document.querySelectorAll(".nav .menu-open").forEach((i) => i.classList.remove("menu-open"));
  $("#settings").classList.remove("open");
  $("#network-menu").hidden = true;
  $("#network").classList.remove("open");
}

// A click on a menu's own button is left to toggle it.
document.addEventListener("mousedown", (e) => {
  if (!e.target.closest(".menu, .nav .more, #settings, #network, #network-menu")) closeMenu();
});
document.addEventListener("mousedown", (e) => {
  if (state.task && e.button === 0 && !e.target.closest("#panel, .card, .menu, .search-dialog")) closeTask();
});
window.addEventListener("resize", closeMenu);
window.addEventListener("blur", closeMenu);
$("#nav").addEventListener("scroll", closeMenu);

function currentProject() {
  return state.board.projects.find((p) => p.slug === state.project) || null;
}

function renderHeader() {
  const p = currentProject();
  const { projects, tasks } = state.board;
  const header = $("#header");
  $("#new-task").hidden = !projects.length;
  $("#human-only").hidden = !projects.length || isPropertyHidden("human");
  $("#filter-box").hidden = !projects.length;
  $("#view").hidden = !projects.length;
  $("#view").setAttribute("aria-pressed", String(isList()));
  $("#filters").replaceChildren(...(projects.length ? savedFilters() : []).map((f, i) =>
    el("span", { class: "chip" }, el("span", {}, queryNodes(f)),
      el("button", { type: "button", title: `Remove "${f}"`, "aria-label": `Remove the filter ${f}`,
                     onclick: () => setFilters(savedFilters().filter((_, j) => j !== i)) }, icon("close")))));
  $("#human-only").setAttribute("aria-pressed", String(isHumanOnly()));
  if (!projects.length) { header.replaceChildren(); return; }
  const open = (n) => plural(n, "open task");
  if (!p) {
    const count = projects.reduce((n, p) => n + p.open, 0);
    header.replaceChildren(el("h1", {}, "All projects"),
      el("span", { class: "sub" }, `${open(count)} in ${plural(projects.length, "project")}`));
    return;
  }
  header.replaceChildren(
    el("h1", {}, p.name),
    el("span", { class: "sub" }, open(p.open)),
    ...p.repos.map((r) => el("a", { class: "repo-link", href: `https://github.com/${r}`, target: "_blank", rel: "noopener",
                                    title: `Open ${r} on GitHub` }, icon("github"), r)),
  );
}

async function deleteProject(p) {
  const tasks = p.total ? ` and its ${plural(p.total, "task")}, with their comments` : "";
  if (!confirm(`Delete project ${p.slug}${tasks}? This cannot be undone.`)) return;
  try {
    await api("DELETE", `/projects/${encodeURIComponent(p.slug)}`);
    if (panelTask?.project === p.slug) { fresh = null; closeTask(); }
    if (state.project === p.slug) setProject("all"); else loadBoard();
  } catch (err) { toast(err.message); }
}

function renderColumns() {
  const { statuses, projects, tasks } = state.board;
  const columns = $("#columns");
  const inList = isList() && projects.length > 0;
  columns.hidden = inList;
  $("#list").hidden = $("#list-tools").hidden = !inList;
  if (!projects.length) {
    state.layout = "";
    columns.style.display = "flex";
    columns.replaceChildren(el("div", { class: "empty" },
      el("h1", {}, "Welcome to Kanzo"),
      el("p", {}, "Create your first project in the sidebar. Link it to a GitHub repo and agents working in that repo will find it on their own."),
      el("button", { class: "btn primary", onclick: showNewProject }, "New project")));
    return;
  }
  columns.style.display = "";

  // Build the columns once per layout so a refresh never wipes a half-typed task.
  const layout = statuses.join(",") + "|" + state.project;
  if (state.layout !== layout) {
    state.layout = layout;
    columns.replaceChildren(...statuses.map((status) => {
      const collapsible = status === "backlog" || status === "done";
      const col = el("section", { class: "column", "data-status": status },
        el("h2", {},
          statusIcon(status), status,
          el("span", { class: "count" }),
          collapsible ? el("button", { class: "collapse", onclick: (e) => { e.stopPropagation(); setCollapsed(status, !isCollapsed(status)); } }) : null),
        el("div", { class: "cards" }),
      );
      wireDrop(col, col.querySelector(".cards"), status);
      if (collapsible) col.addEventListener("click", () => { if (isCollapsed(status)) setCollapsed(status, false); });
      return col;
    }));
    showCollapsed();
    $("#bulk-status").replaceChildren(el("option", { value: "", hidden: true }, "Move to…"), ...statuses.map((s) => el("option", { value: s }, s)));
  }

  // During a drag the DOM is being moved by hand; refresh once it ends.
  if (state.dragging || state.sweeping) return;
  const all = state.project === "all";
  const humanOnly = isHumanOnly();
  const query = filterQuery();
  $("#filter").classList.toggle("on", query.length > 0);
  // Filtered, the server's count of older done tasks says nothing about human ones.
  const filtered = humanOnly || query.length > 0;
  const hiddenDone = state.allDone ? 0 : state.board.done_hidden || 0;
  const more = hiddenDone
    ? el("button", { class: "ghost more", onclick: () => { state.allDone = true; loadBoard(); } },
         filtered ? "Show older" : `Show ${hiddenDone} older`)
    : null;
  if (inList) {
    renderList(tasks.filter((t) => !humanOnly || t.human), all, more);
  } else {
    for (const status of statuses) {
      const col = columns.querySelector(`[data-status="${CSS.escape(status)}"]`);
      const mine = tasks.filter((t) => t.status === status && (!humanOnly || t.human));
      const last = status === statuses[statuses.length - 1];
      const count = mine.length + (!last || filtered ? 0 : hiddenDone);
      col.querySelector("h2 .count").textContent = count ? String(count) : "";
      col.querySelector(".cards").replaceChildren(...(last && more ? [more] : []), ...mine.map((t) => card(t, all)));
    }
  }
  highlight("filter", inList ? $("#list") : columns, wordsOf(query));
  for (const id of [...state.selected]) {
    if (!document.querySelector(`.card[data-id="${id}"]`)) state.selected.delete(id);
  }
  showSelection();
}

function isList() {
  return localStorageGet("kanzo.view") === "list";
}

// Only the view on screen is rendered: every refresh would otherwise build both.
$("#view").addEventListener("click", () => {
  localStorageSet("kanzo.view", isList() ? "" : "list");
  state.layout = "";
  render();
});
for (const [select, name] of [[$("#list-sort"), "kanzo.sort"], [$("#list-group"), "kanzo.group"]]) {
  select.value = localStorageGet(name) || "";
  select.addEventListener("change", () => { localStorageSet(name, select.value); renderColumns(); });
}

function renderList(tasks, all, more) {
  const { statuses, projects } = state.board;
  const by = $("#list-sort").value, group = $("#list-group").value;
  const sorted = !by ? tasks : [...tasks].sort((a, b) =>
    by === "id" ? a.id - b.id : by === "title" ? a.title.localeCompare(b.title) : b[by].localeCompare(a[by]));
  const groups = new Map((group === "status" ? statuses : group === "project" ? projects.map((p) => p.slug) : []).map((g) => [g, []]));
  for (const t of sorted) {
    for (const g of group === "tags" ? (t.tags.length ? t.tags : [""]) : [group ? t[group] : ""]) {
      if (!groups.has(g)) groups.set(g, []);
      groups.get(g).push(t);
    }
  }
  const names = group === "tags" ? [...groups.keys()].sort((a, b) => !a - !b || a.localeCompare(b)) : [...groups.keys()];
  $("#list").replaceChildren(...names.flatMap((g) => !groups.get(g).length ? [] : [
    group ? el("h2", { style: group === "status" ? "text-transform:capitalize" : null }, g || "No tag", el("span", { class: "count" }, String(groups.get(g).length))) : null,
    ...groups.get(g).map((t) => card(t, all, true)),
  ]).filter(Boolean), ...(more ? [more] : []));
}

function showSelection() {
  $("#bulk").hidden = !state.selected.size;
  $("#bulk-count").textContent = `${state.selected.size} selected`;
}

function setSelected(id, on) {
  if (on) state.selected.add(id); else state.selected.delete(id);
  // Grouped by tag, a task can have several rows.
  document.querySelectorAll(`.card[data-id="${id}"]`).forEach((n) => n.classList.toggle("selected", on));
}

function clearSelection() {
  state.selected.forEach((id) => setSelected(id, false));
  showSelection();
}

// Closes the panel: one or the other, so the bar can't cover it.
function toggleSelected(id) {
  if (state.task) closeTask();
  setSelected(id, !state.selected.has(id));
  showSelection();
}

// Drag on empty board space to select, with ⌘ to add to what's selected. Cards are measured once, when
// the drag starts, so moving the mouse never forces a layout.
function sweep(down) {
  const marquee = el("div", { class: "marquee" });
  const kept = new Set(down.metaKey || down.ctrlKey ? state.selected : []);
  let cards, x, y, frame = 0;
  const paint = () => {
    frame = 0;
    const x1 = Math.min(down.clientX, x), x2 = Math.max(down.clientX, x), y1 = Math.min(down.clientY, y), y2 = Math.max(down.clientY, y);
    marquee.style.cssText = `left:${x1}px;top:${y1}px;width:${x2 - x1}px;height:${y2 - y1}px`;
    for (const { id, r, list } of cards) {
      // A card scrolled out of its column can't be under the box.
      const hit = r.right > x1 && r.left < x2 && r.bottom > Math.max(y1, list.top) && r.top < Math.min(y2, list.bottom);
      if ((hit || kept.has(id)) !== state.selected.has(id)) setSelected(id, !state.selected.has(id));
    }
    showSelection();
  };
  const move = (e) => {
    x = e.clientX; y = e.clientY;
    if (!cards) {
      if (Math.hypot(x - down.clientX, y - down.clientY) < 4) return;
      state.sweeping = true;
      document.body.append(marquee);
      cards = [...$("#columns").querySelectorAll(".column:not(.collapsed) .card")].map((n) =>
        ({ id: +n.dataset.id, r: n.getBoundingClientRect(), list: n.parentNode.getBoundingClientRect() }));
    }
    if (!frame) frame = requestAnimationFrame(paint);
  };
  const up = () => {
    cancelAnimationFrame(frame);
    document.removeEventListener("mousemove", move);
    document.removeEventListener("mouseup", up);
    marquee.remove();
    state.sweeping = false;
    if (cards) renderColumns();
    else if (!kept.size) clearSelection();
  };
  document.addEventListener("mousemove", move);
  document.addEventListener("mouseup", up);
}

$("#columns").addEventListener("mousedown", (e) => {
  const onScrollbar = e.offsetX > e.target.clientWidth || e.offsetY > e.target.clientHeight;
  if (e.button || onScrollbar || e.target.closest(".card, button, a, .collapsed")) return;
  e.preventDefault();
  sweep(e);
});

async function changeSelected(change) {
  const ids = state.board.tasks.filter((t) => state.selected.has(t.id)).map((t) => t.id);
  try { await api("POST", "/tasks/bulk", { ids, ...change }); } catch (err) { toast(err.message); }
  loadBoard();
}

$("#bulk").addEventListener("keydown", (e) => {
  const { name, value } = e.target;
  if (e.key === "Escape") { e.stopPropagation(); e.target.blur(); clearSelection(); }
  else if (e.key === "Enter" && name && value.trim()) { changeSelected({ [name]: value }); e.target.value = ""; }
});
$("#bulk-status").addEventListener("change", (e) => {
  changeSelected({ status: e.target.value });
  e.target.value = "";
});
$("#bulk-delete").addEventListener("click", () => {
  if (confirm(`Delete ${plural(state.selected.size, "task")}, with comments and images? This cannot be undone.`)) changeSelected({ delete: true });
});
$("#bulk-clear").addEventListener("click", clearSelection);

function savedFilters() {
  try { return JSON.parse(localStorageGet("kanzo.filters")) || []; } catch { return []; }
}

function setFilters(filters) {
  localStorageSet("kanzo.filters", JSON.stringify(filters));
  loadBoard();
}

function filterQuery() {
  return [...savedFilters(), $("#filter").value].join(" ").trim();
}

const anyText = () => true;
const isNumber = (v) => /^[+-]?\d+$/.test(v);
const isDate = (v) => /^\d{4}-\d{2}-\d{2}$/.test(v);
const inRange = (side) => (v) => {
  const ends = v.split("..");
  return ends.length > 1 ? ends.length === 2 && ends.every(side) : side(v.replace(/^(>=|<=|>|<)/, ""));
};
const isField = (v) => ["description", "tags", "url", "comments", "images"].includes(v);
// What the server takes as each qualifier's value: anything else makes it a plain word.
const QUALIFIERS = new Map([
  ["tag", anyText], ["status", anyText], ["project", anyText], ["title", anyText], ["description", anyText],
  ["url", anyText], ["comment", anyText], ["id", inRange(isNumber)], ["comments", inRange(isNumber)],
  ["images", inRange(isNumber)], ["created", inRange(isDate)], ["updated", inRange(isDate)],
  ["is", (v) => v === "human" || v === "waiting"], ["has", isField], ["no", isField],
]);

// A part of a query, read as the server reads it. Null for a word.
function qualifierIn(part) {
  const match = !part.startsWith('"') && part.replace(/"/g, "").toLowerCase().match(/^(-?)([^:]+):(.*)$/);
  const valid = match && QUALIFIERS.get(match[2]);
  if (!valid) return null;
  const [, negated, name, value] = match;
  // With no value yet it's still being typed; with a wrong one, it's a word.
  return !value || valid(value) ? { negated: negated === "-", name, value } : null;
}

// The text to highlight on cards: words, phrases, and what tag:, title: and description: look for.
function wordsOf(query) {
  const words = [];
  for (const part of query.match(/(?:[^\s"]+|"[^"]*"?)+/g) || []) {
    const qualifier = qualifierIn(part);
    if (!qualifier) words.push(part.replace(/"/g, "").toLowerCase());
    else if (!qualifier.negated && ["tag", "title", "description"].includes(qualifier.name)) words.push(qualifier.value);
  }
  return words.filter(Boolean);
}

// A query as text, with its qualifiers marked. Keeps every character, for .query-mirror to line up.
function queryNodes(query) {
  return (query.match(/\s+|(?:[^\s"]+|"[^"]*"?)+/g) || []).flatMap((part) => {
    if (!qualifierIn(part)) return part;
    const key = part.indexOf(":") + 1;
    return [el("span", { class: "qualifier-key" }, part.slice(0, key)), part.slice(key)];
  });
}

// An input can't color part of its text, so a copy behind it shows it, and the input keeps the caret.
function paintQuery(input) {
  const mirror = input.previousElementSibling;
  mirror.firstElementChild.replaceChildren(...queryNodes(input.value));
  mirror.scrollLeft = input.scrollLeft;
}

for (const input of document.querySelectorAll(".query input")) {
  for (const type of ["input", "scroll", "keyup", "click"]) input.addEventListener(type, () => paintQuery(input));
}

function highlight(name, root, words) {
  if (!window.Highlight) return;
  const re = new RegExp(words.map((w) => w.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|"), "gi");
  const ranges = [];
  for (const node of words.length ? root.querySelectorAll(".title, .desc, .tag") : []) {
    const text = node.firstChild;
    for (const m of text?.nodeType === Node.TEXT_NODE ? text.data.matchAll(re) : []) {
      const range = new Range();
      range.setStart(text, m.index);
      range.setEnd(text, m.index + m[0].length);
      ranges.push(range);
    }
  }
  CSS.highlights.set(name, new Highlight(...ranges));
}

function summary(t, ...extra) {
  const chips = [...extra.filter(Boolean), ...(isPropertyHidden("tags") ? [] : t.tags.map((tag) => el("span", { class: "tag" }, tag)))];
  return [
    t.description ? el("span", { class: "desc" }, plainText(t.description)) : null,
    chips.length ? el("span", { class: "tags" }, chips) : null,
  ];
}

// Only on cards: search rows are buttons, and a link can't sit inside one.
function urlChip(t) {
  if (!t.url || isPropertyHidden("url")) return null;
  return el("a", { class: "tag url", href: t.url, target: "_blank", rel: "noopener", title: t.url, draggable: "false",
                   onclick: (e) => e.stopPropagation() },
    icon("url"), t.url.split("/")[2].replace(/^www\./, ""));
}

// Only on cards, like the URL. Most tasks have no waiting_on at all, to keep the board's reply small.
function waitChip(t) {
  return t.waiting_on ? el("span", { class: "tag waits" }, `Waits on ${t.waiting_on.map((id) => `#${id}`).join(", ")}`) : null;
}

// A board on an older version sends no hidden_properties.
function isPropertyHidden(property) {
  return (state.board?.hidden_properties || []).includes(property);
}

function isHumanOnly() {
  return localStorageGet("kanzo.humanOnly") === "1" && !isPropertyHidden("human");
}

function setHumanOnly(humanOnly) {
  localStorageSet("kanzo.humanOnly", humanOnly ? "1" : "0");
  render();
}

function isCollapsed(status) {
  return localStorageGet(`kanzo.${status}Collapsed`) !== "0";
}

function setCollapsed(status, collapsed) {
  localStorageSet(`kanzo.${status}Collapsed`, collapsed ? "1" : "0");
  showCollapsed();
}

function showCollapsed() {
  const columns = $("#columns");
  const cols = [...columns.querySelectorAll(".column")];
  if (!cols.length) return;
  for (const col of cols) {
    const button = col.querySelector(".collapse");
    if (!button) continue;
    const { status } = col.dataset;
    const collapsed = isCollapsed(status);
    col.classList.toggle("collapsed", collapsed);
    // The arrows point the way the column folds: to the right edge, or for backlog,
    // to the left one.
    const left = status === "backlog";
    button.replaceChildren(icon(collapsed !== left ? "expand" : "collapse"));
    button.title = collapsed ? `Show ${status}` : `Collapse ${status}`;
    button.setAttribute("aria-label", button.title);
  }
  columns.style.gridTemplateColumns = cols.map((c) => (c.classList.contains("collapsed") ? "48px" : "minmax(260px, 1fr)")).join(" ");
}

function plainText(md) {
  return md.split("\n")
    .map((l) => l.replace(/^\s*(#{1,6}|>|[-*+]|\d+[.)])\s+/, "").replace(/^\[[ xX]\]\s+/, ""))
    .filter((l) => l.trim() && !/^\s*```/.test(l))
    .join(" ")
    .replace(/\*\*|__|`/g, "")
    .replace(/\s+/g, " ")
    .trim();
}

// In the list, a row: no drag to reorder, since a sorted list has no one position to drop into.
function card(t, showProject, row) {
  const node = el("div", { class: "card" + (row ? " row" : "") + (state.task === t.id ? " open" : "") + (t.waiting_on ? " waiting" : "") + (state.selected.has(t.id) ? " selected" : ""),
                           draggable: row ? null : "true", "data-id": t.id,
                           onclick: (e) => { if (e.metaKey || e.ctrlKey) toggleSelected(t.id); else openTask(t.id); } },
    row ? statusIcon(t.status) : null,
    el("div", { class: "head" },
      el("div", { class: "title" }, t.title),
      t.human ? el("span", { class: "human-mark", title: "Human task: agents skip it" }, icon("human")) : null,
      el("span", { class: "num", title: `Copy ${key(t)}`, onclick: (e) => { e.stopPropagation(); copyKey(key(t)); } },
        showProject ? el("span", { class: "dot", style: `background:${projectColor(t.project)}` }) : null, `#${t.id}`)),
    ...summary(t, waitChip(t), urlChip(t)).slice(row ? 1 : 0));
  node.addEventListener("dragstart", (e) => {
    state.dragging = t.id;
    e.dataTransfer.effectAllowed = "move";
    e.dataTransfer.setData("text/plain", String(t.id));
    requestAnimationFrame(() => node.classList.add("dragging"));
  });
  node.addEventListener("dragend", () => {
    node.classList.remove("dragging");
    document.querySelectorAll(".drop-line").forEach((d) => d.remove());
    state.dragging = null;
    // A drop reloads once the move is saved; a cancelled drag puts the card back.
    if (!state.dropped) loadBoard();
    state.dropped = false;
  });
  return node;
}

function wireDrop(col, list, status) {
  const line = el("div", { class: "drop-line" });
  const cardAfter = (y) => {
    const cards = [...list.querySelectorAll(".card:not(.dragging)")];
    return cards.find((c) => { const r = c.getBoundingClientRect(); return y < r.top + r.height / 2; }) || null;
  };
  col.addEventListener("dragover", (e) => {
    if (!state.dragging) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
    const after = cardAfter(e.clientY);
    if (after) list.insertBefore(line, after); else list.append(line);
  });
  col.addEventListener("dragleave", (e) => {
    if (!col.contains(e.relatedTarget)) line.remove();
  });
  col.addEventListener("drop", async (e) => {
    e.preventDefault();
    const id = state.dragging;
    if (!id) return;
    const after = cardAfter(e.clientY);
    const dragged = document.querySelector(`.card[data-id="${id}"]`);
    if (dragged) list.insertBefore(dragged, line.parentNode === list ? line : null);
    line.remove();
    state.dropped = true;
    if (fresh === id) fresh = null;
    const before = after ? Number(after.dataset.id) : null;
    try {
      await api("POST", `/tasks/${id}/move`, { status, before });
      if (state.task === id) refreshTask();
    } catch (err) { toast(err.message); }
    loadBoard();
  });
}

const NEW_TITLE = "New task";
let fresh = null;

async function newTask() {
  const { projects } = state.board;
  if (!projects.length) { toast("Create a project first"); return; }
  const last = localStorageGet("kanzo.newTaskProject");
  const project = state.project !== "all" ? state.project
    : projects.some((p) => p.slug === last) ? last : projects[0].slug;
  try {
    const task = await api("POST", "/tasks", { project, title: NEW_TITLE });
    await openTask(task.id);
    fresh = task.id;
    loadBoard();
    $("#p-title").focus({ preventScroll: true });
    $("#p-title").select();
  } catch (err) { toast(err.message); }
}

function dropFresh() {
  if (!fresh) return;
  const id = fresh;
  fresh = null;
  api("DELETE", `/tasks/${id}`).then(loadBoard, () => {});
}

// A reload or a closed tab may never blur the field being typed in.
window.addEventListener("pagehide", () => {
  const active = document.activeElement;
  const fields = !panelTask ? null
    : active === $("#p-desc") && active.value.trim() !== panelTask.description ? { description: active.value }
    : active === $("#p-tags") && cleanTags(active.value).join() !== panelTask.tags.join() ? { tags: cleanTags(active.value).join() }
    : null;
  if (fields) {
    fresh = null;
    fetch(`/api/tasks/${panelTask.id}`, { method: "PATCH", keepalive: true,
                                          headers: { "Content-Type": "application/json" }, body: JSON.stringify(fields) });
  }
  if (fresh) fetch(`/api/tasks/${fresh}`, { method: "DELETE", keepalive: true });
});

function showNewProject() {
  $("#project-dialog").hidden = false;
  $("#new-project [name=slug]").focus();
}

function hideNewProject() {
  $("#new-project").reset();
  $("#project-dialog").hidden = true;
}

$("#new-task").addEventListener("click", newTask);
$("#human-only").addEventListener("click", () => setHumanOnly(!isHumanOnly()));
let filterTimer;
$("#filter").addEventListener("input", () => { clearTimeout(filterTimer); filterTimer = setTimeout(loadBoard, 120); });
$("#filter").addEventListener("keydown", (e) => {
  const box = e.target, text = box.value.trim();
  if (e.key === "Escape") { e.stopPropagation(); box.value = ""; paintQuery(box); loadBoard(); box.blur(); }
  else if (e.key === "Enter" && text) { box.value = ""; paintQuery(box); setFilters([...savedFilters(), text]); }
  else if (e.key === "Backspace" && !box.value && savedFilters().length) setFilters(savedFilters().slice(0, -1));
});
$("#new-project-toggle").addEventListener("click", showNewProject);
$("#new-project-cancel").addEventListener("click", hideNewProject);
$("#new-project").addEventListener("submit", async (e) => {
  e.preventDefault();
  const f = new FormData(e.target);
  try {
    const p = await api("POST", "/projects", { slug: f.get("slug"), name: f.get("name"), repo: f.get("repo") || null });
    hideNewProject();
    setProject(p.slug);
  } catch (err) { toast(err.message); }
});

const panel = $("#panel");
let panelTask = null;
let panelWaits = "";
// Unsent comments by task, so closing the panel doesn't lose one.
const drafts = new Map();

const taskLink = (t) => el("button", { type: "button", class: "tag", title: key(t), onclick: () => pick(t) },
  statusIcon(t.status), el("span", {}, t.title));

async function openTask(id) {
  clearSelection();
  leavePanelField();
  if (fresh !== id) dropFresh();
  state.task = id;
  document.querySelectorAll(".card.open").forEach((c) => c.classList.remove("open"));
  document.querySelector(`.card[data-id="${id}"]`)?.classList.add("open");
  $("#p-comment").value = drafts.get(id) || "";
  pending = [];
  renderPending();
  fit($("#p-comment"));
  updateHash();
  await refreshTask();
  if (state.task !== id) return;
  panel.classList.add("shown");
  panel.setAttribute("aria-hidden", "false");
}

// Blurring a field saves it, so do that before the panel lets go of a task.
function leavePanelField() {
  if (panel.contains(document.activeElement)) document.activeElement.blur();
}

function closeTask() {
  leavePanelField();
  dropFresh();
  state.task = null;
  panelTask = null;
  updateHash();
  panel.classList.remove("shown");
  panel.setAttribute("aria-hidden", "true");
  document.querySelectorAll(".card.open").forEach((c) => c.classList.remove("open"));
}

async function refreshTask() {
  const id = state.task;
  if (!id) return;
  let data;
  try {
    data = await api("GET", `/tasks/${id}`);
  } catch (e) {
    if (state.task === id && /no task/.test(e.message)) { toast("That task was deleted"); closeTask(); }
    return;
  }
  // Another task may have taken the panel meanwhile.
  if (state.task !== id) return;
  // A board on an older version sends neither list.
  const { task, comments, images, waits_on = [], blocks = [] } = data;
  panelTask = task;
  panelWaits = waits_on.map(key).join(", ");
  $("#p-key").textContent = key(task);
  $("#p-key").title = `Copy ${key(task)}`;
  // Never overwrite what is being typed or picked.
  if (!$("#p-project").contains(document.activeElement)) renderProjectPick(task);
  for (const [field, value] of [[$("#p-title"), task.title], [$("#p-desc"), task.description], [$("#p-tags"), task.tags.join(", ")], [$("#p-url"), task.url || ""], [$("#p-waits"), panelWaits]]) {
    if (document.activeElement !== field) { field.value = value; fit(field); }
  }
  showDesc();
  const status = $("#p-status");
  const statuses = state.board ? state.board.statuses : [task.status];
  status.replaceChildren(...statuses.map((s) => el("option", { value: s }, s)));
  if (!statuses.includes(task.status)) status.append(el("option", { value: task.status }, task.status));
  status.value = task.status;
  $("#p-human").checked = task.human;
  $("#p-waits-links").replaceChildren(...waits_on.map(taskLink));
  $("#p-blocks").replaceChildren(...blocks.map(taskLink));
  $("#p-blocks-row").hidden = !blocks.length;
  $("#p-created").replaceChildren(timeEl(task.created_at));
  $("#p-updated").replaceChildren(timeEl(task.updated_at));
  $("#p-comment-count").textContent = comments.length ? String(comments.length) : "";
  $("#p-images").replaceChildren(...images.filter((i) => i.comment_id === null).map((i) =>
    thumb(i.file, async () => {
      try { await api("DELETE", `/images/${i.id}`); refreshTask(); } catch (err) { toast(err.message); }
    })));
  $("#p-comments").replaceChildren(...comments.map((c) => {
    const body = el("div", { class: "md" });
    body.innerHTML = markdown(c.body);
    const attached = images.filter((i) => i.comment_id === c.id);
    return el("div", { class: "comment" },
      el("div", { class: "who" }, el("b", {}, c.author), " · ", timeEl(c.created_at),
        el("button", { class: "remove", title: "Delete comment", "aria-label": "Delete comment",
                       onclick: () => deleteComment(c) }, icon("trash"))),
      c.body ? body : null,
      attached.length ? el("div", { class: "thumbs" }, attached.map((i) => thumb(i.file))) : null);
  }));
}

async function deleteComment(c) {
  if (!confirm("Delete this comment? This cannot be undone.")) return;
  try {
    await api("DELETE", `/comments/${c.id}`);
    await refreshTask();
    loadBoard();
  } catch (err) { toast(err.message); }
}

function renderProjectPick(task) {
  const projects = state.board ? state.board.projects.map((p) => p.slug) : [];
  if (!projects.includes(task.project)) projects.push(task.project);
  const select = el("select", { title: "Project", "aria-label": "Project" },
    ...projects.map((slug) => el("option", { value: slug }, slug)));
  select.value = task.project;
  select.addEventListener("change", () => {
    localStorageSet("kanzo.newTaskProject", select.value);
    patchTask({ project: select.value });
  });
  $("#p-project").replaceChildren(el("label", { class: "tag project-pick" },
    el("span", { class: "dot", style: `background:${projectColor(task.project)}` }), select));
}

function fit(t) {
  if (t.tagName !== "TEXTAREA") return;
  t.style.height = "auto";
  t.style.height = t.scrollHeight + "px";
}

async function patchTask(fields) {
  if (!panelTask) return;
  if (fresh === panelTask.id) fresh = null;
  try {
    await api("PATCH", `/tasks/${panelTask.id}`, fields);
    await refreshTask();
    loadBoard();
  } catch (e) { toast(e.message); refreshTask(); }
}

function cancelField(e, saved) {
  e.stopPropagation();
  if (fresh && fresh === state.task) { closeTask(); return; }
  e.target.value = saved;
  fit(e.target);
  e.target.blur();
}

// For writing worth keeping: Esc leaves the field, and leaving it saves. Closing a new task
// saves what was typed, and drops the task if nothing was.
function leaveField(e) {
  e.stopPropagation();
  if (fresh && fresh === state.task) closeTask();
  else e.target.blur();
}

// A page can't open a file: link, so the board does, on the computer it runs on.
panel.addEventListener("click", async (e) => {
  const link = e.target.closest('a[href^="file:///"]');
  if (!link) return;
  e.preventDefault();
  try {
    const path = decodeURIComponent(link.getAttribute("href").slice(7)).replace(/^\/([a-zA-Z]:)/, "$1");
    await api("POST", "/open", { path });
  } catch (err) { toast(err.message); }
});

// "" is the default width. A drag saves and refits only when it ends: browsers send one move a frame.
function setPanelWidth(width, save) {
  panel.style.setProperty("--panel-width", width);
  if (!save) return;
  localStorageSet("kanzo.panelWidth", width);
  [$("#p-title"), $("#p-desc"), $("#p-comment")].forEach(fit);
}

setPanelWidth(localStorageGet("kanzo.panelWidth") || "");
$("#p-wide").addEventListener("click", () => setPanelWidth(panel.style.getPropertyValue("--panel-width") === "100vw" ? "" : "100vw", true));
$("#p-edge").addEventListener("dblclick", () => setPanelWidth("", true));
$("#p-edge").addEventListener("pointerdown", (down) => {
  const edge = down.target;
  edge.setPointerCapture(down.pointerId);
  edge.onpointermove = (e) => setPanelWidth(`${Math.max(360, innerWidth - e.clientX)}px`);
  edge.onpointerup = () => {
    edge.onpointermove = edge.onpointerup = null;
    setPanelWidth(panel.style.getPropertyValue("--panel-width"), true);
  };
});

$("#p-close").addEventListener("click", closeTask);
$("#p-key").addEventListener("click", () => { if (panelTask) copyKey(key(panelTask)); });
for (const field of [$("#p-title"), $("#p-desc")]) field.addEventListener("input", () => fit(field));

$("#p-title").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); closeTask(); }
  else if ((e.key === "Enter" || e.key === "Tab") && !e.shiftKey) { e.preventDefault(); editDesc(); }
  else if (e.key === "Escape") cancelField(e, panelTask ? panelTask.title : "");
});
$("#p-title").addEventListener("blur", (e) => {
  if (!panelTask) return;
  const v = e.target.value.trim();
  if (v && v !== panelTask.title) patchTask({ title: v });
  else { e.target.value = panelTask.title; fit(e.target); }
});

$("#p-desc").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); closeTask(); }
  else if (e.key === "Escape") leaveField(e);
});
$("#p-desc").addEventListener("blur", (e) => {
  if (panelTask && e.target.value.trim() !== panelTask.description) patchTask({ description: e.target.value });
  showDesc();
});

// Empty, the textarea stays, so its placeholder has something to click.
function showDesc() {
  const desc = $("#p-desc"), view = $("#p-desc-view");
  const editing = document.activeElement === desc || !desc.value.trim();
  desc.hidden = !editing;
  view.hidden = editing;
  // Hidden, it measured 0, as it does when the task before this one had a description.
  if (editing) fit(desc);
  else view.innerHTML = markdown(desc.value);
}

function editDesc() {
  const desc = $("#p-desc");
  $("#p-desc-view").hidden = true;
  desc.hidden = false;
  fit(desc);
  desc.focus();
}

// A click on a link opens it instead of editing.
$("#p-desc-view").addEventListener("mousedown", (e) => { if (e.target.closest("a")) e.preventDefault(); });
$("#p-desc-view").addEventListener("focus", editDesc);

$("#p-status").addEventListener("change", (e) => patchTask({ status: e.target.value }));
$("#p-human").addEventListener("change", (e) => patchTask({ human: e.target.checked }));

let pending = [];

function imagesIn(transfer) {
  if (!transfer) return [];
  const fromItems = transfer.items ? [...transfer.items].filter((i) => i.kind === "file").map((i) => i.getAsFile()) : [];
  return (fromItems.length ? fromItems : [...(transfer.files || [])]).filter((f) => f && f.type.startsWith("image/"));
}

async function uploadImage(image) {
  return (await api("POST", "/images", image)).file;
}

async function attachImages(images, toComment) {
  if (!panelTask) return;
  const task = panelTask.id;
  try {
    for (const image of images) {
      const file = await uploadImage(image);
      if (toComment) {
        pending.push(file);
        renderPending();
      } else {
        await api("POST", `/tasks/${task}/images`, { file });
        if (fresh === task) fresh = null;
      }
    }
    if (!toComment) refreshTask();
  } catch (err) { toast(err.message); }
}

function thumb(file, onRemove) {
  return el("div", { class: "thumb" },
    el("a", { href: `/images/${file}`, target: "_blank", rel: "noopener" }, el("img", { src: `/images/${file}`, alt: "Attached image" })),
    onRemove ? el("button", { type: "button", class: "remove-image", title: "Remove image", "aria-label": "Remove image", onclick: onRemove }, icon("close")) : null);
}

function renderPending() {
  $("#p-pending").replaceChildren(...pending.map((file, i) => thumb(file, () => { pending.splice(i, 1); renderPending(); })));
  $("#p-send").disabled = !$("#p-comment").value.trim() && !pending.length;
}

for (const field of [$("#p-title"), $("#p-desc"), $("#p-comment")]) {
  field.addEventListener("paste", (e) => {
    const images = imagesIn(e.clipboardData);
    if (!images.length) return;
    e.preventDefault();
    attachImages(images, field.id === "p-comment");
  });
}
for (const [zone, toComment] of [[$("#p-desc").closest(".section"), false], [$("#p-comment-form"), true]]) {
  zone.addEventListener("dragover", (e) => {
    if (!e.dataTransfer || ![...e.dataTransfer.types].includes("Files")) return;
    e.preventDefault();
    zone.classList.add("dropping");
  });
  zone.addEventListener("dragleave", (e) => { if (!zone.contains(e.relatedTarget)) zone.classList.remove("dropping"); });
  zone.addEventListener("drop", (e) => {
    zone.classList.remove("dropping");
    const images = imagesIn(e.dataTransfer);
    if (!images.length) return;
    e.preventDefault();
    attachImages(images, toComment);
  });
}

const cleanTags = (text) => [...new Set(text.split(",").map((t) => t.trim().replace(/\s+/g, " ").toLowerCase()).filter(Boolean))];
$("#p-tags").addEventListener("keydown", (e) => {
  if (e.key === "Enter") { e.preventDefault(); e.target.blur(); }
  else if (e.key === "Escape") leaveField(e);
});
$("#p-tags").addEventListener("blur", (e) => {
  if (!panelTask) return;
  const tags = cleanTags(e.target.value);
  if (tags.join(",") !== panelTask.tags.join(",")) patchTask({ tags: tags.join(", ") });
  else e.target.value = panelTask.tags.join(", ");
});

$("#p-url").addEventListener("keydown", (e) => {
  if (e.key === "Enter") { e.preventDefault(); e.target.blur(); }
  else if (e.key === "Escape") cancelField(e, panelTask ? panelTask.url || "" : "");
});
$("#p-url").addEventListener("blur", (e) => {
  if (!panelTask) return;
  const url = e.target.value.trim();
  if (url !== (panelTask.url || "")) patchTask({ url });
  else e.target.value = url;
});

$("#p-waits").addEventListener("keydown", (e) => {
  if (e.key === "Enter") { e.preventDefault(); e.target.blur(); }
  else if (e.key === "Escape") cancelField(e, panelWaits);
});
$("#p-waits").addEventListener("blur", (e) => {
  if (panelTask && e.target.value.trim() !== panelWaits) patchTask({ waits_on: e.target.value });
});

$("#p-comment-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const body = $("#p-comment").value.trim();
  if ((!body && !pending.length) || !panelTask) return;
  try {
    await api("POST", `/tasks/${panelTask.id}/comments`, { body, images: pending });
    if (fresh === panelTask.id) fresh = null;
    drafts.delete(panelTask.id);
    $("#p-comment").value = "";
    pending = [];
    renderPending();
    fit($("#p-comment"));
    await refreshTask();
    loadBoard();
  } catch (err) { toast(err.message); }
});
$("#p-comment").addEventListener("input", (e) => {
  if (e.target.value) drafts.set(state.task, e.target.value); else drafts.delete(state.task);
  $("#p-send").disabled = !e.target.value.trim() && !pending.length;
  fit(e.target);
});
$("#p-comment").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); $("#p-comment-form").requestSubmit(); }
});

$("#p-delete").addEventListener("click", async () => {
  if (!panelTask) return;
  const extra = panelTask.comments ? ` and its ${plural(panelTask.comments, "comment")}` : "";
  if (!confirm(`Delete ${key(panelTask)}${extra}? This cannot be undone.`)) return;
  try {
    if (fresh === panelTask.id) fresh = null;
    await api("DELETE", `/tasks/${panelTask.id}`);
    closeTask();
    loadBoard();
  } catch (err) { toast(err.message); }
});

let results = [], selected = 0, searchRequest = 0, searchTimer;

function openSearch() {
  closeMenu();
  $("#search-dialog").hidden = false;
  $("#search").value = "";
  paintQuery($("#search"));
  $("#search").focus();
  runSearch();
}

function closeSearch() {
  $("#search-dialog").hidden = true;
}

async function runSearch() {
  const q = $("#search").value.trim();
  const request = ++searchRequest;
  try {
    const found = await api("GET", "/search?q=" + encodeURIComponent(q));
    if (request !== searchRequest) return;
    results = found;
    selected = 0;
    $("#search-results").replaceChildren(
      ...(results.length ? [el("div", { class: "label" }, q ? "Tasks" : "Recent")] : []),
      ...results.map((t, i) => el("button", { type: "button", class: "result" + (i === 0 ? " selected" : ""),
                                             onmousemove: () => select(i), onclick: () => pick(t) },
        el("span", { class: "dot", style: `background:${projectColor(t.project)}` }),
        el("span", { class: "body" }, el("span", { class: "title" }, t.title), ...summary(t)),
        el("span", { class: "meta" },
          t.comments ? el("span", { class: "comments", title: `${t.comments} comment${t.comments > 1 ? "s" : ""}` },
                          icon("comment"), String(t.comments)) : null,
          el("span", { class: "mono" }, key(t)), t.status))),
      ...(results.length ? [] : [el("div", { class: "note" }, q ? "No tasks match." : "No tasks yet.")]));
    highlight("search", $("#search-results"), wordsOf(q));
  } catch (err) { toast(err.message); }
}

function select(i) {
  const rows = $("#search-results").querySelectorAll(".result");
  if (!rows.length) return;
  selected = (i + rows.length) % rows.length;
  rows.forEach((r, j) => r.classList.toggle("selected", j === selected));
  rows[selected].scrollIntoView({ block: "nearest" });
}

function pick(t) {
  closeSearch();
  if (state.project !== "all" && state.project !== t.project) setProject(t.project);
  openTask(t.id);
}

$("#search-open").addEventListener("click", openSearch);
$("#search").addEventListener("input", () => { clearTimeout(searchTimer); searchTimer = setTimeout(runSearch, 120); });
$("#search").addEventListener("keydown", (e) => {
  if (e.key === "ArrowDown" || e.key === "ArrowUp") { e.preventDefault(); select(selected + (e.key === "ArrowDown" ? 1 : -1)); }
  else if (e.key === "Enter" && results[selected]) { e.preventDefault(); pick(results[selected]); }
});

document.addEventListener("keydown", (e) => {
  const typing = e.target.closest("input, textarea, select");
  const plain = !typing && !e.metaKey && !e.ctrlKey;
  if (e.key === "Escape" && state.menu) { closeMenu(); return; }
  // Only in the installed app: in a browser tab, these keys belong to the browser.
  if (/^\d$/.test(e.key) && (e.metaKey || e.ctrlKey) && state.board && matchMedia("(display-mode: standalone)").matches) {
    const { projects } = state.board;
    const slug = e.key === "0" ? "all" : projects[e.key === "9" ? projects.length - 1 : e.key - 1]?.slug;
    e.preventDefault();
    if (slug) setProject(slug);
  }
  if (e.key === "Escape" && state.task && !typing) closeTask();
  if (e.key === "Escape" && !typing) clearSelection();
  if ((e.key === "/" && plain) || (e.key === "k" && (e.metaKey || e.ctrlKey))) {
    e.preventDefault();
    openSearch();
  }
  if (e.key === "n" && plain && !$("#new-task").hidden) { e.preventDefault(); newTask(); }
  if (e.key === "f" && plain && !$("#filter-box").hidden) { e.preventDefault(); $("#filter").focus(); }
  if (e.key === "h" && plain && !$("#human-only").hidden) { e.preventDefault(); setHumanOnly(!isHumanOnly()); }
});

window.addEventListener("hashchange", () => {
  const { slug, task } = readHash();
  if ((slug || "all") !== state.project) setProject(slug || "all");
  if (task && task !== state.task) openTask(task);
  else if (!task && state.task) closeTask();
});

async function poll() {
  if (document.hidden) return schedulePoll();
  try {
    const { changes, guests = null } = await api("GET", "/changes");
    $("#offline").hidden = true;
    if (guests !== state.guests) {
      state.guests = guests;
      renderNetwork();
      if (state.menu === "network") loadNetwork();
    }
    if (changes !== state.changes) {
      state.changes = changes;
      // Joining or leaving a board, even from the terminal, shows another board.
      loadNetwork();
      await loadBoard();
      if (state.task) refreshTask();
    }
  } catch {
    $("#offline").hidden = false;
  }
  schedulePoll();
}

// The next check starts only after this one finished, so a slow or busy server
// never piles up requests.
function schedulePoll() {
  setTimeout(poll, 2000);
}
schedulePoll();

let syncing = false;

async function syncNow(keep) {
  if (syncing) return;
  syncing = true;
  toast("Syncing…");
  let diverged = null;
  try {
    toast((await api("POST", "/sync", { keep })).message);
    loadBoard();
  } catch (err) {
    if (err.data?.diverged) diverged = err.data;
    else toast(err.message);
  }
  syncing = false;
  if (diverged) chooseSide(diverged);
}

function chooseSide({ first }) {
  const repo = repoName(syncRemote);
  if (first) {
    if (confirm(`${repo} already has a board. Use it on this machine?\n\nThis machine's current board is kept in kanzo.db.bak.`)) syncNow("remote");
    else if (confirm(`Replace the board in ${repo} with this machine's board instead?`)) syncNow("local");
    return;
  }
  if (confirm("This machine and another one both changed the board since the last sync, and boards can't be merged.\n\nKeep THIS machine's board, and overwrite the other one?")) syncNow("local");
  else if (confirm("Take the other machine's board instead? This machine's board is kept in kanzo.db.bak.")) syncNow("remote");
}

let syncRemote = null;

function repoName(url) {
  return (url || "").replace(/\.git$/, "").split(/[/:]/).filter(Boolean).slice(-2).join("/");
}

async function loadSyncRemote() {
  try { syncRemote = (await api("GET", "/sync")).remote; } catch {}
  return syncRemote;
}

async function sync() {
  closeMenu();
  if (await loadSyncRemote()) syncNow();
  else openSettings("sync");
}

$("#sync-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const save = $("#sync-save");
  save.disabled = true;
  save.textContent = "Checking…";
  try {
    syncRemote = (await api("PUT", "/sync", { url: $("#sync-url").value })).remote;
    closeSettings();
    syncNow();
  } catch (err) {
    $("#sync-error").textContent = err.message;
    $("#sync-error").hidden = false;
  }
  save.disabled = false;
  save.textContent = "Save and sync";
});

async function loadNetwork() {
  let net = null;
  try { net = await api("GET", "/network"); } catch {}
  setNet(net);
}

function setNet(net) {
  state.net = net;
  // A joined board's count comes from its host, with each check of /api/changes.
  if (net && net.state !== "joined") state.guests = net.state === "sharing" ? net.guests.length : null;
  renderNetwork();
}

function renderNetwork() {
  const net = state.net;
  const button = $("#network");
  // Opened from another computer's browser, the board can't tell which computer it is on.
  const away = !net && !["localhost", "127.0.0.1", "[::1]"].includes(location.hostname);
  const hosted = net?.state === "joined" || away;
  const host = net?.host?.replace(/:4242$/, "") || location.hostname;
  const n = state.guests;
  const shared = hosted || n != null;
  const computers = n == null ? "" : `: ${plural(n, "computer")} connected`;
  button.replaceChildren(icon("share"), ...(shared ? ["Shared"] : []), ...(shared && n != null ? [el("span", { class: "count" }, String(n))] : []));
  button.classList.toggle("on", shared);
  button.title = hosted ? `Shared by ${host}${computers}` : shared ? `Shared on your network${computers}` : "Share this board, or join another";
  if (state.menu === "network") renderNetworkMenu();
}

// While a change is on its way, the popover keeps showing it.
let networkBusy = false;

function renderNetworkMenu() {
  if (networkBusy) return;
  const net = state.net;
  const now = net?.state;
  const share = $("#network-share");
  share.checked = now === "sharing";
  share.disabled = !net || now === "joined";
  $("#network-share-row").classList.toggle("disabled", share.disabled);
  $("#network-share-note").textContent =
    !net ? "Only the computer with the board can change this"
    : now === "sharing" ? `Other computers join it at ${net.addresses.length > 1 ? "the address for their network"
      : net.addresses.length ? "this address" : "this computer's IP address"}`
    : now === "joined" ? "Leave the shared board to share this one"
    : "Other computers on your network can use it";
  const addresses = $("#network-addresses");
  addresses.hidden = now !== "sharing" || !net.addresses.length;
  if (now === "sharing") {
    addresses.replaceChildren(...net.addresses.map((a) => el("button", { class: "copy", type: "button", title: "Copy the address" },
      el("span", { class: "mono" }, a), icon("copy"))));
  }
  const guests = $("#network-guests");
  guests.hidden = now !== "sharing";
  if (now === "sharing") {
    guests.replaceChildren(
      el("span", { class: "label" }, net.guests.length ? "Connected now" : "No other computer is connected yet"),
      ...net.guests.map((g) => el("div", { class: "guest" },
        el("span", { class: "dot" }), g.name || "A browser", el("span", { class: "address" }, g.address))));
  }
  const joined = now === "joined";
  $("#network-join").hidden = !net;
  $("#network-join").classList.toggle("disabled", now === "sharing");
  $("#network-join-text").replaceChildren(joined ? `Connected to ${net.host}` : "Join a shared board",
    el("span", {}, joined ? "This computer uses that board"
      : now === "sharing" ? "Stop sharing to join another board"
      : "The address the sharing computer shows"));
  $("#network-leave").hidden = !joined;
  $("#network-join-fields").hidden = joined;
  $("#join-host").disabled = $("#join-submit").disabled = now === "sharing";
}

function toggleNetworkMenu() {
  if (!toggleMenu("network")) return;
  const button = $("#network");
  button.classList.add("open");
  $("#network-error").hidden = true;
  renderNetworkMenu();
  const menu = $("#network-menu");
  menu.hidden = false;
  const r = button.getBoundingClientRect();
  menu.style.top = `${r.bottom + 6}px`;
  menu.style.left = `${Math.max(8, Math.min(r.right - menu.offsetWidth, innerWidth - menu.offsetWidth - 8))}px`;
  loadNetwork();
}

// Joining or leaving shows another board, so the page reloads.
async function changeNetwork(change) {
  $("#network-error").hidden = true;
  networkBusy = true;
  let net = state.net;
  try {
    net = await api("POST", "/network", change);
    if (change.action === "join" || change.action === "leave") { location.reload(); return; }
  } catch (err) {
    $("#network-error").textContent = err.message;
    $("#network-error").hidden = false;
  }
  networkBusy = false;
  setNet(net);
}

$("#network").addEventListener("click", toggleNetworkMenu);
$("#network-share").addEventListener("change", (e) => changeNetwork({ action: e.target.checked ? "on" : "off" }));
$("#network-addresses").addEventListener("click", (e) => {
  const button = e.target.closest(".copy");
  if (!button) return;
  copyKey(button.textContent);
  button.querySelector(".icon").replaceWith(icon("check"));
  setTimeout(() => button.querySelector(".icon").replaceWith(icon("copy")), 1500);
});
$("#network-leave").addEventListener("click", () => changeNetwork({ action: "leave" }));
$("#network-join").addEventListener("submit", async (e) => {
  e.preventDefault();
  const submit = $("#join-submit");
  submit.disabled = true;
  submit.textContent = "Joining…";
  await changeNetwork({ action: "join", host: $("#join-host").value });
  submit.disabled = false;
  submit.textContent = "Join";
});

function toggleSettingsMenu() {
  if (!state.board || !toggleMenu("settings")) return;
  const button = $("#settings");
  button.classList.add("open");
  const menu = el("div", { class: "menu", role: "menu", tabindex: "-1" },
    el("button", { onclick: () => openSettings() }, icon("settings"), "Settings"),
    el("button", { onclick: () => { closeMenu(); checkForUpdates(); } }, icon("update"), "Check for updates",
      el("span", { class: "value" }, `v${state.board.version}`)),
    el("button", { onclick: () => openSettings("about") }, icon("news"), "What's new?"),
    el("button", { onclick: sync }, icon("sync"), "Sync"));
  document.body.append(menu);
  const r = button.getBoundingClientRect();
  menu.style.width = `${r.width}px`;
  menu.style.left = `${r.left}px`;
  menu.style.top = `${r.top - 6 - menu.offsetHeight}px`;
  menu.focus();
}

$("#settings").addEventListener("click", toggleSettingsMenu);

function openSettings(section = "board") {
  closeMenu();
  $("#settings-backlog").checked = state.board?.statuses.includes("backlog") ?? false;
  document.querySelectorAll("[data-property]").forEach((box) => { box.checked = !isPropertyHidden(box.dataset.property); });
  $("#sync-url").value = syncRemote || "";
  $("#sync-create").hidden = !!syncRemote;
  $("#sync-error").hidden = true;
  $("#settings-version").textContent = state.board ? `Kanzo ${state.board.version}` : "";
  showSection(section);
  $("#settings-dialog").hidden = false;
  if (section === "sync") $("#sync-url").focus();
  else $("#settings-close").focus();
}

function showSection(section) {
  document.querySelectorAll("#settings-nav button").forEach((b) => b.classList.toggle("active", b.dataset.section === section));
  document.querySelectorAll(".settings-pane").forEach((p) => { p.hidden = p.dataset.section !== section; });
  if (section === "about") loadWhatsNew();
}

// Fetched when first shown, so the page doesn't carry the notes on every load.
async function loadWhatsNew() {
  const notes = $("#whats-new");
  if (notes.firstChild) return;
  try {
    const res = await fetch("/changelog.md");
    if (res.ok) notes.innerHTML = markdown(await res.text());
  } catch {}
}

function closeSettings() {
  $("#settings-dialog").hidden = true;
}

$("#settings-nav").addEventListener("click", (e) => {
  const b = e.target.closest("button");
  if (b) showSection(b.dataset.section);
});
$("#settings-close").addEventListener("click", closeSettings);
for (const [dialog, close] of [[$("#project-dialog"), hideNewProject], [$("#search-dialog"), closeSearch],
                               [$("#settings-dialog"), closeSettings]]) {
  dialog.addEventListener("mousedown", (e) => { if (e.target === dialog) close(); });
  dialog.addEventListener("keydown", (e) => { if (e.key === "Escape") { e.stopPropagation(); close(); } });
}
async function saveSetting(box, fields) {
  try {
    await api("PATCH", "/settings", fields);
    loadBoard();
  } catch (err) { box.checked = !box.checked; toast(err.message); }
}
$("#settings-backlog").addEventListener("change", (e) => saveSetting(e.target, { backlog: e.target.checked }));
$('.settings-pane[data-section="board"]').append(...["Description", "Tags", "URL", "Human", "Waiting", "Created", "Updated", "Comments", "Images"].map((name) => {
  const property = name.toLowerCase();
  return el("label", { class: "setting" }, el("span", { class: "text" }, name),
    el("span", { class: "switch" },
      el("input", { type: "checkbox", "data-property": property, "aria-label": name,
                    onchange: (e) => saveSetting(e.target, { property, visible: e.target.checked }) }),
      el("span")));
}));
$("#settings-update").addEventListener("click", checkForUpdates);

async function checkUpdate() {
  try {
    showUpdate(await api("GET", "/update"));
  } catch {}
}

function showUpdate({ current, available }) {
  const button = $("#update");
  button.hidden = !available;
  if (!available) return;
  $("#update-label").textContent = `Update to ${available}`;
  button.title = `You have ${current}`;
  button.onclick = () => {
    if (confirm(`Update Kanzo from ${current} to ${available}? The board restarts, which takes a few seconds.`)) installUpdate(available);
  };
}

async function checkForUpdates() {
  toast("Checking for updates…");
  try {
    const status = await api("GET", "/update?now=1");
    showUpdate(status);
    if (status.available) {
      if (confirm(`Kanzo ${status.available} is out (you have ${status.current}). Download and install it now? The board restarts.`)) {
        installUpdate(status.available);
      }
    } else if (status.latest) {
      toast(`Kanzo is up to date (${status.current})`);
    } else {
      toast("Can't reach GitHub to check for updates");
    }
  } catch (err) { toast(err.message); }
}

async function installUpdate(available) {
  const button = $("#update");
  button.hidden = false;
  button.disabled = true;
  $("#update-label").textContent = "Updating…";
  let res;
  try {
    res = await api("POST", "/update");
  } catch (err) {
    toast(err.message);
    button.disabled = false;
    $("#update-label").textContent = `Update to ${available}`;
    return;
  }
  if (!res.restarting) {
    $("#update-label").textContent = "Restart Kanzo to finish";
    toast(`Kanzo ${available} is installed. Restart Kanzo to use it.`);
    return;
  }
  // The old program answered, then gives way to the new one: wait for it, then reload.
  await new Promise((r) => setTimeout(r, 1500));
  for (let i = 0; i < 60; i++) {
    try { await api("GET", "/update"); break; } catch { await new Promise((r) => setTimeout(r, 1000)); }
  }
  location.reload();
}

function setRail(on) {
  document.documentElement.classList.toggle("rail", on);
  $("#rail").setAttribute("aria-expanded", String(!on));
  localStorageSet("kanzo.rail", on ? "1" : "");
}
setRail(localStorageGet("kanzo.rail") === "1");
document.documentElement.classList.add("ready");
$("#rail").addEventListener("click", () => setRail($("#rail").getAttribute("aria-expanded") === "true"));

// Chromium's browsers ask this only while the board isn't installed; the others never do.
let installPrompt;
window.addEventListener("beforeinstallprompt", (e) => {
  e.preventDefault();
  installPrompt = e;
  $("#install").hidden = false;
});
window.addEventListener("appinstalled", () => { $("#install").hidden = true; });
$("#install").addEventListener("click", () => installPrompt.prompt());

checkUpdate();
setInterval(checkUpdate, 24 * 60 * 60 * 1000);
loadSyncRemote();
loadNetwork();

setInterval(() => {
  document.querySelectorAll("time[datetime]").forEach((t) => { t.textContent = ago(t.getAttribute("datetime")); });
}, 60000);

{
  const { slug, task } = readHash();
  state.project = slug || localStorageGet("kanzo.project") || "all";
  loadBoard().then(() => { if (task) openTask(task); });
}
