use std::env;
use std::fs;
use std::path::PathBuf;

use napi_ohos_uniffi_engine::{
  generate_ohos_source, napi_family_core::HostFlavor, OhosArgumentBinding, OhosArgumentPlan,
  OhosBridgePlan, OhosErrorBinding, OhosOperationPlan, OhosOperationTarget, OhosReceiverPlan,
  OhosResourceHook, OhosResourceHooks, OhosReturnBinding,
};
use proc_macro2::{Ident, Span};
use uniffi_js_abi::{
  ArgumentDefinition, AsyncKind, ComponentDefinition, ComponentId, ComponentKey,
  IdentifiedComponent, IdentifiedOperation, IdentifiedType, NamedTypeKind, OperationDefinition,
  OperationId, OperationKind, OperationOwner, OperationSignature, OperationSourceKey, Ownership,
  ScalarType, TypeDefinition, TypeId, TypeSourceKey, ValueType,
};
use uniffi_js_engine_schema::{
  BridgePlan, BridgePlanInput, CallbackContract, CallbackReentrancy, CallbackRetention,
  CallbackThreading, CallbackUseSite, PlannedOperation,
  StreamContract, StreamUseSite, ValuePath,
};

fn type_key(component: &ComponentKey, name: &str) -> TypeSourceKey {
  TypeSourceKey::new(component.clone(), name).expect("valid fixture type key")
}

fn arg(name: &str, ty: ValueType) -> ArgumentDefinition {
  ArgumentDefinition::new(name, ty, Ownership::Owned).expect("valid fixture argument")
}

#[expect(clippy::too_many_arguments)]
fn operation(
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
      format!("generated_fixture::{name}"),
      format!("generated_fixture_private_{id}"),
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

fn bridge() -> BridgePlan {
  let component = ComponentKey::new("ohos_generated_fixture").unwrap();
  let observer = type_key(&component, "Observer");
  let thing = type_key(&component, "Thing");
  let failure = type_key(&component, "Failure");
  let byte_stream = type_key(&component, "ByteStream");
  let operations = vec![
    operation(
      &component,
      0,
      OperationOwner::Namespace,
      OperationKind::Function,
      "syncEcho",
      vec![arg("value", ValueType::Scalar(ScalarType::U32))],
      Some(ValueType::Scalar(ScalarType::U32)),
      AsyncKind::Sync,
      None,
    ),
    operation(
      &component,
      1,
      OperationOwner::Namespace,
      OperationKind::Function,
      "asyncEcho",
      vec![arg("value", ValueType::Scalar(ScalarType::U32))],
      Some(ValueType::Scalar(ScalarType::U32)),
      AsyncKind::Async,
      None,
    ),
    operation(
      &component,
      2,
      OperationOwner::Namespace,
      OperationKind::Function,
      "echoI64",
      vec![arg("value", ValueType::Scalar(ScalarType::I64))],
      Some(ValueType::Scalar(ScalarType::I64)),
      AsyncKind::Sync,
      None,
    ),
    operation(
      &component,
      3,
      OperationOwner::Namespace,
      OperationKind::Function,
      "echoU64",
      vec![arg("value", ValueType::Scalar(ScalarType::U64))],
      Some(ValueType::Scalar(ScalarType::U64)),
      AsyncKind::Sync,
      None,
    ),
    operation(
      &component,
      4,
      OperationOwner::Namespace,
      OperationKind::Function,
      "fail",
      vec![arg("value", ValueType::Scalar(ScalarType::U32))],
      Some(ValueType::Scalar(ScalarType::U32)),
      AsyncKind::Sync,
      Some(failure.clone()),
    ),
    operation(
      &component,
      5,
      OperationOwner::Namespace,
      OperationKind::Function,
      "roundtripThing",
      vec![arg("thing", ValueType::Named(thing.clone()))],
      Some(ValueType::Named(thing.clone())),
      AsyncKind::Sync,
      None,
    ),
    operation(
      &component,
      6,
      OperationOwner::Namespace,
      OperationKind::Function,
      "observeSync",
      vec![arg("observer", ValueType::Named(observer.clone()))],
      Some(ValueType::Scalar(ScalarType::U32)),
      AsyncKind::Sync,
      Some(failure.clone()),
    ),
    operation(
      &component,
      7,
      OperationOwner::Namespace,
      OperationKind::Function,
      "observeAsync",
      vec![arg("observer", ValueType::Named(observer.clone()))],
      Some(ValueType::Scalar(ScalarType::U32)),
      AsyncKind::Async,
      None,
    ),
    operation(
      &component,
      8,
      OperationOwner::Callback(observer.clone()),
      OperationKind::CallbackMethod,
      "onValueAsyncFallible",
      vec![arg("value", ValueType::Scalar(ScalarType::U32))],
      Some(ValueType::Scalar(ScalarType::U32)),
      AsyncKind::Async,
      Some(failure.clone()),
    ),
    operation(
      &component,
      9,
      OperationOwner::Callback(observer.clone()),
      OperationKind::CallbackMethod,
      "onValueSyncFallible",
      vec![arg("value", ValueType::Scalar(ScalarType::U32))],
      Some(ValueType::Scalar(ScalarType::U32)),
      AsyncKind::Sync,
      Some(failure.clone()),
    ),
    operation(
      &component,
      10,
      OperationOwner::Callback(observer.clone()),
      OperationKind::CallbackMethod,
      "onValueAsyncInfallible",
      vec![arg("value", ValueType::Scalar(ScalarType::U32))],
      Some(ValueType::Scalar(ScalarType::U32)),
      AsyncKind::Async,
      None,
    ),
    operation(
      &component,
      11,
      OperationOwner::Callback(observer.clone()),
      OperationKind::CallbackMethod,
      "onValueSyncInfallible",
      vec![arg("value", ValueType::Scalar(ScalarType::U32))],
      Some(ValueType::Scalar(ScalarType::U32)),
      AsyncKind::Sync,
      None,
    ),
    operation(
      &component,
      12,
      OperationOwner::Namespace,
      OperationKind::OutputStreamStart,
      "openStream",
      vec![arg("observer", ValueType::Named(observer.clone()))],
      Some(ValueType::output_stream(ValueType::Scalar(ScalarType::U32))),
      AsyncKind::Sync,
      None,
    ),
    operation(
      &component,
      13,
      OperationOwner::Object(byte_stream.clone()),
      OperationKind::OutputStreamNext,
      "nextStream",
      Vec::new(),
      Some(ValueType::optional(ValueType::Scalar(ScalarType::U32))),
      AsyncKind::Async,
      None,
    ),
    operation(
      &component,
      14,
      OperationOwner::Object(byte_stream.clone()),
      OperationKind::OutputStreamCancel,
      "cancelStream",
      Vec::new(),
      None,
      AsyncKind::Async,
      None,
    ),
    operation(
      &component,
      15,
      OperationOwner::Namespace,
      OperationKind::Function,
      "consumeInput",
      vec![arg(
        "source",
        ValueType::input_stream(ValueType::Scalar(ScalarType::U32)),
      )],
      Some(ValueType::Scalar(ScalarType::U32)),
      AsyncKind::Async,
      None,
    ),
    operation(
      &component,
      16,
      OperationOwner::Namespace,
      OperationKind::InputStreamPull,
      "pullInput",
      vec![arg("streamId", ValueType::Scalar(ScalarType::U32))],
      Some(ValueType::optional(ValueType::Scalar(ScalarType::U32))),
      AsyncKind::Async,
      None,
    ),
    operation(
      &component,
      17,
      OperationOwner::Namespace,
      OperationKind::InputStreamCancel,
      "cancelInput",
      vec![arg("streamId", ValueType::Scalar(ScalarType::U32))],
      None,
      AsyncKind::Async,
      None,
    ),
    operation(
      &component,
      18,
      OperationOwner::Namespace,
      OperationKind::Function,
      "releaseCount",
      Vec::new(),
      Some(ValueType::Scalar(ScalarType::U32)),
      AsyncKind::Sync,
      None,
    ),
  ];

  let sync_contract = CallbackContract {
    retention: CallbackRetention::Retained,
    threading: CallbackThreading::CallingThread,
    reentrancy: CallbackReentrancy::Forbidden,
  };
  BridgePlan::build(BridgePlanInput {
    components: vec![IdentifiedComponent {
      id: ComponentId::new(0),
      definition: ComponentDefinition::new(component, "ohosGeneratedFixture").unwrap(),
    }],
    types: vec![
      IdentifiedType {
        id: TypeId::new(0),
        definition: TypeDefinition::new(observer, "Observer", NamedTypeKind::Callback).unwrap(),
      },
      IdentifiedType {
        id: TypeId::new(1),
        definition: TypeDefinition::new(thing, "Thing", NamedTypeKind::Object).unwrap(),
      },
      IdentifiedType {
        id: TypeId::new(2),
        definition: TypeDefinition::new(
          failure,
          "Failure",
          NamedTypeKind::Error {
            variants: Vec::new(),
          },
        )
        .unwrap(),
      },
      IdentifiedType {
        id: TypeId::new(3),
        definition: TypeDefinition::new(byte_stream, "ByteStream", NamedTypeKind::Object).unwrap(),
      },
    ],
    operations,
    callbacks: vec![
      CallbackUseSite {
        operation_id: OperationId::new(6),
        callback_type: TypeId::new(0),
        path: ValuePath::argument(0),
        contract: sync_contract.clone(),
      },
      CallbackUseSite {
        operation_id: OperationId::new(7),
        callback_type: TypeId::new(0),
        path: ValuePath::argument(0),
        contract: CallbackContract {
          retention: CallbackRetention::Retained,
          // The mixed callback interface contains synchronous methods, so a
          // single use-site cannot promise cross-thread entry.  The async
          // methods still use a real TSFN for safe worker-to-Ark dispatch.
          threading: CallbackThreading::CallingThread,
          reentrancy: CallbackReentrancy::Forbidden,
        },
      },
      CallbackUseSite {
        operation_id: OperationId::new(12),
        callback_type: TypeId::new(0),
        path: ValuePath::argument(0),
        contract: CallbackContract {
          retention: CallbackRetention::Scoped,
          ..sync_contract
        },
      },
    ],
    streams: vec![
      StreamUseSite {
        operation_id: OperationId::new(12),
        path: ValuePath::return_value(),
        contract: StreamContract::output(),
      },
      StreamUseSite {
        operation_id: OperationId::new(15),
        path: ValuePath::argument(0),
        contract: StreamContract::input(),
      },
    ],
    targets: vec![HostFlavor::Ohos.capabilities()],
  })
  .unwrap()
}

fn name(value: &str) -> Ident {
  Ident::new(value, Span::call_site())
}

fn direct_argument(value: &str, ty: syn::Type) -> OhosArgumentPlan {
  OhosArgumentPlan {
    name: name(value),
    binding: OhosArgumentBinding::Direct { carrier_type: ty },
  }
}

fn operation_plans(bridge: &BridgePlan) -> OhosBridgePlan {
  let native = |id, call, arguments, return_binding, error_binding| OhosOperationPlan {
    operation_id: OperationId::new(id),
    target: OhosOperationTarget::Native { call },
    receiver: None,
    arguments,
    return_binding,
    error_binding,
  };
  let host = |id, target| OhosOperationPlan {
    operation_id: OperationId::new(id),
    target,
    receiver: None,
    arguments: Vec::new(),
    return_binding: OhosReturnBinding::Unit,
    error_binding: OhosErrorBinding::Infallible,
  };
  OhosBridgePlan::build_with_resource_hooks(
    bridge,
    vec![
      native(
        0,
        syn::parse_quote!(crate::generated_fixture::sync_echo),
        vec![direct_argument("value", syn::parse_quote!(u32))],
        OhosReturnBinding::Direct {
          carrier_type: syn::parse_quote!(u32),
        },
        OhosErrorBinding::Infallible,
      ),
      native(
        1,
        syn::parse_quote!(crate::generated_fixture::async_echo),
        vec![direct_argument("value", syn::parse_quote!(u32))],
        OhosReturnBinding::Direct {
          carrier_type: syn::parse_quote!(u32),
        },
        OhosErrorBinding::Infallible,
      ),
      native(
        2,
        syn::parse_quote!(crate::generated_fixture::echo_i64),
        vec![OhosArgumentPlan {
          name: name("value"),
          binding: OhosArgumentBinding::I64BigInt,
        }],
        OhosReturnBinding::I64BigInt,
        OhosErrorBinding::Infallible,
      ),
      native(
        3,
        syn::parse_quote!(crate::generated_fixture::echo_u64),
        vec![OhosArgumentPlan {
          name: name("value"),
          binding: OhosArgumentBinding::U64BigInt,
        }],
        OhosReturnBinding::U64BigInt,
        OhosErrorBinding::Infallible,
      ),
      native(
        4,
        syn::parse_quote!(crate::generated_fixture::fail),
        vec![direct_argument("value", syn::parse_quote!(u32))],
        OhosReturnBinding::Direct {
          carrier_type: syn::parse_quote!(u32),
        },
        OhosErrorBinding::Descriptor {
          map: syn::parse_quote!(crate::generated_fixture::map_failure),
        },
      ),
      native(
        5,
        syn::parse_quote!(crate::generated_fixture::roundtrip_thing),
        vec![OhosArgumentPlan {
          name: name("thing"),
          binding: OhosArgumentBinding::ObjectLease {
            carrier_type: syn::parse_quote!(napi_ohos::bindgen_prelude::Object<'static>),
            lower: syn::parse_quote!(crate::generated_fixture::lower_thing),
            ownership: Ownership::Owned,
          },
        }],
        OhosReturnBinding::ObjectLease {
          carrier_type: syn::parse_quote!(napi_ohos::bindgen_prelude::Object<'static>),
          lift: syn::parse_quote!(crate::generated_fixture::lift_thing),
        },
        OhosErrorBinding::Infallible,
      ),
      native(
        6,
        syn::parse_quote!(crate::generated_fixture::observe_sync),
        vec![OhosArgumentPlan {
          name: name("observer"),
          binding: OhosArgumentBinding::CallbackProxy {
            rust_type: syn::parse_quote!(crate::generated_fixture::SyncObserverProxy),
            build: syn::parse_quote!(crate::generated_fixture::build_sync_observer),
          },
        }],
        OhosReturnBinding::Direct {
          carrier_type: syn::parse_quote!(u32),
        },
        OhosErrorBinding::Descriptor {
          map: syn::parse_quote!(crate::generated_fixture::identity_error),
        },
      ),
      native(
        7,
        syn::parse_quote!(crate::generated_fixture::observe_async),
        vec![OhosArgumentPlan {
          name: name("observer"),
          binding: OhosArgumentBinding::CallbackProxy {
            rust_type: syn::parse_quote!(crate::generated_fixture::AsyncObserverProxy),
            build: syn::parse_quote!(crate::generated_fixture::build_async_observer),
          },
        }],
        OhosReturnBinding::Direct {
          carrier_type: syn::parse_quote!(u32),
        },
        OhosErrorBinding::Infallible,
      ),
      host(8, OhosOperationTarget::CallbackHost),
      host(9, OhosOperationTarget::CallbackHost),
      host(10, OhosOperationTarget::CallbackHost),
      host(11, OhosOperationTarget::CallbackHost),
      native(
        12,
        syn::parse_quote!(crate::generated_fixture::open_stream),
        vec![OhosArgumentPlan {
          name: name("observer"),
          binding: OhosArgumentBinding::CallbackProxy {
            rust_type: syn::parse_quote!(crate::generated_fixture::StreamFactoryProxy),
            build: syn::parse_quote!(crate::generated_fixture::build_stream_factory),
          },
        }],
        OhosReturnBinding::OutputStreamLease {
          carrier_type: syn::parse_quote!(napi_ohos::bindgen_prelude::Object<'static>),
          lift: syn::parse_quote!(crate::generated_fixture::lift_stream),
        },
        OhosErrorBinding::Infallible,
      ),
      OhosOperationPlan {
        operation_id: OperationId::new(13),
        target: OhosOperationTarget::Native {
          call: syn::parse_quote!(crate::generated_fixture::next_stream),
        },
        receiver: Some(OhosReceiverPlan {
          name: name("stream"),
          binding: OhosArgumentBinding::OutputStreamLease {
            carrier_type: syn::parse_quote!(u32),
            lower: syn::parse_quote!(crate::generated_fixture::lower_handle),
            ownership: Ownership::Borrowed,
          },
        }),
        arguments: Vec::new(),
        return_binding: OhosReturnBinding::LiftWith {
          carrier_type: syn::parse_quote!(Option<u32>),
          lift: syn::parse_quote!(crate::generated_fixture::lift_optional_u32),
        },
        error_binding: OhosErrorBinding::Infallible,
      },
      OhosOperationPlan {
        operation_id: OperationId::new(14),
        target: OhosOperationTarget::Native {
          call: syn::parse_quote!(crate::generated_fixture::cancel_stream),
        },
        receiver: Some(OhosReceiverPlan {
          name: name("stream"),
          binding: OhosArgumentBinding::OutputStreamLease {
            carrier_type: syn::parse_quote!(u32),
            lower: syn::parse_quote!(crate::generated_fixture::lower_handle),
            ownership: Ownership::Borrowed,
          },
        }),
        arguments: Vec::new(),
        return_binding: OhosReturnBinding::Unit,
        error_binding: OhosErrorBinding::Infallible,
      },
      native(
        15,
        syn::parse_quote!(crate::generated_fixture::consume_input),
        vec![OhosArgumentPlan {
          name: name("source"),
          binding: OhosArgumentBinding::InputStreamProxy {
            rust_type: syn::parse_quote!(crate::generated_fixture::InputProxy),
            build: syn::parse_quote!(crate::generated_fixture::build_input_proxy),
          },
        }],
        OhosReturnBinding::Direct {
          carrier_type: syn::parse_quote!(u32),
        },
        OhosErrorBinding::Infallible,
      ),
      host(16, OhosOperationTarget::InputStreamHostPull),
      host(17, OhosOperationTarget::InputStreamHostCancel),
      native(
        18,
        syn::parse_quote!(crate::generated_fixture::release_count),
        Vec::new(),
        OhosReturnBinding::Direct {
          carrier_type: syn::parse_quote!(u32),
        },
        OhosErrorBinding::Infallible,
      ),
    ],
    OhosResourceHooks {
      release_object: Some(OhosResourceHook {
        call: syn::parse_quote!(crate::generated_fixture::release_object),
        carrier_type: syn::parse_quote!(u32),
      }),
      cancel_output_stream: Some(OhosResourceHook {
        call: syn::parse_quote!(crate::generated_fixture::cancel_output_stream),
        carrier_type: syn::parse_quote!(u32),
      }),
      release_output_stream: Some(OhosResourceHook {
        call: syn::parse_quote!(crate::generated_fixture::release_output_stream),
        carrier_type: syn::parse_quote!(u32),
      }),
    },
  )
  .expect("structured OHOS Rust bridge plan is valid")
}

fn main() {
  napi_build_ohos::setup();
  println!("cargo:rerun-if-changed=build.rs");
  println!("cargo:rerun-if-changed=src/lib.rs");
  let bridge = bridge();
  let generated = generate_ohos_source(&bridge, operation_plans(&bridge))
    .expect("OHOS generated fixture plan must lower successfully");
  let output =
    PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set")).join("generated_ohos_module.rs");
  fs::write(output, generated.source().to_string()).expect("write generated OHOS source");
}
