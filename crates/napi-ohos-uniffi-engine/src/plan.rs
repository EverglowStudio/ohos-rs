use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::Ident;
use syn::{Path, Type};
use uniffi_js_abi::{OperationId, OperationKind, OperationOwner, Ownership, ScalarType, ValueType};
use uniffi_js_engine_schema::{BridgePlan, Capability};

use crate::OhosEngineError;

/// Structured lowering for one public carrier argument.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OhosArgumentBinding {
  Direct {
    carrier_type: Type,
  },
  I64BigInt,
  U64BigInt,
  LowerWith {
    carrier_type: Type,
    lower: Path,
  },
  ObjectLease {
    carrier_type: Type,
    lower: Path,
    ownership: Ownership,
  },
  OutputStreamLease {
    carrier_type: Type,
    lower: Path,
    ownership: Ownership,
  },
  CallbackProxy {
    rust_type: Type,
    build: Path,
  },
  InputStreamProxy {
    rust_type: Type,
    build: Path,
  },
}

impl OhosArgumentBinding {
  pub(crate) fn carrier_type(&self) -> Type {
    match self {
      Self::Direct { carrier_type }
      | Self::LowerWith { carrier_type, .. }
      | Self::ObjectLease { carrier_type, .. }
      | Self::OutputStreamLease { carrier_type, .. } => carrier_type.clone(),
      Self::I64BigInt | Self::U64BigInt => {
        syn::parse_quote!(napi_ohos::bindgen_prelude::BigInt)
      }
      Self::CallbackProxy { .. } | Self::InputStreamProxy { .. } => syn::parse_quote!(u32),
    }
  }

  pub(crate) fn rust_parameter_type(&self) -> Type {
    match self {
      Self::CallbackProxy { rust_type, .. } | Self::InputStreamProxy { rust_type, .. } => {
        rust_type.clone()
      }
      _ => self.carrier_type(),
    }
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OhosArgumentPlan {
  pub name: Ident,
  pub binding: OhosArgumentBinding,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OhosReturnBinding {
  Unit,
  Direct { carrier_type: Type },
  I64BigInt,
  U64BigInt,
  LiftWith { carrier_type: Type, lift: Path },
  ObjectLease { carrier_type: Type, lift: Path },
  CallbackLease { carrier_type: Type, lift: Path },
  OutputStreamLease { carrier_type: Type, lift: Path },
}

impl OhosReturnBinding {
  pub(crate) fn carrier_type(&self) -> Type {
    match self {
      Self::Unit => syn::parse_quote!(()),
      Self::Direct { carrier_type }
      | Self::LiftWith { carrier_type, .. }
      | Self::ObjectLease { carrier_type, .. }
      | Self::CallbackLease { carrier_type, .. }
      | Self::OutputStreamLease { carrier_type, .. } => carrier_type.clone(),
      Self::I64BigInt | Self::U64BigInt => {
        syn::parse_quote!(napi_ohos::bindgen_prelude::BigInt)
      }
    }
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OhosErrorBinding {
  Infallible,
  Descriptor { map: Path },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OhosOperationTarget {
  Native { call: Path },
  CallbackHost,
  InputStreamHostPull,
  InputStreamHostCancel,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OhosReceiverPlan {
  pub name: Ident,
  pub binding: OhosArgumentBinding,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OhosOperationPlan {
  pub operation_id: OperationId,
  pub target: OhosOperationTarget,
  pub receiver: Option<OhosReceiverPlan>,
  pub arguments: Vec<OhosArgumentPlan>,
  pub return_binding: OhosReturnBinding,
  pub error_binding: OhosErrorBinding,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OhosResourceHook {
  pub call: Path,
  pub carrier_type: Type,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OhosResourceHooks {
  pub release_object: Option<OhosResourceHook>,
  pub cancel_output_stream: Option<OhosResourceHook>,
  pub release_output_stream: Option<OhosResourceHook>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OhosBridgePlan {
  operations: Vec<OhosOperationPlan>,
  resource_hooks: OhosResourceHooks,
}

impl OhosBridgePlan {
  pub fn build(
    bridge: &BridgePlan,
    operations: Vec<OhosOperationPlan>,
  ) -> Result<Self, OhosEngineError> {
    Self::build_with_resource_hooks(bridge, operations, OhosResourceHooks::default())
  }

  pub fn build_with_resource_hooks(
    bridge: &BridgePlan,
    operations: Vec<OhosOperationPlan>,
    resource_hooks: OhosResourceHooks,
  ) -> Result<Self, OhosEngineError> {
    let type_kinds = bridge
      .types()
      .iter()
      .map(|ty| (&ty.definition.source_key, &ty.definition.kind))
      .collect::<BTreeMap<_, _>>();
    let mut by_id = BTreeMap::new();
    for operation in operations {
      let id = operation.operation_id.index();
      if by_id.insert(id, operation).is_some() {
        return Err(OhosEngineError::DuplicateRustOperation { id });
      }
    }
    if by_id.len() != bridge.operations().len() {
      return Err(OhosEngineError::OperationCount {
        expected: bridge.operations().len(),
        actual: by_id.len(),
      });
    }

    let mut validated = Vec::with_capacity(by_id.len());
    for (expected, bridge_operation) in bridge.operations().iter().enumerate() {
      let expected = u32::try_from(expected).map_err(|_| OhosEngineError::TooManyOperations)?;
      let Some(operation) = by_id.remove(&expected) else {
        return Err(OhosEngineError::MissingRustOperation { id: expected });
      };
      let signature = &bridge_operation.operation.definition.signature;
      validate_target(
        bridge_operation.operation.definition.source_key.kind(),
        &operation,
      )?;
      let native = matches!(operation.target, OhosOperationTarget::Native { .. });
      if native {
        if operation.arguments.len() != signature.arguments.len() {
          return Err(OhosEngineError::ArgumentCount {
            operation_id: operation.operation_id,
            expected: signature.arguments.len(),
            actual: operation.arguments.len(),
          });
        }
        validate_receiver(
          bridge_operation.operation.definition.source_key.owner(),
          bridge_operation.operation.definition.source_key.kind(),
          &operation,
        )?;
      } else if operation.receiver.is_some() || !operation.arguments.is_empty() {
        return Err(OhosEngineError::HostOperationHasRustBindings {
          operation_id: operation.operation_id,
        });
      }

      let mut names = BTreeSet::new();
      if let Some(receiver) = &operation.receiver {
        names.insert(receiver.name.to_string());
      }
      for argument in &operation.arguments {
        let name = argument.name.to_string();
        if !names.insert(name.clone()) {
          return Err(OhosEngineError::DuplicateRustArgument {
            operation_id: operation.operation_id,
            name,
          });
        }
      }

      if native {
        for (index, (argument, semantic)) in operation
          .arguments
          .iter()
          .zip(&signature.arguments)
          .enumerate()
        {
          validate_argument(
            operation.operation_id,
            index,
            &semantic.ty,
            semantic.ownership,
            &argument.binding,
            &type_kinds,
          )?;
        }
        validate_return(
          operation.operation_id,
          signature.return_type.as_ref(),
          &operation.return_binding,
          &type_kinds,
        )?;
        match (&signature.throws, &operation.error_binding) {
          (None, OhosErrorBinding::Infallible) | (Some(_), OhosErrorBinding::Descriptor { .. }) => {
          }
          (Some(_), OhosErrorBinding::Infallible) => {
            return Err(OhosEngineError::MissingErrorDescriptor {
              operation_id: operation.operation_id,
            })
          }
          (None, OhosErrorBinding::Descriptor { .. }) => {
            return Err(OhosEngineError::UnexpectedErrorDescriptor {
              operation_id: operation.operation_id,
            })
          }
        }
      }
      validated.push(operation);
    }

    validate_resource_hooks(bridge, &resource_hooks)?;
    Ok(Self {
      operations: validated,
      resource_hooks,
    })
  }

  pub fn operations(&self) -> &[OhosOperationPlan] {
    &self.operations
  }

  pub fn resource_hooks(&self) -> &OhosResourceHooks {
    &self.resource_hooks
  }
}

fn validate_resource_hooks(
  bridge: &BridgePlan,
  hooks: &OhosResourceHooks,
) -> Result<(), OhosEngineError> {
  let needs_object = bridge.operations().iter().any(|operation| {
    operation
      .required_capabilities
      .contains(Capability::ObjectLease)
      || matches!(
        operation.operation.definition.source_key.owner(),
        OperationOwner::Object(_)
      ) && matches!(
        operation.operation.definition.source_key.kind(),
        OperationKind::Method | OperationKind::Constructor
      )
  });
  let needs_output = bridge.operations().iter().any(|operation| {
    operation
      .required_capabilities
      .contains(Capability::OutputStream)
      || matches!(
        operation.operation.definition.source_key.kind(),
        OperationKind::OutputStreamStart
          | OperationKind::OutputStreamNext
          | OperationKind::OutputStreamCancel
      )
  });
  if needs_object && hooks.release_object.is_none() {
    return Err(OhosEngineError::MissingResourceHook {
      role: "object release",
    });
  }
  if needs_output && hooks.cancel_output_stream.is_none() {
    return Err(OhosEngineError::MissingResourceHook {
      role: "output-stream cancel",
    });
  }
  if needs_output && hooks.release_output_stream.is_none() {
    return Err(OhosEngineError::MissingResourceHook {
      role: "output-stream release",
    });
  }
  Ok(())
}

fn validate_argument(
  operation_id: OperationId,
  index: usize,
  value: &ValueType,
  ownership: Ownership,
  binding: &OhosArgumentBinding,
  type_kinds: &BTreeMap<&uniffi_js_abi::TypeSourceKey, &uniffi_js_abi::NamedTypeKind>,
) -> Result<(), OhosEngineError> {
  let valid = match value {
    ValueType::Scalar(ScalarType::I64) => matches!(binding, OhosArgumentBinding::I64BigInt),
    ValueType::Scalar(ScalarType::U64) => matches!(binding, OhosArgumentBinding::U64BigInt),
    ValueType::Scalar(
      ScalarType::Bool
      | ScalarType::I8
      | ScalarType::U8
      | ScalarType::I16
      | ScalarType::U16
      | ScalarType::I32
      | ScalarType::U32
      | ScalarType::F32
      | ScalarType::F64
      | ScalarType::String,
    ) => matches!(
      binding,
      OhosArgumentBinding::Direct { .. } | OhosArgumentBinding::LowerWith { .. }
    ),
    ValueType::Named(key) => match type_kinds.get(key) {
      Some(uniffi_js_abi::NamedTypeKind::Callback) => {
        matches!(binding, OhosArgumentBinding::CallbackProxy { .. })
      }
      Some(uniffi_js_abi::NamedTypeKind::Object) => matches!(
        binding,
        OhosArgumentBinding::ObjectLease {
          ownership: binding_ownership,
          ..
        } if *binding_ownership == ownership
      ),
      Some(_) => matches!(binding, OhosArgumentBinding::LowerWith { .. }),
      None => false,
    },
    ValueType::InputStream(_) => matches!(binding, OhosArgumentBinding::InputStreamProxy { .. }),
    ValueType::OutputStream(_) => false,
    _ => matches!(binding, OhosArgumentBinding::LowerWith { .. }),
  };
  if valid {
    Ok(())
  } else {
    Err(OhosEngineError::InvalidArgumentBinding {
      operation_id,
      argument: index,
      expected: binding_expectation(value),
    })
  }
}

fn validate_return(
  operation_id: OperationId,
  value: Option<&ValueType>,
  binding: &OhosReturnBinding,
  type_kinds: &BTreeMap<&uniffi_js_abi::TypeSourceKey, &uniffi_js_abi::NamedTypeKind>,
) -> Result<(), OhosEngineError> {
  let valid = match value {
    None => matches!(binding, OhosReturnBinding::Unit),
    Some(ValueType::Scalar(ScalarType::I64)) => matches!(binding, OhosReturnBinding::I64BigInt),
    Some(ValueType::Scalar(ScalarType::U64)) => matches!(binding, OhosReturnBinding::U64BigInt),
    Some(ValueType::Scalar(
      ScalarType::Bool
      | ScalarType::I8
      | ScalarType::U8
      | ScalarType::I16
      | ScalarType::U16
      | ScalarType::I32
      | ScalarType::U32
      | ScalarType::F32
      | ScalarType::F64
      | ScalarType::String,
    )) => matches!(
      binding,
      OhosReturnBinding::Direct { .. } | OhosReturnBinding::LiftWith { .. }
    ),
    Some(ValueType::Named(key)) => match type_kinds.get(key) {
      Some(uniffi_js_abi::NamedTypeKind::Object) => {
        matches!(binding, OhosReturnBinding::ObjectLease { .. })
      }
      Some(uniffi_js_abi::NamedTypeKind::Callback) => {
        matches!(binding, OhosReturnBinding::CallbackLease { .. })
      }
      Some(_) => matches!(binding, OhosReturnBinding::LiftWith { .. }),
      None => false,
    },
    Some(ValueType::OutputStream(_)) => {
      matches!(binding, OhosReturnBinding::OutputStreamLease { .. })
    }
    Some(_) => matches!(binding, OhosReturnBinding::LiftWith { .. }),
  };
  if valid {
    Ok(())
  } else {
    Err(OhosEngineError::InvalidReturnBinding {
      operation_id,
      expected: value.map_or("unit", binding_expectation),
    })
  }
}

fn validate_target(
  kind: OperationKind,
  operation: &OhosOperationPlan,
) -> Result<(), OhosEngineError> {
  let valid = match kind {
    OperationKind::CallbackMethod => matches!(operation.target, OhosOperationTarget::CallbackHost),
    OperationKind::InputStreamPull => {
      matches!(operation.target, OhosOperationTarget::InputStreamHostPull)
    }
    OperationKind::InputStreamCancel => {
      matches!(operation.target, OhosOperationTarget::InputStreamHostCancel)
    }
    _ => matches!(operation.target, OhosOperationTarget::Native { .. }),
  };
  if valid {
    Ok(())
  } else {
    Err(OhosEngineError::InvalidOperationTarget {
      operation_id: operation.operation_id,
      kind,
    })
  }
}

fn validate_receiver(
  owner: &OperationOwner,
  kind: OperationKind,
  operation: &OhosOperationPlan,
) -> Result<(), OhosEngineError> {
  let required = matches!(
    (owner, kind),
    (OperationOwner::Object(_), OperationKind::Method)
      | (OperationOwner::Object(_), OperationKind::OutputStreamNext)
      | (OperationOwner::Object(_), OperationKind::OutputStreamCancel)
  );
  match (required, &operation.receiver) {
    (false, None) => Ok(()),
    (true, Some(receiver)) => {
      let valid = if matches!(
        kind,
        OperationKind::OutputStreamNext | OperationKind::OutputStreamCancel
      ) {
        matches!(
          receiver.binding,
          OhosArgumentBinding::OutputStreamLease {
            ownership: Ownership::Borrowed,
            ..
          }
        )
      } else {
        matches!(
          receiver.binding,
          OhosArgumentBinding::ObjectLease {
            ownership: Ownership::Borrowed,
            ..
          }
        )
      };
      if valid {
        Ok(())
      } else {
        Err(OhosEngineError::InvalidObjectReceiver {
          operation_id: operation.operation_id,
        })
      }
    }
    (true, None) => Err(OhosEngineError::MissingObjectReceiver {
      operation_id: operation.operation_id,
    }),
    (false, Some(_)) => Err(OhosEngineError::UnexpectedObjectReceiver {
      operation_id: operation.operation_id,
    }),
  }
}

fn binding_expectation(value: &ValueType) -> &'static str {
  match value {
    ValueType::Scalar(ScalarType::I64) => "lossless signed BigInt",
    ValueType::Scalar(ScalarType::U64) => "lossless unsigned BigInt",
    ValueType::Scalar(
      ScalarType::Bool
      | ScalarType::I8
      | ScalarType::U8
      | ScalarType::I16
      | ScalarType::U16
      | ScalarType::I32
      | ScalarType::U32
      | ScalarType::F32
      | ScalarType::F64
      | ScalarType::String,
    ) => "direct Ark N-API carrier or explicit adapter",
    _ => "explicit carrier adapter",
  }
}
