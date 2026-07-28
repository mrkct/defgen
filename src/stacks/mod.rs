//! GATT server generation for firmware.
//!
//! A [`Backend`](crate::backends::Backend) turns a schema into codecs for a
//! *language*; a [`Stack`] turns the same schema's `service`/`characteristic`
//! bindings (SPEC.md §10) into the service table a *BLE stack* wants — the
//! part SPEC.md §10 leaves to each target, because a Zephyr
//! `BT_GATT_SERVICE_DEFINE`, a NimBLE `struct ble_gatt_svc_def[]` and an
//! ESP-IDF attribute table share nothing but the UUIDs.
//!
//! The two are separate registries, and the CLI keeps them on separate
//! subcommands, because they vary independently: the stack is not a language,
//! and every stack here generates C either way.
//!
//! A stack's output is never self-contained. It calls the C backend's codecs
//! (`status_encode`, `STATUS_SIZE`) and `#include`s its header, so a service
//! table is only ever emitted alongside that header — see `defgen server`,
//! which generates both by default. The dependency is one-way on purpose: the
//! codec never learns that a BLE stack exists, which is what keeps it
//! byte-identical to what every other backend generates (§13).

pub mod zephyr;

use crate::backends::{Generated, Options};
use crate::model::Model;

/// A GATT server generator for one BLE stack.
pub trait Stack {
    /// The name `--stack` accepts. Lowercase, no spaces.
    fn name(&self) -> &'static str;

    /// One line, for `--help` and the "unknown stack" message.
    fn description(&self) -> &'static str;

    /// The language whose codec header this stack's output includes. Only C
    /// today, but naming it here keeps `defgen server` from hard-coding the
    /// pairing.
    fn codec_backend(&self) -> &'static str {
        "c"
    }

    /// Generates the service table for `model`. Infallible, like
    /// [`Backend::generate`](crate::backends::Backend::generate): the model is
    /// already valid. A schema with no `service` at all is rejected by the CLI
    /// before this runs, since there would be nothing to generate.
    fn generate(&self, model: &Model, opts: &Options) -> Generated;
}

/// Every stack, in the order they should be listed.
pub fn all() -> Vec<Box<dyn Stack>> {
    vec![Box::new(zephyr::ZephyrStack)]
}

/// The names `--stack` accepts.
pub fn names() -> Vec<&'static str> {
    all().iter().map(|s| s.name()).collect()
}

/// Looks a stack up by the name `--stack` was given.
pub fn find(name: &str) -> Option<Box<dyn Stack>> {
    all().into_iter().find(|s| s.name() == name)
}
