use super::*;
use proc_macro2::{Ident, Span};
use std::sync::{Arc, Mutex};

use uniffi_js_abi::{
  ArgumentDefinition, AsyncKind, ComponentDefinition, ComponentId, ComponentKey, EnumVariant,
  FieldDefinition, IdentifiedComponent, IdentifiedOperation, IdentifiedType, NamedTypeKind,
  OperationDefinition, OperationId, OperationKind, OperationOwner, OperationSignature,
  OperationSourceKey, Ownership, ScalarType, TypeDefinition, TypeId, TypeSourceKey, ValueType,
};
use uniffi_js_engine_schema::{
  BridgePlan, BridgePlanInput, CallbackContract, CallbackReentrancy, CallbackRetention,
  CallbackThreading, CallbackUseSite, PlannedOperation, StreamContract, StreamUseSite, ValuePath,
};

fn ident(name: &str) -> Ident {
  Ident::new(name, Span::call_site())
}

fn argument(name: &str, binding: OhosArgumentBinding) -> OhosArgumentPlan {
  OhosArgumentPlan {
    name: ident(name),
    binding,
  }
}

fn direct(carrier_type: syn::Type) -> OhosArgumentBinding {
  OhosArgumentBinding::Direct { carrier_type }
}

fn lower(carrier_type: syn::Type, lower: syn::Path) -> OhosArgumentBinding {
  OhosArgumentBinding::LowerWith {
    carrier_type,
    lower,
  }
}

fn native_operation(
  id: u32,
  call: syn::Path,
  arguments: Vec<OhosArgumentPlan>,
  return_binding: OhosReturnBinding,
  error_binding: OhosErrorBinding,
) -> OhosOperationPlan {
  OhosOperationPlan {
    operation_id: OperationId::new(id),
    target: OhosOperationTarget::Native { call },
    receiver: None,
    arguments,
    return_binding,
    error_binding,
  }
}

fn host_operation(id: u32, target: OhosOperationTarget) -> OhosOperationPlan {
  OhosOperationPlan {
    operation_id: OperationId::new(id),
    target,
    receiver: None,
    arguments: Vec::new(),
    return_binding: OhosReturnBinding::Unit,
    error_binding: OhosErrorBinding::Infallible,
  }
}

fn with_receiver(
  mut operation: OhosOperationPlan,
  name: &str,
  binding: OhosArgumentBinding,
) -> OhosOperationPlan {
  operation.receiver = Some(OhosReceiverPlan {
    name: ident(name),
    binding,
  });
  operation
}

fn type_key(component: &ComponentKey, name: &str) -> TypeSourceKey {
  TypeSourceKey::new(component.clone(), name).unwrap()
}

#[allow(clippy::too_many_arguments)]
fn planned_operation(
  component: &ComponentKey,
  id: u32,
  owner: OperationOwner,
  kind: OperationKind,
  name: &str,
  arguments: Vec<ArgumentDefinition>,
  return_type: Option<ValueType>,
  async_kind: AsyncKind,
  throws: Option<TypeSourceKey>,
) -> PlannedOperation {
  PlannedOperation::new(IdentifiedOperation {
    id: OperationId::new(id),
    definition: OperationDefinition::new(
      OperationSourceKey::new(component.clone(), owner, kind, name).unwrap(),
      name,
      format!("fixture::{name}"),
      format!("fixture_private_{id}"),
      OperationSignature {
        arguments,
        return_type,
        async_kind,
        throws,
      },
    )
    .unwrap(),
  })
}

fn owned_arg(name: &str, ty: ValueType) -> ArgumentDefinition {
  ArgumentDefinition::new(name, ty, Ownership::Owned).unwrap()
}

fn build_bridge(
  component: ComponentKey,
  types: Vec<IdentifiedType>,
  operations: Vec<PlannedOperation>,
  callbacks: Vec<CallbackUseSite>,
  streams: Vec<StreamUseSite>,
) -> BridgePlan {
  BridgePlan::build(BridgePlanInput {
    components: vec![IdentifiedComponent {
      id: ComponentId::new(0),
      definition: ComponentDefinition::new(component, "fixture").unwrap(),
    }],
    types,
    operations,
    callbacks,
    streams,
    targets: vec![napi_family_core::HostFlavor::Ohos.capabilities()],
  })
  .unwrap()
}

fn echo_bridge(async_kind: AsyncKind) -> BridgePlan {
  let component = ComponentKey::new("ohos_fixture").unwrap();
  let operation = planned_operation(
    &component,
    0,
    OperationOwner::Namespace,
    OperationKind::Function,
    "echo",
    vec![owned_arg("value", ValueType::Scalar(ScalarType::U64))],
    Some(ValueType::Scalar(ScalarType::U64)),
    async_kind,
    None,
  );
  build_bridge(
    component,
    Vec::new(),
    vec![operation],
    Vec::new(),
    Vec::new(),
  )
}

fn echo_plan(bridge: &BridgePlan) -> OhosBridgePlan {
  OhosBridgePlan::build(
    bridge,
    vec![native_operation(
      0,
      syn::parse_quote!(fixture::echo),
      vec![argument("value", OhosArgumentBinding::U64BigInt)],
      OhosReturnBinding::U64BigInt,
      OhosErrorBinding::Infallible,
    )],
  )
  .unwrap()
}

fn return_bridge(
  return_type: ValueType,
  types: Vec<IdentifiedType>,
  streams: Vec<StreamUseSite>,
) -> BridgePlan {
  let component = ComponentKey::new("ohos_return_fixture").unwrap();
  let operation = planned_operation(
    &component,
    0,
    OperationOwner::Namespace,
    OperationKind::Function,
    "make",
    Vec::new(),
    Some(return_type),
    AsyncKind::Sync,
    None,
  );
  build_bridge(component, types, vec![operation], Vec::new(), streams)
}

fn object_return_bridge() -> BridgePlan {
  let component = ComponentKey::new("ohos_return_fixture").unwrap();
  let object = type_key(&component, "Thing");
  return_bridge(
    ValueType::Named(object.clone()),
    vec![IdentifiedType {
      id: TypeId::new(0),
      definition: TypeDefinition::new(object, "Thing", NamedTypeKind::Object).unwrap(),
    }],
    Vec::new(),
  )
}

fn object_return_plan(bridge: &BridgePlan) -> OhosBridgePlan {
  OhosBridgePlan::build_with_resource_hooks(
    bridge,
    vec![native_operation(
      0,
      syn::parse_quote!(fixture::make),
      Vec::new(),
      OhosReturnBinding::ObjectLease {
        carrier_type: syn::parse_quote!(fixture::ObjectHandle),
        lift: syn::parse_quote!(fixture::lift_object),
      },
      OhosErrorBinding::Infallible,
    )],
    OhosResourceHooks {
      release_object: Some(OhosResourceHook {
        call: syn::parse_quote!(fixture::release_object),
        carrier_type: syn::parse_quote!(fixture::ObjectHandle),
      }),
      ..OhosResourceHooks::default()
    },
  )
  .unwrap()
}

fn output_stream_return_bridge() -> BridgePlan {
  return_bridge(
    ValueType::output_stream(ValueType::Scalar(ScalarType::Bytes)),
    Vec::new(),
    vec![StreamUseSite {
      operation_id: OperationId::new(0),
      path: ValuePath::return_value(),
      contract: StreamContract::output(),
    }],
  )
}

fn output_stream_return_plan(bridge: &BridgePlan) -> OhosBridgePlan {
  OhosBridgePlan::build_with_resource_hooks(
    bridge,
    vec![native_operation(
      0,
      syn::parse_quote!(fixture::make),
      Vec::new(),
      OhosReturnBinding::OutputStreamLease {
        carrier_type: syn::parse_quote!(fixture::OutputStreamHandle),
        lift: syn::parse_quote!(fixture::lift_output_stream),
      },
      OhosErrorBinding::Infallible,
    )],
    OhosResourceHooks {
      cancel_output_stream: Some(OhosResourceHook {
        call: syn::parse_quote!(fixture::cancel_output_stream_resource),
        carrier_type: syn::parse_quote!(fixture::OutputStreamHandle),
      }),
      release_output_stream: Some(OhosResourceHook {
        call: syn::parse_quote!(fixture::release_output_stream),
        carrier_type: syn::parse_quote!(fixture::OutputStreamHandle),
      }),
      ..OhosResourceHooks::default()
    },
  )
  .unwrap()
}

fn structured_bridge() -> BridgePlan {
  let component = ComponentKey::new("fixture").unwrap();
  let payload = type_key(&component, "Payload");
  let failure = type_key(&component, "Failure");
  let object = type_key(&component, "Thing");
  let callback = type_key(&component, "Observer");
  let output_stream = type_key(&component, "ByteStream");
  let types = vec![
    IdentifiedType {
      id: TypeId::new(0),
      definition: TypeDefinition::new(
        payload.clone(),
        "Payload",
        NamedTypeKind::Record {
          fields: vec![FieldDefinition::new("value", ValueType::Scalar(ScalarType::I64)).unwrap()],
        },
      )
      .unwrap(),
    },
    IdentifiedType {
      id: TypeId::new(1),
      definition: TypeDefinition::new(
        failure.clone(),
        "Failure",
        NamedTypeKind::Error {
          variants: vec![EnumVariant::new(
            "Bad",
            vec![FieldDefinition::new("message", ValueType::Scalar(ScalarType::String)).unwrap()],
          )
          .unwrap()],
        },
      )
      .unwrap(),
    },
    IdentifiedType {
      id: TypeId::new(2),
      definition: TypeDefinition::new(object.clone(), "Thing", NamedTypeKind::Object).unwrap(),
    },
    IdentifiedType {
      id: TypeId::new(3),
      definition: TypeDefinition::new(callback.clone(), "Observer", NamedTypeKind::Callback)
        .unwrap(),
    },
    IdentifiedType {
      id: TypeId::new(4),
      definition: TypeDefinition::new(output_stream.clone(), "ByteStream", NamedTypeKind::Object)
        .unwrap(),
    },
  ];
  let operations = vec![
    planned_operation(
      &component,
      0,
      OperationOwner::Namespace,
      OperationKind::Function,
      "numbers",
      vec![
        owned_arg("signed", ValueType::Scalar(ScalarType::I64)),
        owned_arg("unsigned", ValueType::Scalar(ScalarType::U64)),
        owned_arg("payload", ValueType::Named(payload.clone())),
        owned_arg("bytes", ValueType::Scalar(ScalarType::Bytes)),
      ],
      Some(ValueType::Scalar(ScalarType::I64)),
      AsyncKind::Sync,
      Some(failure.clone()),
    ),
    planned_operation(
      &component,
      1,
      OperationOwner::Namespace,
      OperationKind::Function,
      "makeThing",
      Vec::new(),
      Some(ValueType::Named(object.clone())),
      AsyncKind::Async,
      None,
    ),
    planned_operation(
      &component,
      2,
      OperationOwner::Object(object),
      OperationKind::Method,
      "add",
      vec![owned_arg("delta", ValueType::Scalar(ScalarType::I64))],
      Some(ValueType::Scalar(ScalarType::I64)),
      AsyncKind::Sync,
      None,
    ),
    planned_operation(
      &component,
      3,
      OperationOwner::Callback(callback.clone()),
      OperationKind::CallbackMethod,
      "onValue",
      vec![owned_arg("payload", ValueType::Named(payload))],
      Some(ValueType::Scalar(ScalarType::I64)),
      AsyncKind::Async,
      Some(failure),
    ),
    planned_operation(
      &component,
      4,
      OperationOwner::Namespace,
      OperationKind::Function,
      "observe",
      vec![owned_arg("observer", ValueType::Named(callback))],
      None,
      AsyncKind::Sync,
      None,
    ),
    planned_operation(
      &component,
      5,
      OperationOwner::Namespace,
      OperationKind::OutputStreamStart,
      "read",
      Vec::new(),
      Some(ValueType::output_stream(ValueType::Scalar(
        ScalarType::Bytes,
      ))),
      AsyncKind::Sync,
      None,
    ),
    planned_operation(
      &component,
      6,
      OperationOwner::Object(output_stream.clone()),
      OperationKind::OutputStreamNext,
      "next",
      Vec::new(),
      Some(ValueType::optional(ValueType::Scalar(ScalarType::Bytes))),
      AsyncKind::Async,
      None,
    ),
    planned_operation(
      &component,
      7,
      OperationOwner::Object(output_stream),
      OperationKind::OutputStreamCancel,
      "cancel",
      Vec::new(),
      None,
      AsyncKind::Async,
      None,
    ),
    planned_operation(
      &component,
      8,
      OperationOwner::Namespace,
      OperationKind::Function,
      "write",
      vec![owned_arg(
        "source",
        ValueType::input_stream(ValueType::Scalar(ScalarType::Bytes)),
      )],
      None,
      AsyncKind::Async,
      None,
    ),
    planned_operation(
      &component,
      9,
      OperationOwner::Namespace,
      OperationKind::InputStreamPull,
      "pullInput",
      vec![owned_arg("streamId", ValueType::Scalar(ScalarType::U32))],
      Some(ValueType::optional(ValueType::Scalar(ScalarType::Bytes))),
      AsyncKind::Async,
      None,
    ),
    planned_operation(
      &component,
      10,
      OperationOwner::Namespace,
      OperationKind::InputStreamCancel,
      "cancelInput",
      vec![owned_arg("streamId", ValueType::Scalar(ScalarType::U32))],
      None,
      AsyncKind::Async,
      None,
    ),
  ];
  build_bridge(
    component,
    types,
    operations,
    vec![CallbackUseSite {
      operation_id: OperationId::new(4),
      callback_type: TypeId::new(3),
      path: ValuePath::argument(0),
      contract: CallbackContract {
        retention: CallbackRetention::Retained,
        threading: CallbackThreading::MayCrossThread,
        reentrancy: CallbackReentrancy::Forbidden,
      },
    }],
    vec![
      StreamUseSite {
        operation_id: OperationId::new(5),
        path: ValuePath::return_value(),
        contract: StreamContract::output(),
      },
      StreamUseSite {
        operation_id: OperationId::new(8),
        path: ValuePath::argument(0),
        contract: StreamContract::input(),
      },
    ],
  )
}

fn structured_plan(bridge: &BridgePlan) -> OhosBridgePlan {
  OhosBridgePlan::build_with_resource_hooks(
    bridge,
    vec![
      native_operation(
        0,
        syn::parse_quote!(fixture::numbers),
        vec![
          argument("signed", OhosArgumentBinding::I64BigInt),
          argument("unsigned", OhosArgumentBinding::U64BigInt),
          argument(
            "payload",
            lower(
              syn::parse_quote!(fixture::PayloadCarrier),
              syn::parse_quote!(fixture::lower_payload),
            ),
          ),
          argument(
            "bytes",
            lower(
              syn::parse_quote!(napi_ohos::bindgen_prelude::Uint8Array),
              syn::parse_quote!(fixture::lower_bytes),
            ),
          ),
        ],
        OhosReturnBinding::I64BigInt,
        OhosErrorBinding::Descriptor {
          map: syn::parse_quote!(fixture::map_declared_error),
        },
      ),
      native_operation(
        1,
        syn::parse_quote!(fixture::make_thing),
        Vec::new(),
        OhosReturnBinding::ObjectLease {
          carrier_type: syn::parse_quote!(fixture::ObjectHandle),
          lift: syn::parse_quote!(fixture::lift_object),
        },
        OhosErrorBinding::Infallible,
      ),
      with_receiver(
        native_operation(
          2,
          syn::parse_quote!(fixture::thing_add),
          vec![argument("delta", OhosArgumentBinding::I64BigInt)],
          OhosReturnBinding::I64BigInt,
          OhosErrorBinding::Infallible,
        ),
        "thing",
        OhosArgumentBinding::ObjectLease {
          carrier_type: syn::parse_quote!(fixture::ObjectHandle),
          lower: syn::parse_quote!(fixture::lower_object),
          ownership: Ownership::Borrowed,
        },
      ),
      host_operation(3, OhosOperationTarget::CallbackHost),
      native_operation(
        4,
        syn::parse_quote!(fixture::observe),
        vec![argument(
          "observer",
          OhosArgumentBinding::CallbackProxy {
            rust_type: syn::parse_quote!(fixture::CallbackHandle),
            build: syn::parse_quote!(fixture::build_callback_proxy),
          },
        )],
        OhosReturnBinding::Unit,
        OhosErrorBinding::Infallible,
      ),
      native_operation(
        5,
        syn::parse_quote!(fixture::read),
        Vec::new(),
        OhosReturnBinding::OutputStreamLease {
          carrier_type: syn::parse_quote!(fixture::OutputStreamHandle),
          lift: syn::parse_quote!(fixture::lift_output_stream),
        },
        OhosErrorBinding::Infallible,
      ),
      with_receiver(
        native_operation(
          6,
          syn::parse_quote!(fixture::next_output_stream),
          Vec::new(),
          OhosReturnBinding::LiftWith {
            carrier_type: syn::parse_quote!(Option<napi_ohos::bindgen_prelude::Uint8Array>),
            lift: syn::parse_quote!(fixture::lift_optional_bytes),
          },
          OhosErrorBinding::Infallible,
        ),
        "stream",
        OhosArgumentBinding::OutputStreamLease {
          carrier_type: syn::parse_quote!(fixture::OutputStreamHandle),
          lower: syn::parse_quote!(fixture::lower_output_stream),
          ownership: Ownership::Borrowed,
        },
      ),
      with_receiver(
        native_operation(
          7,
          syn::parse_quote!(fixture::cancel_output_stream),
          Vec::new(),
          OhosReturnBinding::Unit,
          OhosErrorBinding::Infallible,
        ),
        "stream",
        OhosArgumentBinding::OutputStreamLease {
          carrier_type: syn::parse_quote!(fixture::OutputStreamHandle),
          lower: syn::parse_quote!(fixture::lower_output_stream),
          ownership: Ownership::Borrowed,
        },
      ),
      native_operation(
        8,
        syn::parse_quote!(fixture::write),
        vec![argument(
          "source",
          OhosArgumentBinding::InputStreamProxy {
            rust_type: syn::parse_quote!(fixture::InputStreamHandle),
            build: syn::parse_quote!(fixture::build_input_stream_proxy),
          },
        )],
        OhosReturnBinding::Unit,
        OhosErrorBinding::Infallible,
      ),
      host_operation(9, OhosOperationTarget::InputStreamHostPull),
      host_operation(10, OhosOperationTarget::InputStreamHostCancel),
    ],
    resource_hooks(),
  )
  .unwrap()
}

fn resource_hooks() -> OhosResourceHooks {
  OhosResourceHooks {
    release_object: Some(OhosResourceHook {
      call: syn::parse_quote!(fixture::release_object),
      carrier_type: syn::parse_quote!(fixture::ObjectHandle),
    }),
    cancel_output_stream: Some(OhosResourceHook {
      call: syn::parse_quote!(fixture::cancel_output_stream_resource),
      carrier_type: syn::parse_quote!(fixture::OutputStreamHandle),
    }),
    release_output_stream: Some(OhosResourceHook {
      call: syn::parse_quote!(fixture::release_output_stream),
      carrier_type: syn::parse_quote!(fixture::OutputStreamHandle),
    }),
  }
}

fn rebuild(
  bridge: &BridgePlan,
  operations: Vec<OhosOperationPlan>,
) -> Result<OhosBridgePlan, OhosEngineError> {
  OhosBridgePlan::build_with_resource_hooks(bridge, operations, resource_hooks())
}

fn object_argument_bridge(ownership: Ownership) -> BridgePlan {
  let component = ComponentKey::new("object_argument_fixture").unwrap();
  let object = type_key(&component, "Thing");
  let operation = planned_operation(
    &component,
    0,
    OperationOwner::Namespace,
    OperationKind::Function,
    "consume",
    vec![ArgumentDefinition::new("thing", ValueType::Named(object.clone()), ownership).unwrap()],
    None,
    AsyncKind::Sync,
    None,
  );
  build_bridge(
    component,
    vec![IdentifiedType {
      id: TypeId::new(0),
      definition: TypeDefinition::new(object, "Thing", NamedTypeKind::Object).unwrap(),
    }],
    vec![operation],
    Vec::new(),
    Vec::new(),
  )
}

fn object_argument_plan(
  bridge: &BridgePlan,
  ownership: Ownership,
) -> Result<OhosBridgePlan, OhosEngineError> {
  OhosBridgePlan::build_with_resource_hooks(
    bridge,
    vec![native_operation(
      0,
      syn::parse_quote!(fixture::consume),
      vec![argument(
        "thing",
        OhosArgumentBinding::ObjectLease {
          carrier_type: syn::parse_quote!(fixture::ObjectHandle),
          lower: syn::parse_quote!(fixture::lower_object),
          ownership,
        },
      )],
      OhosReturnBinding::Unit,
      OhosErrorBinding::Infallible,
    )],
    OhosResourceHooks {
      release_object: Some(OhosResourceHook {
        call: syn::parse_quote!(fixture::release_object),
        carrier_type: syn::parse_quote!(fixture::ObjectHandle),
      }),
      ..OhosResourceHooks::default()
    },
  )
}

#[derive(Clone, Default)]
struct FixtureHost {
  calls: Arc<Mutex<Vec<String>>>,
  returns: Arc<Mutex<Option<OhosValue>>>,
  released_objects: Arc<Mutex<Vec<u32>>>,
  cancelled_outputs: Arc<Mutex<Vec<u32>>>,
  released_outputs: Arc<Mutex<Vec<u32>>>,
}

impl OhosHost for FixtureHost {
  fn invoke_sync(
    &mut self,
    _operation_id: OperationId,
    args: &[OhosValue],
  ) -> Result<OhosValue, OhosError> {
    self.calls.lock().unwrap().push("sync".into());
    if let Some(value) = self.returns.lock().unwrap().clone() {
      return Ok(value);
    }
    Ok(args.first().cloned().unwrap_or(OhosValue::Unit))
  }

  fn invoke_async<'a>(
    &'a mut self,
    _operation_id: OperationId,
    args: Vec<OhosValue>,
  ) -> OhosFuture<'a> {
    self.calls.lock().unwrap().push("async".into());
    Box::pin(async move { Ok(args.into_iter().next().unwrap_or(OhosValue::Unit)) })
  }

  fn invoke_callback(
    &mut self,
    _callback_type: u32,
    _callback_id: u32,
    _method_id: u32,
    _args: &[OhosValue],
  ) -> Result<OhosValue, OhosError> {
    Ok(OhosValue::Unit)
  }

  fn pull_input_stream(&mut self, _stream_id: u32) -> Result<OhosValue, OhosError> {
    Ok(OhosValue::Null)
  }

  fn cancel_input_stream(&mut self, _stream_id: u32) -> Result<(), OhosError> {
    Ok(())
  }

  fn next_output_stream(&mut self, _stream_id: u32) -> Result<OhosValue, OhosError> {
    Ok(OhosValue::Null)
  }

  fn cancel_output_stream(&mut self, stream_id: u32) -> Result<(), OhosError> {
    self.cancelled_outputs.lock().unwrap().push(stream_id);
    Ok(())
  }

  fn release_output_stream(&mut self, stream_id: u32) -> Result<(), OhosError> {
    self.released_outputs.lock().unwrap().push(stream_id);
    Ok(())
  }

  fn release_object(&mut self, object_id: u32) -> Result<(), OhosError> {
    self.released_objects.lock().unwrap().push(object_id);
    Ok(())
  }
}

#[test]
fn ohos_module_uses_family_plan_and_one_factory() {
  let bridge = echo_bridge(AsyncKind::Sync);
  let module = generate_ohos_module(&bridge, echo_plan(&bridge)).unwrap();
  assert_eq!(module.family().flavor(), napi_family_core::HostFlavor::Ohos);
  assert_eq!(
    module.hooks().async_scheduler,
    OhosAsyncScheduler::ArkEventLoop
  );
  assert_eq!(
    module.public_exports().collect::<Vec<_>>(),
    vec![BACKEND_FACTORY_EXPORT]
  );
  assert_eq!(
    module.raw_operation_names().collect::<Vec<_>>(),
    vec!["__uniffi_raw_operation_0"]
  );
}

#[test]
fn generated_source_uses_structured_plan_and_only_exports_factory() {
  let bridge = structured_bridge();
  let generated = generate_ohos_source(&bridge, structured_plan(&bridge)).unwrap();
  let source = generated.source().to_string();

  assert_eq!(
    generated.module().public_exports().collect::<Vec<_>>(),
    vec![BACKEND_FACTORY_EXPORT]
  );
  assert_eq!(source.matches("register_module_export (").count(), 2);
  assert!(!source.contains("register_module_export ( None , \"__uniffi_raw_operation_"));
  assert!(!source.contains("register_module_export ( None , \"__uniffi_release_"));
  assert!(!source.contains("register_module_export ( None , \"__uniffi_cancel_"));
  assert!(source.contains("__uniffi_backend_factory"));
  assert!(source.contains("create_backend_session"));
  assert!(source.contains("SessionOperationDispatch :: CallbackHostAsync"));
  assert!(source.contains("SessionCallbackErrorStyle :: Fallible"));
  assert!(source.contains("SessionOperationDispatch :: InputStreamHostPull"));
  assert!(source.contains("SessionOperationDispatch :: InputStreamHostCancel"));
  assert_eq!(
    source
      .matches("SessionNativeCall :: HostAndArguments")
      .count(),
    2
  );

  assert!(source.contains("get_i64"));
  assert!(source.contains("get_u64"));
  assert!(source.contains("require_lossless_i64"));
  assert!(source.contains("require_lossless_u64"));
  assert!(source.contains("fixture :: map_declared_error"));
  assert!(source.contains("fixture :: build_callback_proxy"));
  assert!(source.contains("fixture :: build_input_stream_proxy"));
  assert!(source.contains("fixture :: lower_object"));
  assert!(source.contains("fixture :: lift_output_stream"));

  let wrapper_start = source
    .find("fn __uniffi_raw_operation_8_c_callback")
    .expect("async input-proxy wrapper");
  let wrapper_end = source[wrapper_start..]
    .find("unsafe fn _napi_rs_internal_register___uniffi_raw_operation_8")
    .map(|offset| wrapper_start + offset)
    .expect("async input-proxy callback factory");
  let wrapper = &source[wrapper_start..wrapper_end];
  let proxy_builder = wrapper
    .find("fixture :: build_input_stream_proxy")
    .expect("input proxy pre-call");
  let future = wrapper
    .find("execute_tokio_future_with_finalize_callback")
    .expect("async future construction");
  assert!(
    proxy_builder < future,
    "proxy must be built before the future"
  );
  assert!(wrapper.contains("build_input_stream_proxy (& arg0 , arg1) ?"));

  for binding in [
    "release_object : Some (unsafe { _napi_rs_internal_register___uniffi_release_object (env . raw ()) ? })",
    "cancel_output_stream : Some (unsafe { _napi_rs_internal_register___uniffi_cancel_output_stream (env . raw ()) ? })",
    "release_output_stream : Some (unsafe { _napi_rs_internal_register___uniffi_release_output_stream (env . raw ()) ? })",
  ] {
    assert!(source.contains(binding), "missing native callback binding: {binding}");
  }
  assert!(source.contains("fixture :: release_object (handle)"));
  assert!(source.contains("fixture :: cancel_output_stream_resource (handle) . await"));
  assert!(source.contains("fixture :: release_output_stream (handle)"));
  assert!(syn::parse2::<syn::File>(generated.source().clone()).is_ok());
}

#[test]
fn source_contains_structured_rust_call_and_bigint_carrier_signature() {
  let bridge = echo_bridge(AsyncKind::Sync);
  let generated = generate_ohos_source(&bridge, echo_plan(&bridge)).unwrap();
  let source = generated.source().to_string();
  assert!(source.contains("fixture :: echo (value)"));
  assert!(source.contains("value : napi_ohos :: bindgen_prelude :: BigInt"));
  assert!(source.contains("get_u64"));
  assert!(source.contains("fn __uniffi_raw_operation_0"));
  assert!(source.contains("_napi_rs_internal_register_"));
  assert!(!source.contains("register_module_export ( None , \"__uniffi_raw_operation_0"));
  assert!(syn::parse2::<syn::File>(generated.source().clone()).is_ok());
}

#[test]
fn structured_plan_accepts_bigints_declared_errors_proxies_and_resources() {
  let bridge = structured_bridge();
  let plan = structured_plan(&bridge);
  assert!(matches!(
    plan.operations()[0].arguments[0].binding,
    OhosArgumentBinding::I64BigInt
  ));
  assert!(matches!(
    plan.operations()[0].arguments[1].binding,
    OhosArgumentBinding::U64BigInt
  ));
  assert!(matches!(
    plan.operations()[0].return_binding,
    OhosReturnBinding::I64BigInt
  ));
  assert!(matches!(
    plan.operations()[0].error_binding,
    OhosErrorBinding::Descriptor { .. }
  ));
  assert!(matches!(
    plan.operations()[4].arguments[0].binding,
    OhosArgumentBinding::CallbackProxy { .. }
  ));
  assert!(matches!(
    plan.operations()[8].arguments[0].binding,
    OhosArgumentBinding::InputStreamProxy { .. }
  ));
  assert!(plan.resource_hooks().release_object.is_some());
  assert!(plan.resource_hooks().cancel_output_stream.is_some());
  assert!(plan.resource_hooks().release_output_stream.is_some());
}

#[test]
fn rejects_lossy_i64_and_u64_bindings() {
  let bridge = structured_bridge();
  let mut operations = structured_plan(&bridge).operations().to_vec();
  operations[0].arguments[0].binding = direct(syn::parse_quote!(i64));
  assert!(matches!(
    rebuild(&bridge, operations),
    Err(OhosEngineError::InvalidArgumentBinding {
      operation_id,
      argument: 0,
      expected: "lossless signed BigInt",
    }) if operation_id == OperationId::new(0)
  ));

  let mut operations = structured_plan(&bridge).operations().to_vec();
  operations[0].arguments[1].binding = direct(syn::parse_quote!(u64));
  assert!(matches!(
    rebuild(&bridge, operations),
    Err(OhosEngineError::InvalidArgumentBinding {
      operation_id,
      argument: 1,
      expected: "lossless unsigned BigInt",
    }) if operation_id == OperationId::new(0)
  ));

  let bridge = echo_bridge(AsyncKind::Sync);
  let mut operations = echo_plan(&bridge).operations().to_vec();
  operations[0].return_binding = OhosReturnBinding::Direct {
    carrier_type: syn::parse_quote!(u64),
  };
  assert!(matches!(
    OhosBridgePlan::build(&bridge, operations),
    Err(OhosEngineError::InvalidReturnBinding {
      operation_id,
      expected: "lossless unsigned BigInt",
    }) if operation_id == OperationId::new(0)
  ));
}

#[test]
fn rejects_missing_or_unexpected_declared_error_bindings() {
  let bridge = structured_bridge();
  let mut operations = structured_plan(&bridge).operations().to_vec();
  operations[0].error_binding = OhosErrorBinding::Infallible;
  assert_eq!(
    rebuild(&bridge, operations).unwrap_err(),
    OhosEngineError::MissingErrorDescriptor {
      operation_id: OperationId::new(0),
    }
  );

  let mut operations = structured_plan(&bridge).operations().to_vec();
  operations[1].error_binding = OhosErrorBinding::Descriptor {
    map: syn::parse_quote!(fixture::map_unexpected_error),
  };
  assert_eq!(
    rebuild(&bridge, operations).unwrap_err(),
    OhosEngineError::UnexpectedErrorDescriptor {
      operation_id: OperationId::new(1),
    }
  );
}

#[test]
fn rejects_unstructured_callback_and_input_stream_shortcuts() {
  let bridge = structured_bridge();
  let mut operations = structured_plan(&bridge).operations().to_vec();
  operations[4].arguments[0].binding = lower(
    syn::parse_quote!(fixture::CallbackHandle),
    syn::parse_quote!(fixture::lower_callback),
  );
  assert!(matches!(
    rebuild(&bridge, operations),
    Err(OhosEngineError::InvalidArgumentBinding {
      operation_id,
      argument: 0,
      expected: "explicit carrier adapter",
    }) if operation_id == OperationId::new(4)
  ));

  let mut operations = structured_plan(&bridge).operations().to_vec();
  operations[8].arguments[0].binding = lower(
    syn::parse_quote!(fixture::InputStreamHandle),
    syn::parse_quote!(fixture::lower_input_stream),
  );
  assert!(matches!(
    rebuild(&bridge, operations),
    Err(OhosEngineError::InvalidArgumentBinding {
      operation_id,
      argument: 0,
      expected: "explicit carrier adapter",
    }) if operation_id == OperationId::new(8)
  ));
}

#[test]
fn resource_hooks_are_mandatory_for_structured_resources() {
  let bridge = structured_bridge();
  let operations = structured_plan(&bridge).operations().to_vec();
  assert_eq!(
    OhosBridgePlan::build(&bridge, operations.clone()).unwrap_err(),
    OhosEngineError::MissingResourceHook {
      role: "object release",
    }
  );

  let mut hooks = OhosResourceHooks {
    release_object: resource_hooks().release_object,
    ..OhosResourceHooks::default()
  };
  assert_eq!(
    OhosBridgePlan::build_with_resource_hooks(&bridge, operations.clone(), hooks.clone())
      .unwrap_err(),
    OhosEngineError::MissingResourceHook {
      role: "output-stream cancel",
    }
  );

  hooks.cancel_output_stream = resource_hooks().cancel_output_stream;
  assert_eq!(
    OhosBridgePlan::build_with_resource_hooks(&bridge, operations, hooks).unwrap_err(),
    OhosEngineError::MissingResourceHook {
      role: "output-stream release",
    }
  );
}

#[test]
fn rejects_duplicate_incomplete_and_non_dense_operation_ids() {
  let bridge = structured_bridge();
  let mut operations = structured_plan(&bridge).operations().to_vec();
  operations[1].operation_id = OperationId::new(0);
  assert_eq!(
    rebuild(&bridge, operations).unwrap_err(),
    OhosEngineError::DuplicateRustOperation { id: 0 }
  );

  let mut operations = structured_plan(&bridge).operations().to_vec();
  operations.pop();
  assert_eq!(
    rebuild(&bridge, operations).unwrap_err(),
    OhosEngineError::OperationCount {
      expected: 11,
      actual: 10,
    }
  );

  let mut operations = structured_plan(&bridge).operations().to_vec();
  let missing = operations.len() - 1;
  operations[missing].operation_id = OperationId::new(operations.len() as u32);
  assert_eq!(
    rebuild(&bridge, operations).unwrap_err(),
    OhosEngineError::MissingRustOperation { id: missing as u32 }
  );
}

#[test]
fn validates_object_ownership_receiver_target_and_return_shape() {
  let bridge = object_argument_bridge(Ownership::Owned);
  object_argument_plan(&bridge, Ownership::Owned).unwrap();
  assert!(matches!(
    object_argument_plan(&bridge, Ownership::Borrowed),
    Err(OhosEngineError::InvalidArgumentBinding {
      operation_id,
      argument: 0,
      ..
    }) if operation_id == OperationId::new(0)
  ));

  let bridge = structured_bridge();
  let mut operations = structured_plan(&bridge).operations().to_vec();
  operations[2].receiver = None;
  assert_eq!(
    rebuild(&bridge, operations).unwrap_err(),
    OhosEngineError::MissingObjectReceiver {
      operation_id: OperationId::new(2),
    }
  );

  let mut operations = structured_plan(&bridge).operations().to_vec();
  let receiver = operations[2].receiver.as_mut().unwrap();
  let OhosArgumentBinding::ObjectLease { ownership, .. } = &mut receiver.binding else {
    panic!("object method fixture must use an object receiver")
  };
  *ownership = Ownership::Owned;
  assert_eq!(
    rebuild(&bridge, operations).unwrap_err(),
    OhosEngineError::InvalidObjectReceiver {
      operation_id: OperationId::new(2),
    }
  );

  let mut operations = structured_plan(&bridge).operations().to_vec();
  operations[3].target = OhosOperationTarget::Native {
    call: syn::parse_quote!(fixture::fake_callback_call),
  };
  assert_eq!(
    rebuild(&bridge, operations).unwrap_err(),
    OhosEngineError::InvalidOperationTarget {
      operation_id: OperationId::new(3),
      kind: OperationKind::CallbackMethod,
    }
  );

  let mut operations = structured_plan(&bridge).operations().to_vec();
  operations[0].return_binding = OhosReturnBinding::Unit;
  assert!(matches!(
    rebuild(&bridge, operations),
    Err(OhosEngineError::InvalidReturnBinding {
      operation_id,
      expected: "lossless signed BigInt",
    }) if operation_id == OperationId::new(0)
  ));
}

#[test]
fn backend_factory_binds_callable_session_and_lossless_bigint() {
  let bridge = echo_bridge(AsyncKind::Sync);
  let module = generate_ohos_module(&bridge, echo_plan(&bridge)).unwrap();
  let calls = Arc::new(Mutex::new(Vec::new()));
  let host = FixtureHost {
    calls: Arc::clone(&calls),
    ..FixtureHost::default()
  };
  let session = OhosBackendFactory::new(module).open(host);
  let value = OhosValue::BigInt {
    negative: false,
    words: vec![u64::MAX, 3],
  };
  assert_eq!(
    session.invoke_sync(OperationId::new(0), std::slice::from_ref(&value)),
    Ok(value)
  );
  assert_eq!(&*calls.lock().unwrap(), &["sync"]);
  session.close().unwrap();
  assert_eq!(session.close(), Ok(()));
  assert!(matches!(
    session.invoke_sync(OperationId::new(0), &[]),
    Err(OhosEngineError::Closed)
  ));
}

#[test]
fn returned_object_is_released_once_by_explicit_release_and_close() {
  let bridge = object_return_bridge();
  let module = generate_ohos_module(&bridge, object_return_plan(&bridge)).unwrap();
  let released = Arc::new(Mutex::new(Vec::new()));
  let host = FixtureHost {
    returns: Arc::new(Mutex::new(Some(OhosValue::Object(42)))),
    released_objects: Arc::clone(&released),
    ..FixtureHost::default()
  };
  let session = OhosBackendFactory::new(module).open(host);
  assert_eq!(
    session.invoke_sync(OperationId::new(0), &[]),
    Ok(OhosValue::Object(42))
  );
  session.release_object(42).unwrap();
  session.release_object(42).unwrap();
  session.close().unwrap();
  session.close().unwrap();
  assert_eq!(&*released.lock().unwrap(), &[42]);
}

#[test]
fn returned_output_stream_close_cancels_and_releases_once() {
  let bridge = output_stream_return_bridge();
  let module = generate_ohos_module(&bridge, output_stream_return_plan(&bridge)).unwrap();
  let cancelled = Arc::new(Mutex::new(Vec::new()));
  let released = Arc::new(Mutex::new(Vec::new()));
  let host = FixtureHost {
    returns: Arc::new(Mutex::new(Some(OhosValue::OutputStream(7)))),
    cancelled_outputs: Arc::clone(&cancelled),
    released_outputs: Arc::clone(&released),
    ..FixtureHost::default()
  };
  let session = OhosBackendFactory::new(module).open(host);
  assert_eq!(
    session.invoke_sync(OperationId::new(0), &[]),
    Ok(OhosValue::OutputStream(7))
  );
  session.close().unwrap();
  session.close().unwrap();
  assert_eq!(&*cancelled.lock().unwrap(), &[7]);
  assert_eq!(&*released.lock().unwrap(), &[7]);
}

#[test]
fn returned_output_stream_explicit_cancel_release_are_idempotent() {
  let bridge = output_stream_return_bridge();
  let module = generate_ohos_module(&bridge, output_stream_return_plan(&bridge)).unwrap();
  let cancelled = Arc::new(Mutex::new(Vec::new()));
  let released = Arc::new(Mutex::new(Vec::new()));
  let host = FixtureHost {
    returns: Arc::new(Mutex::new(Some(OhosValue::OutputStream(8)))),
    cancelled_outputs: Arc::clone(&cancelled),
    released_outputs: Arc::clone(&released),
    ..FixtureHost::default()
  };
  let session = OhosBackendFactory::new(module).open(host);
  assert_eq!(
    session.invoke_sync(OperationId::new(0), &[]),
    Ok(OhosValue::OutputStream(8))
  );
  session.cancel_output_stream(8).unwrap();
  session.cancel_output_stream(8).unwrap();
  session.release_output_stream(8).unwrap();
  session.release_output_stream(8).unwrap();
  session.close().unwrap();
  assert_eq!(&*cancelled.lock().unwrap(), &[8]);
  assert_eq!(&*released.lock().unwrap(), &[8]);
}

#[test]
fn async_operation_uses_ark_scheduler_hook() {
  let bridge = echo_bridge(AsyncKind::Async);
  let module = generate_ohos_module(&bridge, echo_plan(&bridge)).unwrap();
  assert_eq!(
    module.hooks().async_scheduler,
    OhosAsyncScheduler::ArkEventLoop
  );
  let session = OhosBackendFactory::new(module).open(FixtureHost::default());
  let future = session
    .invoke_async(OperationId::new(0), vec![OhosValue::String("ark".into())])
    .unwrap();
  assert_eq!(
    futures::executor::block_on(future),
    Ok(OhosValue::String("ark".into()))
  );
}

#[test]
fn callback_contract_is_scoped_to_operation_use_site() {
  let allowed = SessionCallbackArgument {
    argument_index: 0,
    callback_type_id: 7,
    retention: SessionCallbackRetention::Scoped,
    threading: SessionCallbackThreading::CallingThread,
    reentrancy: SessionCallbackReentrancy::Allowed,
  };
  let forbidden = SessionCallbackArgument {
    reentrancy: SessionCallbackReentrancy::Forbidden,
    ..allowed
  };
  assert_eq!(
    super::session::callback_reentrancy_for_operation(&[allowed], 7),
    SessionCallbackReentrancy::Allowed
  );
  assert_eq!(
    super::session::callback_reentrancy_for_operation(&[forbidden], 7),
    SessionCallbackReentrancy::Forbidden
  );
}

#[test]
fn callback_method_dispatch_uses_registered_forbidden_reentrancy() {
  let bridge = structured_bridge();
  let module = generate_ohos_module(&bridge, structured_plan(&bridge)).unwrap();
  let session = OhosBackendFactory::new(module).open(FixtureHost::default());
  let first = session
    .invoke_async(OperationId::new(3), vec![OhosValue::Callback(7)])
    .unwrap();
  let second = session.invoke_async(OperationId::new(3), vec![OhosValue::Callback(7)]);
  assert!(matches!(
    second,
    Err(OhosEngineError::ReentrancyForbidden {
      callback_type: 3,
      callback_id: 7,
    })
  ));
  assert_eq!(futures::executor::block_on(first), Ok(OhosValue::Unit));
}
