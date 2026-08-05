//! Executable consumer of the programmatic OHOS UniFFI engine.

#[allow(dead_code)]
mod generated_fixture {
use std::cell::RefCell;
use std::ffi::CString;
use std::future::poll_fn;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Poll, Waker};

  use napi_ohos::bindgen_prelude::{
    Env, FnArgs, FromNapiValue, Function, JsObjectValue, JsValue, Object, ObjectRef, Promise,
    ToNapiValue, TypeName, ValidateNapiValue, ValueType,
  };
  use napi_ohos::sys;
  use napi_ohos::{threadsafe_function::ThreadsafeFunction, Status};
  use napi_ohos_uniffi_engine::{
    BridgeErrorDescriptor, ErrorData, ErrorDomain, SessionCallbackArgument, SessionCallbackInvoker,
    SessionCallbackLease, SessionCallbackRetention, SessionCallbackThreading,
  };

  static RELEASE_COUNT: AtomicU32 = AtomicU32::new(0);
  static NATIVE_NEVER_READY: AtomicBool = AtomicBool::new(false);
  static NATIVE_NEVER_WAKER: OnceLock<Mutex<Option<Waker>>> = OnceLock::new();
  static OUTPUT_CANCEL_READY: AtomicBool = AtomicBool::new(false);
  static OUTPUT_CANCEL_WAKER: OnceLock<Mutex<Option<Waker>>> = OnceLock::new();
  static NESTED_LATE_READY: AtomicBool = AtomicBool::new(false);
  static NESTED_LATE_WAKER: OnceLock<Mutex<Option<Waker>>> = OnceLock::new();

  fn native_never_waker() -> &'static Mutex<Option<Waker>> {
    NATIVE_NEVER_WAKER.get_or_init(|| Mutex::new(None))
  }

  fn output_cancel_waker() -> &'static Mutex<Option<Waker>> {
    OUTPUT_CANCEL_WAKER.get_or_init(|| Mutex::new(None))
  }

  fn nested_late_waker() -> &'static Mutex<Option<Waker>> {
    NESTED_LATE_WAKER.get_or_init(|| Mutex::new(None))
  }

  thread_local! {
    // Keep one native callback proxy alive across an operation boundary so
    // the fixture proves retained leases follow proxy ownership rather than
    // being unconditionally released by the session at return.
    static HELD_SYNC_OBSERVERS: RefCell<Vec<SyncObserverProxy>> = const { RefCell::new(Vec::new()) };
  }

  pub fn sync_echo(value: u32) -> u32 {
    value
  }

  pub async fn async_echo(value: u32) -> u32 {
    value + 1
  }

  pub async fn never_settle_native() -> u32 {
    poll_fn(|context| {
      if NATIVE_NEVER_READY.load(Ordering::Acquire) {
        Poll::Ready(0)
      } else {
        native_never_waker()
          .lock()
          .expect("native never-settle waker mutex poisoned")
          .replace(context.waker().clone());
        Poll::Pending
      }
    })
    .await
  }

  pub fn wake_never_settle_native() -> u32 {
    NATIVE_NEVER_READY.store(true, Ordering::Release);
    if let Some(waker) = native_never_waker()
      .lock()
      .expect("native never-settle waker mutex poisoned")
      .take()
    {
      waker.wake();
    }
    0
  }

  pub fn echo_i64(value: i64) -> i64 {
    value
  }

  pub fn echo_u64(value: u64) -> u64 {
    value
  }

  #[derive(Debug)]
  pub struct FixtureFailure(u32);

  pub fn fail(value: u32) -> Result<u32, FixtureFailure> {
    Err(FixtureFailure(value))
  }

  pub fn map_failure(error: FixtureFailure) -> BridgeErrorDescriptor {
    BridgeErrorDescriptor {
      domain: ErrorDomain::Declared,
      error_name: "Failure".to_owned(),
      variant: Some("Rejected".to_owned()),
      data: ErrorData::Number(f64::from(error.0)),
      message: format!("fixture rejected {}", error.0),
      native_stack: None,
    }
  }

  pub fn identity_error(error: BridgeErrorDescriptor) -> BridgeErrorDescriptor {
    error
  }

  pub fn lower_thing(value: Object<'static>) -> Result<Object<'static>, BridgeErrorDescriptor> {
    Ok(value)
  }

  pub fn roundtrip_thing(value: Object<'static>) -> Object<'static> {
    value
  }

  pub fn lift_thing(value: Object<'static>) -> Result<Object<'static>, BridgeErrorDescriptor> {
    Ok(value)
  }

  #[derive(Clone, Copy)]
  pub struct ValueRecord {
    pub handle: u32,
  }

  pub fn lower_value_record(value: Object<'static>) -> Result<ValueRecord, BridgeErrorDescriptor> {
    value
      .get_named_property::<u32>("handle")
      .map(|handle| ValueRecord { handle })
      .map_err(|error| BridgeErrorDescriptor::validation(error.to_string()))
  }

  pub fn value_record_sync(value: ValueRecord) -> u32 {
    value.handle
  }

  pub async fn value_record_async(value: ValueRecord) -> u32 {
    value.handle + 1
  }

  #[derive(Clone, Copy)]
  pub struct ValueEnum {
    pub handle: u32,
  }

  pub struct SendObjectRef(ObjectRef<false>);

  unsafe impl Send for SendObjectRef {}

  impl TypeName for SendObjectRef {
    fn type_name() -> &'static str {
      "Object"
    }

    fn value_type() -> ValueType {
      ValueType::Object
    }
  }

  impl ValidateNapiValue for SendObjectRef {}

  impl FromNapiValue for SendObjectRef {
    unsafe fn from_napi_value(
      env: napi_ohos::sys::napi_env,
      value: napi_ohos::sys::napi_value,
    ) -> napi_ohos::Result<Self> {
      Ok(Self(ObjectRef::<false>::from_napi_value(env, value)?))
    }
  }

  impl ToNapiValue for SendObjectRef {
    unsafe fn to_napi_value(
      env: napi_ohos::sys::napi_env,
      value: Self,
    ) -> napi_ohos::Result<napi_ohos::sys::napi_value> {
      ObjectRef::<false>::to_napi_value(env, value.0)
    }
  }

  pub fn lower_value_enum(value: Object<'static>) -> Result<ValueEnum, BridgeErrorDescriptor> {
    let tag = value
      .get_named_property::<String>("tag")
      .map_err(|error| BridgeErrorDescriptor::validation(error.to_string()))?;
    if tag != "ready" {
      return Err(BridgeErrorDescriptor::validation("unexpected enum tag"));
    }
    value
      .get_named_property::<u32>("handle")
      .map(|handle| ValueEnum { handle })
      .map_err(|error| BridgeErrorDescriptor::validation(error.to_string()))
  }

  pub fn value_enum_sync(value: ValueEnum) -> u32 {
    value.handle
  }

  pub async fn value_enum_async(value: ValueEnum) -> u32 {
    value.handle + 1
  }

  pub struct SyncObserverProxy {
    host: Object<'static>,
    callback_type_id: u32,
    callback_id: u32,
    invoker: SessionCallbackInvoker,
    lease: SessionCallbackLease,
  }

  impl SyncObserverProxy {
    fn call(&self, value: u32, method_id: u32) -> Result<u32, BridgeErrorDescriptor> {
      self.call_napi(value, method_id).map_err(|error| {
        BridgeErrorDescriptor::backend(format!("sync callback dispatch failed: {error}"))
      })
    }

    fn call_napi(&self, value: u32, method_id: u32) -> napi_ohos::Result<u32> {
      self.invoker.check_open()?;
      let env = self.host.value().env;
      let name = CString::new("invokeCallbackSync")?;
      let mut function = ptr::null_mut();
      napi_ohos::check_status!(unsafe {
        sys::napi_get_named_property(env, self.host.raw(), name.as_ptr(), &mut function)
      })?;
      let callback_type = unsafe { u32::to_napi_value(env, self.callback_type_id)? };
      let callback_id = unsafe { u32::to_napi_value(env, self.callback_id)? };
      let method_id = unsafe { u32::to_napi_value(env, method_id)? };
      let args = unsafe { Vec::<u32>::to_napi_value(env, vec![value])? };
      let mut result = ptr::null_mut();
      napi_ohos::check_status!(unsafe {
        sys::napi_call_function(
          env,
          self.host.raw(),
          function,
          4,
          [callback_type, callback_id, method_id, args].as_ptr(),
          &mut result,
        )
      })?;
      unsafe { u32::from_napi_value(env, result) }
    }
  }

  pub fn build_sync_observer(
    host: &Object<'static>,
    callback_type_id: u32,
    callback_id: u32,
    contract: SessionCallbackArgument,
    invoker: SessionCallbackInvoker,
    lease: SessionCallbackLease,
  ) -> napi_ohos::Result<SyncObserverProxy> {
    if contract.retention != SessionCallbackRetention::Retained
      || contract.threading != SessionCallbackThreading::CallingThread
    {
      return Err(napi_ohos::Error::new(
        napi_ohos::Status::InvalidArg,
        "unexpected sync callback contract",
      ));
    }
    Ok(SyncObserverProxy {
      host: *host,
      callback_type_id,
      callback_id,
      invoker,
      lease,
    })
  }

  pub fn observe_sync(observer: SyncObserverProxy) -> Result<u32, BridgeErrorDescriptor> {
    let fallible = observer.call(5, 1)?;
    let infallible = observer.call(6, 3)?;
    Ok(fallible + infallible)
  }

  pub fn hold_sync_observer(observer: SyncObserverProxy) -> u32 {
    HELD_SYNC_OBSERVERS.with(|held| held.borrow_mut().push(observer));
    0
  }

  pub fn drop_held_sync_observers() -> u32 {
    HELD_SYNC_OBSERVERS.with(|held| held.borrow_mut().clear());
    0
  }

  type AsyncCallbackArgs = FnArgs<(u32, u32, u32, u32, Vec<u32>)>;
  type AsyncCallbackTsfn =
    ThreadsafeFunction<AsyncCallbackArgs, Promise<u32>, AsyncCallbackArgs, Status, false>;

  pub struct AsyncObserverProxy {
    callback_type_id: u32,
    callback_id: u32,
    callback: Arc<AsyncCallbackTsfn>,
    invoker: SessionCallbackInvoker,
    lease: SessionCallbackLease,
  }

  pub fn build_async_observer(
    host: &Object<'static>,
    callback_type_id: u32,
    callback_id: u32,
    contract: SessionCallbackArgument,
    invoker: SessionCallbackInvoker,
    lease: SessionCallbackLease,
  ) -> napi_ohos::Result<AsyncObserverProxy> {
    if contract.retention != SessionCallbackRetention::Retained
      || contract.threading != SessionCallbackThreading::CallingThread
    {
      return Err(napi_ohos::Error::new(
        napi_ohos::Status::InvalidArg,
        "unexpected async callback contract",
      ));
    }

    // Bind the Host callback while the generated pre-call wrapper is still on
    // the JavaScript thread.  The resulting TSFN owns the N-API reference and
    // can safely be moved into the Tokio future below.  The callback uses the
    // standard N-API queue; OHOS-only priority dispatch is intentionally not
    // required for a cross-thread callback contract.
    let callback = host
      .get_named_property::<Function<'static, AsyncCallbackArgs, Promise<u32>>>(
        "invokeCallbackAsync",
      )
      ?
      .build_threadsafe_function()
      .build()?;
    Ok(AsyncObserverProxy {
      callback_type_id,
      callback_id,
      callback: Arc::new(callback),
      invoker,
      lease,
    })
  }

  pub async fn observe_async(observer: AsyncObserverProxy) -> u32 {
    let fallible = observer
      .call(5, 0)
      .await
      .expect("Host.invokeCallbackAsync fallible dispatch/rejection");
    let infallible = observer
      .call(6, 2)
      .await
      .expect("Host.invokeCallbackAsync infallible dispatch/rejection");
    fallible + infallible
  }

  impl AsyncObserverProxy {
    async fn call(&self, value: u32, method_id: u32) -> Result<u32, napi_ohos::Error> {
      let invocation = self
        .callback
        .call_async(AsyncCallbackArgs::from((
          self.callback_type_id,
          self.callback_id,
          method_id,
          self.invoker
            .next_invocation_id()
            .map_err(|error| napi_ohos::Error::new(Status::GenericFailure, error.to_string()))?,
          vec![value],
        )))
        .await
        .map_err(|error| napi_ohos::Error::new(Status::GenericFailure, error.to_string()))?;
      invocation
        .await
        .map_err(|error| napi_ohos::Error::new(Status::GenericFailure, error.to_string()))
    }
  }

  pub struct StreamFactoryProxy(Object<'static>);

  pub fn build_stream_factory(
    host: &Object<'static>,
    _callback_type_id: u32,
    _callback_id: u32,
    contract: SessionCallbackArgument,
    _invoker: SessionCallbackInvoker,
  ) -> napi_ohos::Result<StreamFactoryProxy> {
    if contract.threading != SessionCallbackThreading::CallingThread
    {
      return Err(napi_ohos::Error::new(
        napi_ohos::Status::InvalidArg,
        "unexpected stream factory contract",
      ));
    }
    Ok(StreamFactoryProxy(*host))
  }

  pub fn open_stream(proxy: StreamFactoryProxy) -> Object<'static> {
    proxy.0
  }

  pub fn lift_stream(value: Object<'static>) -> Result<Object<'static>, BridgeErrorDescriptor> {
    Ok(value)
  }

  pub fn lower_handle(value: u32) -> Result<u32, BridgeErrorDescriptor> {
    Ok(value)
  }

  pub async fn next_stream(handle: u32) -> Option<u32> {
    Some(handle + 1)
  }

  pub fn lift_optional_u32(value: Option<u32>) -> Result<Option<u32>, BridgeErrorDescriptor> {
    Ok(value)
  }

  pub async fn cancel_stream(_handle: u32) {}

  #[derive(Clone, Copy)]
  pub struct InputProxy {
    stream_id: u32,
  }

  pub fn build_input_proxy(
    _host: &Object<'static>,
    stream_id: u32,
  ) -> napi_ohos::Result<InputProxy> {
    Ok(InputProxy { stream_id })
  }

  pub async fn consume_input(source: InputProxy) -> u32 {
    source.stream_id + 1
  }

  pub fn release_count() -> u32 {
    RELEASE_COUNT.load(Ordering::Acquire)
  }

  fn nested_outer(env: &Env, optional: Object<'static>, sequence: Vec<Object<'static>>, variant: Object<'static>, optional_name: &str, sequence_name: &str) -> Object<'static> {
    let mut outer = Object::new(env).expect("create nested result");
    outer.set_named_property(optional_name, optional).expect("set nested optional");
    outer.set_named_property(sequence_name, sequence).expect("set nested sequence");
    outer.set_named_property("variant", variant).expect("set nested variant");
    outer
  }

  pub fn nested_object(value: Object<'static>) -> Object<'static> {
    let env = Env::from(value.value().env);
    let mut variant = Object::new(&env).expect("create nested object variant");
    variant.set_named_property("tag", "Ready").expect("set nested object tag");
    variant.set_named_property("object", value).expect("set nested object variant value");
    nested_outer(&env, value, vec![value], variant, "optionalObject", "objects")
  }

  pub fn nested_output(value: Object<'static>) -> Object<'static> {
    let env = Env::from(value.value().env);
    let mut variant = Object::new(&env).expect("create nested output variant");
    variant.set_named_property("tag", "Ready").expect("set nested output tag");
    variant.set_named_property("output", value).expect("set nested output variant value");
    nested_outer(&env, value, vec![value], variant, "optionalOutput", "outputs")
  }

  pub fn nested_input(value: Object<'static>) -> Object<'static> {
    let env = Env::from(value.value().env);
    let mut variant = Object::new(&env).expect("create nested input variant");
    variant.set_named_property("tag", "Ready").expect("set nested input tag");
    variant.set_named_property("input", value).expect("set nested input variant value");
    let mut outer = Object::new(&env).expect("create nested input result");
    outer.set_named_property("optionalInput", value).expect("set nested input optional");
    outer.set_named_property("inputs", vec![value]).expect("set nested input sequence");
    outer.set_named_property("variant", variant).expect("set nested input variant");
    outer
  }

  pub async fn nested_late_object(value: SendObjectRef) -> SendObjectRef {
    poll_fn(|context| {
      if NESTED_LATE_READY.load(Ordering::Acquire) {
        Poll::Ready(())
      } else {
        nested_late_waker()
          .lock()
          .expect("nested late waker mutex poisoned")
          .replace(context.waker().clone());
        Poll::Pending
      }
    })
    .await;
    value
  }

  pub fn wake_nested_late() -> u32 {
    NESTED_LATE_READY.store(true, Ordering::Release);
    if let Some(waker) = nested_late_waker()
      .lock()
      .expect("nested late waker mutex poisoned")
      .take()
    {
      waker.wake();
    }
    0
  }

  pub fn release_object(_handle: u32) -> Result<(), BridgeErrorDescriptor> {
    RELEASE_COUNT.fetch_add(1, Ordering::AcqRel);
    Ok(())
  }

  pub async fn cancel_output_stream(_handle: u32) -> Result<(), BridgeErrorDescriptor> {
    if _handle == 777 {
      poll_fn(|context| {
        if OUTPUT_CANCEL_READY.load(Ordering::Acquire) {
          Poll::Ready(())
        } else {
          output_cancel_waker()
            .lock()
            .expect("output cancel waker mutex poisoned")
            .replace(context.waker().clone());
          Poll::Pending
        }
      })
      .await;
    }
    RELEASE_COUNT.fetch_add(10, Ordering::AcqRel);
    Ok(())
  }

  pub fn release_output_stream(_handle: u32) -> Result<(), BridgeErrorDescriptor> {
    RELEASE_COUNT.fetch_add(100, Ordering::AcqRel);
    Ok(())
  }

  pub fn wake_output_cancel() -> u32 {
    OUTPUT_CANCEL_READY.store(true, Ordering::Release);
    if let Some(waker) = output_cancel_waker()
      .lock()
      .expect("output cancel waker mutex poisoned")
      .take()
    {
      waker.wake();
    }
    0
  }

  include!(concat!(env!("OUT_DIR"), "/generated_ohos_module.rs"));
}
