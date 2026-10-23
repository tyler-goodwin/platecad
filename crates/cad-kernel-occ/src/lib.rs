use cad_ir::{ModelSpec, TessellatedScene};
use cad_kernel::{Kernel, KernelError};

/// Placeholder OpenCascade backend. Linked later; same `Kernel` trait.
#[derive(Debug, Default)]
pub struct OccKernel;

impl Kernel for OccKernel {
    fn name(&self) -> &'static str {
        "occ"
    }

    fn build(&mut self, spec: &ModelSpec) -> Result<TessellatedScene, KernelError> {
        Err(KernelError::Unsupported(format!(
            "OpenCascade backend is not wired yet (got {spec:?})"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occ_stub_rejects_rect() {
        let mut kernel = OccKernel;
        let err = kernel
            .build(&ModelSpec::Rect {
                width: 10.0,
                height: 10.0,
            })
            .unwrap_err();
        match err {
            KernelError::Unsupported(_) => {}
            other => panic!("expected Unsupported, got {other}"),
        }
    }
}
