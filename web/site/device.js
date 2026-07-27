// The Device tab: connects to a real BLE peripheral with Web Bluetooth and
// drives it with the schema just compiled, instead of only showing the code
// for it. Feature-detected and inert everywhere Web Bluetooth doesn't exist
// (Firefox, Safari, iOS — see the note rendered into the tab itself).
//
// The generated javascript backend's module is loaded live: `setSchema`
// wraps its source in a `Blob` and `import()`s that, so every read/write here
// goes through the exact same encode/decode `defgen` just generated, never a
// second hand-written codec. What drives the generic form (structs, enums,
// unions, arrays, scaled values, all nestable) is `summary.types[].shape` —
// see `web/wasm/src/lib.rs` — a recursive description of each type, with the
// exact class/property names the javascript backend gave it so this module
// never has to re-derive JavaScript's own naming/escaping rules.
//
// Two Web Bluetooth realities shape a lot of the code below:
//  - There is no explicit "pair" call in the API. Pairing, where a device
//    demands it, happens at the OS level the moment an operation needs it —
//    the best a page can do is prompt one (Pair below) and report whether it
//    worked.
//  - `getPrimaryServices()` only ever returns services the page named in
//    `optionalServices`/`filters` when it called `requestDevice`. This module
//    asks for every service the schema declares, so "discovered but not in
//    the schema" can only happen for a *characteristic* on a known service —
//    a whole unknown service is not something Web Bluetooth will ever hand
//    back here, by construction.

const BASE_UUID_SUFFIX = "-0000-1000-8000-00805f9b34fb";
const FULL_UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const PROPERTY_LABELS = {
  read: "read",
  write: "write",
  write_without_response: "write w/o resp.",
  notify: "notify",
  indicate: "indicate",
};

// ---------------------------------------------------------------- utilities

/** Same tiny builder as app.js; duplicated rather than imported so this tab
 * stays a self-contained, independently loadable module. */
function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (value === null || value === false || value === undefined) continue;
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

function placeholder(text) {
  return el("div", { class: "placeholder" }, el("p", { text }));
}

function supported() {
  return "bluetooth" in navigator;
}

/** The three GATT UUID forms (§10) collapsed to one canonical 128-bit string,
 * so a short-form schema UUID compares equal to what `getPrimaryServices()`
 * always returns. */
function canonicalUuid(raw) {
  const hex = raw.trim().toLowerCase();
  if (FULL_UUID.test(hex)) return hex;
  if (/^[0-9a-f]{4}$/.test(hex)) return `0000${hex}${BASE_UUID_SUFFIX}`;
  if (/^[0-9a-f]{8}$/.test(hex)) return `${hex}${BASE_UUID_SUFFIX}`;
  return hex;
}

/** Whether a value of this carrier width is a JS `bigint` rather than a
 * `number` in the generated module — the same `> 32` cutoff the javascript
 * backend itself uses (see its module doc). */
function carrierIsBig(carrierBits) {
  return carrierBits > 32;
}

function formatInt(value) {
  return typeof value === "bigint" ? value.toString() : String(value);
}

function parseIntInput(text, big) {
  const trimmed = text.trim();
  if (!/^-?\d+$/.test(trimmed)) throw new Error(`"${text}" is not a whole number`);
  return big ? BigInt(trimmed) : Number(trimmed);
}

function hexToBytes(text) {
  const clean = text.trim().replace(/[\s:]/g, "");
  if (clean === "") return new Uint8Array(0);
  if (!/^[0-9a-fA-F]+$/.test(clean) || clean.length % 2 !== 0) {
    throw new Error("expected an even number of hex digits (spaces/colons allowed as separators)");
  }
  const bytes = new Uint8Array(clean.length / 2);
  for (let i = 0; i < bytes.length; i++) bytes[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16);
  return bytes;
}

function bytesToHex(bytes) {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join(" ");
}

function propertyLabel(property) {
  return PROPERTY_LABELS[property] ?? property;
}

function describeError(error) {
  if (error instanceof DOMException) {
    switch (error.name) {
      case "NotFoundError":
        return "Not found — either nothing was picked, or the device no longer offers it.";
      case "SecurityError":
        return "Blocked by the browser's Bluetooth permission policy.";
      case "NetworkError":
        return "The GATT connection was lost, or the operation failed at the radio level.";
      case "NotSupportedError":
        return "Not supported by this characteristic.";
      default:
        return `${error.name}: ${error.message}`;
    }
  }
  return error?.message ?? String(error);
}

async function writeBytes(characteristic, bytes, withResponse) {
  if (withResponse && characteristic.writeValueWithResponse) return characteristic.writeValueWithResponse(bytes);
  if (!withResponse && characteristic.writeValueWithoutResponse) {
    return characteristic.writeValueWithoutResponse(bytes);
  }
  return characteristic.writeValue(bytes);
}

// ------------------------------------------------------------------- codecs

/** How a characteristic's root value is actually encoded/decoded — dispatches
 * on the `jsCodec` the wasm summary computed (see `codec` in lib.rs). */
function decodeValue(schemaChar, bytes, ctx) {
  const { jsCodec, valueType } = schemaChar;
  if (jsCodec.kind === "class") return ctx.module[valueType.jsName].decode(bytes);
  return ctx.module[jsCodec.decode](bytes);
}

function encodeValue(schemaChar, value, ctx) {
  const { jsCodec } = schemaChar;
  return jsCodec.kind === "class" ? value.encode() : ctx.module[jsCodec.encode](value);
}

// ------------------------------------------------------ value editors (write)
//
// `buildValueEditor(wire, ctx)` walks a `WireType` descriptor (the same shape
// a struct field, an array element or a characteristic's own root value all
// share) and returns `{node, get, set}`: a DOM node, a getter that reads a
// live value back out of it, and a setter that pre-fills it — used both to
// build a fresh value to write and to load one just read for a
// read-modify-write round trip. Nothing here validates a number against its
// declared range: the generated `encode()` already throws a precise
// `DefgenRangeError`/`DefgenLengthError`/... for that, and duplicating the
// check here would just be a second copy of it to keep in sync.

function unsupportedEditor(message) {
  return {
    node: el("span", { class: "value-unsupported", text: message }),
    get: () => {
      throw new Error(message);
    },
    set: () => {},
  };
}

function boolEditor() {
  const input = el("input", { type: "checkbox" });
  return { node: input, get: () => input.checked, set: (v) => (input.checked = Boolean(v)) };
}

function intEditor(wire) {
  const big = carrierIsBig(wire.carrierBits);
  const input = big
    ? el("input", { type: "text", inputmode: "numeric", class: "num-input", placeholder: `${wire.min}…${wire.max}` })
    : el("input", { type: "number", class: "num-input", min: wire.min, max: wire.max, step: "1" });
  input.value = "0";
  return {
    node: input,
    get: () => parseIntInput(input.value, big),
    set: (v) => (input.value = formatInt(v)),
  };
}

function floatEditor() {
  const input = el("input", { type: "number", class: "num-input", step: "any" });
  input.value = "0";
  return {
    node: input,
    get: () => {
      const v = Number(input.value);
      if (!Number.isFinite(v)) throw new Error(`"${input.value}" is not a number`);
      return v;
    },
    set: (v) => (input.value = String(v)),
  };
}

function scaledEditor(shape) {
  const input = el("input", { type: "number", class: "num-input", step: "any" });
  input.value = "0";
  input.title = `raw ${shape.rawMin}…${shape.rawMax} (${shape.physical}), scale ${shape.scale}${
    shape.offset ? `, offset ${shape.offset}` : ""
  }`;
  return {
    node: input,
    get: () => {
      const v = Number(input.value);
      if (!Number.isFinite(v)) throw new Error(`"${input.value}" is not a number`);
      return v;
    },
    set: (v) => (input.value = String(v)),
  };
}

function stringEditor(wire) {
  const input = el("input", { type: "text", class: "str-input" });
  input.title = `up to ${wire.max} UTF-8 bytes`;
  return { node: input, get: () => input.value, set: (v) => (input.value = v ?? "") };
}

function arrayEditor(wire, ctx, count, maxCount, variable) {
  const rows = [];
  const list = el("div", { class: "array-editor" });

  function addRow(initial) {
    const child = buildValueEditor(wire.elem, ctx);
    if (initial !== undefined) child.set(initial);
    const row = el(
      "div",
      { class: "array-row" },
      child.node,
      variable
        ? el("button", {
            type: "button",
            class: "array-remove",
            text: "✕",
            onClick: () => {
              list.removeChild(row);
              rows.splice(rows.indexOf(child), 1);
            },
          })
        : null,
    );
    list.append(row);
    rows.push(child);
  }

  for (let i = 0; i < count; i++) addRow();
  const addBtn = variable
    ? el("button", {
        type: "button",
        class: "array-add",
        text: "+ add element",
        onClick: () => rows.length < maxCount && addRow(),
      })
    : null;

  return {
    node: el("div", { class: "array-field" }, list, addBtn),
    get: () => rows.map((row) => row.get()),
    set: (values = []) => {
      list.replaceChildren();
      rows.length = 0;
      for (const v of values) addRow(v);
    },
  };
}

function structFieldRows(fields, ctx) {
  return fields.map((field) => ({ field, editor: buildValueEditor(field.type, ctx) }));
}

function structFieldsNode(rows) {
  return el(
    "div",
    { class: "struct-editor" },
    rows.map(({ field, editor }) =>
      el(
        "label",
        { class: `struct-field${field.reserved ? " reserved" : ""}` },
        el("span", {
          class: "field-name",
          text: field.name,
          title: field.reserved ? "reserved: firmware-private, normally left as last read" : "",
        }),
        editor.node,
      ),
    ),
  );
}

function structEditor(jsName, shape, ctx) {
  const rows = structFieldRows(shape.fields, ctx);
  return {
    node: structFieldsNode(rows),
    get: () => {
      const value = {};
      for (const { field, editor } of rows) value[field.jsName] = editor.get();
      return new ctx.module[jsName](value);
    },
    set: (instance) => {
      if (!instance) return;
      for (const { field, editor } of rows) editor.set(instance[field.jsName]);
    },
  };
}

function enumEditor(shape, ctx) {
  const big = carrierIsBig(shape.carrierBits);
  const select = el(
    "select",
    {},
    shape.variants.map((v) => el("option", { value: v.jsName, text: `${v.name} (${v.value})` })),
  );
  let otherInput = null;
  if (shape.elseArm) {
    select.append(el("option", { value: "__other__", text: "Other (raw value)…" }));
    otherInput = el("input", { type: "text", inputmode: "numeric", class: "num-input", hidden: true });
    otherInput.placeholder = "raw value";
    select.addEventListener("change", () => (otherInput.hidden = select.value !== "__other__"));
  }

  return {
    node: el("div", { class: "enum-editor" }, select, otherInput),
    get: () => {
      if (select.value === "__other__") {
        const raw = parseIntInput(otherInput.value, big);
        return new ctx.module[shape.elseArm.jsName]({ raw });
      }
      const variant = shape.variants.find((v) => v.jsName === select.value);
      return big ? BigInt(variant.value) : Number(variant.value);
    },
    set: (value) => {
      if (value && typeof value === "object") {
        select.value = "__other__";
        if (otherInput) {
          otherInput.hidden = false;
          otherInput.value = formatInt(value.raw);
        }
        return;
      }
      const variant = shape.variants.find((v) => BigInt(v.value) === BigInt(value));
      if (variant) select.value = variant.jsName;
      if (otherInput) otherInput.hidden = true;
    },
  };
}

function unionEditor(shape, ctx) {
  const big = carrierIsBig(shape.tagCarrierBits);
  const select = el(
    "select",
    {},
    shape.variants.map((v) => el("option", { value: v.jsName, text: `${v.name} (id ${v.id})` })),
    shape.elseArm ? el("option", { value: "__other__", text: "Other (raw id)…" }) : null,
  );
  const body = el("div", { class: "union-body" });
  // Built lazily and kept around switching back and forth doesn't lose what
  // was typed into a variant not currently shown.
  const variantRows = new Map();
  let otherTagInput = null;
  let otherRawInput = null;

  function showVariant(jsName) {
    body.replaceChildren();
    if (jsName === "__other__") {
      otherTagInput = el("input", { type: "text", inputmode: "numeric", class: "num-input" });
      const rows = [el("label", {}, el("span", { text: shape.tagName }), otherTagInput)];
      if (shape.elseArm.rawBits > 0) {
        otherRawInput = el("input", { type: "text", inputmode: "numeric", class: "num-input" });
        rows.push(el("label", {}, el("span", { text: "raw payload" }), otherRawInput));
      } else {
        otherRawInput = null;
      }
      body.append(...rows);
      return;
    }
    if (!variantRows.has(jsName)) {
      const v = shape.variants.find((x) => x.jsName === jsName);
      variantRows.set(jsName, structFieldRows(v.fields, ctx));
    }
    body.append(structFieldsNode(variantRows.get(jsName)));
  }

  select.addEventListener("change", () => showVariant(select.value));
  showVariant(select.value);

  return {
    node: el("div", { class: "union-editor" }, select, body),
    get: () => {
      if (select.value === "__other__") {
        const args = { [shape.tagJsName]: parseIntInput(otherTagInput.value, big) };
        if (otherRawInput) args.raw = parseIntInput(otherRawInput.value, carrierIsBig(shape.elseArm.rawCarrierBits));
        return new ctx.module[shape.elseArm.jsName](args);
      }
      const value = {};
      for (const { field, editor } of variantRows.get(select.value)) value[field.jsName] = editor.get();
      return new ctx.module[select.value](value);
    },
    set: (instance) => {
      if (!instance) return;
      const ctorName = instance.constructor.name;
      const variant = shape.variants.find((v) => v.jsName === ctorName);
      if (variant) {
        select.value = variant.jsName;
        showVariant(variant.jsName);
        for (const { field, editor } of variantRows.get(variant.jsName)) editor.set(instance[field.jsName]);
        return;
      }
      if (shape.elseArm && ctorName === shape.elseArm.jsName) {
        select.value = "__other__";
        showVariant("__other__");
        otherTagInput.value = formatInt(instance[shape.tagJsName]);
        if (otherRawInput) otherRawInput.value = formatInt(instance.raw);
      }
    },
  };
}

/** Dispatches a `{kind: "named", name, jsName}` reference to the editor for
 * whatever that type's own shape turns out to be — an alias recurses
 * straight into its target with no wrapper (aliases have no runtime type of
 * their own, §3), every other kind gets its own editor. */
function namedEditor(wire, ctx) {
  const type = ctx.typesByName.get(wire.name);
  if (!type) return unsupportedEditor(`unknown type ${wire.name}`);
  const shape = type.shape;
  switch (shape.form) {
    case "alias":
      return buildValueEditor(shape.target, ctx);
    case "scaled":
      return scaledEditor(shape);
    case "enum":
      return enumEditor(shape, ctx);
    case "union":
      return unionEditor(shape, ctx);
    case "struct":
      return structEditor(wire.jsName, shape, ctx);
    default:
      return unsupportedEditor(`unhandled type shape "${shape.form}"`);
  }
}

function buildValueEditor(wire, ctx) {
  switch (wire.kind) {
    case "bool":
      return boolEditor();
    case "uint":
    case "int":
      return intEditor(wire);
    case "float":
      return floatEditor();
    case "string":
      return stringEditor(wire);
    case "array":
      return arrayEditor(wire, ctx, wire.count, wire.count, false);
    case "vararray":
      return arrayEditor(wire, ctx, 0, wire.max, true);
    case "named":
      return namedEditor(wire, ctx);
    default:
      return unsupportedEditor(`unhandled wire kind "${wire.kind}"`);
  }
}

// -------------------------------------------------------- value display (read)
//
// The read-only counterpart of the editors above: renders an already-decoded
// value (an instance from a generated class, or a plain number/bigint/string/
// array for a root alias/scaled/enum) as a small DOM tree, recursing the same
// way over the same shape metadata.

function renderValue(wire, value, ctx) {
  switch (wire.kind) {
    case "bool":
      return el("span", { class: "value-bool", text: value ? "true" : "false" });
    case "uint":
    case "int":
      return el("span", { class: "value-num", text: formatInt(value) });
    case "float":
      return el("span", { class: "value-num", text: String(value) });
    case "string":
      return el("span", { class: "value-str", text: JSON.stringify(value) });
    case "array":
    case "vararray":
      return el(
        "div",
        { class: "value-array" },
        Array.from(value, (item, i) =>
          el(
            "div",
            { class: "value-array-item" },
            el("span", { class: "value-index", text: `[${i}]` }),
            renderValue(wire.elem, item, ctx),
          ),
        ),
      );
    case "named":
      return renderNamedValue(wire, value, ctx);
    default:
      return el("span", { class: "value-unknown", text: String(value) });
  }
}

function renderNamedValue(wire, value, ctx) {
  const type = ctx.typesByName.get(wire.name);
  const shape = type?.shape;
  if (!shape) return el("span", { text: String(value) });
  switch (shape.form) {
    case "alias":
      return renderValue(shape.target, value, ctx);
    case "scaled":
      return el("span", { class: "value-scaled", text: `${value} ${shape.physical}` });
    case "enum": {
      if (value && typeof value === "object") {
        return el("span", { class: "value-enum-unknown", text: `${shape.elseArm.name}(${formatInt(value.raw)})` });
      }
      const variant = shape.variants.find((v) => BigInt(v.value) === BigInt(value));
      return el("span", { class: "value-enum", text: variant ? `${variant.name} (${variant.value})` : String(value) });
    }
    case "union": {
      const ctorName = value?.constructor?.name;
      const variant = shape.variants.find((v) => v.jsName === ctorName);
      if (variant) {
        return el(
          "div",
          { class: "value-union" },
          el("div", { class: "value-union-variant", text: variant.name }),
          renderFieldsValue(variant.fields, value, ctx),
        );
      }
      if (shape.elseArm && ctorName === shape.elseArm.jsName) {
        return el("div", { class: "value-union" }, [
          el("div", {
            class: "value-union-variant",
            text: `${shape.elseArm.name} (id ${formatInt(value[shape.tagJsName])})`,
          }),
          shape.elseArm.rawBits > 0
            ? el("div", { class: "value-union-raw", text: `raw: ${formatInt(value.raw)}` })
            : null,
        ]);
      }
      return el("span", { text: "?" });
    }
    case "struct":
      return renderFieldsValue(shape.fields, value, ctx);
    default:
      return el("span", { text: String(value) });
  }
}

function renderFieldsValue(fields, instance, ctx) {
  return el(
    "dl",
    { class: "value-fields" },
    fields.flatMap((field) => [
      el("dt", { text: field.name }),
      el("dd", {}, renderValue(field.type, instance[field.jsName], ctx)),
    ]),
  );
}

// ---------------------------------------------------------------- characteristics
//
// One card per schema-declared characteristic, plus one per characteristic a
// real device turned out to have that the schema doesn't (still uuid-matched
// against a *service* the schema does declare — see the module doc on why a
// wholly unknown service can't come up here).

function buildCharacteristicRow(schemaChar, ctx) {
  const uuid = schemaChar.uuid;
  const canRead = schemaChar.properties.includes("read");
  const canWrite = schemaChar.properties.includes("write");
  const canWriteNoResp = schemaChar.properties.includes("write_without_response");
  const canNotify = schemaChar.properties.includes("notify") || schemaChar.properties.includes("indicate");

  const badge = el("span", { class: "badge" });
  const resultEl = el("div", { class: "char-result" });
  const logEl = el("div", { class: "notify-log", hidden: true });

  let fieldsEditor;
  try {
    fieldsEditor = buildValueEditor(schemaChar.valueType, ctx);
  } catch (error) {
    fieldsEditor = unsupportedEditor(describeError(error));
  }

  const hexInput = el("input", { type: "text", class: "hex-input", placeholder: "e.g. 2a 01 ff" });
  const fieldsPane = el("div", { class: "write-pane" }, fieldsEditor.node);
  const hexPane = el(
    "div",
    { class: "write-pane", hidden: true },
    hexInput,
    el("p", { class: "hint", text: "Space- or colon-separated hex bytes, written as-is — no encoding." }),
  );
  let mode = "fields";
  const modeFields = el("button", {
    type: "button",
    class: "mode-tab active",
    text: "Fields",
    onClick: () => setMode("fields"),
  });
  const modeHex = el("button", {
    type: "button",
    class: "mode-tab",
    text: "Raw hex",
    onClick: () => setMode("hex"),
  });
  function setMode(next) {
    mode = next;
    modeFields.classList.toggle("active", next === "fields");
    modeHex.classList.toggle("active", next === "hex");
    fieldsPane.hidden = next !== "fields";
    hexPane.hidden = next !== "hex";
  }

  function currentEntry() {
    return discovered.get(uuid);
  }

  function reportResult(kind, text) {
    resultEl.replaceChildren(el("p", { class: `result-line ${kind}`, text }));
  }

  function showValue(bytes) {
    const nodes = [
      el(
        "div",
        { class: "value-hex" },
        el("span", { class: "value-hex-label", text: "hex:" }),
        el("code", { text: bytesToHex(bytes) || "(empty)" }),
      ),
    ];
    try {
      const decoded = decodeValue(schemaChar, bytes, ctx);
      nodes.push(el("div", { class: "value-decoded" }, renderValue(schemaChar.valueType, decoded, ctx)));
      fieldsEditor.set(decoded);
    } catch (error) {
      nodes.push(
        el("p", { class: "value-decode-error", text: `Could not decode as ${schemaChar.type}: ${describeError(error)}` }),
      );
    }
    resultEl.replaceChildren(...nodes);
  }

  async function doRead() {
    const entry = currentEntry();
    if (!entry) return;
    readBtn.disabled = true;
    try {
      const dataView = await entry.characteristic.readValue();
      showValue(new Uint8Array(dataView.buffer, dataView.byteOffset, dataView.byteLength));
    } catch (error) {
      reportResult("bad", describeError(error));
    } finally {
      readBtn.disabled = false;
    }
  }

  async function doWrite(withResponse) {
    const entry = currentEntry();
    if (!entry) return;
    let bytes;
    try {
      bytes = mode === "hex" ? hexToBytes(hexInput.value) : encodeValue(schemaChar, fieldsEditor.get(), ctx);
    } catch (error) {
      reportResult("bad", `Could not encode: ${describeError(error)}`);
      return;
    }
    try {
      await writeBytes(entry.characteristic, bytes, withResponse);
      reportResult("ok", `Wrote ${bytes.length} byte${bytes.length === 1 ? "" : "s"}.`);
    } catch (error) {
      reportResult("bad", describeError(error));
    }
  }

  function appendLog(bytes) {
    const line = el(
      "div",
      { class: "notify-entry" },
      el("span", { class: "notify-time", text: new Date().toLocaleTimeString() }),
      el("code", { text: bytesToHex(bytes) || "(empty)" }),
    );
    try {
      line.append(renderValue(schemaChar.valueType, decodeValue(schemaChar, bytes, ctx), ctx));
    } catch (error) {
      line.append(el("span", { class: "value-decode-error", text: `decode error: ${describeError(error)}` }));
    }
    logEl.prepend(line);
    while (logEl.children.length > 50) logEl.removeChild(logEl.lastChild);
  }

  let notifyHandler = null;
  async function toggleNotify() {
    const entry = currentEntry();
    if (!entry) return;
    if (notifyHandler) {
      try {
        await entry.characteristic.stopNotifications();
      } catch {
        // The connection may already be gone; there is nothing left to undo.
      }
      entry.characteristic.removeEventListener("characteristicvaluechanged", notifyHandler);
      notifyHandler = null;
      notifyToggle.textContent = "Subscribe";
      notifyToggle.classList.remove("active");
      return;
    }
    const handler = (event) => {
      const dv = event.target.value;
      appendLog(new Uint8Array(dv.buffer, dv.byteOffset, dv.byteLength));
    };
    entry.characteristic.addEventListener("characteristicvaluechanged", handler);
    try {
      await entry.characteristic.startNotifications();
      notifyHandler = handler;
      notifyToggle.textContent = "Unsubscribe";
      notifyToggle.classList.add("active");
      logEl.hidden = false;
    } catch (error) {
      entry.characteristic.removeEventListener("characteristicvaluechanged", handler);
      reportResult("bad", describeError(error));
    }
  }

  const readBtn = canRead ? el("button", { type: "button", class: "char-btn", text: "Read", onClick: doRead }) : null;
  const writeBtn = canWrite
    ? el("button", { type: "button", class: "char-btn", text: "Write", onClick: () => doWrite(true) })
    : null;
  const writeNoRespBtn = canWriteNoResp
    ? el("button", { type: "button", class: "char-btn", text: "Write w/o response", onClick: () => doWrite(false) })
    : null;
  const notifyToggle = canNotify
    ? el("button", { type: "button", class: "char-btn", text: "Subscribe", onClick: toggleNotify })
    : null;

  const row = el(
    "div",
    { class: "char" },
    el(
      "div",
      { class: "char-head" },
      el("span", { class: "char-name", text: schemaChar.name }),
      el("code", { class: "char-uuid", text: uuid }),
      badge,
      el(
        "span",
        { class: "prop-badges" },
        schemaChar.properties.map((p) => el("span", { class: "prop-badge", text: propertyLabel(p) })),
      ),
    ),
    el("div", { class: "char-actions" }, readBtn, writeBtn, writeNoRespBtn, notifyToggle),
    resultEl,
    canWrite || canWriteNoResp
      ? el("div", { class: "char-write" }, el("div", { class: "mode-tabs" }, modeFields, modeHex), fieldsPane, hexPane)
      : null,
    canNotify ? logEl : null,
  );

  function updateMatch() {
    const entry = currentEntry();
    badge.textContent = entry ? "on device" : "not on device";
    badge.className = `badge ${entry ? "matched" : "unmatched"}`;
    const properties = entry?.characteristic.properties;
    if (readBtn) readBtn.disabled = !properties?.read;
    if (writeBtn) writeBtn.disabled = !properties?.write;
    if (writeNoRespBtn) writeNoRespBtn.disabled = !properties?.writeWithoutResponse;
    if (notifyToggle) notifyToggle.disabled = !(properties?.notify || properties?.indicate);
    if (!entry && notifyHandler) {
      // The browser already tore the subscription down along with the
      // connection; this just brings the button's label back in step.
      notifyHandler = null;
      if (notifyToggle) {
        notifyToggle.textContent = "Subscribe";
        notifyToggle.classList.remove("active");
      }
    }
  }

  updateMatch();
  return row;
}

function buildUnknownCharacteristicRow(characteristic) {
  const resultEl = el("div", { class: "char-result" });
  const hexInput = el("input", { type: "text", class: "hex-input", placeholder: "e.g. 2a 01 ff" });
  const props = characteristic.properties;

  async function doRead() {
    try {
      const dv = await characteristic.readValue();
      const bytes = new Uint8Array(dv.buffer, dv.byteOffset, dv.byteLength);
      resultEl.replaceChildren(el("div", { class: "value-hex" }, el("code", { text: bytesToHex(bytes) || "(empty)" })));
    } catch (error) {
      resultEl.replaceChildren(el("p", { class: "result-line bad", text: describeError(error) }));
    }
  }

  async function doWrite(withResponse) {
    let bytes;
    try {
      bytes = hexToBytes(hexInput.value);
    } catch (error) {
      resultEl.replaceChildren(el("p", { class: "result-line bad", text: describeError(error) }));
      return;
    }
    try {
      await writeBytes(characteristic, bytes, withResponse);
      resultEl.replaceChildren(el("p", { class: "result-line ok", text: `Wrote ${bytes.length} byte(s).` }));
    } catch (error) {
      resultEl.replaceChildren(el("p", { class: "result-line bad", text: describeError(error) }));
    }
  }

  return el(
    "div",
    { class: "char char-unknown" },
    el(
      "div",
      { class: "char-head" },
      el("span", { class: "char-name", text: "Unknown characteristic" }),
      el("code", { class: "char-uuid", text: characteristic.uuid }),
      el("span", { class: "badge unmatched", text: "not in schema" }),
    ),
    el(
      "div",
      { class: "char-actions" },
      props.read ? el("button", { type: "button", class: "char-btn", text: "Read", onClick: doRead }) : null,
      props.write
        ? el("button", { type: "button", class: "char-btn", text: "Write", onClick: () => doWrite(true) })
        : null,
      props.writeWithoutResponse
        ? el("button", { type: "button", class: "char-btn", text: "Write w/o response", onClick: () => doWrite(false) })
        : null,
    ),
    props.write || props.writeWithoutResponse ? el("div", { class: "char-write" }, hexInput) : null,
    resultEl,
  );
}

function renderServiceCard(service, ctx) {
  const knownUuids = new Set(service.characteristics.map((c) => c.uuid));
  const bleService = discoveredServices.get(service.uuid);
  const unknownRows = [];
  if (bleService) {
    for (const [uuid, entry] of discovered) {
      if (entry.service === bleService && !knownUuids.has(uuid)) unknownRows.push(buildUnknownCharacteristicRow(entry.characteristic));
    }
  }
  return el(
    "section",
    { class: "gatt-service" },
    el(
      "div",
      { class: "service-head" },
      el("span", { class: "service-name", text: service.name }),
      el("code", { class: "service-uuid", text: service.uuid }),
      el("span", { class: `badge ${bleService ? "matched" : "unmatched"}`, text: bleService ? "on device" : "not on device" }),
    ),
    el(
      "div",
      { class: "service-chars" },
      service.characteristics.map((c) => buildCharacteristicRow(c, ctx)),
      unknownRows,
    ),
  );
}

// -------------------------------------------------------------------- state

/** DOM built once by [`mount`]. */
let els = null;
/** `{ module, typesByName, services, lastJsSource, moduleUrl }`, or `null`
 * before the first schema with at least one service compiles. */
let schema = null;
let ble = { device: null, server: null };
/** Canonical characteristic uuid -> `{ service, characteristic }`. */
let discovered = new Map();
/** Canonical service uuid -> `BluetoothRemoteGATTService`. */
let discoveredServices = new Map();
/** Bumped on every connect/disconnect/(re)discovery, so [`render`] — called
 * on every tab switch and every compile, most of which change nothing this
 * tab cares about — can skip rebuilding the DOM (and losing whatever the
 * user was mid-typing into a form) when nothing actually did. */
let discoveryRevision = 0;
let renderedSchema;
let renderedDiscoveryRevision = -1;

function setStatus(kind, text) {
  els.statusEl.className = `device-status ${kind}`;
  els.statusEl.textContent = text;
}

function updateToolbar() {
  const ready = supported() && schema && schema.services.length > 0;
  els.connectBtn.disabled = !ready;
  els.pairBtn.disabled = !ready;
  els.disconnectBtn.disabled = !ble.server?.connected;
  els.forgetBtn.hidden = typeof ble.device?.forget !== "function";
}

// ----------------------------------------------------------- connection

async function connect() {
  if (!supported()) {
    setStatus("bad", "Web Bluetooth isn't available in this browser — try Chrome, Edge or Opera on desktop or Android.");
    return;
  }
  if (!schema || schema.services.length === 0) {
    setStatus("bad", "This schema doesn't declare any GATT services yet.");
    return;
  }
  els.connectBtn.disabled = true;
  try {
    const device = await navigator.bluetooth.requestDevice({
      acceptAllDevices: true,
      // Chrome only grants access to services named here — see the module
      // doc. Naming every one the schema declares is what makes discovery
      // below able to see them at all.
      optionalServices: schema.services.map((s) => s.uuid),
    });
    ble.device = device;
    device.addEventListener("gattserverdisconnected", onDisconnected);
    setStatus("info", `Connecting to ${device.name || "device"}…`);
    ble.server = await device.gatt.connect();
    await discoverAll();
    setStatus("ok", `Connected to ${device.name || device.id}.`);
  } catch (error) {
    // The user closing the chooser is not a failure worth a red banner.
    if (error?.name !== "NotFoundError") setStatus("bad", describeError(error));
    else setStatus("", "Not connected.");
  } finally {
    updateToolbar();
  }
}

async function discoverAll() {
  discovered.clear();
  discoveredServices.clear();
  const services = await ble.server.getPrimaryServices();
  for (const service of services) {
    discoveredServices.set(service.uuid, service);
    for (const characteristic of await service.getCharacteristics()) {
      discovered.set(characteristic.uuid, { service, characteristic });
    }
  }
  discoveryRevision++;
  render();
}

function onDisconnected() {
  discovered.clear();
  discoveredServices.clear();
  discoveryRevision++;
  setStatus("info", "Device disconnected.");
  render();
  updateToolbar();
}

function disconnect() {
  ble.device?.gatt?.disconnect();
}

async function forget() {
  if (typeof ble.device?.forget !== "function") return;
  await ble.device.forget();
  setStatus("info", "Forgot this device — Connect will show the chooser again.");
  updateToolbar();
}

/**
 * Web Bluetooth has no explicit "pair" call (see the module doc) — pairing
 * happens at the OS level the moment an operation needs it. The best this
 * page can do is trigger that moment on purpose, with a readable
 * characteristic, and report whether the OS let the read through.
 */
async function pair() {
  if (!ble.server?.connected) {
    await connect();
    if (!ble.server?.connected) return;
  }
  const readable = [...discovered.values()].find(({ characteristic }) => characteristic.properties.read);
  if (!readable) {
    setStatus(
      "info",
      "No readable characteristic on this device to check pairing with — try Read or Write directly; " +
        "your OS should prompt to pair automatically if the device requires it.",
    );
    return;
  }
  setStatus("info", "Checking pairing…");
  try {
    await readable.characteristic.readValue();
    setStatus(
      "ok",
      "Read succeeded. If this device needed pairing, your OS should already have prompted for it — " +
        "Web Bluetooth has no separate pairing step of its own.",
    );
  } catch (error) {
    setStatus(
      "bad",
      `${describeError(error)} This device may need to be paired from your OS's Bluetooth settings first, then reconnected here.`,
    );
  }
}

// -------------------------------------------------------------------- render

function rebuild() {
  updateToolbar();
  if (!schema) {
    els.servicesEl.replaceChildren(placeholder("No schema loaded yet."));
    return;
  }
  if (schema.services.length === 0) {
    els.servicesEl.replaceChildren(
      placeholder("This schema doesn't declare any GATT services — add a `service { characteristic … }` block to use this tab."),
    );
    return;
  }
  const ctx = { module: schema.module, typesByName: schema.typesByName };
  els.servicesEl.replaceChildren(...schema.services.map((service) => renderServiceCard(service, ctx)));
}

/** Re-renders the tab — cheaply, if nothing that changes its shape has
 * happened since the last call (see [`discoveryRevision`]). Safe, and meant,
 * to be called far more often than it actually does anything: app.js calls
 * it on every tab switch and after every recompile. */
export function render() {
  if (!els) return;
  if (renderedSchema === schema && renderedDiscoveryRevision === discoveryRevision) return;
  renderedSchema = schema;
  renderedDiscoveryRevision = discoveryRevision;
  rebuild();
}

/** Builds the tab's static shell into `container`. Called once, from app.js's
 * startup. */
export function mount(container) {
  const connectBtn = el("button", { type: "button", class: "device-btn primary", text: "Connect device…", onClick: connect });
  const pairBtn = el("button", { type: "button", class: "device-btn", text: "Pair", onClick: pair });
  const disconnectBtn = el("button", { type: "button", class: "device-btn", text: "Disconnect", onClick: disconnect });
  const forgetBtn = el("button", { type: "button", class: "device-btn", text: "Forget device", onClick: forget, hidden: true });
  const statusEl = el("p", { class: "device-status", text: "Not connected." });
  const servicesEl = el("div", { class: "device-services" });

  els = { connectBtn, pairBtn, disconnectBtn, forgetBtn, statusEl, servicesEl };

  const note = supported()
    ? el(
        "p",
        { class: "device-hint" },
        "Web Bluetooth can only see the services this schema declares — a real device may expose more, shown here as ",
        el("b", { text: "unknown characteristics" }),
        ". Pairing, if the device needs it, is handled by your OS rather than by this page; ",
        el("b", { text: "Pair" }),
        " triggers the prompt and reports whether it went through.",
      )
    : el("p", { class: "device-hint bad" }, "Web Bluetooth isn't available in this browser. Try Chrome, Edge or Opera on desktop or Android.");

  container.replaceChildren(
    el("div", { class: "device-toolbar" }, connectBtn, pairBtn, disconnectBtn, forgetBtn, statusEl),
    note,
    servicesEl,
  );
  updateToolbar();
}

/**
 * Hands the tab a freshly compiled schema: `summary` (from `defgen.compile`)
 * and `jsSource`, the *javascript*-backend text for that same schema
 * regardless of which backend the Code tab has selected. Loads it live via a
 * blob URL and `import()`; a no-op if the source is byte-for-byte what is
 * already loaded, so app.js can call this after every compile without
 * reloading the module (and losing an open connection or a form mid-edit) on
 * every keystroke.
 */
export async function setSchema({ summary, jsSource }) {
  if (schema?.lastJsSource === jsSource) return;

  const moduleUrl = URL.createObjectURL(new Blob([jsSource], { type: "text/javascript" }));
  let module;
  try {
    module = await import(moduleUrl);
  } catch (error) {
    URL.revokeObjectURL(moduleUrl);
    if (els) setStatus("bad", `Could not load the generated module: ${describeError(error)}`);
    return;
  }
  if (schema?.moduleUrl) URL.revokeObjectURL(schema.moduleUrl);

  schema = {
    lastJsSource: jsSource,
    moduleUrl,
    module,
    typesByName: new Map(summary.types.map((t) => [t.name, t])),
    services: summary.services.map((service) => ({
      name: service.name,
      uuid: canonicalUuid(service.uuid),
      characteristics: service.characteristics.map((c) => ({ ...c, uuid: canonicalUuid(c.uuid) })),
    })),
  };
  render();
}
