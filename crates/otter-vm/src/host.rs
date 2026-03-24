//! Runtime registry for host-callable native function descriptors.

use crate::descriptors::NativeFunctionDescriptor;

/// Stable identifier of a host function stored in the runtime registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HostFunctionId(pub u32);

/// Runtime-owned registry of host-callable native functions.
#[derive(Default, Clone)]
pub struct NativeFunctionRegistry {
    functions: Vec<NativeFunctionDescriptor>,
}

impl NativeFunctionRegistry {
    /// Creates an empty runtime registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a native function descriptor and returns its stable id.
    pub fn register(&mut self, descriptor: NativeFunctionDescriptor) -> HostFunctionId {
        let id = HostFunctionId(u32::try_from(self.functions.len()).unwrap_or(u32::MAX));
        self.functions.push(descriptor);
        id
    }

    /// Resolves a registered native function descriptor by id.
    #[must_use]
    pub fn get(&self, id: HostFunctionId) -> Option<&NativeFunctionDescriptor> {
        self.functions.get(usize::try_from(id.0).ok()?)
    }

    /// Returns the number of registered host functions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.functions.len()
    }

    /// Returns `true` when the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.functions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use crate::descriptors::{NativeFunctionDescriptor, VmNativeFunction};
    use crate::value::RegisterValue;

    use super::{HostFunctionId, NativeFunctionRegistry};

    fn passthrough_callback() -> VmNativeFunction {
        passthrough
    }

    fn passthrough(
        this: &RegisterValue,
        _args: &[RegisterValue],
        _runtime: &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, crate::descriptors::VmNativeCallError> {
        Ok(*this)
    }

    #[test]
    fn native_function_registry_assigns_stable_ids() {
        let mut registry = NativeFunctionRegistry::new();

        let first = registry.register(NativeFunctionDescriptor::method(
            "abs",
            1,
            passthrough_callback(),
        ));
        let second = registry.register(NativeFunctionDescriptor::method(
            "max",
            2,
            passthrough_callback(),
        ));

        assert_eq!(first, HostFunctionId(0));
        assert_eq!(second, HostFunctionId(1));
        assert_eq!(registry.len(), 2);
        assert_eq!(
            registry.get(first).map(|function| function.js_name()),
            Some("abs")
        );

        let value =
            (registry
                .get(second)
                .expect("second function must exist")
                .callback())(&RegisterValue::from_i32(7), &[], &mut Default::default())
            .expect("callback should succeed");
        assert_eq!(value, RegisterValue::from_i32(7));
    }
}
