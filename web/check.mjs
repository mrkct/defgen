#!/usr/bin/env node
// Checks the playground's compiler against the CLI.
//
// The point of the playground is that it is the same compiler, so this
// generates every example schema with every backend twice — once through the
// wasm module, once by running the binary — and requires the two to agree
// byte for byte, diagnostics included.
//
//   ./web/build.sh && node web/check.mjs [path/to/defgen]
//
// The binary defaults to target/release/defgen.

import { spawnSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { Defgen } from "./site/defgen.js";

const web = path.dirname(fileURLToPath(import.meta.url));
const examples = path.join(web, "site", "examples");
const cli = process.argv[2] ?? path.join(web, "..", "target", "release", "defgen");

const { instance } = await WebAssembly.instantiate(
  readFileSync(path.join(web, "site", "defgen.wasm")),
  {},
);
const defgen = new Defgen(instance);
const backends = defgen.backends();

let checked = 0;
const failures = [];

function fail(what, expected, actual) {
  const line = [...expected].findIndex((c, i) => c !== actual[i]);
  failures.push(
    `${what}\n  expected ${expected.length} bytes, got ${actual.length}` +
      (line < 0 ? "" : `\n  first difference at byte ${line}:\n` +
        `    cli:  ${JSON.stringify(expected.slice(line, line + 60))}\n` +
        `    wasm: ${JSON.stringify(actual.slice(line, line + 60))}`),
  );
}

for (const file of readdirSync(examples).filter((name) => name.endsWith(".defs")).sort()) {
  const stem = path.basename(file, ".defs");
  const source = readFileSync(path.join(examples, file), "utf8");

  for (const { name } of backends) {
    // Run the CLI from the examples directory so the path it stamps into the
    // "do not edit" banner is the bare file name the playground passes.
    const run = spawnSync(cli, ["codec", file, "--language", name], { cwd: examples, encoding: "utf8" });
    if (run.error) {
      throw run.error.code === "ENOENT"
        ? new Error(`no defgen binary at ${cli} — build one with \`cargo build --release\``)
        : run.error;
    }
    // Code on stdout, diagnostics on stderr — the CLI colours the latter only
    // for a terminal, which a pipe is not, so both sides render the same text.
    const { stdout: expected, stderr: diagnostics } = run;

    const result = defgen.compile(source, { backend: name, stem });
    if (!result.ok) {
      failures.push(`${file} + ${name}: the wasm module reported ${result.error ?? "errors"}`);
      continue;
    }
    const actual = result.files.map((generated) => generated.contents).join("");
    if (actual !== expected) fail(`${file} + ${name}: generated code differs`, expected, actual);

    const rendered = result.diagnostics.map((d) => d.rendered).join("");
    if (rendered !== diagnostics) fail(`${file} + ${name}: diagnostics differ`, diagnostics, rendered);

    checked++;
  }
}

// A schema with an error has to fail the same way in both, and produce nothing.
const broken = "endian: little;\n---\nstruct S: u8 {\n    a: u4,\n}\n";
const brokenResult = defgen.compile(broken, { backend: "c", stem: "broken" });
if (brokenResult.ok || brokenResult.files.length > 0) {
  failures.push("a schema that does not check produced code");
}
if (!brokenResult.diagnostics.some((d) => d.severity === "error")) {
  failures.push("a schema that does not check reported no error");
}

if (failures.length > 0) {
  console.error(failures.join("\n\n"));
  process.exit(1);
}
console.log(`ok: ${checked} schema/backend pairs match the CLI byte for byte`);
