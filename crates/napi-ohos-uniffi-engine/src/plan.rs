use std::collections::{BTreeMap, BTreeSet};

use napi_family_core::{
  FamilyOperation, FamilyOperationTarget, FamilyPlan, ReceiverBinding, ResourceKind,
  ResourceOwnership, StreamDirection, ValuePathSegment,
};
use proc_macro2::Ident;
use syn::{Path, Type};

use crate::OhosEngineError;

/// Structured lowering for one public carrier argument.  These are the only
/// OHOS-owned Rust details consumed by the engine; names and carrier graphs
/// remain in the frontend's engine-owned [`FamilyPlan`].
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
  /// A structured lowerer for nested callback/input-stream values. The
  /// canonical value path remains in the family plan; this hook receives the
  /// session Host and retained callback transfer table.
  LowerWithHost {
    carrier_type: Type,
    lower: Path,
  },
  ObjectLease {
    carrier_type: Type,
    lower: Path,
    ownership: ResourceOwnership,
  },
  OutputStreamLease {
    carrier_type: Type,
    lower: Path,
    ownership: ResourceOwnership,
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
      | Self::LowerWithHost { carrier_type, .. }
      | Self::ObjectLease { carrier_type, .. }
      | Self::OutputStreamLease { carrier_type, .. } => carrier_type.clone(),
      Self::I64BigInt | Self::U64BigInt => syn::parse_quote!(napi_ohos::bindgen_prelude::BigInt),
      Self::CallbackProxy { .. } | Self::InputStreamProxy { .. } => syn::parse_quote!(u32),
    }
  }

  pub(crate) fn rust_parameter_type(&self) -> Type {
    match self {
      Self::CallbackProxy { rust_type, .. } => syn::parse_quote!(std::sync::Arc<dyn #rust_type>),
      Self::InputStreamProxy { rust_type, .. } => rust_type.clone(),
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
      Self::I64BigInt | Self::U64BigInt => syn::parse_quote!(napi_ohos::bindgen_prelude::BigInt),
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
  pub operation_id: u32,
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
    family: &FamilyPlan,
    operations: Vec<OhosOperationPlan>,
  ) -> Result<Self, OhosEngineError> {
    Self::build_with_resource_hooks(family, operations, OhosResourceHooks::default())
  }

  pub fn build_with_resource_hooks(
    family: &FamilyPlan,
    operations: Vec<OhosOperationPlan>,
    resource_hooks: OhosResourceHooks,
  ) -> Result<Self, OhosEngineError> {
    let mut by_id = BTreeMap::new();
    for operation in operations {
      let id = operation.operation_id;
      if by_id.insert(id, operation).is_some() {
        return Err(OhosEngineError::DuplicateRustOperation { id });
      }
    }
    if by_id.len() != family.operations().len() {
      return Err(OhosEngineError::OperationCount {
        expected: family.operations().len(),
        actual: by_id.len(),
      });
    }

    let mut validated = Vec::with_capacity(by_id.len());
    for family_operation in family.operations() {
      let expected = family_operation.id;
      let Some(operation) = by_id.remove(&expected) else {
        return Err(OhosEngineError::MissingRustOperation { id: expected });
      };
      validate_target(family_operation, &operation)?;
      let native = matches!(operation.target, OhosOperationTarget::Native { .. });
      if native {
        if operation.arguments.len() != family_operation.argument_count {
          return Err(OhosEngineError::ArgumentCount {
            operation_id: operation.operation_id,
            expected: family_operation.argument_count,
            actual: operation.arguments.len(),
          });
        }
        validate_receiver(family_operation, &operation)?;
        validate_result_resource(family_operation, &operation)?;
        validate_structured_bindings(family_operation, &operation)?;
        match (family_operation.fallible, &operation.error_binding) {
          (false, OhosErrorBinding::Infallible) | (true, OhosErrorBinding::Descriptor { .. }) => {}
          (true, OhosErrorBinding::Infallible) => {
            return Err(OhosEngineError::MissingErrorDescriptor {
              operation_id: operation.operation_id,
            });
          }
          (false, OhosErrorBinding::Descriptor { .. }) => {
            return Err(OhosEngineError::UnexpectedErrorDescriptor {
              operation_id: operation.operation_id,
            });
          }
        }
      } else if operation.receiver.is_some()
        || !operation.arguments.is_empty()
        || !matches!(operation.return_binding, OhosReturnBinding::Unit)
        || !matches!(operation.error_binding, OhosErrorBinding::Infallible)
      {
        return Err(OhosEngineError::HostOperationHasRustBindings {
          operation_id: operation.operation_id,
        });
      } else if !family_operation.callbacks.is_empty() || !family_operation.streams.is_empty() {
        return Err(OhosEngineError::HostOperationHasStructuredUseSites {
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
      validated.push(operation);
    }
    validate_resource_hooks(family, &resource_hooks)?;
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
  family: &FamilyPlan,
  hooks: &OhosResourceHooks,
) -> Result<(), OhosEngineError> {
  let needs_object = family.operations().iter().any(|operation| {
    matches!(
      operation.receiver,
      Some(ReceiverBinding::Resource(resource)) if resource.kind == ResourceKind::Object
    ) || operation
      .result_resources
      .iter()
      .any(|result| result.binding.kind == ResourceKind::Object)
  });
  let needs_output = family.operations().iter().any(|operation| {
    matches!(
      operation.receiver,
      Some(ReceiverBinding::Resource(resource)) if resource.kind == ResourceKind::OutputStream
    ) || operation
      .result_resources
      .iter()
      .any(|result| result.binding.kind == ResourceKind::OutputStream)
      || operation
        .streams
        .iter()
        .any(|stream| stream.direction == StreamDirection::Output)
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

fn validate_target(
  family: &FamilyOperation,
  operation: &OhosOperationPlan,
) -> Result<(), OhosEngineError> {
  let valid = match (family.target, &operation.target) {
    (FamilyOperationTarget::Native, OhosOperationTarget::Native { .. }) => true,
    (FamilyOperationTarget::CallbackHost { .. }, OhosOperationTarget::CallbackHost) => true,
    (FamilyOperationTarget::InputStreamHostPull, OhosOperationTarget::InputStreamHostPull) => true,
    (FamilyOperationTarget::InputStreamHostCancel, OhosOperationTarget::InputStreamHostCancel) => {
      true
    }
    _ => false,
  };
  if valid {
    Ok(())
  } else {
    Err(OhosEngineError::InvalidOperationTarget {
      operation_id: operation.operation_id,
      kind: family.kind,
    })
  }
}

fn validate_receiver(
  family: &FamilyOperation,
  operation: &OhosOperationPlan,
) -> Result<(), OhosEngineError> {
  match (family.receiver.as_ref(), &operation.receiver) {
    (None, None) => Ok(()),
    (Some(ReceiverBinding::Value), Some(actual)) => {
      let valid = matches!(
        &actual.binding,
        OhosArgumentBinding::Direct { .. }
          | OhosArgumentBinding::I64BigInt
          | OhosArgumentBinding::U64BigInt
          | OhosArgumentBinding::LowerWith { .. }
          | OhosArgumentBinding::LowerWithHost { .. }
      );
      if valid {
        Ok(())
      } else {
        Err(OhosEngineError::InvalidValueReceiver {
          operation_id: operation.operation_id,
        })
      }
    }
    (Some(ReceiverBinding::Resource(expected)), Some(actual)) => {
      let valid = match (expected.kind, &actual.binding) {
        (ResourceKind::Object, OhosArgumentBinding::ObjectLease { ownership, .. })
        | (ResourceKind::OutputStream, OhosArgumentBinding::OutputStreamLease { ownership, .. }) => {
          *ownership == expected.ownership
        }
        (ResourceKind::InputStream, OhosArgumentBinding::InputStreamProxy { .. }) => true,
        _ => false,
      };
      if valid {
        Ok(())
      } else {
        Err(OhosEngineError::InvalidResourceReceiver {
          operation_id: operation.operation_id,
        })
      }
    }
    (Some(_), None) => Err(OhosEngineError::MissingReceiver {
      operation_id: operation.operation_id,
    }),
    (None, Some(_)) => Err(OhosEngineError::UnexpectedReceiver {
      operation_id: operation.operation_id,
    }),
  }
}

fn validate_result_resource(
  family: &FamilyOperation,
  operation: &OhosOperationPlan,
) -> Result<(), OhosEngineError> {
  let direct = family
    .result_resources
    .iter()
    .filter(|resource| matches!(resource.path.segments(), [ValuePathSegment::Return]))
    .map(|resource| resource.binding.kind)
    .next();
  let valid = match direct {
    Some(ResourceKind::Object) => matches!(
      operation.return_binding,
      OhosReturnBinding::ObjectLease { .. }
    ),
    Some(ResourceKind::OutputStream) => matches!(
      operation.return_binding,
      OhosReturnBinding::OutputStreamLease { .. }
    ),
    // Input streams are represented by a host-owned stream ID in the managed
    // return carrier.  They are tracked mechanically by the session path
    // walker rather than by a dedicated OHOS return binding.
    Some(ResourceKind::InputStream) => !matches!(
      operation.return_binding,
      OhosReturnBinding::ObjectLease { .. } | OhosReturnBinding::OutputStreamLease { .. }
    ),
    None => !matches!(
      operation.return_binding,
      OhosReturnBinding::ObjectLease { .. } | OhosReturnBinding::OutputStreamLease { .. }
    ),
  };
  if valid {
    Ok(())
  } else {
    Err(OhosEngineError::InvalidResourceResult {
      operation_id: operation.operation_id,
    })
  }
}

fn validate_structured_bindings(
  family: &FamilyOperation,
  operation: &OhosOperationPlan,
) -> Result<(), OhosEngineError> {
  for (index, argument) in operation.arguments.iter().enumerate() {
    let callback_paths = family
      .callbacks
      .iter()
      .filter(|use_site| {
        matches!(
          use_site.path.segments().first(),
          Some(ValuePathSegment::Argument(argument_index)) if *argument_index as usize == index
        )
      })
      .collect::<Vec<_>>();
    let direct_callback = callback_paths
      .iter()
      .any(|use_site| matches!(use_site.path.segments(), [ValuePathSegment::Argument(_)]));
    let nested_callback = callback_paths
      .iter()
      .any(|use_site| use_site.path.segments().len() > 1);
    let callback_proxy = matches!(&argument.binding, OhosArgumentBinding::CallbackProxy { .. });
    let lower_with_host = matches!(&argument.binding, OhosArgumentBinding::LowerWithHost { .. });
    if direct_callback != callback_proxy {
      return Err(OhosEngineError::InvalidStructuredBinding {
        operation_id: operation.operation_id,
        argument: index,
        role: "callback",
      });
    }
    let stream_paths = family
      .streams
      .iter()
      .filter(|use_site| {
        use_site.direction == StreamDirection::Input
          && matches!(
            use_site.path.segments().first(),
            Some(ValuePathSegment::Argument(argument_index)) if *argument_index as usize == index
          )
      })
      .collect::<Vec<_>>();
    let direct_stream = stream_paths
      .iter()
      .any(|use_site| matches!(use_site.path.segments(), [ValuePathSegment::Argument(_)]));
    let nested_stream = stream_paths
      .iter()
      .any(|use_site| use_site.path.segments().len() > 1);
    let input_stream_proxy = matches!(
      argument.binding,
      OhosArgumentBinding::InputStreamProxy { .. }
    );
    if direct_stream != input_stream_proxy {
      return Err(OhosEngineError::InvalidStructuredBinding {
        operation_id: operation.operation_id,
        argument: index,
        role: "input stream",
      });
    }
    let nested_structured = nested_callback || nested_stream;
    if nested_structured != lower_with_host {
      return Err(OhosEngineError::InvalidStructuredBinding {
        operation_id: operation.operation_id,
        argument: index,
        role: if nested_callback {
          "callback"
        } else if nested_stream {
          "input stream"
        } else {
          "callback or input stream"
        },
      });
    }
  }

  let return_callbacks = family
    .callbacks
    .iter()
    .filter(|use_site| {
      matches!(
        use_site.path.segments().first(),
        Some(ValuePathSegment::Return)
      )
    })
    .collect::<Vec<_>>();
  let direct_return_callback = return_callbacks
    .iter()
    .any(|use_site| matches!(use_site.path.segments(), [ValuePathSegment::Return]));
  let callback_lease = matches!(
    operation.return_binding,
    OhosReturnBinding::CallbackLease { .. }
  );
  if direct_return_callback != callback_lease {
    return Err(OhosEngineError::InvalidReturnBinding {
      operation_id: operation.operation_id,
      expected: "a direct callback lease matching the canonical return use-site",
    });
  }
  Ok(())
}
