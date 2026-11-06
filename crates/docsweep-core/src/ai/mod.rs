//! Optional help from a language model, with the user's own key. A portal run
//! works without any of this.
//!
//! What leaves the computer is built by [`sanitize`], and what comes back is
//! untrusted: it is only parsed into the typed recipe model and validated.

pub mod output;
pub mod prompts;
pub mod provider;
pub mod sanitize;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
