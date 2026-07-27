// A small syntax highlighter, hand-rolled to match the rest of the site: no
// dependency, no build step. It is not a real lexer for any of these
// languages — just enough regex-driven tokenizing to color a schema and the
// six backends' output.
//
// Each language is a handful of regex fragments plus keyword/type sets,
// combined into one alternation and walked with `exec` in a loop. Alternation
// order matters: earlier fragments win ties, so `typeword` (e.g. `u16`) is
// listed ahead of the generic `ident` fragment it would otherwise also match.

const ESCAPES = { "&": "&amp;", "<": "&lt;", ">": "&gt;" };

function escapeHtml(text) {
  return text.replace(/[&<>]/g, (c) => ESCAPES[c]);
}

function span(cls, text) {
  return `<span class="tok-${cls}">${escapeHtml(text)}</span>`;
}

function words(list) {
  return new Set(list);
}

/** Builds the one big alternation a language's fragments are matched with. */
function pattern(lang) {
  const parts = [];
  if (lang.comment) parts.push(`(?<comment>${lang.comment})`);
  if (lang.attribute) parts.push(`(?<attribute>${lang.attribute})`);
  if (lang.string) parts.push(`(?<string>${lang.string})`);
  if (lang.typeword) parts.push(`(?<typeword>${lang.typeword})`);
  parts.push(`(?<number>${lang.number ?? "\\b0x[0-9a-fA-F_]+\\b|\\b[0-9][0-9_]*(?:\\.[0-9_]+)?(?:[eE][+-]?[0-9]+)?[uUlLfF]*\\b"})`);
  parts.push("(?<ident>[A-Za-z_$][A-Za-z0-9_$]*)");
  return new RegExp(parts.join("|"), "gm");
}

const CORE_KEYWORDS = [
  "break", "case", "class", "const", "continue", "default", "do", "else",
  "enum", "extends", "false", "final", "for", "if", "import", "in",
  "instanceof", "interface", "new", "null", "private", "protected", "public",
  "return", "static", "super", "switch", "this", "throw", "throws", "true",
  "try", "void", "while",
];

const LANGUAGES = {
  // The .defs schema language (GRAMMAR.ebnf). `endian`, property names and
  // the like are contextual keywords in the real grammar — lexed as plain
  // identifiers and recognized by position — but coloring them like keywords
  // here only helps a reader, so this highlighter does not bother with the
  // distinction.
  defs: {
    comment: String.raw`\/\/\/?[^\n]*`,
    string: String.raw`"(?:\\.|[^"\\\n])*"`,
    attribute: String.raw`#\[[^\]\n]*\]`,
    typeword: String.raw`\b[ui][0-9]+\b`,
    keywords: words([
      "alias", "as", "characteristic", "else", "enum", "padding", "reserved",
      "scaled", "service", "string", "struct",
      "endian", "little", "big", "max", "uuid", "properties",
      "read", "write", "write_without_response", "notify", "indicate",
    ]),
    types: words(["bool", "f32", "f64"]),
  },

  c: {
    comment: String.raw`\/\/[^\n]*|\/\*[\s\S]*?\*\/`,
    string: String.raw`"(?:\\.|[^"\\\n])*"|'(?:\\.|[^'\\\n])*'`,
    attribute: String.raw`^[ \t]*#[ \t]*\w[^\n]*`,
    keywords: words([
      ...CORE_KEYWORDS,
      "auto", "defined", "double", "extern", "goto", "inline", "long",
      "register", "restrict", "short", "signed", "sizeof", "struct",
      "typedef", "union", "unsigned", "volatile", "NULL",
    ]),
    types: words([
      "bool", "char", "float", "int", "size_t", "ssize_t",
      "int8_t", "int16_t", "int32_t", "int64_t",
      "uint8_t", "uint16_t", "uint32_t", "uint64_t",
    ]),
  },

  java: {
    comment: String.raw`\/\/[^\n]*|\/\*[\s\S]*?\*\/`,
    string: String.raw`"(?:\\.|[^"\\\n])*"|'(?:\\.|[^'\\\n])*'`,
    attribute: String.raw`@[A-Za-z_][A-Za-z0-9_]*`,
    keywords: words([
      ...CORE_KEYWORDS,
      "abstract", "assert", "boolean", "byte", "catch", "char", "double",
      "finally", "float", "implements", "long", "native", "package", "record",
      "sealed", "short", "strictfp", "synchronized", "transient", "var",
      "permits", "yield", "non-sealed",
    ]),
    types: words([
      "Object", "String", "Integer", "Long", "Short", "Byte", "Double",
      "Float", "Boolean", "Character", "Number", "List", "ArrayList", "Map",
      "HashMap", "Set", "HashSet", "Collections", "Arrays", "Objects",
      "Optional", "BigInteger", "BigDecimal", "Exception", "RuntimeException",
      "IllegalArgumentException", "IllegalStateException", "IndexOutOfBoundsException",
    ]),
  },

  javascript: {
    comment: String.raw`\/\/[^\n]*|\/\*[\s\S]*?\*\/`,
    string: String.raw`"(?:\\.|[^"\\\n])*"|'(?:\\.|[^'\\\n])*'|\`(?:\\.|[^\\\`])*\``,
    attribute: String.raw`@[A-Za-z_][A-Za-z0-9_]*`,
    keywords: words([
      ...CORE_KEYWORDS,
      "async", "await", "delete", "export", "from", "function", "get",
      "let", "of", "set", "typeof", "undefined", "var", "yield", "NaN",
      "Infinity",
    ]),
    types: words([
      "Array", "ArrayBuffer", "BigInt", "Boolean", "DataView", "Date",
      "Error", "Map", "Math", "Number", "Object", "Promise", "Set", "String",
      "Symbol", "TypeError", "RangeError", "Uint8Array", "Int8Array",
      "Uint16Array", "Int16Array", "Uint32Array", "Int32Array",
    ]),
  },

  kotlin: {
    comment: String.raw`\/\/[^\n]*|\/\*[\s\S]*?\*\/`,
    string: String.raw`"""[\s\S]*?"""|"(?:\\.|[^"\\\n])*"|'(?:\\.|[^'\\\n])*'`,
    attribute: String.raw`@[A-Za-z_][A-Za-z0-9_.]*`,
    keywords: words([
      "as", "break", "class", "companion", "const", "constructor",
      "continue", "data", "do", "else", "enum", "false", "for", "fun", "if",
      "import", "in", "init", "inline", "interface", "internal", "is", "it",
      "lateinit", "noinline", "null", "object", "open", "operator",
      "out", "override", "package", "private", "protected", "public",
      "reified", "return", "sealed", "super", "this", "throw", "true", "try",
      "typealias", "val", "var", "vararg", "when", "while", "catch",
      "finally", "crossinline",
    ]),
    types: words([
      "Any", "Array", "Boolean", "Byte", "ByteArray", "Char", "Double",
      "Exception", "Float", "Int", "IntArray", "List", "Long", "Map",
      "MutableList", "MutableMap", "Nothing", "Set", "Short", "String",
      "UByte", "UInt", "ULong", "UShort", "Unit", "BigInteger", "BigDecimal",
    ]),
  },

  swift: {
    comment: String.raw`\/\/[^\n]*|\/\*[\s\S]*?\*\/`,
    string: String.raw`"(?:\\.|[^"\\\n])*"`,
    attribute: String.raw`@[A-Za-z_][A-Za-z0-9_]*`,
    keywords: words([
      "as", "associatedtype", "break", "case", "catch", "class", "continue",
      "default", "defer", "deinit", "do", "else", "enum", "extension",
      "fallthrough", "false", "fileprivate", "final", "for", "func", "guard",
      "if", "import", "in", "indirect", "init", "internal", "is", "let",
      "mutating", "nil", "nonmutating", "open", "operator", "private",
      "protocol", "public", "repeat", "rethrows", "return", "self", "Self",
      "some", "static", "struct", "switch", "throw", "throws", "true", "try",
      "typealias", "var", "where", "while",
    ]),
    types: words([
      "Any", "Array", "Bool", "Character", "Data", "Dictionary", "Double",
      "Error", "Float", "Int", "Int8", "Int16", "Int32", "Int64", "Int128",
      "Set", "String", "UInt", "UInt8", "UInt16", "UInt32", "UInt64",
      "UInt128", "Void",
    ]),
  },

  python: {
    comment: String.raw`#[^\n]*`,
    string: String.raw`(?:[rRbBfF]{1,2})?(?:"""[\s\S]*?"""|'''[\s\S]*?'''|"(?:\\.|[^"\\\n])*"|'(?:\\.|[^'\\\n])*')`,
    attribute: String.raw`@[A-Za-z_][A-Za-z0-9_.]*`,
    keywords: words([
      "and", "as", "assert", "async", "await", "break", "class", "continue",
      "def", "del", "elif", "else", "except", "False", "finally", "for",
      "from", "global", "if", "import", "in", "is", "lambda", "None",
      "nonlocal", "not", "or", "pass", "raise", "return", "True", "try",
      "while", "with", "yield", "self", "cls",
    ]),
    types: words([
      "bool", "bytes", "ClassVar", "dict", "Final", "float", "frozenset",
      "int", "list", "Optional", "set", "str", "tuple", "TypeAlias",
      "TypeVar", "Union",
    ]),
  },
};

for (const lang of Object.values(LANGUAGES)) lang.regex = pattern(lang);

const EXTENSION_LANGUAGE = {
  defs: "defs",
  h: "c",
  c: "c",
  java: "java",
  mjs: "javascript",
  js: "javascript",
  kt: "kotlin",
  swift: "swift",
  py: "python",
};

/** The language this highlighter would use for a file, from its name. */
export function languageForFile(name) {
  const ext = name.slice(name.lastIndexOf(".") + 1).toLowerCase();
  return EXTENSION_LANGUAGE[ext] ?? null;
}

/**
 * Renders `text` as highlighted HTML for `langName`. Falls back to plain
 * escaped text for a language this module does not know.
 */
export function highlight(text, langName) {
  const lang = LANGUAGES[langName];
  if (!lang) return escapeHtml(text);

  lang.regex.lastIndex = 0;
  let out = "";
  let last = 0;
  let match;
  while ((match = lang.regex.exec(text))) {
    out += escapeHtml(text.slice(last, match.index));
    const g = match.groups;
    if (g.comment) out += span("comment", g.comment);
    else if (g.attribute) out += span("attribute", g.attribute);
    else if (g.string) out += span("string", g.string);
    else if (g.typeword) out += span("type", g.typeword);
    else if (g.number) out += span("number", g.number);
    else if (g.ident) {
      if (lang.keywords?.has(g.ident)) out += span("keyword", g.ident);
      else if (lang.types?.has(g.ident)) out += span("type", g.ident);
      else out += escapeHtml(g.ident);
    }
    last = match.index + match[0].length;
  }
  out += escapeHtml(text.slice(last));
  return out;
}
