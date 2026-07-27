// The JavaScript half of the wasm boundary defined in web/wasm/src/lib.rs.
//
// Strings cross as (pointer, length) pairs of UTF-8 bytes in the module's
// linear memory, and every result comes back as a buffer holding a
// little-endian u32 length followed by that many bytes of UTF-8 JSON. Both
// sides free with the length they allocated, so nothing has to be tracked
// across the boundary.

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** Include the parsed syntax tree in the result — the CLI's `--ast`. */
const FLAG_AST = 1 << 0;
/** Include the checked model in the result — the CLI's `--model`. */
const FLAG_MODEL = 1 << 1;

export class Defgen {
  #exports;

  constructor(instance) {
    this.#exports = instance.exports;
  }

  /**
   * The backends the module can generate for, as `{name, description}` — read
   * from the compiler's own registry so a new backend appears here without
   * the page being told about it.
   */
  backends() {
    return this.#take(this.#exports.defgen_backends());
  }

  /**
   * Compiles one schema.
   *
   * `stem` is its base name, which backends derive module names, include
   * guards and generated file names from — the part the CLI takes from the
   * file name.
   *
   * Returns `{ok, error, diagnostics, files, summary, ast, model}`. A schema
   * with errors is a normal result with `ok: false`, not a thrown exception;
   * `error` is non-null only for a bad request, such as an unknown backend.
   */
  compile(source, { backend, stem = "schema", ast = false, model = false } = {}) {
    const args = [source, backend, stem].map((text) => this.#write(text));
    const flags = (ast ? FLAG_AST : 0) | (model ? FLAG_MODEL : 0);
    try {
      return this.#take(this.#exports.defgen_compile(...args.flat(), flags));
    } finally {
      for (const [ptr, len] of args) this.#exports.defgen_free(ptr, len);
    }
  }

  /** Copies `text` into the module's memory as `[pointer, length]`. */
  #write(text) {
    const bytes = encoder.encode(text);
    const ptr = this.#exports.defgen_alloc(bytes.length);
    // Read `buffer` only after the call: allocating can grow the memory, which
    // detaches every view taken before it.
    new Uint8Array(this.#exports.memory.buffer, ptr, bytes.length).set(bytes);
    return [ptr, bytes.length];
  }

  /** Decodes a returned buffer and frees it. */
  #take(ptr) {
    const { memory, defgen_free } = this.#exports;
    const len = new DataView(memory.buffer).getUint32(ptr, true);
    // `decode` copies, so the JSON outlives the buffer it came from.
    const json = decoder.decode(new Uint8Array(memory.buffer, ptr + 4, len));
    defgen_free(ptr, 4 + len);
    return JSON.parse(json);
  }
}

/**
 * Fetches and instantiates the compiler. The module imports nothing, so there
 * is no environment to hand it.
 */
export async function loadDefgen(url = "./defgen.wasm") {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`${url}: ${response.status} ${response.statusText}`);
  }
  try {
    // Compiling as it downloads is the fast path, but it insists on an
    // `application/wasm` content type that not every static file server sends.
    const { instance } = await WebAssembly.instantiateStreaming(response.clone(), {});
    return new Defgen(instance);
  } catch {
    const { instance } = await WebAssembly.instantiate(await response.arrayBuffer(), {});
    return new Defgen(instance);
  }
}
