//! Provider model presentation with core-owned semantics and localized copy.

pub use tcode_core::provider_models::{
    MAX_SLUG_LEN, ModelCapability, ResolvedModel, SlugError, validate_slug,
};

pub fn model_capability_label(capability: ModelCapability) -> String {
    match capability {
        ModelCapability::FastMode => crate::tr!("providers.models.cap_fast").into_owned(),
        ModelCapability::Thinking => crate::tr!("providers.models.cap_thinking").into_owned(),
        ModelCapability::Reasoning => crate::tr!("providers.models.cap_reasoning").into_owned(),
    }
}

pub fn slug_error_message(error: &SlugError) -> String {
    match error {
        SlugError::Empty => crate::tr!("providers.models.err_empty").into_owned(),
        SlugError::AlreadyBuiltIn => crate::tr!("providers.models.err_builtin").into_owned(),
        SlugError::TooLong => {
            crate::tr!("providers.models.err_too_long", limit = MAX_SLUG_LEN).into_owned()
        }
        SlugError::AlreadySaved => crate::tr!("providers.models.err_saved").into_owned(),
    }
}
