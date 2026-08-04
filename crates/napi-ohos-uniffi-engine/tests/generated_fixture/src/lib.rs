//! Executable consumer of the programmatic OHOS UniFFI engine.

#[allow(dead_code)]
mod generated_fixture {
  use std::ffi::CString;
  use std::ptr;
  use std::sync::atomic::{AtomicU32, Ordering};
  use std::sync::Arc;

  use napi_ohos::bindgen_prelude::{
    FnArgs, FromNapiValue, Function, JsObjectValue, JsValue, Object, Promise, ToNapiValue,
  };
  use napi_ohos::sys;
  use napi_ohos::{threadsafe_function::ThreadsafeFunction, Status};
  use napi_ohos_uniffi_engine::{
    BridgeErrorDescriptor, ErrorData, ErrorDomain, SessionCallbackArgument,
    SessionCallbackRetention,
    SessionCallbackThreading,
  };

  static RELEASE_COUNT: AtomicU32 = AtomicU32::new(0);

  pub fn sync_echo(value: u32) -> u32 {
    value
  }

  pub async fn async_echo(value: u32) -> u32 {
    value + 1
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

  pub struct SyncObserverProxy {
    host: Object<'static>,
    callback_type_id: u32,
    callback_id: u32,
  }

  impl SyncObserverProxy {
    fn call(&self, value: u32, method_id: u32) -> Result<u32, BridgeErrorDescriptor> {
      self.call_napi(value, method_id).map_err(|error| {
        BridgeErrorDescriptor::backend(format!("sync callback dispatch failed: {error}"))
      })
    }

    fn call_napi(&self, value: u32, method_id: u32) -> napi_ohos::Result<u32> {
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
    })
  }

  pub fn observe_sync(observer: SyncObserverProxy) -> Result<u32, BridgeErrorDescriptor> {
    let fallible = observer.call(5, 1)?;
    let infallible = observer.call(6, 3)?;
    Ok(fallible + infallible)
  }

  type AsyncCallbackArgs = FnArgs<(u32, u32, u32, u32, Vec<u32>)>;
  type AsyncCallbackTsfn =
    ThreadsafeFunction<AsyncCallbackArgs, Promise<u32>, AsyncCallbackArgs, Status, false>;

  pub struct AsyncObserverProxy {
    callback_type_id: u32,
    callback_id: u32,
    callback: Arc<AsyncCallbackTsfn>,
  }

  pub fn build_async_observer(
    host: &Object<'static>,
    callback_type_id: u32,
    callback_id: u32,
    contract: SessionCallbackArgument,
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
      )?
      .build_threadsafe_function()
      .build()?;
    Ok(AsyncObserverProxy {
      callback_type_id,
      callback_id,
      callback: Arc::new(callback),
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
          0,
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
  ) -> napi_ohos::Result<StreamFactoryProxy> {
    if contract.threading != SessionCallbackThreading::CallingThread
    {
      return Err(napi_ohos::Error::new(
        napi_ohos::Status::InvalidArg,
        "unexpected stream factory contract",
      ));
    }
    let raw_host = host.get_named_property::<Object<'static>>("__uniffiRawHost")?;
    Ok(StreamFactoryProxy(raw_host))
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

  pub fn release_object(_handle: u32) -> Result<(), BridgeErrorDescriptor> {
    RELEASE_COUNT.fetch_add(1, Ordering::AcqRel);
    Ok(())
  }

  pub async fn cancel_output_stream(_handle: u32) -> Result<(), BridgeErrorDescriptor> {
    RELEASE_COUNT.fetch_add(10, Ordering::AcqRel);
    Ok(())
  }

  pub fn release_output_stream(_handle: u32) -> Result<(), BridgeErrorDescriptor> {
    RELEASE_COUNT.fetch_add(100, Ordering::AcqRel);
    Ok(())
  }

  include!(concat!(env!("OUT_DIR"), "/generated_ohos_module.rs"));
}
