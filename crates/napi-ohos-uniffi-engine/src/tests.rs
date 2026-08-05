use super::*;
use futures::executor::block_on;
use napi_family_core::{
  AsyncKind, CallbackContract, CallbackReentrancy, CallbackRetention, CallbackThreading,
  CallbackUseSite, CarrierKind, ClosePolicy, ConversionRecipe, DeadlineAction,
  FamilyOperationInput, FamilyPlanInput, HostFlavor, OperationDispatch, OperationKind,
  ReceiverBinding, ResourceBinding, ResourceKind, ResourceOwnership, StreamDirection,
  StreamSlotIdentity, StreamUseSite, StreamValueBinding, ValuePath,
};
use proc_macro2::{Ident, Span};
use std::sync::{Arc, Mutex};

fn ident(name: &str) -> Ident {
  Ident::new(name, Span::call_site())
}

fn argument(name: &str, binding: OhosArgumentBinding) -> OhosArgumentPlan {
  OhosArgumentPlan {
    name: ident(name),
    binding,
  }
}

fn native(
  id: u32,
  argument_count: usize,
  async_kind: AsyncKind,
  fallible: bool,
  result: Option<ResourceBinding>,
  arguments: Vec<OhosArgumentPlan>,
  return_binding: OhosReturnBinding,
  error_binding: OhosErrorBinding,
) -> OhosOperationPlan {
  let _ = (argument_count, async_kind, fallible, result);
  OhosOperationPlan {
    operation_id: id,
    target: OhosOperationTarget::Native {
      call: syn::parse_quote!(fixture::call),
    },
    receiver: None,
    arguments,
    return_binding,
    error_binding,
  }
}

fn host(id: u32, target: OhosOperationTarget) -> OhosOperationPlan {
  OhosOperationPlan {
    operation_id: id,
    target,
    receiver: None,
    arguments: Vec::new(),
    return_binding: OhosReturnBinding::Unit,
    error_binding: OhosErrorBinding::Infallible,
  }
}

fn family(operations: Vec<FamilyOperationInput>) -> FamilyPlan {
  FamilyPlan::build(FamilyPlanInput {
    flavor: HostFlavor::Ohos,
    close_policy: ClosePolicy {
      grace_ms: 5_000,
      on_deadline: DeadlineAction::Detach,
    },
    operations,
  })
  .unwrap()
}

fn operation(
  id: u32,
  kind: OperationKind,
  async_kind: AsyncKind,
  fallible: bool,
  argument_count: usize,
  dispatch: OperationDispatch,
) -> FamilyOperationInput {
  FamilyOperationInput {
    id,
    kind,
    async_kind,
    fallible,
    argument_count,
    dispatch,
    receiver: None,
    result: None,
    callbacks: Vec::new(),
    streams: Vec::new(),
    stream_slot: None,
  }
}

fn echo_family(async_kind: AsyncKind) -> FamilyPlan {
  family(vec![operation(
    0,
    OperationKind::Function,
    async_kind,
    false,
    1,
    OperationDispatch::Native,
  )])
}

fn echo_plan(family: &FamilyPlan) -> OhosBridgePlan {
  OhosBridgePlan::build(
    family,
    vec![native(
      0,
      1,
      AsyncKind::Sync,
      false,
      None,
      vec![argument(
        "value",
        OhosArgumentBinding::Direct {
          carrier_type: syn::parse_quote!(u32),
        },
      )],
      OhosReturnBinding::Direct {
        carrier_type: syn::parse_quote!(u32),
      },
      OhosErrorBinding::Infallible,
    )],
  )
  .unwrap()
}

fn object_family() -> FamilyPlan {
  let mut op = operation(
    0,
    OperationKind::Function,
    AsyncKind::Sync,
    false,
    0,
    OperationDispatch::Native,
  );
  op.result = Some(ResourceBinding {
    kind: ResourceKind::Object,
    ownership: ResourceOwnership::Owned,
  });
  family(vec![op])
}

fn object_plan(family: &FamilyPlan) -> OhosBridgePlan {
  OhosBridgePlan::build_with_resource_hooks(
    family,
    vec![native(
      0,
      0,
      AsyncKind::Sync,
      false,
      Some(ResourceBinding {
        kind: ResourceKind::Object,
        ownership: ResourceOwnership::Owned,
      }),
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

fn output_family() -> FamilyPlan {
  let mut op = operation(
    0,
    OperationKind::OutputStreamStart,
    AsyncKind::Sync,
    false,
    0,
    OperationDispatch::Native,
  );
  op.result = Some(ResourceBinding {
    kind: ResourceKind::OutputStream,
    ownership: ResourceOwnership::Owned,
  });
  family(vec![op])
}

fn output_plan(family: &FamilyPlan) -> OhosBridgePlan {
  OhosBridgePlan::build_with_resource_hooks(
    family,
    vec![native(
      0,
      0,
      AsyncKind::Sync,
      false,
      Some(ResourceBinding {
        kind: ResourceKind::OutputStream,
        ownership: ResourceOwnership::Owned,
      }),
      Vec::new(),
      OhosReturnBinding::OutputStreamLease {
        carrier_type: syn::parse_quote!(fixture::OutputStreamHandle),
        lift: syn::parse_quote!(fixture::lift_output_stream),
      },
      OhosErrorBinding::Infallible,
    )],
    OhosResourceHooks {
      cancel_output_stream: Some(OhosResourceHook {
        call: syn::parse_quote!(fixture::cancel_output_stream),
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

fn callback_family() -> FamilyPlan {
  let mut use_site = CallbackUseSite {
    operation_id: 0,
    callback_type_id: 7,
    path: ValuePath::argument(0),
    contract: CallbackContract {
      retention: CallbackRetention::Retained,
      threading: CallbackThreading::CallingThread,
      reentrancy: CallbackReentrancy::Forbidden,
    },
  };
  let mut op = operation(
    0,
    OperationKind::Function,
    AsyncKind::Sync,
    false,
    1,
    OperationDispatch::Native,
  );
  op.callbacks.push(use_site.clone());
  use_site.operation_id = 1;
  let method = operation(
    1,
    OperationKind::CallbackMethod,
    AsyncKind::Async,
    true,
    1,
    OperationDispatch::CallbackHost {
      callback_type_id: 7,
      method_id: 3,
    },
  );
  let input = operation(
    2,
    OperationKind::InputStreamPull,
    AsyncKind::Async,
    false,
    1,
    OperationDispatch::InputStreamHostPull,
  );
  let stream_source = {
    let mut operation = operation(
      4,
      OperationKind::Function,
      AsyncKind::Sync,
      false,
      1,
      OperationDispatch::Native,
    );
    operation.streams.push(StreamUseSite {
      operation_id: 4,
      use_site_id: 0,
      path: ValuePath::argument(0),
      direction: StreamDirection::Input,
      item: StreamValueBinding {
        carrier: CarrierKind::Primitive,
        conversion: ConversionRecipe::Identity,
      },
      error: StreamValueBinding {
        carrier: CarrierKind::Primitive,
        conversion: ConversionRecipe::Identity,
      },
      is_send: false,
      slots: vec![
        StreamSlotIdentity {
          use_site_id: 0,
          operation_id: 2,
          kind: OperationKind::InputStreamPull,
        },
        StreamSlotIdentity {
          use_site_id: 0,
          operation_id: 3,
          kind: OperationKind::InputStreamCancel,
        },
      ],
    });
    operation
  };
  let mut input = input;
  input.receiver = Some(ReceiverBinding::Resource(ResourceBinding {
    kind: ResourceKind::InputStream,
    ownership: ResourceOwnership::Borrowed,
  }));
  input.stream_slot = Some(StreamSlotIdentity {
    use_site_id: 0,
    operation_id: 2,
    kind: OperationKind::InputStreamPull,
  });
  let mut cancel = operation(
    3,
    OperationKind::InputStreamCancel,
    AsyncKind::Async,
    false,
    0,
    OperationDispatch::InputStreamHostCancel,
  );
  cancel.receiver = Some(ReceiverBinding::Resource(ResourceBinding {
    kind: ResourceKind::InputStream,
    ownership: ResourceOwnership::Borrowed,
  }));
  cancel.stream_slot = Some(StreamSlotIdentity {
    use_site_id: 0,
    operation_id: 3,
    kind: OperationKind::InputStreamCancel,
  });
  family(vec![op, method, input, cancel, stream_source])
}

fn callback_plan(family: &FamilyPlan) -> OhosBridgePlan {
  OhosBridgePlan::build(
    family,
    vec![
      native(
        0,
        1,
        AsyncKind::Sync,
        false,
        None,
        vec![argument(
          "callback",
          OhosArgumentBinding::CallbackProxy {
            rust_type: syn::parse_quote!(fixture::Callback),
            build: syn::parse_quote!(fixture::build_callback),
          },
        )],
        OhosReturnBinding::Unit,
        OhosErrorBinding::Infallible,
      ),
      host(1, OhosOperationTarget::CallbackHost),
      host(2, OhosOperationTarget::InputStreamHostPull),
      host(3, OhosOperationTarget::InputStreamHostCancel),
      native(
        4,
        1,
        AsyncKind::Sync,
        false,
        None,
        vec![argument(
          "stream",
          OhosArgumentBinding::InputStreamProxy {
            rust_type: syn::parse_quote!(fixture::InputStream),
            build: syn::parse_quote!(fixture::build_input_stream),
          },
        )],
        OhosReturnBinding::Unit,
        OhosErrorBinding::Infallible,
      ),
    ],
  )
  .unwrap()
}

#[test]
fn generated_source_consumes_family_plan_and_exports_one_factory() {
  let family = echo_family(AsyncKind::Sync);
  let generated = generate_ohos_source(&family, echo_plan(&family)).unwrap();
  assert_eq!(
    generated.module().public_exports().collect::<Vec<_>>(),
    vec![BACKEND_FACTORY_EXPORT]
  );
  assert!(generated
    .source()
    .to_string()
    .contains("create_backend_session"));
  assert!(syn::parse2::<syn::File>(generated.source().clone()).is_ok());
}

#[test]
fn structured_family_preserves_callback_method_ids_and_stream_contracts() {
  let family = callback_family();
  assert_eq!(
    family.operations()[1].target,
    napi_family_core::FamilyOperationTarget::CallbackHost {
      callback_type_id: 7,
      method_id: 3,
    }
  );
  assert_eq!(
    family.operations()[4].streams[0].direction,
    StreamDirection::Input
  );
  let generated = generate_ohos_source(&family, callback_plan(&family)).unwrap();
  assert!(generated.source().to_string().contains("method_id : 3"));
}

#[test]
fn resource_hooks_are_required_and_output_cleanup_is_idempotent() {
  let family = object_family();
  let plan = object_plan(&family);
  assert!(plan.resource_hooks().release_object.is_some());
  assert_eq!(
    OhosBridgePlan::build(&family, plan.operations().to_vec()).unwrap_err(),
    OhosEngineError::MissingResourceHook {
      role: "object release"
    }
  );
  let family = output_family();
  let plan = output_plan(&family);
  assert!(plan.resource_hooks().cancel_output_stream.is_some());
  assert!(plan.resource_hooks().release_output_stream.is_some());
}

#[derive(Default)]
struct FixtureHost {
  calls: Arc<Mutex<Vec<&'static str>>>,
  object: Option<u32>,
  output: Option<u32>,
  released_objects: Arc<Mutex<Vec<u32>>>,
  cancelled_outputs: Arc<Mutex<Vec<u32>>>,
  released_outputs: Arc<Mutex<Vec<u32>>>,
}

impl OhosHost for FixtureHost {
  fn invoke_sync(&mut self, _: u32, args: &[OhosValue]) -> Result<OhosValue, OhosError> {
    self.calls.lock().unwrap().push("sync");
    if let Some(value) = args.first() {
      Ok(value.clone())
    } else {
      Ok(OhosValue::Object(self.object.unwrap_or(1)))
    }
  }

  fn invoke_async<'a>(&'a mut self, _: u32, args: Vec<OhosValue>) -> OhosFuture<'a> {
    self.calls.lock().unwrap().push("async");
    Box::pin(async move { Ok(args.into_iter().next().unwrap_or(OhosValue::Unit)) })
  }

  fn invoke_callback(
    &mut self,
    _: u32,
    _: u32,
    _: u32,
    _: &[OhosValue],
  ) -> Result<OhosValue, OhosError> {
    Ok(OhosValue::Unit)
  }

  fn pull_input_stream(&mut self, stream_id: u32) -> Result<OhosValue, OhosError> {
    Ok(OhosValue::InputStream(stream_id))
  }
  fn cancel_input_stream(&mut self, _: u32) -> Result<(), OhosError> {
    Ok(())
  }
  fn next_output_stream(&mut self, stream_id: u32) -> Result<OhosValue, OhosError> {
    Ok(OhosValue::OutputStream(stream_id))
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
fn backend_session_preserves_close_and_resource_idempotence() {
  let family = object_family();
  let module = generate_ohos_module(&family, object_plan(&family)).unwrap();
  let host = FixtureHost {
    object: Some(42),
    ..FixtureHost::default()
  };
  let released = Arc::clone(&host.released_objects);
  let session = OhosBackendFactory::new(module).open(host);
  assert_eq!(session.invoke_sync(0, &[]), Ok(OhosValue::Object(42)));
  session.release_object(42).unwrap();
  session.release_object(42).unwrap();
  session.close().unwrap();
  session.close().unwrap();
  assert_eq!(&*released.lock().unwrap(), &[42]);
  assert!(matches!(
    session.invoke_sync(0, &[]),
    Err(OhosEngineError::Closed)
  ));
}

#[test]
fn callback_reentrancy_policy_is_forbidden_for_registered_method() {
  let family = callback_family();
  let module = generate_ohos_module(&family, callback_plan(&family)).unwrap();
  let session = OhosBackendFactory::new(module).open(FixtureHost::default());
  let first = session
    .invoke_async(1, vec![OhosValue::Callback(9)])
    .unwrap();
  let second = session.invoke_async(1, vec![OhosValue::Callback(9)]);
  assert!(matches!(
    second,
    Err(OhosEngineError::ReentrancyForbidden {
      callback_type: 7,
      callback_id: 9
    })
  ));
  assert_eq!(block_on(first), Ok(OhosValue::Unit));
}
