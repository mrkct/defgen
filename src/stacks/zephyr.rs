//! The Zephyr stack: a `BT_GATT_SERVICE_DEFINE` table per `service`, wired to
//! the C backend's codecs.
//!
//! # Shape of the output
//!
//! Two files, because a service table is not a header. `BT_GATT_SERVICE_DEFINE`
//! expands to objects in a linker section, so it has to live in exactly one
//! translation unit — unlike the codec header, which is `static inline`
//! throughout and meant to be included everywhere. So:
//!
//! | File | Contents |
//! |---|---|
//! | `<stem>_gatt.h` | the application-facing surface: hooks to implement, notify helpers |
//! | `<stem>_gatt.c` | UUID objects, ATT access callbacks, the service table |
//!
//! `<stem>_gatt.h` includes `<stem>.h`, so the codec header has to be
//! generated alongside it — `defgen server` does that by default.
//!
//! # The hook surface
//!
//! Every readable characteristic gets an application hook that is *declared and
//! not defined*:
//!
//! ```c
//! int hearing_aid_control_status_char_read(struct bt_conn *conn, Status *out);
//! ```
//!
//! A missing one is a link error rather than a characteristic that silently
//! reads back zeroes — the §0 "fail loud by default" rule applied to the part
//! of the server the schema cannot describe. The generated ATT callback around
//! it does the encoding, the length handling and the error mapping, so the
//! application only ever sees decoded values of the schema's own types.
//!
//! # What is deliberately not generated
//!
//! Advertising, connection management and `bt_enable` are the application's:
//! the schema says nothing about them (§10). Security permissions are derived
//! from `properties` (`read` implies `BT_GATT_PERM_READ`) because SPEC.md §14
//! puts security metadata out of scope for v1 — a schema that needs encrypted
//! access edits the generated table's permissions, or waits for the schema to
//! grow the vocabulary.

use super::Stack;
use crate::ast::{Docs, Property};
use crate::backends::{Generated, GeneratedFile, Options, c::ident, sanitize_stem, screaming, snake};
use crate::model::{Characteristic, Model, Service};

pub struct ZephyrStack;

impl Stack for ZephyrStack {
    fn name(&self) -> &'static str {
        "zephyr"
    }

    fn description(&self) -> &'static str {
        "a Zephyr GATT service table (BT_GATT_SERVICE_DEFINE) plus ATT callbacks"
    }

    fn generate(&self, model: &Model, opts: &Options) -> Generated {
        let stem = sanitize_stem(&opts.stem);
        let e = Emitter::new(model, &stem, opts.source.as_deref());
        Generated {
            files: vec![
                GeneratedFile { name: format!("{stem}_gatt.h"), contents: e.clone().header() },
                GeneratedFile { name: format!("{stem}_gatt.c"), contents: e.source() },
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

/// The prefix every generated name for one characteristic is built from:
/// service and characteristic together, since §10 lets two services declare
/// characteristics of the same name.
fn chr_prefix(service: &Service, c: &Characteristic) -> String {
    format!("{}_{}", snake(&service.name), snake(&c.name))
}

/// A UUID as the initializer Zephyr's `bt_uuid_*` types take, picked by the
/// form the schema wrote it in (§10 allows all three). The 128-bit form goes
/// through `BT_UUID_128_ENCODE`, which takes the UUID's five fields in the
/// order they are written and handles the byte reversal itself — so unlike the
/// C backend's `*_UUID_BYTES`, nothing here reverses anything by hand.
fn uuid_init(uuid: &str) -> (&'static str, String) {
    let hex: String = uuid.chars().filter(|c| *c != '-').collect();
    match hex.len() {
        4 => ("bt_uuid_16", format!("BT_UUID_INIT_16(0x{hex})")),
        8 => ("bt_uuid_32", format!("BT_UUID_INIT_32(0x{hex})")),
        _ => {
            let f = |range: std::ops::Range<usize>| &hex[range];
            (
                "bt_uuid_128",
                format!(
                    "BT_UUID_INIT_128(BT_UUID_128_ENCODE(0x{}, 0x{}, 0x{}, 0x{}, 0x{}))",
                    f(0..8),
                    f(8..12),
                    f(12..16),
                    f(16..20),
                    f(20..32)
                ),
            )
        }
    }
}

/// `BT_GATT_CHRC_*` flags for a characteristic's declared properties (§10).
fn chrc_flags(c: &Characteristic) -> String {
    let flags: Vec<String> = c
        .properties
        .iter()
        .map(|p| {
            let name = match p {
                Property::Read => "READ",
                Property::Write => "WRITE",
                Property::WriteWithoutResponse => "WRITE_WITHOUT_RESP",
                Property::Notify => "NOTIFY",
                Property::Indicate => "INDICATE",
            };
            format!("BT_GATT_CHRC_{name}")
        })
        .collect();
    if flags.is_empty() { "0".to_string() } else { flags.join(" | ") }
}

/// ATT permissions for the value attribute, derived from `properties`.
///
/// §14 keeps security metadata out of the schema for v1, so this is the plain
/// unauthenticated mapping: a readable characteristic is readable, a writable
/// one is writable, and a notify-only one needs no permission on its value at
/// all (a subscriber reads through the CCC, not the value).
fn perm_flags(c: &Characteristic) -> String {
    let mut flags = Vec::new();
    if c.properties.contains(&Property::Read) {
        flags.push("BT_GATT_PERM_READ");
    }
    if c.properties.iter().any(|p| matches!(p, Property::Write | Property::WriteWithoutResponse)) {
        flags.push("BT_GATT_PERM_WRITE");
    }
    if flags.is_empty() { "BT_GATT_PERM_NONE".to_string() } else { flags.join(" | ") }
}

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

/// One characteristic, with everything both files need to agree on already
/// worked out: what it is called, what it carries, and where its value
/// attribute lands in the service's attribute array.
struct Bound<'m> {
    service: &'m Service,
    chr: &'m Characteristic,
    /// `hearing_aid_control_status_char`.
    prefix: String,
    /// The C type the characteristic carries, e.g. `Status`.
    ty: String,
    /// The macro holding the largest buffer an encoding can need.
    cap: String,
    /// `snake` prefix of that type's codec functions, e.g. `status`.
    codec: String,
    /// Index of the value attribute within `BT_GATT_SERVICE_DEFINE`'s array —
    /// what `bt_gatt_notify` and friends need a pointer to.
    value_attr: usize,
    readable: bool,
    writable: bool,
    notify: bool,
    indicate: bool,
}

impl Bound<'_> {
    fn subscribable(&self) -> bool {
        self.notify || self.indicate
    }
}

#[derive(Clone)]
struct Emitter<'m> {
    m: &'m Model,
    out: String,
    stem: String,
    source: Option<&'m str>,
}

impl<'m> Emitter<'m> {
    fn new(m: &'m Model, stem: &str, source: Option<&'m str>) -> Emitter<'m> {
        Emitter { m, out: String::with_capacity(16 * 1024), stem: stem.to_string(), source }
    }

    /// Every characteristic in the schema, in declaration order, with its
    /// attribute index resolved.
    ///
    /// The index is the one fact the two generated files could disagree about,
    /// so it is computed once here rather than twice at the point of use. It
    /// follows from how `BT_GATT_SERVICE_DEFINE` lays its array out: the
    /// primary service takes one attribute, each characteristic takes two (its
    /// declaration and its value), and a `BT_GATT_CCC` takes one more.
    fn bound(&self) -> Vec<Bound<'m>> {
        let m = self.m;
        let mut out = Vec::new();
        for service in &m.services {
            let mut idx = 1;
            for chr in &service.characteristics {
                let def = m.get(chr.ty);
                let prefix = screaming(&def.name);
                let notify = chr.properties.contains(&Property::Notify);
                let indicate = chr.properties.contains(&Property::Indicate);
                out.push(Bound {
                    service,
                    chr,
                    prefix: chr_prefix(service, chr),
                    ty: ident(&def.name),
                    cap: if chr.layout.is_variable() {
                        format!("{prefix}_MAX_SIZE")
                    } else {
                        format!("{prefix}_SIZE")
                    },
                    codec: snake(&def.name),
                    value_attr: idx + 1,
                    readable: chr.properties.contains(&Property::Read),
                    writable: chr
                        .properties
                        .iter()
                        .any(|p| matches!(p, Property::Write | Property::WriteWithoutResponse)),
                    notify,
                    indicate,
                });
                idx += 2;
                if notify || indicate {
                    idx += 1;
                }
            }
        }
        out
    }

    // -- output primitives --------------------------------------------------
    //
    // Deliberately the same set the C backend has, spelled the same way: the
    // two files sit next to each other in a project, and generated code that
    // changes house style halfway through reads like two different tools.

    fn line(&mut self, ind: usize, text: &str) {
        for _ in 0..ind {
            self.out.push_str("    ");
        }
        self.out.push_str(text);
        self.out.push('\n');
    }

    fn lines(&mut self, ind: usize, texts: &[&str]) {
        for text in texts {
            self.line(ind, text);
        }
    }

    fn blank(&mut self) {
        self.out.push('\n');
    }

    fn banner(&mut self, title: &str) {
        let rule = "-".repeat(68usize.saturating_sub(title.len()));
        self.blank();
        self.line(0, &format!("/* {title} {rule} */"));
        self.blank();
    }

    /// Doc comments as Doxygen (§1, §12).
    fn docs(&mut self, ind: usize, docs: &Docs) {
        if docs.is_empty() {
            return;
        }
        self.line(ind, "/**");
        for doc in docs {
            let text = doc.text.replace("*/", "*\\/");
            if text.is_empty() {
                self.line(ind, " *");
            } else {
                self.line(ind, &format!(" * {text}"));
            }
        }
        self.line(ind, " */");
    }

    fn note(&mut self, ind: usize, text: &str) {
        const WIDTH: usize = 78;
        let budget = WIDTH.saturating_sub(ind * 4);
        if text.len() + 7 <= budget {
            self.line(ind, &format!("/** {text} */"));
            return;
        }
        self.line(ind, "/**");
        let mut current = String::new();
        for word in text.split_whitespace() {
            if !current.is_empty() && current.len() + 1 + word.len() + 3 > budget {
                self.line(ind, &format!(" * {current}"));
                current.clear();
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        if !current.is_empty() {
            self.line(ind, &format!(" * {current}"));
        }
        self.line(ind, " */");
    }

    fn file_header(&mut self, summary: &[&str]) {
        let from = match self.source {
            Some(path) => format!(" from `{path}`"),
            None => String::new(),
        };
        self.line(0, "/*");
        self.line(0, &format!(" * Generated by defgen{from}. Do not edit."));
        self.line(0, " *");
        for text in summary {
            if text.is_empty() {
                self.line(0, " *");
            } else {
                self.line(0, &format!(" * {text}"));
            }
        }
        self.line(0, " */");
    }

    /// The one-line reminder of what a characteristic carries, which is what
    /// makes the hook signatures below readable without the schema open.
    fn carries(&mut self, b: &Bound) {
        let size = match b.chr.layout.tail {
            None => format!("{} bytes", b.chr.layout.fixed_bytes()),
            Some(_) => format!("{}..{} bytes", b.chr.layout.fixed_bytes(), b.chr.layout.max_bytes()),
        };
        let props: Vec<&str> = b.chr.properties.iter().map(|p| p.as_str()).collect();
        let ty = b.ty.clone();
        self.note(0, &format!("`{}` — carries a `{ty}` ({size}), [{}].", b.chr.name, props.join(", ")));
    }

    // ---------------------------------------------------------------------
    // The header
    // ---------------------------------------------------------------------

    fn header(mut self) -> String {
        let guard = format!("{}_GATT_H", self.stem.to_ascii_uppercase());
        let registered =
            format!("link time by {}_gatt.c, and what is left for the application is the", self.stem);
        self.file_header(&[
            "Zephyr GATT server for this schema's services: the table is registered at",
            &registered,
            "hooks declared below.",
            "",
            "Every `*_read`/`*_write` hook here is declared and not defined: the",
            "application supplies it, and the linker is what reports a missing one.",
            "Hooks return 0 on success, or a negative errno to fail the ATT operation.",
        ]);
        self.line(0, &format!("#ifndef {guard}"));
        self.line(0, &format!("#define {guard}"));
        self.blank();
        self.lines(
            0,
            &[
                "#include <stdbool.h>",
                "#include <stddef.h>",
                "#include <stdint.h>",
                "",
                "#include <zephyr/bluetooth/conn.h>",
                "#include <zephyr/bluetooth/gatt.h>",
            ],
        );
        self.blank();
        self.note(0, "The codecs these hooks exchange values in.");
        let stem = self.stem.clone();
        self.line(0, &format!("#include \"{stem}.h\""));
        self.blank();
        self.lines(0, &["#ifdef __cplusplus", "extern \"C\" {", "#endif"]);

        for b in self.bound() {
            self.header_characteristic(&b);
        }

        self.blank();
        self.lines(0, &["#ifdef __cplusplus", "} /* extern \"C\" */", "#endif"]);
        self.blank();
        self.line(0, &format!("#endif /* {guard} */"));
        self.out
    }

    fn header_characteristic(&mut self, b: &Bound) {
        let title = format!("{}.{}", b.service.name, b.chr.name);
        self.banner(&title);
        self.docs(0, &b.chr.docs);
        self.carries(b);
        self.blank();

        let Bound { prefix, ty, .. } = b;
        self.note(
            0,
            "The value attribute, for the bt_gatt_* calls this header does not wrap \
             (bt_gatt_indicate with your own parameters, say).",
        );
        self.line(0, &format!("const struct bt_gatt_attr *{prefix}_attr(void);"));
        self.blank();

        if b.readable {
            self.note(
                0,
                &format!(
                    "Supplies the value for an ATT read of `{}`. Fill `out` and return 0; \
                     return a negative errno to fail the read.",
                    b.chr.name
                ),
            );
            self.line(0, &format!("int {prefix}_read(struct bt_conn *conn, {ty} *out);"));
            self.blank();
        }

        if b.writable {
            self.note(
                0,
                &format!(
                    "Receives a decoded ATT write of `{}`. Return 0 to accept it; return a \
                     negative errno to fail the write. A payload that does not decode never \
                     reaches here — the ATT layer has already rejected it.",
                    b.chr.name
                ),
            );
            self.line(0, &format!("int {prefix}_write(struct bt_conn *conn, const {ty} *value);"));
            self.blank();
        }

        if b.subscribable() {
            self.note(
                0,
                "Whether any client currently has notifications or indications enabled — \
                 worth checking before doing work only a subscriber would see.",
            );
            self.line(0, &format!("bool {prefix}_is_subscribed(void);"));
            self.blank();
        }

        if b.notify {
            self.note(
                0,
                "Encodes `v` and notifies `conn`, or every subscriber when `conn` is NULL. \
                 Returns 0, or a negative errno from the encode or from bt_gatt_notify.",
            );
            self.line(0, &format!("int {prefix}_notify(struct bt_conn *conn, const {ty} *v);"));
            self.blank();
        }

        if b.indicate {
            self.note(
                0,
                "Encodes `v` and indicates it to `conn`. `params` and the generated payload \
                 buffer are both read until `params->func` fires, so one indication per \
                 characteristic may be outstanding at a time.",
            );
            self.line(
                0,
                &format!(
                    "int {prefix}_indicate(struct bt_conn *conn, struct bt_gatt_indicate_params \
                     *params, const {ty} *v);"
                ),
            );
            self.blank();
        }
    }

    // ---------------------------------------------------------------------
    // The translation unit
    // ---------------------------------------------------------------------

    fn source(mut self) -> String {
        self.file_header(&[
            "Zephyr GATT server for this schema's services: UUID objects, the ATT",
            "callbacks that encode and decode through the generated codecs, and the",
            "BT_GATT_SERVICE_DEFINE table itself.",
            "",
            "The table registers itself at link time, so nothing here needs calling",
            "from the application's start-up path — only bt_enable() and advertising,",
            "which are outside what the schema describes.",
        ]);
        let stem = self.stem.clone();
        self.line(0, &format!("#include \"{stem}_gatt.h\""));
        self.blank();
        self.lines(
            0,
            &[
                "#include <errno.h>",
                "#include <stddef.h>",
                "#include <stdint.h>",
                "",
                "#include <zephyr/bluetooth/att.h>",
                "#include <zephyr/bluetooth/gatt.h>",
                "#include <zephyr/bluetooth/uuid.h>",
                "#include <zephyr/sys/util.h>",
            ],
        );

        let bound = self.bound();
        // A schema whose characteristics are all notify-only has no ATT
        // callback to map an error for, and an unused `static` function is a
        // warning in every build a firmware project runs.
        if bound.iter().any(|b| b.readable || b.writable) {
            self.error_mapping();
        }
        self.uuids();
        for b in &bound {
            self.callbacks(b);
        }

        self.tables();

        for b in &bound {
            self.helpers(b);
        }
        self.out
    }

    /// The one place a `defgen_err_t` becomes an ATT error code.
    ///
    /// Which code a failure earns is a judgement about the protocol, not about
    /// the codec, so it lives here rather than being repeated — and guessed at
    /// slightly differently — in every generated callback.
    fn error_mapping(&mut self) {
        self.banner("Errors");
        self.note(
            0,
            "A codec failure as an ATT error. A length the type has no encoding for is \
             INVALID_ATTRIBUTE_LEN; a payload of the right length that means nothing (an \
             undeclared enum value, non-zero validated padding, malformed UTF-8) is \
             VALUE_NOT_ALLOWED.",
        );
        self.line(0, "static ssize_t defgen_att_error(defgen_err_t err)");
        self.line(0, "{");
        self.line(1, "switch (err) {");
        self.line(1, "case DEFGEN_ERR_LENGTH:");
        self.line(1, "case DEFGEN_ERR_BUFFER_TOO_SMALL:");
        self.line(2, "return BT_GATT_ERR(BT_ATT_ERR_INVALID_ATTRIBUTE_LEN);");
        self.line(1, "case DEFGEN_ERR_RANGE:");
        self.line(1, "case DEFGEN_ERR_UNKNOWN_VALUE:");
        self.line(1, "case DEFGEN_ERR_PADDING:");
        self.line(1, "case DEFGEN_ERR_UTF8:");
        self.line(2, "return BT_GATT_ERR(BT_ATT_ERR_VALUE_NOT_ALLOWED);");
        self.line(1, "default:");
        self.line(2, "return BT_GATT_ERR(BT_ATT_ERR_UNLIKELY);");
        self.line(1, "}");
        self.line(0, "}");
        self.blank();

        self.note(0, "An application hook's negative errno as an ATT error.");
        self.line(0, "static ssize_t defgen_hook_error(int err)");
        self.line(0, "{");
        self.line(1, "switch (err) {");
        self.line(1, "case -EACCES:");
        self.line(2, "return BT_GATT_ERR(BT_ATT_ERR_READ_NOT_PERMITTED);");
        self.line(1, "case -EINVAL:");
        self.line(2, "return BT_GATT_ERR(BT_ATT_ERR_VALUE_NOT_ALLOWED);");
        self.line(1, "default:");
        self.line(2, "return BT_GATT_ERR(BT_ATT_ERR_UNLIKELY);");
        self.line(1, "}");
        self.line(0, "}");
        self.blank();
    }

    fn uuids(&mut self) {
        self.banner("UUIDs");
        self.note(
            0,
            "Non-const because BT_GATT_PRIMARY_SERVICE and BT_GATT_CHARACTERISTIC store \
             their UUID as the attribute's `user_data`, which is a `void *`.",
        );
        self.blank();

        let services = self.m.services.clone();
        for service in &services {
            let (ty, init) = uuid_init(&service.uuid);
            let name = snake(&service.name);
            self.docs(0, &service.docs);
            self.note(0, &format!("`{}` — {}", service.name, service.uuid));
            self.line(0, &format!("static struct {ty} {name}_uuid = {init};"));
            for chr in &service.characteristics {
                let (ty, init) = uuid_init(&chr.uuid);
                let prefix = chr_prefix(service, chr);
                self.note(0, &format!("`{}` — {}", chr.name, chr.uuid));
                self.line(0, &format!("static struct {ty} {prefix}_uuid = {init};"));
            }
            self.blank();
        }
    }

    /// The ATT callbacks for one characteristic: everything between the wire
    /// and the application's hook.
    fn callbacks(&mut self, b: &Bound) {
        let title = format!("{}.{}", b.service.name, b.chr.name);
        self.banner(&title);
        self.carries(b);
        self.blank();

        let Bound { prefix, ty, cap, codec, .. } = b;

        if b.subscribable() {
            self.line(0, &format!("static bool {prefix}_subscribed;"));
            self.blank();
            self.line(
                0,
                &format!("static void {prefix}_ccc_changed(const struct bt_gatt_attr *attr, uint16_t value)"),
            );
            self.line(0, "{");
            self.line(1, "ARG_UNUSED(attr);");
            self.line(1, &format!("{prefix}_subscribed = value != 0;"));
            self.line(0, "}");
            self.blank();
            self.line(0, &format!("bool {prefix}_is_subscribed(void)"));
            self.line(0, "{");
            self.line(1, &format!("return {prefix}_subscribed;"));
            self.line(0, "}");
            self.blank();
        }

        if b.readable {
            self.note(
                0,
                "Encodes into a stack buffer and hands it to bt_gatt_attr_read, which does \
                 the offset and MTU slicing a long read needs.",
            );
            self.line(0, &format!("static ssize_t {prefix}_read_cb(struct bt_conn *conn,"));
            self.line(4, "const struct bt_gatt_attr *attr, void *buf,");
            self.line(4, "uint16_t len, uint16_t offset)");
            self.line(0, "{");
            self.line(1, &format!("{ty} value;"));
            self.line(1, &format!("uint8_t encoded[{cap}];"));
            self.line(1, "size_t encoded_len;");
            self.line(1, "defgen_err_t err;");
            self.line(1, "int hook;");
            self.blank();
            self.line(1, &format!("hook = {prefix}_read(conn, &value);"));
            self.line(1, "if (hook != 0) {");
            self.line(2, "return defgen_hook_error(hook);");
            self.line(1, "}");
            self.line(1, &format!("err = {codec}_encode(&value, encoded, sizeof encoded, &encoded_len);"));
            self.line(1, "if (err != DEFGEN_OK) {");
            self.line(2, "return defgen_att_error(err);");
            self.line(1, "}");
            self.line(
                1,
                "return bt_gatt_attr_read(conn, attr, buf, len, offset, encoded, (uint16_t)encoded_len);",
            );
            self.line(0, "}");
            self.blank();
        }

        if b.writable {
            self.note(
                0,
                "Decodes the whole payload before the application sees it. A partial write \
                 has no meaning for a value with a fixed layout (§6), so a non-zero offset \
                 and a prepare-write are both refused rather than reassembled.",
            );
            self.line(0, &format!("static ssize_t {prefix}_write_cb(struct bt_conn *conn,"));
            self.line(4, "const struct bt_gatt_attr *attr, const void *buf,");
            self.line(4, "uint16_t len, uint16_t offset, uint8_t flags)");
            self.line(0, "{");
            self.line(1, &format!("{ty} value;"));
            self.line(1, "defgen_err_t err;");
            self.line(1, "int hook;");
            self.blank();
            self.line(1, "ARG_UNUSED(attr);");
            self.line(1, "if (offset != 0) {");
            self.line(2, "return BT_GATT_ERR(BT_ATT_ERR_INVALID_OFFSET);");
            self.line(1, "}");
            self.line(1, "if ((flags & BT_GATT_WRITE_FLAG_PREPARE) != 0) {");
            self.line(2, "return BT_GATT_ERR(BT_ATT_ERR_NOT_SUPPORTED);");
            self.line(1, "}");
            self.line(1, &format!("err = {codec}_decode(&value, (const uint8_t *)buf, len);"));
            self.line(1, "if (err != DEFGEN_OK) {");
            self.line(2, "return defgen_att_error(err);");
            self.line(1, "}");
            self.line(1, &format!("hook = {prefix}_write(conn, &value);"));
            self.line(1, "if (hook != 0) {");
            self.line(2, "return defgen_hook_error(hook);");
            self.line(1, "}");
            self.line(1, "return (ssize_t)len;");
            self.line(0, "}");
            self.blank();
        }

        if b.indicate {
            self.note(
                0,
                "Payload for an outstanding indication: bt_gatt_indicate reads it after it \
                 returns, so it cannot live on the caller's stack.",
            );
            self.line(0, &format!("static uint8_t {prefix}_ind_payload[{cap}];"));
            self.blank();
        }
    }

    /// One `BT_GATT_SERVICE_DEFINE` per service.
    ///
    /// The attribute order here is what [`Emitter::bound`] predicted when it
    /// worked out each characteristic's value index; the two must not drift.
    fn tables(&mut self) {
        self.banner("Service tables");
        self.note(
            0,
            "BT_GATT_SERVICE_DEFINE registers the table in a linker section, so it takes \
             effect without being referenced from anywhere.",
        );
        self.blank();

        // Continuation lines align under `BT_GATT_CHARACTERISTIC(`'s own open
        // paren, which is where a hand-written Zephyr table puts them.
        const PAD: &str = "                           ";

        let services = self.m.services.clone();
        for service in &services {
            let sname = snake(&service.name);
            let mut entries: Vec<String> = vec![format!("BT_GATT_PRIMARY_SERVICE(&{sname}_uuid.uuid)")];
            for chr in &service.characteristics {
                let prefix = chr_prefix(service, chr);
                let readable = chr.properties.contains(&Property::Read);
                let writable = chr
                    .properties
                    .iter()
                    .any(|p| matches!(p, Property::Write | Property::WriteWithoutResponse));
                let read_cb = if readable { format!("{prefix}_read_cb") } else { "NULL".to_string() };
                let write_cb = if writable { format!("{prefix}_write_cb") } else { "NULL".to_string() };
                entries.push(format!(
                    "BT_GATT_CHARACTERISTIC(&{prefix}_uuid.uuid,\n{PAD}{},\n{PAD}{},\n{PAD}{read_cb}, \
                     {write_cb}, NULL)",
                    chrc_flags(chr),
                    perm_flags(chr)
                ));
                if chr.properties.iter().any(|p| matches!(p, Property::Notify | Property::Indicate)) {
                    entries.push(format!(
                        "BT_GATT_CCC({prefix}_ccc_changed, BT_GATT_PERM_READ | BT_GATT_PERM_WRITE)"
                    ));
                }
            }

            self.docs(0, &service.docs);
            self.line(0, &format!("BT_GATT_SERVICE_DEFINE({sname}_svc,"));
            let last = entries.len() - 1;
            for (i, entry) in entries.into_iter().enumerate() {
                // No trailing comma on the last one: it would pass
                // BT_GATT_SERVICE_DEFINE an empty variadic argument, which
                // -pedantic has an opinion about.
                let comma = if i == last { "" } else { "," };
                self.line(1, &format!("{entry}{comma}"));
            }
            self.line(0, ");");
            self.blank();
        }
    }

    /// The application-facing functions that need the table to exist: the
    /// attribute accessor and the notify/indicate wrappers.
    fn helpers(&mut self, b: &Bound) {
        let title = format!("{}.{} helpers", b.service.name, b.chr.name);
        self.banner(&title);

        let Bound { prefix, ty, cap, codec, value_attr, .. } = b;
        let svc = snake(&b.service.name);

        self.note(
            0,
            &format!(
                "Attribute {value_attr} of `{svc}_svc`: the primary service takes one \
                 attribute, and every characteristic before this one takes two, plus one \
                 more for a CCC."
            ),
        );
        self.line(0, &format!("const struct bt_gatt_attr *{prefix}_attr(void)"));
        self.line(0, "{");
        self.line(1, &format!("return &{svc}_svc.attrs[{value_attr}];"));
        self.line(0, "}");
        self.blank();

        if b.notify {
            self.line(0, &format!("int {prefix}_notify(struct bt_conn *conn, const {ty} *v)"));
            self.line(0, "{");
            self.line(1, &format!("uint8_t encoded[{cap}];"));
            self.line(1, "size_t encoded_len;");
            self.line(1, "defgen_err_t err;");
            self.blank();
            self.line(1, &format!("err = {codec}_encode(v, encoded, sizeof encoded, &encoded_len);"));
            self.line(1, "if (err != DEFGEN_OK) {");
            self.line(2, "return -EINVAL;");
            self.line(1, "}");
            self.line(
                1,
                &format!("return bt_gatt_notify(conn, {prefix}_attr(), encoded, (uint16_t)encoded_len);"),
            );
            self.line(0, "}");
            self.blank();
        }

        if b.indicate {
            self.line(
                0,
                &format!(
                    "int {prefix}_indicate(struct bt_conn *conn, struct bt_gatt_indicate_params *params,"
                ),
            );
            self.line(4, &format!("const {ty} *v)"));
            self.line(0, "{");
            self.line(1, "size_t encoded_len;");
            self.line(1, "defgen_err_t err;");
            self.blank();
            self.line(
                1,
                &format!(
                    "err = {codec}_encode(v, {prefix}_ind_payload, sizeof {prefix}_ind_payload, \
                     &encoded_len);"
                ),
            );
            self.line(1, "if (err != DEFGEN_OK) {");
            self.line(2, "return -EINVAL;");
            self.line(1, "}");
            self.line(1, &format!("params->attr = {prefix}_attr();"));
            self.line(1, &format!("params->data = {prefix}_ind_payload;"));
            self.line(1, "params->len = (uint16_t)encoded_len;");
            self.line(1, "return bt_gatt_indicate(conn, params);");
            self.line(0, "}");
            self.blank();
        }
    }
}
