use std::env;
use std::fs;
use std::path::PathBuf;

use napi_ohos_uniffi_engine::{
  generate_ohos_source, napi_family_core::*, OhosArgumentBinding, OhosArgumentPlan,
  OhosBridgePlan, OhosErrorBinding, OhosOperationPlan, OhosOperationTarget, OhosReceiverPlan,
  OhosResourceHook, OhosResourceHooks, OhosReturnBinding,
};
use proc_macro2::{Ident, Span};

fn name(value: &str) -> Ident {
  Ident::new(value, Span::call_site())
}

fn direct_argument(value: &str, ty: syn::Type) -> OhosArgumentPlan {
  OhosArgumentPlan {
    name: name(value),
    binding: OhosArgumentBinding::Direct { carrier_type: ty },
  }
}

fn family() -> FamilyPlan {
  let mut operations = Vec::with_capacity(28);
  push_operation(&mut operations, 0, OperationKind::Function, AsyncKind::Sync, false, 1, OperationDispatch::Native);
  push_operation(&mut operations, 1, OperationKind::Function, AsyncKind::Async, false, 1, OperationDispatch::Native);
  let i64_op = operation(2, OperationKind::Function, AsyncKind::Sync, false, 1, OperationDispatch::Native);
  operations.push(i64_op);
  push_operation(&mut operations, 3, OperationKind::Function, AsyncKind::Sync, false, 1, OperationDispatch::Native);
  push_operation(&mut operations, 4, OperationKind::Function, AsyncKind::Sync, true, 1, OperationDispatch::Native);
  let mut object_op = operation(5, OperationKind::Function, AsyncKind::Sync, false, 1, OperationDispatch::Native);
  object_op.result = Some(ResourceBinding { kind: ResourceKind::Object, ownership: ResourceOwnership::Owned });
  operations.push(object_op);
  let mut sync_callback = operation(6, OperationKind::Function, AsyncKind::Sync, true, 1, OperationDispatch::Native);
  sync_callback.callbacks.push(callback_site(6, CallbackThreading::CallingThread));
  operations.push(sync_callback);
  let mut async_callback = operation(7, OperationKind::Function, AsyncKind::Async, false, 1, OperationDispatch::Native);
  async_callback.callbacks.push(callback_site(7, CallbackThreading::CallingThread));
  operations.push(async_callback);
  push_operation(&mut operations, 8, OperationKind::CallbackMethod, AsyncKind::Async, true, 1, OperationDispatch::CallbackHost { callback_type_id: 0, method_id: 0 });
  push_operation(&mut operations, 9, OperationKind::CallbackMethod, AsyncKind::Sync, true, 1, OperationDispatch::CallbackHost { callback_type_id: 0, method_id: 1 });
  push_operation(&mut operations, 10, OperationKind::CallbackMethod, AsyncKind::Async, false, 1, OperationDispatch::CallbackHost { callback_type_id: 0, method_id: 2 });
  push_operation(&mut operations, 11, OperationKind::CallbackMethod, AsyncKind::Sync, false, 1, OperationDispatch::CallbackHost { callback_type_id: 0, method_id: 3 });
  let mut output_start = operation(12, OperationKind::OutputStreamStart, AsyncKind::Sync, false, 1, OperationDispatch::Native);
  output_start.result = Some(ResourceBinding { kind: ResourceKind::OutputStream, ownership: ResourceOwnership::Owned });
  output_start.callbacks.push(CallbackUseSite {
    operation_id: 12,
    callback_type_id: 0,
    path: ValuePath::argument(0),
    contract: CallbackContract { retention: CallbackRetention::Scoped, threading: CallbackThreading::CallingThread, reentrancy: CallbackReentrancy::Forbidden },
  });
  output_start.stream_slot = Some(StreamSlotIdentity { use_site_id: 1, operation_id: 12, kind: OperationKind::OutputStreamStart });
  output_start.streams.push(StreamUseSite {
    operation_id: 12,
    use_site_id: 1,
    path: ValuePath::return_value(),
    direction: StreamDirection::Output,
    item: stream_value_binding(),
    error: stream_value_binding(),
    is_send: true,
    slots: vec![
      StreamSlotIdentity { use_site_id: 1, operation_id: 12, kind: OperationKind::OutputStreamStart },
      StreamSlotIdentity { use_site_id: 1, operation_id: 13, kind: OperationKind::OutputStreamNext },
      StreamSlotIdentity { use_site_id: 1, operation_id: 14, kind: OperationKind::OutputStreamCancel },
    ],
  });
  operations.push(output_start);
  let mut output_next = operation(13, OperationKind::OutputStreamNext, AsyncKind::Async, false, 0, OperationDispatch::Native);
  output_next.receiver = Some(ReceiverBinding::Resource(ResourceBinding { kind: ResourceKind::OutputStream, ownership: ResourceOwnership::Borrowed }));
  output_next.stream_slot = Some(StreamSlotIdentity { use_site_id: 1, operation_id: 13, kind: OperationKind::OutputStreamNext });
  operations.push(output_next);
  let mut output_cancel = operation(14, OperationKind::OutputStreamCancel, AsyncKind::Async, false, 0, OperationDispatch::Native);
  output_cancel.receiver = Some(ReceiverBinding::Resource(ResourceBinding { kind: ResourceKind::OutputStream, ownership: ResourceOwnership::Borrowed }));
  output_cancel.stream_slot = Some(StreamSlotIdentity { use_site_id: 1, operation_id: 14, kind: OperationKind::OutputStreamCancel });
  operations.push(output_cancel);
  let mut input = operation(15, OperationKind::Function, AsyncKind::Async, false, 1, OperationDispatch::Native);
  input.streams.push(StreamUseSite {
    operation_id: 15,
    use_site_id: 0,
    path: ValuePath::argument(0),
    direction: StreamDirection::Input,
    item: stream_value_binding(),
    error: stream_value_binding(),
    is_send: false,
    slots: vec![
      StreamSlotIdentity { use_site_id: 0, operation_id: 16, kind: OperationKind::InputStreamPull },
      StreamSlotIdentity { use_site_id: 0, operation_id: 17, kind: OperationKind::InputStreamCancel },
    ],
  });
  operations.push(input);
  let mut input_pull = operation(16, OperationKind::InputStreamPull, AsyncKind::Async, false, 0, OperationDispatch::InputStreamHostPull);
  input_pull.receiver = Some(ReceiverBinding::Resource(ResourceBinding { kind: ResourceKind::InputStream, ownership: ResourceOwnership::Borrowed }));
  input_pull.stream_slot = Some(StreamSlotIdentity { use_site_id: 0, operation_id: 16, kind: OperationKind::InputStreamPull });
  operations.push(input_pull);
  let mut input_cancel = operation(17, OperationKind::InputStreamCancel, AsyncKind::Async, false, 0, OperationDispatch::InputStreamHostCancel);
  input_cancel.receiver = Some(ReceiverBinding::Resource(ResourceBinding { kind: ResourceKind::InputStream, ownership: ResourceOwnership::Borrowed }));
  input_cancel.stream_slot = Some(StreamSlotIdentity { use_site_id: 0, operation_id: 17, kind: OperationKind::InputStreamCancel });
  operations.push(input_cancel);
  push_operation(&mut operations, 18, OperationKind::Function, AsyncKind::Sync, false, 0, OperationDispatch::Native);
  let mut held_callback = operation(19, OperationKind::Function, AsyncKind::Sync, false, 1, OperationDispatch::Native);
  held_callback.callbacks.push(callback_site(19, CallbackThreading::CallingThread));
  operations.push(held_callback);
  push_operation(&mut operations, 20, OperationKind::Function, AsyncKind::Sync, false, 0, OperationDispatch::Native);
  let mut record_sync = operation(21, OperationKind::Method, AsyncKind::Sync, false, 0, OperationDispatch::Native);
  record_sync.receiver = Some(ReceiverBinding::Value);
  operations.push(record_sync);
  let mut record_async = operation(22, OperationKind::Method, AsyncKind::Async, false, 0, OperationDispatch::Native);
  record_async.receiver = Some(ReceiverBinding::Value);
  operations.push(record_async);
  let mut enum_sync = operation(23, OperationKind::Method, AsyncKind::Sync, false, 0, OperationDispatch::Native);
  enum_sync.receiver = Some(ReceiverBinding::Value);
  operations.push(enum_sync);
  let mut enum_async = operation(24, OperationKind::Method, AsyncKind::Async, false, 0, OperationDispatch::Native);
  enum_async.receiver = Some(ReceiverBinding::Value);
  operations.push(enum_async);
  push_operation(&mut operations, 25, OperationKind::Function, AsyncKind::Async, false, 0, OperationDispatch::Native);
  push_operation(&mut operations, 26, OperationKind::Function, AsyncKind::Sync, false, 0, OperationDispatch::Native);
  push_operation(&mut operations, 27, OperationKind::Function, AsyncKind::Sync, false, 0, OperationDispatch::Native);
  FamilyPlan::build(FamilyPlanInput {
    flavor: HostFlavor::Ohos,
    close_policy: ClosePolicy { grace_ms: 40, on_deadline: DeadlineAction::Detach },
    operations,
  }).expect("valid generated fixture family plan")
}

fn push_operation(
  operations: &mut Vec<FamilyOperationInput>,
  id: u32,
  kind: OperationKind,
  async_kind: AsyncKind,
  fallible: bool,
  argument_count: usize,
  dispatch: OperationDispatch,
) {
  operations.push(operation(id, kind, async_kind, fallible, argument_count, dispatch));
}

fn operation(
  id: u32,
  kind: OperationKind,
  async_kind: AsyncKind,
  fallible: bool,
  argument_count: usize,
  dispatch: OperationDispatch,
) -> FamilyOperationInput {
  FamilyOperationInput { id, kind, async_kind, fallible, argument_count, dispatch, receiver: None, result: None, callbacks: Vec::new(), streams: Vec::new(), stream_slot: None }
}

fn stream_value_binding() -> StreamValueBinding {
  StreamValueBinding { carrier: CarrierKind::Primitive, conversion: ConversionRecipe::Identity }
}

fn callback_site(operation_id: u32, threading: CallbackThreading) -> CallbackUseSite {
  CallbackUseSite {
    operation_id,
    callback_type_id: 0,
    path: ValuePath::argument(0),
    contract: CallbackContract { retention: CallbackRetention::Retained, threading, reentrancy: CallbackReentrancy::Forbidden },
  }
}

fn operation_plans(family: &FamilyPlan) -> OhosBridgePlan {
  let native = |id, call, arguments, return_binding, error_binding| OhosOperationPlan {
    operation_id: id,
    target: OhosOperationTarget::Native { call },
    receiver: None,
    arguments,
    return_binding,
    error_binding,
  };
  let host = |id, target| OhosOperationPlan {
    operation_id: id,
    target,
    receiver: None,
    arguments: Vec::new(),
    return_binding: OhosReturnBinding::Unit,
    error_binding: OhosErrorBinding::Infallible,
  };
  OhosBridgePlan::build_with_resource_hooks(
    family,
    vec![
      native(0, syn::parse_quote!(crate::generated_fixture::sync_echo), vec![direct_argument("value", syn::parse_quote!(u32))], OhosReturnBinding::Direct { carrier_type: syn::parse_quote!(u32) }, OhosErrorBinding::Infallible),
      native(1, syn::parse_quote!(crate::generated_fixture::async_echo), vec![direct_argument("value", syn::parse_quote!(u32))], OhosReturnBinding::Direct { carrier_type: syn::parse_quote!(u32) }, OhosErrorBinding::Infallible),
      native(2, syn::parse_quote!(crate::generated_fixture::echo_i64), vec![OhosArgumentPlan { name: name("value"), binding: OhosArgumentBinding::I64BigInt }], OhosReturnBinding::I64BigInt, OhosErrorBinding::Infallible),
      native(3, syn::parse_quote!(crate::generated_fixture::echo_u64), vec![OhosArgumentPlan { name: name("value"), binding: OhosArgumentBinding::U64BigInt }], OhosReturnBinding::U64BigInt, OhosErrorBinding::Infallible),
      native(4, syn::parse_quote!(crate::generated_fixture::fail), vec![direct_argument("value", syn::parse_quote!(u32))], OhosReturnBinding::Direct { carrier_type: syn::parse_quote!(u32) }, OhosErrorBinding::Descriptor { map: syn::parse_quote!(crate::generated_fixture::map_failure) }),
      native(5, syn::parse_quote!(crate::generated_fixture::roundtrip_thing), vec![OhosArgumentPlan { name: name("thing"), binding: OhosArgumentBinding::ObjectLease { carrier_type: syn::parse_quote!(napi_ohos::bindgen_prelude::Object<'static>), lower: syn::parse_quote!(crate::generated_fixture::lower_thing), ownership: ResourceOwnership::Owned } }], OhosReturnBinding::ObjectLease { carrier_type: syn::parse_quote!(napi_ohos::bindgen_prelude::Object<'static>), lift: syn::parse_quote!(crate::generated_fixture::lift_thing) }, OhosErrorBinding::Infallible),
      native(6, syn::parse_quote!(crate::generated_fixture::observe_sync), vec![OhosArgumentPlan { name: name("observer"), binding: OhosArgumentBinding::CallbackProxy { rust_type: syn::parse_quote!(crate::generated_fixture::SyncObserverProxy), build: syn::parse_quote!(crate::generated_fixture::build_sync_observer) } }], OhosReturnBinding::Direct { carrier_type: syn::parse_quote!(u32) }, OhosErrorBinding::Descriptor { map: syn::parse_quote!(crate::generated_fixture::identity_error) }),
      native(7, syn::parse_quote!(crate::generated_fixture::observe_async), vec![OhosArgumentPlan { name: name("observer"), binding: OhosArgumentBinding::CallbackProxy { rust_type: syn::parse_quote!(crate::generated_fixture::AsyncObserverProxy), build: syn::parse_quote!(crate::generated_fixture::build_async_observer) } }], OhosReturnBinding::Direct { carrier_type: syn::parse_quote!(u32) }, OhosErrorBinding::Infallible),
      host(8, OhosOperationTarget::CallbackHost), host(9, OhosOperationTarget::CallbackHost), host(10, OhosOperationTarget::CallbackHost), host(11, OhosOperationTarget::CallbackHost),
      native(12, syn::parse_quote!(crate::generated_fixture::open_stream), vec![OhosArgumentPlan { name: name("observer"), binding: OhosArgumentBinding::CallbackProxy { rust_type: syn::parse_quote!(crate::generated_fixture::StreamFactoryProxy), build: syn::parse_quote!(crate::generated_fixture::build_stream_factory) } }], OhosReturnBinding::OutputStreamLease { carrier_type: syn::parse_quote!(napi_ohos::bindgen_prelude::Object<'static>), lift: syn::parse_quote!(crate::generated_fixture::lift_stream) }, OhosErrorBinding::Infallible),
      OhosOperationPlan { operation_id: 13, target: OhosOperationTarget::Native { call: syn::parse_quote!(crate::generated_fixture::next_stream) }, receiver: Some(OhosReceiverPlan { name: name("stream"), binding: OhosArgumentBinding::OutputStreamLease { carrier_type: syn::parse_quote!(u32), lower: syn::parse_quote!(crate::generated_fixture::lower_handle), ownership: ResourceOwnership::Borrowed } }), arguments: Vec::new(), return_binding: OhosReturnBinding::LiftWith { carrier_type: syn::parse_quote!(Option<u32>), lift: syn::parse_quote!(crate::generated_fixture::lift_optional_u32) }, error_binding: OhosErrorBinding::Infallible },
      OhosOperationPlan { operation_id: 14, target: OhosOperationTarget::Native { call: syn::parse_quote!(crate::generated_fixture::cancel_stream) }, receiver: Some(OhosReceiverPlan { name: name("stream"), binding: OhosArgumentBinding::OutputStreamLease { carrier_type: syn::parse_quote!(u32), lower: syn::parse_quote!(crate::generated_fixture::lower_handle), ownership: ResourceOwnership::Borrowed } }), arguments: Vec::new(), return_binding: OhosReturnBinding::Unit, error_binding: OhosErrorBinding::Infallible },
      native(15, syn::parse_quote!(crate::generated_fixture::consume_input), vec![OhosArgumentPlan { name: name("source"), binding: OhosArgumentBinding::InputStreamProxy { rust_type: syn::parse_quote!(crate::generated_fixture::InputProxy), build: syn::parse_quote!(crate::generated_fixture::build_input_proxy) } }], OhosReturnBinding::Direct { carrier_type: syn::parse_quote!(u32) }, OhosErrorBinding::Infallible),
      host(16, OhosOperationTarget::InputStreamHostPull), host(17, OhosOperationTarget::InputStreamHostCancel),
      native(18, syn::parse_quote!(crate::generated_fixture::release_count), Vec::new(), OhosReturnBinding::Direct { carrier_type: syn::parse_quote!(u32) }, OhosErrorBinding::Infallible),
      native(19, syn::parse_quote!(crate::generated_fixture::hold_sync_observer), vec![OhosArgumentPlan { name: name("observer"), binding: OhosArgumentBinding::CallbackProxy { rust_type: syn::parse_quote!(crate::generated_fixture::SyncObserverProxy), build: syn::parse_quote!(crate::generated_fixture::build_sync_observer) } }], OhosReturnBinding::Direct { carrier_type: syn::parse_quote!(u32) }, OhosErrorBinding::Infallible),
      native(20, syn::parse_quote!(crate::generated_fixture::drop_held_sync_observers), Vec::new(), OhosReturnBinding::Direct { carrier_type: syn::parse_quote!(u32) }, OhosErrorBinding::Infallible),
      value_method(21, false, "record", "lower_value_record", "value_record_sync"),
      value_method(22, true, "record", "lower_value_record", "value_record_async"),
      value_method(23, false, "value", "lower_value_enum", "value_enum_sync"),
      value_method(24, true, "value", "lower_value_enum", "value_enum_async"),
      native(25, syn::parse_quote!(crate::generated_fixture::never_settle_native), Vec::new(), OhosReturnBinding::Direct { carrier_type: syn::parse_quote!(u32) }, OhosErrorBinding::Infallible),
      native(26, syn::parse_quote!(crate::generated_fixture::wake_output_cancel), Vec::new(), OhosReturnBinding::Direct { carrier_type: syn::parse_quote!(u32) }, OhosErrorBinding::Infallible),
      native(27, syn::parse_quote!(crate::generated_fixture::wake_never_settle_native), Vec::new(), OhosReturnBinding::Direct { carrier_type: syn::parse_quote!(u32) }, OhosErrorBinding::Infallible),
    ],
    OhosResourceHooks {
      release_object: Some(OhosResourceHook { call: syn::parse_quote!(crate::generated_fixture::release_object), carrier_type: syn::parse_quote!(u32) }),
      cancel_output_stream: Some(OhosResourceHook { call: syn::parse_quote!(crate::generated_fixture::cancel_output_stream), carrier_type: syn::parse_quote!(u32) }),
      release_output_stream: Some(OhosResourceHook { call: syn::parse_quote!(crate::generated_fixture::release_output_stream), carrier_type: syn::parse_quote!(u32) }),
    },
  ).expect("structured OHOS Rust bridge plan is valid")
}

fn value_method(
  id: u32,
  _asynchronous: bool,
  argument_name: &str,
  lower: &str,
  call: &str,
) -> OhosOperationPlan {
  OhosOperationPlan {
    operation_id: id,
    target: OhosOperationTarget::Native { call: syn::parse_str(&format!("crate::generated_fixture::{call}")).unwrap() },
    receiver: Some(OhosReceiverPlan {
      name: name(argument_name),
      binding: OhosArgumentBinding::LowerWith {
        carrier_type: syn::parse_quote!(napi_ohos::bindgen_prelude::Object<'static>),
        lower: syn::parse_str(&format!("crate::generated_fixture::{lower}")).unwrap(),
      },
    }),
    arguments: Vec::new(),
    return_binding: OhosReturnBinding::Direct { carrier_type: syn::parse_quote!(u32) },
    error_binding: OhosErrorBinding::Infallible,
  }
}

fn main() {
  napi_build_ohos::setup();
  println!("cargo:rerun-if-changed=build.rs");
  println!("cargo:rerun-if-changed=src/lib.rs");
  let family = family();
  let generated = generate_ohos_source(&family, operation_plans(&family)).expect("OHOS generated fixture plan must lower successfully");
  let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set")).join("generated_ohos_module.rs");
  fs::write(output, generated.source().to_string()).expect("write generated OHOS source");
}
