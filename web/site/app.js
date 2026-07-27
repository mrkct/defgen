// The playground's UI: an editor, the options the CLI takes as flags, and the
// panels a result is shown in. Everything below is presentation — the compiler
// itself is the wasm module behind ./defgen.js.

import { loadDefgen } from "./defgen.js";
import { EXAMPLES, loadExample } from "./examples.js";
import { highlight, languageForFile } from "./highlight.js";
import * as device from "./device.js";

const dom = Object.fromEntries(
  [
    "toolbar",
    "backends",
    "backend-hint",
    "stem",
    "opt-ast",
    "opt-model",
    "opt-auto",
    "generate",
    "examples",
    "share",
    "gutter",
    "highlight",
    "source",
    "tabs",
    "result",
    "device",
    "code-actions",
    "file-picker",
    "files",
    "copy",
    "download",
    "status",
    "timing",
  ].map((id) => [id.replace(/-(.)/g, (_, c) => c.toUpperCase()), document.getElementById(id)]),
);

const STORAGE_KEY = "defgen.playground.v1";
/** Long enough that typing a word does not compile it letter by letter. */
const TYPING_PAUSE_MS = 250;

const TABS = [
  { id: "code", label: "Code" },
  { id: "problems", label: "Problems" },
  { id: "schema", label: "Schema" },
  // Sticks around once a schema has bound at least one characteristic, even
  // if the schema is mid-edit and currently has errors — hiding it the
  // moment a keystroke breaks parsing would drop a connected device off the
  // tab bar under the user's hands.
  { id: "device", label: "Device", shown: () => deviceAvailable },
  { id: "ast", label: "Syntax tree", shown: () => dom.optAst.checked },
  { id: "model", label: "Model", shown: () => dom.optModel.checked },
];

/** Whether the schema last compiled cleanly has bound GATT characteristics. */
let deviceAvailable = false;

/** The compiler, once it has arrived. */
let defgen = null;
/** The last result, or null before the first compile. */
let result = null;
let activeTab = "code";
let activeFile = 0;

// ---------------------------------------------------------------- utilities

function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    // A null or false prop is how a caller says "not this one" — an absent
    // title, a row that is not clickable.
    if (value === null || value === false) continue;
    if (key === "class") node.className = value;
    else if (key === "text") node.textContent = value;
    else if (key.startsWith("on")) node.addEventListener(key.slice(2).toLowerCase(), value);
    else node.setAttribute(key, value === true ? "" : value);
  }
  for (const child of children.flat()) {
    if (child !== null && child !== undefined) node.append(child);
  }
  return node;
}

function plural(count, noun) {
  return `${count.toLocaleString()} ${noun}${count === 1 ? "" : "s"}`;
}

function bytes(count) {
  return count < 1024 ? `${count} B` : `${(count / 1024).toFixed(1)} KiB`;
}

/** Line count of generated source, which ends with a newline it does not owe a line to. */
function lines(text) {
  return plural(text.split("\n").length - (text.endsWith("\n") ? 1 : 0), "line");
}

/** A layout, as the size a reader of the table wants: `6 bytes`, `4 bits`. */
function sizeText(size) {
  if (size.variable) {
    return `${Math.ceil(size.fixedBits / 8)}–${size.maxBytes} bytes`;
  }
  if (size.fixedBits % 8 !== 0) {
    return plural(size.fixedBits, "bit");
  }
  return plural(size.maxBytes, "byte");
}

// -------------------------------------------------------------------- state

function options() {
  return {
    backend: dom.backends.querySelector("input:checked")?.value ?? "c",
    stem: dom.stem.value,
    ast: dom.optAst.checked,
    model: dom.optModel.checked,
  };
}

function save() {
  const { backend, stem, ast, model } = options();
  const state = { source: dom.source.value, backend, stem, ast, model, auto: dom.optAuto.checked };
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
  } catch {
    // Private browsing, a full quota: losing the draft is not worth an error.
  }
}

function restore() {
  const shared = readHash();
  if (shared) return shared;
  try {
    return JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "null");
  } catch {
    return null;
  }
}

/** Base64url, so a schema survives a round trip through a URL fragment. */
function encodeSource(text) {
  const bytes = new TextEncoder().encode(text);
  const binary = Array.from(bytes, (byte) => String.fromCharCode(byte)).join("");
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

function decodeSource(encoded) {
  const binary = atob(encoded.replaceAll("-", "+").replaceAll("_", "/"));
  return new TextDecoder().decode(Uint8Array.from(binary, (c) => c.charCodeAt(0)));
}

function readHash() {
  const params = new URLSearchParams(location.hash.slice(1));
  const source = params.get("source");
  if (!source) return null;
  try {
    return {
      source: decodeSource(source),
      backend: params.get("backend"),
      stem: params.get("stem"),
    };
  } catch {
    return null;
  }
}

function shareLink() {
  const { backend, stem } = options();
  const params = new URLSearchParams({ backend, stem, source: encodeSource(dom.source.value) });
  return `${location.origin}${location.pathname}#${params}`;
}

// ------------------------------------------------------------------- editor

/** Line count the gutter currently shows, so it is only rebuilt when it moves. */
let gutterLines = 0;

function syncGutter() {
  const lines = dom.source.value.split("\n").length;
  if (lines !== gutterLines) {
    gutterLines = lines;
    dom.gutter.textContent = Array.from({ length: lines }, (_, i) => i + 1).join("\n");
  }
  dom.gutter.scrollTop = dom.source.scrollTop;
  dom.highlight.scrollTop = dom.source.scrollTop;
  dom.highlight.scrollLeft = dom.source.scrollLeft;
}

/** Redraws the highlighted layer sitting behind the (invisible) textarea. */
function highlightSchema() {
  dom.highlight.firstElementChild.innerHTML = highlight(dom.source.value, "defs");
}

/** Puts the caret on `line`, so a diagnostic can be clicked to reach it. */
function jumpTo(line, column) {
  const lines = dom.source.value.split("\n");
  const offset =
    lines.slice(0, line - 1).reduce((total, text) => total + text.length + 1, 0) + (column - 1);
  dom.source.focus();
  dom.source.setSelectionRange(offset, offset);
  const lineHeight = parseFloat(getComputedStyle(dom.source).lineHeight);
  dom.source.scrollTop = Math.max(0, (line - 1) * lineHeight - dom.source.clientHeight / 3);
  syncGutter();
}

function onEditorKeydown(event) {
  if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
    event.preventDefault();
    compile();
    return;
  }
  // A schema is indented with spaces, and the browser's default of moving
  // focus out of the editor is never what someone mid-struct wanted.
  if (event.key === "Tab") {
    event.preventDefault();
    const { selectionStart, selectionEnd, value } = dom.source;
    dom.source.value = `${value.slice(0, selectionStart)}    ${value.slice(selectionEnd)}`;
    dom.source.setSelectionRange(selectionStart + 4, selectionStart + 4);
    onEdit();
  }
}

// ---------------------------------------------------------------- compiling

let pending = null;

function scheduleCompile() {
  clearTimeout(pending);
  pending = setTimeout(compile, TYPING_PAUSE_MS);
}

function onEdit() {
  syncGutter();
  highlightSchema();
  save();
  if (dom.optAuto.checked) scheduleCompile();
}

function compile() {
  clearTimeout(pending);
  if (!defgen) return;

  const started = performance.now();
  try {
    result = defgen.compile(dom.source.value, options());
  } catch (error) {
    // A trap leaves the module's allocator in an unknown state, so the
    // instance is not reusable — reload it rather than keep handing it work.
    // The fresh instance is deliberately not handed the same schema again:
    // whatever tripped it is still in the editor.
    console.error(error);
    result = null;
    defgen = null;
    crashed = true;
    setStatus("bad", "The compiler stopped on that schema. Reloading it — please report this.");
    boot().catch(fatal);
    return;
  }
  dom.timing.textContent = `generated in ${(performance.now() - started).toFixed(1)} ms`;

  if (result.ok) {
    deviceAvailable = result.summary.services.length > 0;
    updateDevice(result);
  }

  activeFile = 0;
  if (result.diagnostics.some((d) => d.severity === "error")) {
    activeTab = "problems";
  } else if (activeTab === "problems" && result.diagnostics.length === 0) {
    activeTab = "code";
  }
  render();
}

/**
 * Hands the Device tab a live, importable copy of the schema.
 *
 * The Device tab needs the *javascript* backend's output specifically, to
 * `import()` its codecs — regardless of which backend is selected in the
 * toolbar for the Code tab. Reuses the result already in hand when
 * javascript is what's selected; otherwise asks the (already-loaded, so
 * this is cheap) compiler for it again.
 */
function updateDevice(result) {
  const { backend, stem } = options();
  const js = backend === "javascript" ? result : defgen.compile(dom.source.value, { backend: "javascript", stem });
  const jsSource = js.files[0]?.contents;
  if (jsSource) device.setSchema({ summary: result.summary, jsSource });
}

// ---------------------------------------------------------------- rendering

function render() {
  renderTabs();
  renderPanel();
  renderStatus();
}

function visibleTabs() {
  return TABS.filter((tab) => !tab.shown || tab.shown());
}

function renderTabs() {
  const problems = result?.diagnostics.length ?? 0;
  const tabs = visibleTabs();
  if (!tabs.some((tab) => tab.id === activeTab)) activeTab = "code";

  dom.tabs.replaceChildren(
    ...tabs.map((tab) =>
      el("button", {
        type: "button",
        role: "tab",
        "aria-selected": String(tab.id === activeTab),
        text: tab.id === "problems" && problems > 0 ? `${tab.label} (${problems})` : tab.label,
        onClick: () => {
          activeTab = tab.id;
          render();
        },
      }),
    ),
  );
}

function renderPanel() {
  const files = result?.files ?? [];
  dom.codeActions.hidden = activeTab !== "code" || files.length === 0;
  dom.filePicker.hidden = files.length < 2;
  if (files.length > 1) {
    dom.files.replaceChildren(
      ...files.map((file, i) => el("option", { value: String(i), text: file.name })),
    );
    dom.files.value = String(activeFile);
  }

  // The Device tab owns a persistent DOM subtree of its own — an open
  // connection, a subscribed notification, a form mid-edit — so it is shown
  // or hidden rather than rebuilt into `dom.result` like every other tab.
  dom.device.hidden = activeTab !== "device";
  dom.result.hidden = activeTab === "device";
  if (activeTab === "device") {
    device.render();
    return;
  }

  if (!result) {
    dom.result.replaceChildren(placeholder("Write a schema on the left, then generate."));
    return;
  }

  switch (activeTab) {
    case "code":
      dom.result.replaceChildren(
        files.length > 0
          ? el("pre", {}, codeBlock(files[activeFile]))
          : placeholder(
              result.error ??
                "No code: the schema has errors. See the Problems tab for what to fix.",
            ),
      );
      break;
    case "problems":
      dom.result.replaceChildren(renderProblems());
      break;
    case "schema":
      dom.result.replaceChildren(
        result.summary ? renderSummary(result.summary) : placeholder("No checked schema yet."),
      );
      break;
    case "ast":
      dom.result.replaceChildren(
        result.ast
          ? el("pre", {}, el("code", { text: result.ast }))
          : placeholder("The schema did not parse."),
      );
      break;
    case "model":
      dom.result.replaceChildren(
        result.model
          ? el("pre", {}, el("code", { text: result.model }))
          : placeholder("The schema did not check."),
      );
      break;
  }
  dom.result.scrollTop = 0;
}

function placeholder(text) {
  return el("div", { class: "placeholder" }, el("p", { text }));
}

/** A generated file's contents, highlighted for the language its extension implies. */
function codeBlock(file) {
  const code = el("code", {});
  code.innerHTML = highlight(file.contents, languageForFile(file.name));
  return code;
}

function renderProblems() {
  if (result.error) {
    return el("div", { class: "diagnostics" }, diagnosticCard({ severity: "error", message: result.error }));
  }
  if (result.diagnostics.length === 0) {
    return placeholder("No errors, no warnings.");
  }
  return el("div", { class: "diagnostics" }, result.diagnostics.map(diagnosticCard));
}

function diagnosticCard(diagnostic) {
  const { severity, message, line, column, rendered } = diagnostic;
  const where = line ? `${line}:${column}` : null;
  return el(
    "div",
    { class: `diagnostic ${severity}` },
    el(
      "button",
      {
        type: "button",
        class: "diagnostic-head",
        title: where ? "Go to this line" : null,
        onClick: where ? () => jumpTo(line, column) : null,
      },
      el("span", { class: "badge", text: severity }),
      el("span", { text: message }),
      where ? el("span", { class: "diagnostic-where", text: where }) : null,
    ),
    rendered ? el("pre", { text: rendered }) : null,
  );
}

function renderSummary(summary) {
  const types = summary.types.map((type) =>
    el(
      "tr",
      {},
      el("td", { class: "name", text: type.name }),
      el("td", { text: type.kind }),
      el("td", { class: "size", text: sizeText(type.size) }),
      el(
        "td",
        {},
        type.root ? el("span", { class: "tag", text: "root" }) : null,
        // Only worth showing where it was chosen: a nested type is encoded in
        // its container's byte order, not its own.
        type.endianExplicit ? el("span", { class: "tag", text: `${type.endian}-endian` }) : null,
      ),
    ),
  );

  const consts = summary.consts.map((constant) =>
    el(
      "tr",
      {},
      el("td", { class: "name", text: constant.name }),
      el("td", { text: constant.type }),
      el("td", { class: "size", text: constant.value }),
    ),
  );

  const services = summary.services.flatMap((service) => [
    el(
      "p",
      { class: "service-name" },
      service.name,
      el("span", { text: service.uuid }),
    ),
    table(
      ["Characteristic", "Type", "Size", "Properties", "UUID"],
      service.characteristics.map((characteristic) =>
        el(
          "tr",
          {},
          el("td", { class: "name", text: characteristic.name }),
          el("td", { class: "name", text: characteristic.type }),
          el("td", { class: "size", text: sizeText(characteristic.size) }),
          el("td", { text: characteristic.properties.join(", ") }),
          el("td", { class: "uuid", text: characteristic.uuid }),
        ),
      ),
    ),
  ]);

  return el(
    "div",
    { class: "summary" },
    el(
      "div",
      { class: "facts" },
      el("span", { class: "fact" }, el("b", { text: summary.endian }), "-endian by default"),
      el("span", { class: "fact", text: plural(summary.types.length, "type") }),
      summary.consts.length > 0
        ? el("span", { class: "fact", text: plural(summary.consts.length, "constant") })
        : null,
      el("span", { class: "fact", text: plural(summary.services.length, "service") }),
    ),
    el("h3", { text: "Types" }),
    table(["Name", "Kind", "Size", ""], types),
    consts.length > 0 ? el("h3", { text: "Constants" }) : null,
    consts.length > 0 ? table(["Name", "Type", "Value"], consts) : null,
    services.length > 0 ? el("h3", { text: "GATT bindings" }) : null,
    services,
  );
}

function table(headers, rows) {
  return el(
    "table",
    {},
    el("thead", {}, el("tr", {}, headers.map((header) => el("th", { text: header })))),
    el("tbody", {}, rows),
  );
}

function renderStatus() {
  if (!result) return;
  if (result.error) {
    setStatus("bad", result.error);
    return;
  }
  const errors = result.diagnostics.filter((d) => d.severity === "error").length;
  const warnings = result.diagnostics.length - errors;
  const warned = warnings > 0 ? `, ${plural(warnings, "warning")}` : "";

  if (errors > 0) {
    setStatus("bad", `${plural(errors, "error")}${warned} — no code generated`);
    return;
  }
  const written = result.files.map((file) => `${file.name} (${lines(file.contents)}, ${bytes(file.contents.length)})`);
  setStatus("ok", `${written.join(", ")}${warned}`);
}

function setStatus(kind, text) {
  dom.status.className = `status ${kind}`;
  dom.status.textContent = text;
}

function fatal(error) {
  console.error(error);
  setStatus("bad", `Could not start the compiler: ${error.message}`);
  dom.generate.disabled = true;
  dom.result.replaceChildren(
    placeholder("The WebAssembly module could not be loaded. Reload the page to try again."),
  );
}

// --------------------------------------------------------------------- boot

/** Set once a compile has trapped, which stops the reload from retrying it. */
let crashed = false;

/** Loads (or reloads) the compiler and fills in what it tells us about itself. */
async function boot() {
  defgen = await loadDefgen();
  if (dom.backends.childElementCount === 0) {
    renderBackends(defgen.backends());
  }
  dom.generate.disabled = false;
  if (!crashed) compile();
}

function renderBackends(backends) {
  const wanted = dom.backends.dataset.wanted;
  const selected = backends.some((backend) => backend.name === wanted) ? wanted : backends[0].name;

  dom.backends.replaceChildren(
    ...backends.map((backend) =>
      el(
        "label",
        { title: backend.description },
        el("input", {
          type: "radio",
          name: "backend",
          value: backend.name,
          checked: backend.name === selected,
          onChange: () => {
            describeBackend(backends);
            save();
            compile();
          },
        }),
        backend.name,
      ),
    ),
  );
  describeBackend(backends);
}

function describeBackend(backends) {
  const { backend } = options();
  dom.backendHint.textContent = backends.find((b) => b.name === backend)?.description ?? "";
}

function fillExamples() {
  dom.examples.replaceChildren(
    el("option", { value: "", text: "Load an example…" }),
    ...EXAMPLES.map((example) => el("option", { value: example.id, text: example.label })),
  );
}

async function useExample(id) {
  const example = EXAMPLES.find((candidate) => candidate.id === id);
  if (!example) return;
  try {
    dom.source.value = await loadExample(example);
  } catch (error) {
    setStatus("bad", `Could not load the example: ${error.message}`);
    return;
  }
  dom.stem.value = example.stem;
  history.replaceState(null, "", location.pathname);
  onEdit();
  compile();
}

function download() {
  const file = result?.files[activeFile];
  if (!file) return;
  const url = URL.createObjectURL(new Blob([file.contents], { type: "text/plain" }));
  const link = el("a", { href: url, download: file.name });
  link.click();
  URL.revokeObjectURL(url);
}

async function copy() {
  const file = result?.files[activeFile];
  if (!file) return;
  try {
    await navigator.clipboard.writeText(file.contents);
    setStatus("ok", `Copied ${file.name} to the clipboard.`);
  } catch {
    setStatus("bad", "The browser would not allow copying — select the code and copy it instead.");
  }
}

function wire() {
  dom.toolbar.addEventListener("submit", (event) => {
    event.preventDefault();
    compile();
  });
  dom.source.addEventListener("input", onEdit);
  dom.source.addEventListener("scroll", syncGutter);
  dom.source.addEventListener("keydown", onEditorKeydown);
  dom.stem.addEventListener("input", () => {
    save();
    if (dom.optAuto.checked) scheduleCompile();
  });
  for (const toggle of [dom.optAst, dom.optModel]) {
    toggle.addEventListener("change", () => {
      save();
      compile();
    });
  }
  dom.optAuto.addEventListener("change", save);
  dom.examples.addEventListener("change", (event) => {
    useExample(event.target.value);
    event.target.value = "";
  });
  dom.files.addEventListener("change", (event) => {
    activeFile = Number(event.target.value);
    renderPanel();
  });
  dom.copy.addEventListener("click", copy);
  dom.download.addEventListener("click", download);
  dom.share.addEventListener("click", async () => {
    const link = shareLink();
    history.replaceState(null, "", link);
    try {
      await navigator.clipboard.writeText(link);
      setStatus("ok", "Link copied — it carries the schema and these options.");
    } catch {
      setStatus("bad", "The browser would not allow copying, but the address bar now has the link.");
    }
  });
}

async function start() {
  wire();
  fillExamples();
  device.mount(dom.device);
  dom.generate.disabled = true;

  const saved = restore();
  if (saved?.source) {
    dom.source.value = saved.source;
    dom.stem.value = saved.stem || "schema";
    dom.optAst.checked = Boolean(saved.ast);
    dom.optModel.checked = Boolean(saved.model);
    dom.optAuto.checked = saved.auto !== false;
    if (saved.backend) dom.backends.dataset.wanted = saved.backend;
  } else {
    const [first] = EXAMPLES;
    dom.source.value = await loadExample(first).catch(() => "");
    dom.stem.value = first.stem;
  }
  syncGutter();
  highlightSchema();
  await boot();
}

start().catch(fatal);
