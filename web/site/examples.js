// The schemas the Example picker offers. They are fetched rather than inlined
// so they stay real `.defs` files that the CLI can compile — `hearing-aid` is
// the spec's worked example, copied in by web/build.sh from
// tests/examples/commands.defs rather than kept as a second copy that could
// drift from it.

export const EXAMPLES = [
  {
    id: "light",
    label: "Light bulb — a first schema",
    stem: "light",
    file: "./examples/light.defs",
  },
  {
    id: "sensor",
    label: "Weather sensor — units, history, byte order",
    stem: "sensor",
    file: "./examples/sensor.defs",
  },
  {
    id: "hearing-aid",
    label: "Hearing aid — every language feature",
    stem: "commands",
    file: "./examples/hearing-aid.defs",
  },
];

const cache = new Map();

/** Fetches an example's source, keeping it for the rest of the session. */
export async function loadExample(example) {
  if (!cache.has(example.id)) {
    const response = await fetch(example.file);
    if (!response.ok) {
      throw new Error(`${example.file}: ${response.status} ${response.statusText}`);
    }
    cache.set(example.id, await response.text());
  }
  return cache.get(example.id);
}
