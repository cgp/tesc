//! The compiled-in tier: a trait implemented in-tree and registered by name.
//!
//! For the cases where even Lua's per-call overhead matters (design-engine §7.2) —
//! multi-megabyte XML assembly, request signing, on-the-fly compression — or where an
//! existing Rust type can be serialized straight out with no intermediate
//! representation. No interpreter, no process, no serialization of the context.
//!
//! **The tier is the registry.** A plugin is compiled into the binary, so the set of
//! them is fixed when the engine is built, and a plan naming one that is not here is
//! refused at load with the list of what is. Nothing ships registered today: a plugin
//! exists because some specific plan needs one, and inventing an example to fill the
//! table would put code on the hot path that no plan asks for.

use super::{Built, Context};

/// What a compiled-in generator implements.
///
/// Synchronous and `Sync`: the point of this tier is that there is nothing to wait
/// for. A plugin that wanted to await something wants the exec tier, where the
/// process boundary makes the cost explicit.
pub(crate) trait Plugin: Sync + Send {
    fn build(&self, context: &mut Context<'_>) -> Result<Built, String>;
}

/// Every plugin compiled into this engine, by the name a plan uses.
static REGISTRY: &[(&str, &dyn Plugin)] = &[];

/// One plugin a plan named, resolved against the registry.
pub(crate) struct Registered {
    plugin: &'static dyn Plugin,
}

impl Registered {
    pub fn find(at: &str, name: &str) -> Result<Self, String> {
        let plugin = REGISTRY
            .iter()
            .find(|(registered, _)| *registered == name)
            .map(|(_, plugin)| *plugin)
            .ok_or_else(|| {
                format!(
                    "{at}/name: no plugin named {name:?} is compiled into this engine{}. A \
                     plugin tier is part of the binary, so a plan that needs one needs a \
                     build that has it.",
                    if REGISTRY.is_empty() {
                        "; none are".to_owned()
                    } else {
                        format!(
                            "; compiled in: {}",
                            REGISTRY
                                .iter()
                                .map(|(name, _)| *name)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    }
                )
            })?;
        Ok(Self { plugin })
    }

    pub fn call(&self, context: &mut Context<'_>) -> Result<Built, String> {
        self.plugin.build(context)
    }
}
