//! Ark N-API implementation of the private UniFFI `BackendSession` boundary.
//!
//! The generated factory supplies a dense operation descriptor table.  This
//! module keeps the Host and native operation callbacks alive with N-API
//! references and exposes only the session methods required by the generated
//! facade.  Raw operation functions never become module exports.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{c_void, CString};
use std::ptr;
use std::rc::Rc;

use napi_ohos::bindgen_prelude::{check_status, JsValue, Object, PromiseRaw, Unknown};
use napi_ohos::{sys, Env, Error, Result, Status};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionCallbackRetention {
  Scoped,
  Retained,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionCallbackThreading {
  CallingThread,
  MayCrossThread,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionCallbackCallStyle {
  Sync,
  Async,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionCallbackErrorStyle {
  Infallible,
  Fallible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionCallbackReentrancy {
  Allowed,
  Forbidden,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionCallbackArgument {
  pub argument_index: u32,
  pub callback_type_id: u32,
  pub retention: SessionCallbackRetention,
  pub threading: SessionCallbackThreading,
  pub call_style: SessionCallbackCallStyle,
  pub error_style: SessionCallbackErrorStyle,
  pub reentrancy: SessionCallbackReentrancy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionStreamDirection {
  Input,
  Output,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionStreamArgument {
  pub argument_index: u32,
  pub direction: SessionStreamDirection,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionResourceReceiver {
  Object,
  OutputStream,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionOperationDispatch {
  NativeSync,
  NativeAsync,
  CallbackHostSync {
    callback_type_id: u32,
    method_id: u32,
  },
  CallbackHostAsync {
    callback_type_id: u32,
    method_id: u32,
  },
  InputStreamHostPull,
  InputStreamHostCancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionNativeCall {
  ArgumentsOnly,
  HostAndArguments,
}

/// One dense slot emitted by the OHOS frontend.  `callback` is only present
/// for native operations and is converted into a strong N-API reference by
/// [`create_backend_session`].
pub struct SessionOperationDescriptor {
  pub dispatch: SessionOperationDispatch,
  pub callback: Option<sys::napi_value>,
  pub native_call: SessionNativeCall,
  pub receiver: Option<SessionResourceReceiver>,
  pub result_receiver: Option<SessionResourceReceiver>,
  pub callback_arguments: Vec<SessionCallbackArgument>,
  pub stream_arguments: Vec<SessionStreamArgument>,
}

pub struct SessionResourceCallbacks {
  pub release_object: Option<sys::napi_value>,
  pub cancel_output_stream: Option<sys::napi_value>,
  pub release_output_stream: Option<sys::napi_value>,
}

struct ResourceCallbacks {
  release_object: Cell<sys::napi_ref>,
  cancel_output_stream: Cell<sys::napi_ref>,
  release_output_stream: Cell<sys::napi_ref>,
}

/// Resolve a callback contract from one operation's use-site table.  The
/// operation table is intentionally passed by the caller; looking through all
/// operations would make the first use-site win for shared callback types.
pub(crate) fn callback_reentrancy_for_operation(
  callback_arguments: &[SessionCallbackArgument],
  callback_type_id: u32,
) -> SessionCallbackReentrancy {
  callback_arguments
    .iter()
    .find(|argument| argument.callback_type_id == callback_type_id)
    .map(|argument| argument.reentrancy)
    .unwrap_or(SessionCallbackReentrancy::Allowed)
}

#[cfg(test)]
pub(crate) fn callback_reentrancy_for_registry(
  registry: &BTreeMap<CallbackKey, SessionCallbackArgument>,
  callback_type_id: u32,
  callback_id: u32,
) -> SessionCallbackReentrancy {
  registry
    .get(&CallbackKey {
      callback_type_id,
      callback_id,
    })
    .map(|argument| argument.reentrancy)
    .unwrap_or(SessionCallbackReentrancy::Allowed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct CallbackKey {
  pub(crate) callback_type_id: u32,
  pub(crate) callback_id: u32,
}

struct TrackedResource {
  reference: sys::napi_ref,
  kind: SessionResourceReceiver,
  cancelled: Cell<bool>,
  cancel_promise: Cell<sys::napi_ref>,
}

#[derive(Clone, Copy)]
struct CallbackRegistrationToken {
  key: CallbackKey,
  token: u64,
}

#[derive(Clone, Copy)]
struct RegisteredCallbackContract {
  token: Option<u64>,
  contract: SessionCallbackArgument,
}

struct SessionOperation {
  dispatch: SessionOperationDispatch,
  callback: Cell<sys::napi_ref>,
  native_call: SessionNativeCall,
  receiver: Option<SessionResourceReceiver>,
  result_receiver: Option<SessionResourceReceiver>,
  callback_arguments: Vec<SessionCallbackArgument>,
  stream_arguments: Vec<SessionStreamArgument>,
}

struct SessionState {
  env: sys::napi_env,
  host: Cell<sys::napi_ref>,
  proxy_host: Cell<sys::napi_ref>,
  session_weak: Cell<sys::napi_ref>,
  operations: Vec<SessionOperation>,
  closed: Cell<bool>,
  next_invocation_id: Cell<u32>,
  next_callback_registration_id: Cell<u64>,
  next_pending_operation_id: Cell<u64>,
  retained_callbacks: RefCell<BTreeSet<CallbackKey>>,
  callback_contracts: RefCell<BTreeMap<CallbackKey, Vec<RegisteredCallbackContract>>>,
  active_callbacks: RefCell<BTreeSet<CallbackKey>>,
  input_streams: RefCell<BTreeSet<u32>>,
  input_stream_cancels: RefCell<BTreeMap<u32, sys::napi_ref>>,
  pending_operations: RefCell<BTreeMap<u64, sys::napi_ref>>,
  resources: RefCell<Vec<TrackedResource>>,
  close_promise: Cell<sys::napi_ref>,
  resource_callbacks: ResourceCallbacks,
}

struct SessionKeepalive {
  env: sys::napi_env,
  reference: Cell<sys::napi_ref>,
}

impl SessionKeepalive {
  fn finish(&self) {
    delete_reference(self.env, self.reference.replace(ptr::null_mut()));
  }
}

impl Drop for SessionKeepalive {
  fn drop(&mut self) {
    self.finish();
  }
}

struct AsyncDispatchLease {
  state: *const SessionState,
  session_reference: Cell<sys::napi_ref>,
  pending_id: Cell<Option<u64>>,
  scoped_callbacks: RefCell<Option<Vec<CallbackRegistrationToken>>>,
  finished: Cell<bool>,
}

impl AsyncDispatchLease {
  fn finish(&self) {
    if self.finished.replace(true) {
      return;
    }
    let state = unsafe { &*self.state };
    if let Some(callbacks) = self.scoped_callbacks.borrow_mut().take() {
      state.release_scoped_callback_tokens(&callbacks);
    }
    if let Some(pending_id) = self.pending_id.take() {
      state.finish_pending_operation(pending_id);
    }
    delete_reference(state.env, self.session_reference.replace(ptr::null_mut()));
  }
}

impl Drop for AsyncDispatchLease {
  fn drop(&mut self) {
    self.finish();
  }
}

struct CallbackGuardLease {
  state: *const SessionState,
  session_reference: Cell<sys::napi_ref>,
  key: CallbackKey,
  finished: Cell<bool>,
}

impl CallbackGuardLease {
  fn finish(&self) {
    if self.finished.replace(true) {
      return;
    }
    let state = unsafe { &*self.state };
    release_callback_guard(&state.active_callbacks, self.key);
    delete_reference(state.env, self.session_reference.replace(ptr::null_mut()));
  }
}

impl Drop for CallbackGuardLease {
  fn drop(&mut self) {
    self.finish();
  }
}

struct OutputCancelLease {
  state: *const SessionState,
  session_reference: Cell<sys::napi_ref>,
  resource_reference: sys::napi_ref,
  finished: Cell<bool>,
}

impl OutputCancelLease {
  fn finish(&self) {
    if self.finished.replace(true) {
      return;
    }
    let state = unsafe { &*self.state };
    state.finish_output_resource(self.resource_reference);
    delete_reference(state.env, self.session_reference.replace(ptr::null_mut()));
  }
}

impl Drop for OutputCancelLease {
  fn drop(&mut self) {
    self.finish();
  }
}

struct InputCancelLease {
  state: *const SessionState,
  session_reference: Cell<sys::napi_ref>,
  stream_id: u32,
  finished: Cell<bool>,
}

impl InputCancelLease {
  fn finish(&self) {
    if self.finished.replace(true) {
      return;
    }
    let state = unsafe { &*self.state };
    state.finish_input_stream(self.stream_id);
    delete_reference(state.env, self.session_reference.replace(ptr::null_mut()));
  }
}

impl Drop for InputCancelLease {
  fn drop(&mut self) {
    self.finish();
  }
}

impl SessionState {
  fn ensure_open(&self) -> Result<()> {
    if self.closed.get() {
      Err(Error::new(
        Status::GenericFailure,
        "UniFFI backend session is closed",
      ))
    } else {
      Ok(())
    }
  }

  fn host_value(&self) -> Result<sys::napi_value> {
    reference_value(self.env, self.host.get(), "Host")
  }

  fn session_value(&self) -> Result<sys::napi_value> {
    let value = reference_value(self.env, self.session_weak.get(), "backend session")?;
    if value.is_null() {
      Err(Error::new(
        Status::GenericFailure,
        "backend session is no longer alive",
      ))
    } else {
      Ok(value)
    }
  }

  fn operation(&self, id: u32) -> Result<&SessionOperation> {
    self.operations.get(id as usize).ok_or_else(|| {
      Error::new(
        Status::InvalidArg,
        format!("unknown UniFFI operation ID {id}"),
      )
    })
  }

  fn retain_inputs_and_receiver(
    &self,
    operation: &SessionOperation,
    args: &[sys::napi_value],
  ) -> Result<Vec<CallbackRegistrationToken>> {
    let receiver_offset = usize::from(operation.receiver.is_some());
    let mut callbacks = Vec::with_capacity(operation.callback_arguments.len());
    for callback in &operation.callback_arguments {
      let index = receiver_offset + callback.argument_index as usize;
      let value = *args.get(index).ok_or_else(|| {
        Error::new(
          Status::InvalidArg,
          format!("missing callback argument at index {index}"),
        )
      })?;
      let callback_id = value_u32(self.env, value, "callback ID")?;
      callbacks.push((
        CallbackKey {
          callback_type_id: callback.callback_type_id,
          callback_id,
        },
        *callback,
      ));
    }
    let mut streams = Vec::new();
    for stream in &operation.stream_arguments {
      if stream.direction != SessionStreamDirection::Input {
        continue;
      }
      let index = receiver_offset + stream.argument_index as usize;
      let value = *args.get(index).ok_or_else(|| {
        Error::new(
          Status::InvalidArg,
          format!("missing input stream argument at index {index}"),
        )
      })?;
      streams.push(value_u32(self.env, value, "input stream ID")?);
    }
    let mut retain_arguments = BTreeMap::new();
    for (key, contract) in &callbacks {
      if contract.retention == SessionCallbackRetention::Retained
        && !self.retained_callbacks.borrow().contains(key)
        && !retain_arguments.contains_key(key)
      {
        retain_arguments.insert(
          *key,
          [
            js_u32(self.env, key.callback_type_id)?,
            js_u32(self.env, key.callback_id)?,
          ],
        );
      }
    }
    let receiver = if let Some(kind) = operation.receiver {
      let resource = *args
        .first()
        .ok_or_else(|| Error::new(Status::InvalidArg, "missing resource receiver"))?;
      Some((resource, kind, self.retain_resource(resource, kind)?))
    } else {
      None
    };

    let mut newly_retained = Vec::new();
    for (key, contract) in &callbacks {
      if contract.retention != SessionCallbackRetention::Retained
        || self.retained_callbacks.borrow().contains(key)
        || newly_retained.contains(key)
      {
        continue;
      }
      let retain_args = retain_arguments
        .get(key)
        .expect("retained callback arguments were prepared");
      if let Err(error) = self.call_host("retainCallback", retain_args) {
        for retained in newly_retained.drain(..) {
          self.retained_callbacks.borrow_mut().remove(&retained);
          if let (Ok(callback_type), Ok(callback_id)) = (
            js_u32(self.env, retained.callback_type_id),
            js_u32(self.env, retained.callback_id),
          ) {
            let _ = self.call_host("releaseCallback", &[callback_type, callback_id]);
          }
        }
        if let Some((resource, kind, true)) = receiver {
          self.forget_resource(resource, kind);
        }
        return Err(error);
      }
      self.retained_callbacks.borrow_mut().insert(*key);
      newly_retained.push(*key);
    }

    let mut scoped = Vec::new();
    let mut contracts = self.callback_contracts.borrow_mut();
    for (key, contract) in callbacks {
      let registrations = contracts.entry(key).or_default();
      if contract.retention == SessionCallbackRetention::Retained {
        if !registrations
          .iter()
          .any(|registered| registered.token.is_none() && registered.contract == contract)
        {
          registrations.push(RegisteredCallbackContract {
            token: None,
            contract,
          });
        }
      } else {
        let token = self.next_callback_registration_id.get();
        self
          .next_callback_registration_id
          .set(token.wrapping_add(1));
        registrations.push(RegisteredCallbackContract {
          token: Some(token),
          contract,
        });
        scoped.push(CallbackRegistrationToken { key, token });
      }
    }
    drop(contracts);
    self.input_streams.borrow_mut().extend(streams);
    Ok(scoped)
  }

  fn forget_resource(&self, resource: sys::napi_value, kind: SessionResourceReceiver) {
    let mut resources = self.resources.borrow_mut();
    let Some(index) = resources.iter().position(|tracked| {
      tracked.kind == kind
        && reference_value(self.env, tracked.reference, "resource lease")
          .ok()
          .is_some_and(|existing| strict_equals(self.env, existing, resource))
    }) else {
      return;
    };
    let tracked = resources.swap_remove(index);
    delete_reference(self.env, tracked.cancel_promise.get());
    delete_reference(self.env, tracked.reference);
  }

  fn retain_resource(
    &self,
    resource: sys::napi_value,
    kind: SessionResourceReceiver,
  ) -> Result<bool> {
    if let Some(tracked) = self.resources.borrow().iter().find(|tracked| {
      tracked.kind == kind
        && reference_value(self.env, tracked.reference, "resource lease")
          .ok()
          .is_some_and(|existing| strict_equals(self.env, existing, resource))
    }) {
      if tracked.cancelled.get() {
        return Err(Error::new(
          Status::InvalidArg,
          "UniFFI resource lease is cancelling or released",
        ));
      }
      return Ok(false);
    }
    self.resources.borrow_mut().push(TrackedResource {
      reference: create_reference(self.env, resource, "resource lease")?,
      kind,
      cancelled: Cell::new(false),
      cancel_promise: Cell::new(ptr::null_mut()),
    });
    Ok(true)
  }

  fn retain_result(
    &self,
    result: sys::napi_value,
    kind: Option<SessionResourceReceiver>,
  ) -> Result<()> {
    let Some(kind) = kind else { return Ok(()) };
    let mut value_type = sys::ValueType::napi_undefined;
    check_status!(unsafe { sys::napi_typeof(self.env, result, &mut value_type) })?;
    if value_type != sys::ValueType::napi_object {
      return Ok(());
    }
    // Generated object/stream leases have a stable `handle` property.  The
    // full lease identity is retained, rather than a user supplied numeric ID.
    let handle = named_property(self.env, result, "handle")?;
    let _ = handle;
    self.retain_resource(result, kind).map(|_| ())
  }

  fn retain_enveloped_result(
    &self,
    envelope: sys::napi_value,
    kind: Option<SessionResourceReceiver>,
  ) -> Result<()> {
    if kind.is_none() {
      return Ok(());
    }
    let value = named_property(self.env, envelope, "value")?;
    self.retain_result(value, kind)
  }

  fn dispatch(
    &self,
    operation_id: u32,
    args: Vec<sys::napi_value>,
    asynchronous: bool,
    this: sys::napi_value,
  ) -> Result<sys::napi_value> {
    self.ensure_open()?;
    let operation = self.operation(operation_id)?;
    let expected_async = matches!(
      operation.dispatch,
      SessionOperationDispatch::NativeAsync
        | SessionOperationDispatch::CallbackHostAsync { .. }
        | SessionOperationDispatch::InputStreamHostPull
        | SessionOperationDispatch::InputStreamHostCancel
    );
    if expected_async != asynchronous {
      return Err(Error::new(
        Status::InvalidArg,
        format!(
          "operation {operation_id} is {}, but was invoked through {}",
          if expected_async { "async" } else { "sync" },
          if asynchronous {
            "invokeAsync"
          } else {
            "invokeSync"
          }
        ),
      ));
    }
    let scoped_callbacks = self.retain_inputs_and_receiver(operation, &args)?;
    let result = (|| match operation.dispatch {
      SessionOperationDispatch::NativeSync | SessionOperationDispatch::NativeAsync => {
        let callback = reference_value(self.env, operation.callback.get(), "operation callback")?;
        let mut args = args;
        if operation.receiver.is_some() {
          args[0] = named_property(self.env, args[0], "handle")?;
        }
        let mut native_args = Vec::with_capacity(
          args.len() + usize::from(operation.native_call == SessionNativeCall::HostAndArguments),
        );
        if operation.native_call == SessionNativeCall::HostAndArguments {
          native_args.push(reference_value(
            self.env,
            self.proxy_host.get(),
            "native callback Host adapter",
          )?);
        }
        native_args.extend(args);
        call_function(self.env, this, callback, &native_args)
      }
      SessionOperationDispatch::CallbackHostSync {
        callback_type_id,
        method_id,
      } => self.dispatch_callback_host(
        callback_type_id,
        method_id,
        args,
        false,
        &operation.callback_arguments,
        this,
      ),
      SessionOperationDispatch::CallbackHostAsync {
        callback_type_id,
        method_id,
      } => self.dispatch_callback_host(
        callback_type_id,
        method_id,
        args,
        true,
        &operation.callback_arguments,
        this,
      ),
      SessionOperationDispatch::InputStreamHostPull => {
        let stream = *args
          .first()
          .ok_or_else(|| Error::new(Status::InvalidArg, "pull requires stream ID"))?;
        self.call_host("pullInputStream", &[stream])
      }
      SessionOperationDispatch::InputStreamHostCancel => {
        let stream = *args
          .first()
          .ok_or_else(|| Error::new(Status::InvalidArg, "cancel requires stream ID"))?;
        let stream_id = value_u32(self.env, stream, "input stream ID")?;
        self.cancel_input_stream(this, stream_id, &args)
      }
    })();
    let result = match result {
      Ok(result) => result,
      Err(error) => {
        self.release_scoped_callback_tokens(&scoped_callbacks);
        return Err(error);
      }
    };
    if asynchronous {
      match is_promise(self.env, result) {
        Ok(true) => {}
        Ok(false) => {
          self.release_scoped_callback_tokens(&scoped_callbacks);
          return Err(Error::new(
            Status::InvalidArg,
            format!("async operation {operation_id} did not return a Promise"),
          ));
        }
        Err(error) => {
          self.release_scoped_callback_tokens(&scoped_callbacks);
          return Err(error);
        }
      }
      self.finish_async_dispatch(this, result, operation.result_receiver, scoped_callbacks)
    } else {
      match is_promise(self.env, result) {
        Ok(false) => {}
        Ok(true) => {
          self.release_scoped_callback_tokens(&scoped_callbacks);
          return Err(Error::new(
            Status::InvalidArg,
            format!("sync operation {operation_id} returned a Promise"),
          ));
        }
        Err(error) => {
          self.release_scoped_callback_tokens(&scoped_callbacks);
          return Err(error);
        }
      }
      let retain_result = self.retain_enveloped_result(result, operation.result_receiver);
      self.release_scoped_callback_tokens(&scoped_callbacks);
      retain_result?;
      Ok(result)
    }
  }

  fn finish_async_dispatch(
    &self,
    session: sys::napi_value,
    promise: sys::napi_value,
    result_receiver: Option<SessionResourceReceiver>,
    scoped_callbacks: Vec<CallbackRegistrationToken>,
  ) -> Result<sys::napi_value> {
    let session_reference = match create_reference(self.env, session, "async session") {
      Ok(reference) => reference,
      Err(error) => {
        self.release_scoped_callback_tokens(&scoped_callbacks);
        return Err(error);
      }
    };
    let lease = Rc::new(AsyncDispatchLease {
      state: self,
      session_reference: Cell::new(session_reference),
      pending_id: Cell::new(None),
      scoped_callbacks: RefCell::new(Some(scoped_callbacks)),
      finished: Cell::new(false),
    });
    let state = self as *const SessionState;
    let then_lease = Rc::clone(&lease);
    let promise = PromiseRaw::<Unknown<'static>>::new(self.env, promise);
    let chained = promise.then(move |context| {
      if then_lease.finished.get() {
        return Ok(context.value);
      }
      let state = unsafe { &*state };
      state.retain_enveloped_result(context.value.raw(), result_receiver)?;
      // Keep the session alive through the resolve handler even when a
      // user-overridden Promise method retains this callback and then throws.
      let _ = &then_lease;
      Ok(context.value)
    })?;
    let mut chained = chained;
    let finally_lease = Rc::clone(&lease);
    let settled = chained.finally::<(), _>(move |_| {
      finally_lease.finish();
      Ok(())
    })?;
    let pending_id = self.track_pending_operation(settled.raw())?;
    lease.pending_id.set(Some(pending_id));
    Ok(settled.raw())
  }

  fn track_pending_operation(&self, promise: sys::napi_value) -> Result<u64> {
    let id = self.next_pending_operation_id.get();
    self.next_pending_operation_id.set(id.wrapping_add(1));
    let reference = create_reference(self.env, promise, "pending async operation")?;
    self.pending_operations.borrow_mut().insert(id, reference);
    Ok(id)
  }

  fn finish_pending_operation(&self, id: u64) {
    if let Some(reference) = self.pending_operations.borrow_mut().remove(&id) {
      delete_reference(self.env, reference);
    }
  }

  fn dispatch_callback_host(
    &self,
    callback_type_id: u32,
    method_id: u32,
    args: Vec<sys::napi_value>,
    asynchronous: bool,
    callback_arguments: &[SessionCallbackArgument],
    session: sys::napi_value,
  ) -> Result<sys::napi_value> {
    let callback_id_value = *args.first().ok_or_else(|| {
      Error::new(
        Status::InvalidArg,
        "callback invocation requires callback ID",
      )
    })?;
    let callback_id = value_u32(self.env, callback_id_value, "callback ID")?;
    let key = CallbackKey {
      callback_type_id,
      callback_id,
    };
    let expected_style = if asynchronous {
      SessionCallbackCallStyle::Async
    } else {
      SessionCallbackCallStyle::Sync
    };
    let registered = self
      .callback_contracts
      .borrow()
      .get(&key)
      .cloned()
      .unwrap_or_default();
    let contract = registered
      .iter()
      .map(|registered| registered.contract)
      .find(|contract| contract.call_style == expected_style)
      .or_else(|| {
        callback_arguments
          .iter()
          .find(|argument| {
            argument.callback_type_id == callback_type_id && argument.call_style == expected_style
          })
          .copied()
      });
    if !registered.is_empty() && contract.is_none() {
      return Err(Error::new(
        Status::InvalidArg,
        format!(
          "callback type {callback_type_id} method {method_id} has no active {} contract",
          if asynchronous { "async" } else { "sync" },
        ),
      ));
    }
    let operation_reentrancy =
      callback_reentrancy_for_operation(callback_arguments, callback_type_id);
    let guarded = operation_reentrancy == SessionCallbackReentrancy::Forbidden
      || registered
        .iter()
        .any(|registered| registered.contract.reentrancy == SessionCallbackReentrancy::Forbidden)
      || contract.is_some_and(|value| value.reentrancy == SessionCallbackReentrancy::Forbidden);
    if let Some(contract) = contract {
      let expected_async = contract.call_style == SessionCallbackCallStyle::Async;
      if expected_async != asynchronous {
        return Err(Error::new(
          Status::InvalidArg,
          format!(
            "callback type {callback_type_id} method {method_id} is {}, but the operation is {}",
            if expected_async { "async" } else { "sync" },
            if asynchronous { "async" } else { "sync" },
          ),
        ));
      }
      if !asynchronous && contract.threading == SessionCallbackThreading::MayCrossThread {
        return Err(Error::new(
          Status::InvalidArg,
          "synchronous callbacks cannot use the may-cross-thread policy",
        ));
      }
    }
    if guarded && !self.active_callbacks.borrow_mut().insert(key) {
      return Err(Error::new(
        Status::InvalidArg,
        "callback reentrancy is forbidden",
      ));
    }
    let callback_args = match js_array(self.env, &args[1..]) {
      Ok(value) => value,
      Err(error) => {
        clear_callback_guard_on_error(&self.active_callbacks, guarded, key);
        return Err(error);
      }
    };
    let result = match (|| {
      let mut host_args = vec![
        js_u32(self.env, callback_type_id)?,
        callback_id_value,
        js_u32(self.env, method_id)?,
      ];
      if asynchronous {
        let invocation_id = self.next_invocation_id.get();
        self
          .next_invocation_id
          .set(invocation_id.checked_add(1).unwrap_or(u32::MAX));
        host_args.push(js_u32(self.env, invocation_id)?);
        host_args.push(callback_args);
        self.call_host("invokeCallbackAsync", &host_args)
      } else {
        host_args.push(callback_args);
        self.call_host("invokeCallbackSync", &host_args)
      }
    })() {
      Ok(value) => value,
      Err(error) => {
        clear_callback_guard_on_error(&self.active_callbacks, guarded, key);
        return Err(error);
      }
    };
    let result_is_promise = match is_promise(self.env, result) {
      Ok(value) => value,
      Err(error) => {
        clear_callback_guard_on_error(&self.active_callbacks, guarded, key);
        return Err(error);
      }
    };
    if asynchronous != result_is_promise {
      clear_callback_guard_on_error(&self.active_callbacks, guarded, key);
      return Err(Error::new(
        Status::InvalidArg,
        if asynchronous {
          "Host.invokeCallbackAsync must return a Promise"
        } else {
          "Host.invokeCallbackSync must not return a Promise"
        },
      ));
    }
    if guarded && asynchronous {
      match result_is_promise {
        true => {
          // `invokeCallbackAsync` returns a host Promise.  Keep the
          // reentrancy guard active until *either* settlement path runs;
          // removing it as soon as `call_host` returns would allow a second
          // callback while the first one is still pending.
          return self.release_callback_guard_after_promise(session, result, key);
        }
        false => unreachable!("async callback Promise shape was validated above"),
      }
    } else if guarded {
      release_callback_guard(&self.active_callbacks, key);
    }
    Ok(result)
  }

  fn release_callback_guard_after_promise(
    &self,
    session: sys::napi_value,
    promise: sys::napi_value,
    key: CallbackKey,
  ) -> Result<sys::napi_value> {
    let session_reference = match create_reference(self.env, session, "callback session") {
      Ok(reference) => reference,
      Err(error) => {
        release_callback_guard(&self.active_callbacks, key);
        return Err(error);
      }
    };
    let lease = Rc::new(CallbackGuardLease {
      state: self,
      session_reference: Cell::new(session_reference),
      key,
      finished: Cell::new(false),
    });
    let mut promise = PromiseRaw::<()>::new(self.env, promise);
    let finally_lease = Rc::clone(&lease);
    let settled = promise.finally::<(), _>(move |_| {
      finally_lease.finish();
      Ok(())
    })?;
    Ok(settled.raw())
  }

  fn call_host(&self, name: &str, args: &[sys::napi_value]) -> Result<sys::napi_value> {
    call_named(self.env, self.host_value()?, name, args)
  }

  fn release_scoped_callback_tokens(&self, tokens: &[CallbackRegistrationToken]) {
    let mut contracts = self.callback_contracts.borrow_mut();
    remove_callback_registrations(&mut contracts, tokens);
  }

  fn resource_callback(&self, kind: SessionResourceReceiver, cancel: bool) -> sys::napi_ref {
    match (kind, cancel) {
      (SessionResourceReceiver::Object, _) => self.resource_callbacks.release_object.get(),
      (SessionResourceReceiver::OutputStream, true) => {
        self.resource_callbacks.cancel_output_stream.get()
      }
      (SessionResourceReceiver::OutputStream, false) => {
        self.resource_callbacks.release_output_stream.get()
      }
    }
  }

  fn call_resource_callback(
    &self,
    resource: sys::napi_value,
    kind: SessionResourceReceiver,
    cancel: bool,
  ) -> Result<Option<sys::napi_value>> {
    let callback = self.resource_callback(kind, cancel);
    if callback.is_null() {
      return Ok(None);
    }
    let callback = reference_value(self.env, callback, "native resource callback")?;
    let handle = named_property(self.env, resource, "handle")?;
    call_function(self.env, resource, callback, &[handle]).map(Some)
  }

  /// Release is intentionally synchronous, idempotent and non-throwing at the
  /// public ABI.  The caller decides whether a native diagnostic should be
  /// surfaced; the lease is always revoked exactly once.
  fn release_resource(
    &self,
    resource: sys::napi_value,
    kind: SessionResourceReceiver,
  ) -> Result<()> {
    let Some(reference) = self.take_releasable_resource_reference(resource, kind) else {
      return Ok(());
    };
    let result = self.call_resource_callback(resource, kind, false);
    delete_reference(self.env, reference);
    result.map(|_| ())
  }

  /// Begin the output-stream cancel state machine.  The public lease is
  /// synchronously marked cancelling; the native release hook runs from a
  /// Promise `finally`, so both resolve and reject settle the lease once.
  fn cancel_output_resource(
    &self,
    session: sys::napi_value,
    resource: sys::napi_value,
  ) -> Result<sys::napi_value> {
    let reference = {
      let resources = self.resources.borrow();
      let Some(tracked) = resources.iter().find(|tracked| {
        tracked.kind == SessionResourceReceiver::OutputStream
          && reference_value(self.env, tracked.reference, "resource lease")
            .ok()
            .is_some_and(|existing| strict_equals(self.env, existing, resource))
      }) else {
        return resolved_promise(self.env);
      };
      if tracked.cancelled.replace(true) {
        let cancel_promise = tracked.cancel_promise.get();
        return if cancel_promise.is_null() {
          Err(Error::new(
            Status::GenericFailure,
            "output-stream cancellation is still being registered",
          ))
        } else {
          reference_value(self.env, cancel_promise, "output-stream cancellation")
        };
      }
      tracked.reference
    };

    let session_reference =
      match create_reference(self.env, session, "output-stream cancel session") {
        Ok(reference) => reference,
        Err(error) => {
          self.finish_output_resource(reference);
          return Err(error);
        }
      };

    let promise =
      match self.call_resource_callback(resource, SessionResourceReceiver::OutputStream, true) {
        Ok(Some(value)) => match is_promise(self.env, value) {
          Ok(true) => value,
          Ok(false) => {
            self.finish_output_resource(reference);
            delete_reference(self.env, session_reference);
            return Err(Error::new(
              Status::InvalidArg,
              "native output-stream cancel hook must return a Promise",
            ));
          }
          Err(error) => {
            self.finish_output_resource(reference);
            delete_reference(self.env, session_reference);
            return Err(error);
          }
        },
        Ok(None) => {
          self.finish_output_resource(reference);
          delete_reference(self.env, session_reference);
          return Err(Error::new(
            Status::GenericFailure,
            "native output-stream cancel hook is not installed",
          ));
        }
        Err(error) => {
          self.finish_output_resource(reference);
          delete_reference(self.env, session_reference);
          return Err(error);
        }
      };
    let lease = Rc::new(OutputCancelLease {
      state: self,
      session_reference: Cell::new(session_reference),
      resource_reference: reference,
      finished: Cell::new(false),
    });
    let mut promise = PromiseRaw::<Unknown<'static>>::new(self.env, promise);
    let finally_lease = Rc::clone(&lease);
    let settled = promise.finally::<(), _>(move |_| {
      finally_lease.finish();
      Ok(())
    })?;
    let cancel_reference = create_reference(
      self.env,
      settled.raw(),
      "output-stream cancellation settlement",
    )?;
    if let Some(tracked) = self
      .resources
      .borrow()
      .iter()
      .find(|tracked| tracked.reference == reference)
    {
      tracked.cancel_promise.set(cancel_reference);
    } else {
      delete_reference(self.env, cancel_reference);
    }
    Ok(settled.raw())
  }

  fn finish_output_resource(&self, reference: sys::napi_ref) {
    let resource = reference_value(self.env, reference, "output-stream lease").ok();
    if let Some(resource) = resource {
      // `releaseOutputStream` is a non-throwing ABI hook.  A native failure is
      // diagnostic only and must not replace the cancel Promise settlement.
      let _ = self.call_resource_callback(resource, SessionResourceReceiver::OutputStream, false);
    }
    let tracked = {
      let mut resources = self.resources.borrow_mut();
      resources
        .iter()
        .position(|tracked| tracked.reference == reference)
        .map(|index| resources.swap_remove(index))
    };
    if let Some(tracked) = tracked {
      delete_reference(self.env, tracked.cancel_promise.get());
      delete_reference(self.env, tracked.reference);
    }
  }

  fn take_releasable_resource_reference(
    &self,
    resource: sys::napi_value,
    kind: SessionResourceReceiver,
  ) -> Option<sys::napi_ref> {
    let mut resources = self.resources.borrow_mut();
    let index = resources.iter().position(|tracked| {
      tracked.kind == kind
        && !tracked.cancelled.get()
        && reference_value(self.env, tracked.reference, "resource lease")
          .ok()
          .is_some_and(|existing| strict_equals(self.env, existing, resource))
    })?;
    Some(resources.swap_remove(index).reference)
  }

  fn cancel_input_stream(
    &self,
    session: sys::napi_value,
    stream_id: u32,
    args: &[sys::napi_value],
  ) -> Result<sys::napi_value> {
    if let Some(reference) = self.input_stream_cancels.borrow().get(&stream_id).copied() {
      return reference_value(self.env, reference, "input-stream cancellation");
    }
    if !self.input_streams.borrow_mut().remove(&stream_id) {
      return resolved_promise(self.env);
    }
    let session_reference = match create_reference(self.env, session, "input-stream cancel session")
    {
      Ok(reference) => reference,
      Err(error) => {
        self.release_input_stream(stream_id);
        return Err(error);
      }
    };
    let promise = match self.call_host("cancelInputStream", args) {
      Ok(value) => match is_promise(self.env, value) {
        Ok(true) => value,
        Ok(false) => {
          self.release_input_stream(stream_id);
          delete_reference(self.env, session_reference);
          return Err(Error::new(
            Status::InvalidArg,
            "Host.cancelInputStream must return a Promise",
          ));
        }
        Err(error) => {
          self.release_input_stream(stream_id);
          delete_reference(self.env, session_reference);
          return Err(error);
        }
      },
      Err(error) => {
        self.release_input_stream(stream_id);
        delete_reference(self.env, session_reference);
        return Err(error);
      }
    };
    let lease = Rc::new(InputCancelLease {
      state: self,
      session_reference: Cell::new(session_reference),
      stream_id,
      finished: Cell::new(false),
    });
    let mut promise = PromiseRaw::<Unknown<'static>>::new(self.env, promise);
    let finally_lease = Rc::clone(&lease);
    let settled = promise.finally::<(), _>(move |_| {
      finally_lease.finish();
      Ok(())
    })?;
    let cancel_reference = create_reference(
      self.env,
      settled.raw(),
      "input-stream cancellation settlement",
    )?;
    if lease.finished.get() {
      delete_reference(self.env, cancel_reference);
    } else {
      self
        .input_stream_cancels
        .borrow_mut()
        .insert(stream_id, cancel_reference);
    }
    Ok(settled.raw())
  }

  fn finish_input_stream(&self, stream_id: u32) {
    self.release_input_stream(stream_id);
    if let Some(reference) = self.input_stream_cancels.borrow_mut().remove(&stream_id) {
      delete_reference(self.env, reference);
    }
  }

  fn release_input_stream(&self, stream_id: u32) {
    if let Ok(value) = js_u32(self.env, stream_id) {
      // Release is a non-throwing cleanup notification.  A Host diagnostic
      // must not prevent the native lease from reaching its terminal state.
      let _ = self.call_host("releaseInputStream", &[value]);
    }
  }

  fn close(&self, session: sys::napi_value) -> Result<sys::napi_value> {
    let existing = self.close_promise.get();
    if !existing.is_null() {
      return reference_value(self.env, existing, "session close Promise");
    }
    self.closed.set(true);
    let session_reference = create_reference(self.env, session, "closing session")?;
    let keepalive = Rc::new(SessionKeepalive {
      env: self.env,
      reference: Cell::new(session_reference),
    });
    let base = resolved_promise(self.env)?;
    let state = self as *const SessionState;
    let start_keepalive = Rc::clone(&keepalive);
    let base = PromiseRaw::<Unknown<'static>>::new(self.env, base);
    let started = base.then(move |_| {
      if start_keepalive.reference.get().is_null() {
        return Err(Error::new(
          Status::GenericFailure,
          "session close handler ran after cleanup",
        ));
      }
      let state = unsafe { &*state };
      let cleanup = state.begin_close_cleanup(Rc::clone(&start_keepalive))?;
      Ok(unsafe { Unknown::from_raw_unchecked(state.env, cleanup) })
    })?;
    let mut started = started;
    let finish_keepalive = Rc::clone(&keepalive);
    let settled = started.finally::<(), _>(move |_| {
      finish_keepalive.finish();
      Ok(())
    })?;
    let close_reference = create_reference(self.env, settled.raw(), "session close Promise")?;
    self.close_promise.set(close_reference);
    Ok(settled.raw())
  }

  fn begin_close_cleanup(&self, keepalive: Rc<SessionKeepalive>) -> Result<sys::napi_value> {
    let mut pending = Vec::new();
    for reference in self.pending_operations.borrow().values().copied() {
      pending.push(reference_value(
        self.env,
        reference,
        "pending async operation",
      )?);
    }
    let drained = promise_all_settled(self.env, &pending, None)?;
    let state = self as *const SessionState;
    let drained = PromiseRaw::<Unknown<'static>>::new(self.env, drained);
    let cleaned = drained.then(move |_| {
      if keepalive.reference.get().is_null() {
        return Err(Error::new(
          Status::GenericFailure,
          "session cleanup handler ran after close settled",
        ));
      }
      let state = unsafe { &*state };
      let session = reference_value(state.env, keepalive.reference.get(), "closing session")?;
      let cleanup = state.cleanup_after_pending(session)?;
      Ok(unsafe { Unknown::from_raw_unchecked(state.env, cleanup) })
    })?;
    Ok(cleaned.raw())
  }

  fn cleanup_after_pending(&self, session: sys::napi_value) -> Result<sys::napi_value> {
    let mut pending = Vec::new();
    let mut first_error = None;

    let input = self
      .input_streams
      .borrow()
      .iter()
      .copied()
      .collect::<Vec<_>>();
    for stream in input {
      match js_u32(self.env, stream)
        .and_then(|value| self.cancel_input_stream(session, stream, &[value]))
      {
        Ok(promise) => pending.push(promise),
        Err(error) => {
          if first_error.is_none() {
            first_error = Some(error);
          }
        }
      }
    }
    for reference in self.input_stream_cancels.borrow().values().copied() {
      if let Ok(promise) = reference_value(self.env, reference, "input-stream cancellation") {
        pending.push(promise);
      }
    }

    let resources = self
      .resources
      .borrow()
      .iter()
      .filter_map(|tracked| {
        reference_value(self.env, tracked.reference, "resource lease")
          .ok()
          .map(|value| (value, tracked.kind))
      })
      .collect::<Vec<_>>();
    for (resource, kind) in resources {
      if kind == SessionResourceReceiver::OutputStream {
        match self.cancel_output_resource(session, resource) {
          Ok(promise) => pending.push(promise),
          Err(error) => {
            if first_error.is_none() {
              first_error = Some(error);
            }
          }
        }
      } else {
        // Object release is specified as a non-throwing cleanup queue hook.
        let _ = self.release_resource(resource, kind);
      }
    }

    let callbacks = std::mem::take(&mut *self.retained_callbacks.borrow_mut());
    self.callback_contracts.borrow_mut().clear();
    self.active_callbacks.borrow_mut().clear();
    for callback in callbacks {
      if let (Ok(callback_type), Ok(callback_id)) = (
        js_u32(self.env, callback.callback_type_id),
        js_u32(self.env, callback.callback_id),
      ) {
        let _ = self.call_host("releaseCallback", &[callback_type, callback_id]);
      }
    }
    promise_all_settled(self.env, &pending, first_error)
  }

  fn cleanup_references(&self) {
    self.closed.set(true);
    self.callback_contracts.borrow_mut().clear();
    self.active_callbacks.borrow_mut().clear();
    let callbacks = std::mem::take(&mut *self.retained_callbacks.borrow_mut());
    for callback in callbacks {
      if let (Ok(callback_type), Ok(callback_id)) = (
        js_u32(self.env, callback.callback_type_id),
        js_u32(self.env, callback.callback_id),
      ) {
        let _ = self.call_host("releaseCallback", &[callback_type, callback_id]);
      }
    }
    let input = std::mem::take(&mut *self.input_streams.borrow_mut());
    for stream in input {
      if let Ok(value) = js_u32(self.env, stream) {
        let _ = self.call_host("releaseInputStream", &[value]);
      }
    }
    for (_, reference) in std::mem::take(&mut *self.input_stream_cancels.borrow_mut()) {
      delete_reference(self.env, reference);
    }
    for (_, reference) in std::mem::take(&mut *self.pending_operations.borrow_mut()) {
      delete_reference(self.env, reference);
    }
    for operation in &self.operations {
      delete_reference(self.env, operation.callback.replace(ptr::null_mut()));
    }
    for tracked in std::mem::take(&mut *self.resources.borrow_mut()) {
      if let Ok(resource) = reference_value(self.env, tracked.reference, "resource lease") {
        let _ = self.call_resource_callback(resource, tracked.kind, false);
      }
      delete_reference(self.env, tracked.cancel_promise.get());
      delete_reference(self.env, tracked.reference);
    }
    for callback in [
      &self.resource_callbacks.release_object,
      &self.resource_callbacks.cancel_output_stream,
      &self.resource_callbacks.release_output_stream,
    ] {
      delete_reference(self.env, callback.replace(ptr::null_mut()));
    }
    delete_reference(self.env, self.close_promise.replace(ptr::null_mut()));
    delete_reference(self.env, self.proxy_host.replace(ptr::null_mut()));
    delete_reference(self.env, self.session_weak.replace(ptr::null_mut()));
    delete_reference(self.env, self.host.replace(ptr::null_mut()));
  }
}

/// Build the one generated backend factory session from its operation table.
pub fn create_backend_session(
  env: &Env,
  host: Object<'static>,
  descriptors: Vec<SessionOperationDescriptor>,
  resource_callbacks: SessionResourceCallbacks,
) -> Result<Object<'static>> {
  let host_reference = create_reference(env.raw(), host.value().value, "Host")?;
  let mut operations = Vec::with_capacity(descriptors.len());
  for (id, descriptor) in descriptors.into_iter().enumerate() {
    let callback = match (descriptor.dispatch, descriptor.callback) {
      (
        SessionOperationDispatch::NativeSync | SessionOperationDispatch::NativeAsync,
        Some(value),
      ) => create_reference(env.raw(), value, "operation callback")?,
      (SessionOperationDispatch::NativeSync | SessionOperationDispatch::NativeAsync, None) => {
        return Err(Error::new(
          Status::InvalidArg,
          format!("native UniFFI operation slot {id} has no callback"),
        ))
      }
      (_, None) => ptr::null_mut(),
      (_, Some(_)) => {
        return Err(Error::new(
          Status::InvalidArg,
          format!("host UniFFI operation slot {id} unexpectedly has a native callback"),
        ))
      }
    };
    operations.push(SessionOperation {
      dispatch: descriptor.dispatch,
      callback: Cell::new(callback),
      native_call: descriptor.native_call,
      receiver: descriptor.receiver,
      result_receiver: descriptor.result_receiver,
      callback_arguments: descriptor.callback_arguments,
      stream_arguments: descriptor.stream_arguments,
    });
  }
  let state = Box::new(SessionState {
    env: env.raw(),
    host: Cell::new(host_reference),
    proxy_host: Cell::new(ptr::null_mut()),
    session_weak: Cell::new(ptr::null_mut()),
    operations,
    closed: Cell::new(false),
    next_invocation_id: Cell::new(0),
    next_callback_registration_id: Cell::new(0),
    next_pending_operation_id: Cell::new(0),
    retained_callbacks: RefCell::new(BTreeSet::new()),
    callback_contracts: RefCell::new(BTreeMap::new()),
    active_callbacks: RefCell::new(BTreeSet::new()),
    input_streams: RefCell::new(BTreeSet::new()),
    input_stream_cancels: RefCell::new(BTreeMap::new()),
    pending_operations: RefCell::new(BTreeMap::new()),
    resources: RefCell::new(Vec::new()),
    close_promise: Cell::new(ptr::null_mut()),
    resource_callbacks: ResourceCallbacks {
      release_object: Cell::new(optional_reference(
        env.raw(),
        resource_callbacks.release_object,
        "object release callback",
      )?),
      cancel_output_stream: Cell::new(optional_reference(
        env.raw(),
        resource_callbacks.cancel_output_stream,
        "output-stream cancel callback",
      )?),
      release_output_stream: Cell::new(optional_reference(
        env.raw(),
        resource_callbacks.release_output_stream,
        "output-stream release callback",
      )?),
    },
  });
  let session = Object::new(env)?;
  let raw_session = session.value().value;
  let state = Box::into_raw(state);
  check_status!(unsafe {
    sys::napi_wrap(
      env.raw(),
      raw_session,
      state.cast(),
      Some(finalize_session),
      ptr::null_mut(),
      ptr::null_mut(),
    )
  })?;
  unsafe { &*state }.session_weak.set(create_weak_reference(
    env.raw(),
    raw_session,
    "backend session",
  )?);
  let proxy_host = Object::new(env)?;
  let raw_proxy_host = proxy_host.value().value;
  set_named_property(
    env.raw(),
    raw_proxy_host,
    "__uniffiRawHost",
    host.value().value,
  )?;
  add_data_method(
    env.raw(),
    raw_proxy_host,
    "invokeCallbackSync",
    proxy_invoke_callback_sync,
    state.cast(),
  )?;
  add_data_method(
    env.raw(),
    raw_proxy_host,
    "invokeCallbackAsync",
    proxy_invoke_callback_async,
    state.cast(),
  )?;
  add_data_method(
    env.raw(),
    raw_proxy_host,
    "pullInputStream",
    proxy_pull_input_stream,
    state.cast(),
  )?;
  add_data_method(
    env.raw(),
    raw_proxy_host,
    "cancelInputStream",
    proxy_cancel_input_stream,
    state.cast(),
  )?;
  add_data_method(
    env.raw(),
    raw_proxy_host,
    "releaseInputStream",
    proxy_release_input_stream,
    state.cast(),
  )?;
  let proxy_reference =
    create_reference(env.raw(), raw_proxy_host, "native callback Host adapter")?;
  unsafe { &*state }.proxy_host.set(proxy_reference);
  add_method(env.raw(), raw_session, "invokeSync", session_invoke_sync)?;
  add_method(env.raw(), raw_session, "invokeAsync", session_invoke_async)?;
  add_method(
    env.raw(),
    raw_session,
    "releaseObject",
    session_release_object,
  )?;
  add_method(
    env.raw(),
    raw_session,
    "cancelOutputStream",
    session_cancel_output_stream,
  )?;
  add_method(
    env.raw(),
    raw_session,
    "releaseOutputStream",
    session_release_output_stream,
  )?;
  add_method(env.raw(), raw_session, "close", session_close)?;
  Ok(session)
}

unsafe extern "C" fn finalize_session(_env: sys::napi_env, data: *mut c_void, _hint: *mut c_void) {
  if !data.is_null() {
    let state = unsafe { Box::<SessionState>::from_raw(data.cast()) };
    state.cleanup_references();
  }
}

unsafe extern "C" fn proxy_invoke_callback_sync(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  callback_result(env, || {
    let (state, args) = proxy_callback_args(env, info, 4, 4)?;
    state.ensure_open()?;
    let callback_type_id = value_u32(env, args[0], "callback type ID")?;
    let method_id = value_u32(env, args[2], "callback method ID")?;
    let mut callback_args = vec![args[1]];
    callback_args.extend(array_values(env, args[3])?);
    state.dispatch_callback_host(
      callback_type_id,
      method_id,
      callback_args,
      false,
      &[],
      state.session_value()?,
    )
  })
}

unsafe extern "C" fn proxy_invoke_callback_async(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  callback_result(env, || {
    let (state, args) = proxy_callback_args(env, info, 5, 5)?;
    state.ensure_open()?;
    let callback_type_id = value_u32(env, args[0], "callback type ID")?;
    let method_id = value_u32(env, args[2], "callback method ID")?;
    let mut callback_args = vec![args[1]];
    callback_args.extend(array_values(env, args[4])?);
    state.dispatch_callback_host(
      callback_type_id,
      method_id,
      callback_args,
      true,
      &[],
      state.session_value()?,
    )
  })
}

unsafe extern "C" fn proxy_pull_input_stream(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  callback_result(env, || {
    let (state, args) = proxy_callback_args(env, info, 1, 1)?;
    state.call_host("pullInputStream", &args)
  })
}

unsafe extern "C" fn proxy_cancel_input_stream(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  callback_result(env, || {
    let (state, args) = proxy_callback_args(env, info, 1, 2)?;
    state.call_host("cancelInputStream", &args)
  })
}

unsafe extern "C" fn proxy_release_input_stream(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  callback_result(env, || {
    let (state, args) = proxy_callback_args(env, info, 1, 1)?;
    state.call_host("releaseInputStream", &args)
  })
}

unsafe extern "C" fn session_invoke_sync(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  callback_result(env, || {
    let (this, args) = callback_args(env, info, 2)?;
    let operation_id = value_u32(env, args[0], "operation ID")?;
    state(env, this)?.dispatch(operation_id, array_values(env, args[1])?, false, this)
  })
}

unsafe extern "C" fn session_invoke_async(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  callback_result(env, || {
    let (this, args) = callback_args(env, info, 2)?;
    let operation_id = value_u32(env, args[0], "operation ID")?;
    state(env, this)?.dispatch(operation_id, array_values(env, args[1])?, true, this)
  })
}

unsafe extern "C" fn session_release_object(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  let result = (|| {
    let (this, args) = callback_args(env, info, 1)?;
    let _ = state(env, this)?.release_resource(args[0], SessionResourceReceiver::Object);
    js_undefined(env)
  })();
  result.unwrap_or_else(|_| js_undefined(env).unwrap_or(ptr::null_mut()))
}

unsafe extern "C" fn session_cancel_output_stream(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  callback_result(env, || {
    let (this, args) = callback_args(env, info, 1)?;
    state(env, this)?.cancel_output_resource(this, args[0])
  })
}

unsafe extern "C" fn session_release_output_stream(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  let result = (|| {
    let (this, args) = callback_args(env, info, 1)?;
    let _ = state(env, this)?.release_resource(args[0], SessionResourceReceiver::OutputStream);
    js_undefined(env)
  })();
  result.unwrap_or_else(|_| js_undefined(env).unwrap_or(ptr::null_mut()))
}

unsafe extern "C" fn session_close(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  callback_result(env, || {
    let (this, _) = callback_args(env, info, 0)?;
    state(env, this)?.close(this)
  })
}

fn state(env: sys::napi_env, this: sys::napi_value) -> Result<&'static SessionState> {
  let mut data = ptr::null_mut();
  check_status!(unsafe { sys::napi_unwrap(env, this, &mut data) })?;
  if data.is_null() {
    return Err(Error::new(Status::GenericFailure, "invalid UniFFI session"));
  }
  Ok(unsafe { &*data.cast::<SessionState>() })
}

fn callback_args(
  env: sys::napi_env,
  info: sys::napi_callback_info,
  expected: usize,
) -> Result<(sys::napi_value, Vec<sys::napi_value>)> {
  let mut argc = expected;
  let mut args = vec![ptr::null_mut(); expected];
  let mut this = ptr::null_mut();
  check_status!(unsafe {
    sys::napi_get_cb_info(
      env,
      info,
      &mut argc,
      args.as_mut_ptr(),
      &mut this,
      ptr::null_mut(),
    )
  })?;
  if argc != expected {
    return Err(Error::new(
      Status::InvalidArg,
      format!("expected {expected} arguments, received {argc}"),
    ));
  }
  Ok((this, args))
}

fn proxy_callback_args(
  env: sys::napi_env,
  info: sys::napi_callback_info,
  minimum: usize,
  maximum: usize,
) -> Result<(&'static SessionState, Vec<sys::napi_value>)> {
  let mut argc = maximum;
  let mut args = vec![ptr::null_mut(); maximum];
  let mut data = ptr::null_mut();
  check_status!(unsafe {
    sys::napi_get_cb_info(
      env,
      info,
      &mut argc,
      args.as_mut_ptr(),
      ptr::null_mut(),
      &mut data,
    )
  })?;
  if argc < minimum || argc > maximum {
    return Err(Error::new(
      Status::InvalidArg,
      format!("expected {minimum}..={maximum} arguments, received {argc}"),
    ));
  }
  if data.is_null() {
    return Err(Error::new(
      Status::GenericFailure,
      "invalid native callback Host adapter",
    ));
  }
  args.truncate(argc);
  Ok((unsafe { &*data.cast::<SessionState>() }, args))
}

fn array_values(env: sys::napi_env, array: sys::napi_value) -> Result<Vec<sys::napi_value>> {
  let mut is_array = false;
  check_status!(unsafe { sys::napi_is_array(env, array, &mut is_array) })?;
  if !is_array {
    return Err(Error::new(
      Status::InvalidArg,
      "UniFFI invocation arguments must be an array",
    ));
  }
  let mut length = 0;
  check_status!(unsafe { sys::napi_get_array_length(env, array, &mut length) })?;
  let mut values = Vec::with_capacity(length as usize);
  for index in 0..length {
    let mut value = ptr::null_mut();
    check_status!(unsafe { sys::napi_get_element(env, array, index, &mut value) })?;
    values.push(value);
  }
  Ok(values)
}

fn add_method(
  env: sys::napi_env,
  object: sys::napi_value,
  name: &str,
  callback: unsafe extern "C" fn(sys::napi_env, sys::napi_callback_info) -> sys::napi_value,
) -> Result<()> {
  let name = CString::new(name)?;
  let mut function = ptr::null_mut();
  check_status!(unsafe {
    sys::napi_create_function(
      env,
      name.as_ptr(),
      name.as_bytes().len() as isize,
      Some(callback),
      ptr::null_mut(),
      &mut function,
    )
  })?;
  check_status!(unsafe { sys::napi_set_named_property(env, object, name.as_ptr(), function) })
}

fn add_data_method(
  env: sys::napi_env,
  object: sys::napi_value,
  name: &str,
  callback: unsafe extern "C" fn(sys::napi_env, sys::napi_callback_info) -> sys::napi_value,
  data: *mut c_void,
) -> Result<()> {
  let name = CString::new(name)?;
  let mut function = ptr::null_mut();
  check_status!(unsafe {
    sys::napi_create_function(
      env,
      name.as_ptr(),
      name.as_bytes().len() as isize,
      Some(callback),
      data,
      &mut function,
    )
  })?;
  check_status!(unsafe { sys::napi_set_named_property(env, object, name.as_ptr(), function) })
}

fn set_named_property(
  env: sys::napi_env,
  object: sys::napi_value,
  name: &str,
  value: sys::napi_value,
) -> Result<()> {
  let name = CString::new(name)?;
  check_status!(unsafe { sys::napi_set_named_property(env, object, name.as_ptr(), value) })
}

fn call_named(
  env: sys::napi_env,
  this: sys::napi_value,
  name: &str,
  args: &[sys::napi_value],
) -> Result<sys::napi_value> {
  let name = CString::new(name)?;
  let mut function = ptr::null_mut();
  check_status!(unsafe { sys::napi_get_named_property(env, this, name.as_ptr(), &mut function) })?;
  let mut value_type = sys::ValueType::napi_undefined;
  check_status!(unsafe { sys::napi_typeof(env, function, &mut value_type) })?;
  if value_type != sys::ValueType::napi_function {
    return Err(Error::new(
      Status::InvalidArg,
      format!("Host.{name:?} is not callable"),
    ));
  }
  call_function(env, this, function, args)
}

fn named_property(
  env: sys::napi_env,
  object: sys::napi_value,
  name: &str,
) -> Result<sys::napi_value> {
  let name = CString::new(name)?;
  let mut value = ptr::null_mut();
  check_status!(unsafe { sys::napi_get_named_property(env, object, name.as_ptr(), &mut value) })?;
  Ok(value)
}

fn call_function(
  env: sys::napi_env,
  this: sys::napi_value,
  function: sys::napi_value,
  args: &[sys::napi_value],
) -> Result<sys::napi_value> {
  let mut result = ptr::null_mut();
  check_status!(unsafe {
    sys::napi_call_function(env, this, function, args.len(), args.as_ptr(), &mut result)
  })?;
  Ok(result)
}

fn is_promise(env: sys::napi_env, value: sys::napi_value) -> Result<bool> {
  let mut promise = false;
  check_status!(unsafe { sys::napi_is_promise(env, value, &mut promise) })?;
  Ok(promise)
}

fn release_callback_guard(active_callbacks: &RefCell<BTreeSet<CallbackKey>>, key: CallbackKey) {
  active_callbacks.borrow_mut().remove(&key);
}

fn clear_callback_guard_on_error(
  active_callbacks: &RefCell<BTreeSet<CallbackKey>>,
  guarded: bool,
  key: CallbackKey,
) {
  if guarded {
    release_callback_guard(active_callbacks, key);
  }
}

fn remove_callback_registrations(
  contracts: &mut BTreeMap<CallbackKey, Vec<RegisteredCallbackContract>>,
  tokens: &[CallbackRegistrationToken],
) {
  for registration in tokens {
    let remove_key = if let Some(entries) = contracts.get_mut(&registration.key) {
      entries.retain(|entry| entry.token != Some(registration.token));
      entries.is_empty()
    } else {
      false
    };
    if remove_key {
      contracts.remove(&registration.key);
    }
  }
}

fn create_reference(
  env: sys::napi_env,
  value: sys::napi_value,
  role: &str,
) -> Result<sys::napi_ref> {
  let mut reference = ptr::null_mut();
  check_status!(
    unsafe { sys::napi_create_reference(env, value, 1, &mut reference) },
    "failed to retain {role}"
  )?;
  Ok(reference)
}

fn create_weak_reference(
  env: sys::napi_env,
  value: sys::napi_value,
  role: &str,
) -> Result<sys::napi_ref> {
  let mut reference = ptr::null_mut();
  check_status!(
    unsafe { sys::napi_create_reference(env, value, 0, &mut reference) },
    "failed to retain weak {role} reference"
  )?;
  Ok(reference)
}

fn optional_reference(
  env: sys::napi_env,
  value: Option<sys::napi_value>,
  role: &str,
) -> Result<sys::napi_ref> {
  value
    .map(|value| create_reference(env, value, role))
    .transpose()
    .map(|value| value.unwrap_or(ptr::null_mut()))
}

fn reference_value(
  env: sys::napi_env,
  reference: sys::napi_ref,
  role: &str,
) -> Result<sys::napi_value> {
  if reference.is_null() {
    return Err(Error::new(
      Status::GenericFailure,
      format!("{role} reference is released"),
    ));
  }
  let mut value = ptr::null_mut();
  check_status!(
    unsafe { sys::napi_get_reference_value(env, reference, &mut value) },
    "failed to resolve {role}"
  )?;
  Ok(value)
}

fn delete_reference(env: sys::napi_env, reference: sys::napi_ref) {
  if !reference.is_null() {
    let _ = unsafe { sys::napi_delete_reference(env, reference) };
  }
}

fn strict_equals(env: sys::napi_env, left: sys::napi_value, right: sys::napi_value) -> bool {
  let mut result = false;
  (unsafe { sys::napi_strict_equals(env, left, right, &mut result) }) == sys::Status::napi_ok
    && result
}

fn value_u32(env: sys::napi_env, value: sys::napi_value, role: &str) -> Result<u32> {
  let mut result = 0;
  check_status!(
    unsafe { sys::napi_get_value_uint32(env, value, &mut result) },
    "{role} must be an unsigned 32-bit integer"
  )?;
  Ok(result)
}

fn js_u32(env: sys::napi_env, value: u32) -> Result<sys::napi_value> {
  let mut result = ptr::null_mut();
  check_status!(unsafe { sys::napi_create_uint32(env, value, &mut result) })?;
  Ok(result)
}

fn js_array(env: sys::napi_env, values: &[sys::napi_value]) -> Result<sys::napi_value> {
  let mut result = ptr::null_mut();
  check_status!(unsafe { sys::napi_create_array_with_length(env, values.len(), &mut result) })?;
  for (index, value) in values.iter().copied().enumerate() {
    check_status!(unsafe { sys::napi_set_element(env, result, index as u32, value) })?;
  }
  Ok(result)
}

fn js_undefined(env: sys::napi_env) -> Result<sys::napi_value> {
  let mut value = ptr::null_mut();
  check_status!(unsafe { sys::napi_get_undefined(env, &mut value) })?;
  Ok(value)
}

fn resolved_promise(env: sys::napi_env) -> Result<sys::napi_value> {
  let mut deferred = ptr::null_mut();
  let mut promise = ptr::null_mut();
  check_status!(unsafe { sys::napi_create_promise(env, &mut deferred, &mut promise) })?;
  check_status!(unsafe { sys::napi_resolve_deferred(env, deferred, js_undefined(env)?) })?;
  Ok(promise)
}

fn promise_all_settled(
  env: sys::napi_env,
  promises: &[sys::napi_value],
  setup_error: Option<Error>,
) -> Result<sys::napi_value> {
  let settled = if promises.is_empty() {
    resolved_promise(env)?
  } else {
    let mut global = ptr::null_mut();
    check_status!(unsafe { sys::napi_get_global(env, &mut global) })?;
    let promise_constructor = named_property(env, global, "Promise")?;
    let all_settled = named_property(env, promise_constructor, "allSettled")?;
    let promises = js_array(env, promises)?;
    call_function(env, promise_constructor, all_settled, &[promises])?
  };
  let settled = PromiseRaw::<Unknown<'static>>::new(env, settled);
  Ok(
    settled
      .then(move |_| match setup_error {
        Some(error) => Err(error),
        None => Ok(()),
      })?
      .raw(),
  )
}

fn callback_result(
  env: sys::napi_env,
  callback: impl FnOnce() -> Result<sys::napi_value>,
) -> sys::napi_value {
  match callback() {
    Ok(value) => value,
    Err(error) => {
      let reason = CString::new(error.reason.clone())
        .unwrap_or_else(|_| CString::new("UniFFI backend error").expect("literal is valid"));
      let _ = unsafe { sys::napi_throw_error(env, ptr::null(), reason.as_ptr()) };
      ptr::null_mut()
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn async_callback_guard_is_held_until_promise_settlement() {
    let key = CallbackKey {
      callback_type_id: 3,
      callback_id: 9,
    };
    let active_callbacks = RefCell::new(BTreeSet::from([key]));

    // `dispatch_callback_host` leaves this entry in place after receiving the
    // host Promise.  The settlement callback is the only path that removes it.
    assert!(active_callbacks.borrow().contains(&key));
    release_callback_guard(&active_callbacks, key);
    assert!(!active_callbacks.borrow().contains(&key));
  }

  #[test]
  fn callback_guard_is_cleared_when_host_dispatch_fails() {
    let key = CallbackKey {
      callback_type_id: 4,
      callback_id: 10,
    };
    let active_callbacks = RefCell::new(BTreeSet::from([key]));
    clear_callback_guard_on_error(&active_callbacks, true, key);
    assert!(!active_callbacks.borrow().contains(&key));
  }

  #[test]
  fn scoped_callback_settlement_preserves_overlapping_registrations() {
    let key = CallbackKey {
      callback_type_id: 5,
      callback_id: 12,
    };
    let contract = SessionCallbackArgument {
      argument_index: 0,
      callback_type_id: 5,
      retention: SessionCallbackRetention::Scoped,
      threading: SessionCallbackThreading::CallingThread,
      call_style: SessionCallbackCallStyle::Sync,
      error_style: SessionCallbackErrorStyle::Infallible,
      reentrancy: SessionCallbackReentrancy::Forbidden,
    };
    let mut registry = BTreeMap::from([(
      key,
      vec![
        RegisteredCallbackContract {
          token: None,
          contract: SessionCallbackArgument {
            retention: SessionCallbackRetention::Retained,
            ..contract
          },
        },
        RegisteredCallbackContract {
          token: Some(1),
          contract,
        },
        RegisteredCallbackContract {
          token: Some(2),
          contract,
        },
      ],
    )]);

    remove_callback_registrations(
      &mut registry,
      &[CallbackRegistrationToken { key, token: 1 }],
    );
    assert_eq!(registry[&key].len(), 2);
    assert!(registry[&key].iter().any(|entry| entry.token.is_none()));
    assert!(registry[&key].iter().any(|entry| entry.token == Some(2)));

    remove_callback_registrations(
      &mut registry,
      &[CallbackRegistrationToken { key, token: 2 }],
    );
    assert_eq!(registry[&key].len(), 1);
    assert!(registry[&key][0].token.is_none());
  }
}
