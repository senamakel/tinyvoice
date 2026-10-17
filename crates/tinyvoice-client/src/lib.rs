//! Typed host calls to the TinyVoice TinyBus module.
//!
//! The client owns the method names and wire conversions. Hosts provide one
//! configured [`tinybus::module::ModuleClient`]; module loading, reporting,
//! deadlines, and transport remain TinyBus responsibilities.

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
