use cad_ir::{ModelSpec, TessellatedScene};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum KernelError {
    #[error("{0}")]
    Message(String),
    #[error("unsupported model: {0}")]
    Unsupported(String),
    #[error("invalid spec: {0}")]
    Invalid(String),
}

pub trait Kernel {
    fn name(&self) -> &'static str;

    fn build(&mut self, spec: &ModelSpec) -> Result<TessellatedScene, KernelError>;
}
