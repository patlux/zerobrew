//! Standalone app bundles use a separate path from relocatable formula kegs.
//! No code here rewrites Mach-O files, removes quarantine or re-signs an app.
mod archive;
mod bundle;

pub use archive::stage_app;
pub use bundle::{BundleMetadata, fingerprint, inspect_bundle};

use zb_core::Error;

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidArgument {
        message: message.into(),
    }
}
