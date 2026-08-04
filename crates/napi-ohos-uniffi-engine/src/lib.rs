//! Programmatic UniFFI engine support for Ark N-API.
//!
//! The engine is deliberately a consumer of frozen plans.  It does not walk
//! UniFFI component metadata, inspect process configuration, or read files.
//! `napi-family-core` owns the N-API family operation/carrier validation; this
//! crate only selects the OHOS hooks and supplies the Ark runtime session.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::ffi::CString;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::ptr;
use std::sync::{Arc, Mutex};

use napi_derive_backend_ohos::{NapiFn, NapiFnArg, NapiFnArgKind, NapiFnBuilder, TryToTokens};
use napi_family_core::{
  AsyncKind, BigIntWords, CallbackReentrancy, CallbackRetention, CallbackThreading,
  FamilyOperation, FamilyOperationTarget, FamilyPlan, FamilyPlanError, OperationKind,
  StreamDirection, ValuePathSegment,
};
use napi_ohos::bindgen_prelude::{BigInt, ToNapiValue};
use napi_ohos::{sys, Result as NapiResult};
use proc_macro2::{Ident, Span, TokenStream};
use quote::{format_ident, quote};

pub use napi_family_core;
mod plan;
pub use plan::*;
mod session;
pub use session::{
  create_backend_session, SessionCallbackArgument, SessionCallbackErrorStyle,
  SessionCallbackReentrancy, SessionCallbackRetention, SessionCallbackThreading, SessionNativeCall,
  SessionOperationDescriptor, SessionOperationDispatch, SessionResourceCallbacks,
  SessionResourceReceiver, SessionStreamArgument, SessionStreamDirection,
};

/// The single public native export installed by a generated OHOS module.
pub const BACKEND_FACTORY_EXPORT: &str = "__uniffi_backend_factory";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorDomain {
  Declared,
  Validation,
  Backend,
  Panic,
}

impl ErrorDomain {
  const fn as_str(self) -> &'static str {
    match self {
      Self::Declared => "declared",
      Self::Validation => "validation",
      Self::Backend => "backend",
      Self::Panic => "panic",
    }
  }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ErrorData {
  Null,
  Boolean(bool),
  Number(f64),
  BigInt(BigIntWords),
  String(String),
  Bytes(Vec<u8>),
  Sequence(Vec<ErrorData>),
  Record(BTreeMap<String, ErrorData>),
}

impl ToNapiValue for ErrorData {
  unsafe fn to_napi_value(env: sys::napi_env, value: Self) -> NapiResult<sys::napi_value> {
    match value {
      Self::Null => {
        let mut raw = ptr::null_mut();
        napi_ohos::check_status!(unsafe { sys::napi_get_null(env, &mut raw) })?;
        Ok(raw)
      }
      Self::Boolean(value) => unsafe { bool::to_napi_value(env, value) },
      Self::Number(value) => unsafe { f64::to_napi_value(env, value) },
      Self::BigInt(value) => unsafe {
        BigInt::to_napi_value(
          env,
          BigInt {
            sign_bit: value.negative,
            words: value.words,
          },
        )
      },
      Self::String(value) => unsafe { String::to_napi_value(env, value) },
      Self::Bytes(value) => create_uint8_array(env, &value),
      Self::Sequence(value) => unsafe { Vec::<ErrorData>::to_napi_value(env, value) },
      Self::Record(value) => {
        let object = create_object(env)?;
        for (name, value) in value {
          set_named(env, object, &name, value)?;
        }
        Ok(object)
      }
    }
  }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BridgeErrorDescriptor {
  pub domain: ErrorDomain,
  pub error_name: String,
  pub variant: Option<String>,
  pub data: ErrorData,
  pub message: String,
  pub native_stack: Option<String>,
}

impl BridgeErrorDescriptor {
  pub fn validation(message: impl Into<String>) -> Self {
    let message = message.into();
    Self {
      domain: ErrorDomain::Validation,
      error_name: "ValidationError".to_owned(),
      variant: None,
      data: ErrorData::String(message.clone()),
      message,
      native_stack: None,
    }
  }

  pub fn backend(message: impl Into<String>) -> Self {
    let message = message.into();
    Self {
      domain: ErrorDomain::Backend,
      error_name: "BackendError".to_owned(),
      variant: None,
      data: ErrorData::String(message.clone()),
      message,
      native_stack: None,
    }
  }
}

impl fmt::Display for BridgeErrorDescriptor {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(&self.message)
  }
}

impl ToNapiValue for BridgeErrorDescriptor {
  unsafe fn to_napi_value(env: sys::napi_env, value: Self) -> NapiResult<sys::napi_value> {
    let object = create_object(env)?;
    set_named(env, object, "domain", value.domain.as_str())?;
    set_named(env, object, "errorName", value.error_name)?;
    set_named(env, object, "variant", value.variant)?;
    set_named(env, object, "data", value.data)?;
    set_named(env, object, "message", value.message)?;
    if let Some(native_stack) = value.native_stack {
      set_named(env, object, "nativeStack", native_stack)?;
    }
    Ok(object)
  }
}

#[derive(Clone, Debug, PartialEq)]
pub enum OhosCallResult<T> {
  Value(T),
  Error(BridgeErrorDescriptor),
}

impl<T: ToNapiValue> ToNapiValue for OhosCallResult<T> {
  unsafe fn to_napi_value(env: sys::napi_env, value: Self) -> NapiResult<sys::napi_value> {
    let object = create_object(env)?;
    match value {
      Self::Value(value) => {
        set_named(env, object, "kind", "value")?;
        set_named(env, object, "value", value)?;
      }
      Self::Error(error) => {
        set_named(env, object, "kind", "error")?;
        set_named(env, object, "error", error)?;
      }
    }
    Ok(object)
  }
}

unsafe fn create_object(env: sys::napi_env) -> NapiResult<sys::napi_value> {
  let mut object = ptr::null_mut();
  napi_ohos::check_status!(unsafe { sys::napi_create_object(env, &mut object) })?;
  Ok(object)
}

unsafe fn create_uint8_array(env: sys::napi_env, value: &[u8]) -> NapiResult<sys::napi_value> {
  let mut data = ptr::null_mut();
  let mut array_buffer = ptr::null_mut();
  napi_ohos::check_status!(unsafe {
    sys::napi_create_arraybuffer(env, value.len(), &mut data, &mut array_buffer)
  })?;
  if !value.is_empty() {
    unsafe { ptr::copy_nonoverlapping(value.as_ptr(), data.cast::<u8>(), value.len()) };
  }
  let mut typed_array = ptr::null_mut();
  napi_ohos::check_status!(unsafe {
    sys::napi_create_typedarray(
      env,
      sys::TypedarrayType::uint8_array,
      value.len(),
      array_buffer,
      0,
      &mut typed_array,
    )
  })?;
  Ok(typed_array)
}

unsafe fn set_named<T: ToNapiValue>(
  env: sys::napi_env,
  object: sys::napi_value,
  name: &str,
  value: T,
) -> NapiResult<()> {
  let name = CString::new(name)?;
  let value = unsafe { T::to_napi_value(env, value)? };
  napi_ohos::check_status!(unsafe {
    sys::napi_set_named_property(env, object, name.as_ptr(), value)
  })
}

/// Ark-specific runtime hooks selected independently from the shared family
/// plan.  Keeping these as data makes Node/Ark differences explicit and easy
/// to test without constructing an Ark runtime in a build-time process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OhosRuntimeHooks {
  pub module_registration: OhosModuleRegistration,
  pub async_scheduler: OhosAsyncScheduler,
  pub callback_dispatch: OhosCallbackDispatch,
  pub cleanup_queue: OhosCleanupQueue,
}

impl Default for OhosRuntimeHooks {
  fn default() -> Self {
    Self {
      module_registration: OhosModuleRegistration::ArkNapi,
      async_scheduler: OhosAsyncScheduler::ArkEventLoop,
      callback_dispatch: OhosCallbackDispatch::PriorityThreadsafeFunction,
      cleanup_queue: OhosCleanupQueue::ArkRuntime,
    }
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OhosModuleRegistration {
  ArkNapi,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OhosAsyncScheduler {
  ArkEventLoop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OhosCallbackDispatch {
  PriorityThreadsafeFunction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OhosCleanupQueue {
  ArkRuntime,
}

/// A lossless value at the private Ark host boundary.
#[derive(Clone, Debug, PartialEq)]
pub enum OhosValue {
  Unit,
  Null,
  Boolean(bool),
  Number(f64),
  BigInt { negative: bool, words: Vec<u64> },
  String(String),
  Bytes(Vec<u8>),
  Object(u32),
  Callback(u32),
  InputStream(u32),
  OutputStream(u32),
}

impl OhosValue {
  pub const fn bigint(negative: bool, words: Vec<u64>) -> Self {
    Self::BigInt { negative, words }
  }
}

/// A small, host-neutral error that can be carried through the session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OhosError {
  message: String,
}

impl OhosError {
  pub fn new(message: impl Into<String>) -> Self {
    Self {
      message: message.into(),
    }
  }

  pub fn message(&self) -> &str {
    &self.message
  }
}

impl fmt::Display for OhosError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(&self.message)
  }
}

impl Error for OhosError {}

type OhosFuture<'a> = Pin<Box<dyn Future<Output = Result<OhosValue, OhosError>> + 'a>>;

/// The only runtime input required by the OHOS backend session.
///
/// An application adapter implements this trait with `napi-ohos`/Ark values.
/// The engine never stores an Ark `napi_env` in its plan and never fabricates
/// public declarations for these operations.
pub trait OhosHost: Send + 'static {
  fn invoke_sync(&mut self, operation_id: u32, args: &[OhosValue]) -> Result<OhosValue, OhosError>;

  fn invoke_async<'a>(&'a mut self, operation_id: u32, args: Vec<OhosValue>) -> OhosFuture<'a>;

  fn invoke_callback(
    &mut self,
    callback_type: u32,
    callback_id: u32,
    method_id: u32,
    args: &[OhosValue],
  ) -> Result<OhosValue, OhosError>;

  fn retain_callback(&mut self, _callback_type: u32, _callback_id: u32) -> Result<(), OhosError> {
    Ok(())
  }

  fn release_callback(&mut self, _callback_type: u32, _callback_id: u32) -> Result<(), OhosError> {
    Ok(())
  }

  fn pull_input_stream(&mut self, stream_id: u32) -> Result<OhosValue, OhosError>;

  fn cancel_input_stream(&mut self, stream_id: u32) -> Result<(), OhosError>;

  fn release_input_stream(&mut self, _stream_id: u32) -> Result<(), OhosError> {
    Ok(())
  }

  fn next_output_stream(&mut self, stream_id: u32) -> Result<OhosValue, OhosError>;

  fn cancel_output_stream(&mut self, stream_id: u32) -> Result<(), OhosError>;

  fn release_output_stream(&mut self, _stream_id: u32) -> Result<(), OhosError> {
    Ok(())
  }

  fn release_object(&mut self, object_id: u32) -> Result<(), OhosError>;
}

/// A generated OHOS module descriptor.  Raw operation names are private
/// implementation details; only the backend factory is public.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneratedOhosModule {
  family: FamilyPlan,
  hooks: OhosRuntimeHooks,
  rust: OhosBridgePlan,
}

/// Source emitted by the OHOS frontend through the programmatic
/// `napi-derive-backend-ohos` AST.  Raw operation callbacks are kept private;
/// only the backend factory is registered as a module export.
#[derive(Clone, Debug)]
pub struct GeneratedOhosSource {
  module: GeneratedOhosModule,
  source: TokenStream,
}

impl GeneratedOhosSource {
  pub fn module(&self) -> &GeneratedOhosModule {
    &self.module
  }

  pub fn source(&self) -> &TokenStream {
    &self.source
  }
}

impl GeneratedOhosModule {
  pub fn family(&self) -> &FamilyPlan {
    &self.family
  }

  pub const fn hooks(&self) -> OhosRuntimeHooks {
    self.hooks
  }

  pub fn operations(&self) -> &[OhosOperationPlan] {
    self.rust.operations()
  }

  pub fn public_exports(&self) -> impl Iterator<Item = &'static str> {
    std::iter::once(BACKEND_FACTORY_EXPORT)
  }

  pub fn raw_operation_names(&self) -> impl Iterator<Item = String> + '_ {
    self
      .rust
      .operations()
      .iter()
      .map(|operation| format!("__uniffi_raw_operation_{}", operation.operation_id))
  }
}

/// Build one OHOS engine module from frozen bridge and Rust operation plans.
pub fn generate_ohos_module(
  family: &FamilyPlan,
  rust: OhosBridgePlan,
) -> Result<GeneratedOhosModule, OhosEngineError> {
  if family.operations().len() != rust.operations().len() {
    return Err(OhosEngineError::OperationCount {
      expected: family.operations().len(),
      actual: rust.operations().len(),
    });
  }
  Ok(GeneratedOhosModule {
    family: family.clone(),
    hooks: OhosRuntimeHooks::default(),
    rust,
  })
}

/// Lower the already validated OHOS operation table through the same backend
/// AST/codegen used by `#[napi]`.  The factory body intentionally forwards the
/// host object to the concrete adapter; no public raw operation is emitted.
pub fn generate_ohos_source(
  family: &FamilyPlan,
  rust: OhosBridgePlan,
) -> Result<GeneratedOhosSource, OhosEngineError> {
  let module = generate_ohos_module(family, rust)?;
  let mut source = TokenStream::new();
  let mut callbacks = Vec::with_capacity(module.operations().len());

  for (family_operation, operation) in module.family().operations().iter().zip(module.operations())
  {
    if family_operation.id != operation.operation_id {
      return Err(OhosEngineError::NonDenseOperation {
        expected: family_operation.id,
        actual: operation.operation_id,
      });
    }
    if family_operation.target == FamilyOperationTarget::Native {
      let generated = generate_operation(operation, family_operation)?;
      source.extend(generated.body);
      generated
        .function
        .try_to_tokens(&mut source)
        .map_err(|error| OhosEngineError::Codegen(format!("{error:?}")))?;
      callbacks.push(Some(generated.callback_factory));
    } else {
      callbacks.push(None);
    }
  }

  let resource_callbacks = generate_resource_callbacks(module.rust.resource_hooks(), &mut source)?;

  let factory = Ident::new("__uniffi_backend_factory", Span::call_site());
  let descriptors = module
    .family()
    .operations()
    .iter()
    .zip(module.operations())
    .zip(callbacks.iter())
    .map(|((family_operation, operation), callback)| {
      session_descriptor_tokens(family_operation, operation, callback.as_ref())
    })
    .collect::<Result<Vec<_>, _>>()?;
  let release_object = optional_callback_factory(resource_callbacks.release_object.as_ref());
  let cancel_output_stream =
    optional_callback_factory(resource_callbacks.cancel_output_stream.as_ref());
  let release_output_stream =
    optional_callback_factory(resource_callbacks.release_output_stream.as_ref());
  source.extend(quote! {
    #[doc(hidden)]
    fn #factory(
      env: &napi_ohos::Env,
      host: napi_ohos::bindgen_prelude::Object<'static>,
    ) -> napi_ohos::Result<napi_ohos::bindgen_prelude::Object<'static>> {
      napi_ohos_uniffi_engine::create_backend_session(
        env,
        host,
        vec![#(#descriptors),*],
        napi_ohos_uniffi_engine::SessionResourceCallbacks {
          release_object: #release_object,
          cancel_output_stream: #cancel_output_stream,
          release_output_stream: #release_output_stream,
        },
      )
    }
  });
  let factory_function = NapiFnBuilder::new(factory, BACKEND_FACTORY_EXPORT)
    .argument(NapiFnArg {
      kind: NapiFnArgKind::PatType(Box::new(syn::parse_quote!(env: &napi_ohos::Env))),
      ts_arg_type: None,
    })
    .argument(NapiFnArg {
      kind: NapiFnArgKind::PatType(Box::new(syn::parse_quote!(
        host: napi_ohos::bindgen_prelude::Object<'static>
      ))),
      ts_arg_type: None,
    })
    .result_return_type(syn::parse_quote!(
      napi_ohos::bindgen_prelude::Object<'static>
    ))
    .strict(true)
    .skip_typescript(true)
    .register_name(Ident::new(
      "__napi_register_uniffi_backend_factory",
      Span::call_site(),
    ))
    .build();
  factory_function
    .try_to_tokens(&mut source)
    .map_err(|error| OhosEngineError::Codegen(format!("{error:?}")))?;

  Ok(GeneratedOhosSource { module, source })
}

struct GeneratedOperation {
  body: TokenStream,
  function: NapiFn,
  callback_factory: Ident,
}

fn generate_operation(
  operation: &OhosOperationPlan,
  family: &FamilyOperation,
) -> Result<GeneratedOperation, OhosEngineError> {
  let OhosOperationTarget::Native { call } = &operation.target else {
    return Err(OhosEngineError::InvalidOperationTarget {
      operation_id: operation.operation_id,
      kind: family.kind,
    });
  };
  let id = operation.operation_id;
  let function_name = format_ident!("__uniffi_raw_operation_{id}");
  let callback_factory = format_ident!("_napi_rs_internal_register___uniffi_raw_operation_{id}");
  let register_name = format_ident!("__napi_register_uniffi_raw_operation_{id}");
  let return_carrier = operation.return_binding.carrier_type();
  let requires_host = operation.arguments.iter().any(|argument| {
    matches!(
      argument.binding,
      OhosArgumentBinding::CallbackProxy { .. } | OhosArgumentBinding::InputStreamProxy { .. }
    )
  });

  let mut builder = NapiFnBuilder::new(function_name.clone(), function_name.to_string());
  if requires_host {
    builder = builder.argument(NapiFnArg {
      kind: NapiFnArgKind::PatType(Box::new(syn::parse_quote!(
        __uniffi_host: napi_ohos::bindgen_prelude::Object<'static>
      ))),
      ts_arg_type: None,
    });
  }

  let mut argument_names = Vec::with_capacity(operation.arguments.len() + 1);
  let mut lowerings = Vec::new();
  if let Some(receiver) = &operation.receiver {
    let name = &receiver.name;
    let carrier_type = receiver.binding.carrier_type();
    builder = builder.argument(NapiFnArg {
      kind: NapiFnArgKind::PatType(Box::new(syn::parse_quote!(#name: #carrier_type))),
      ts_arg_type: None,
    });
    argument_names.push(name.clone());
    let lower = match &receiver.binding {
      OhosArgumentBinding::ObjectLease { lower, .. }
      | OhosArgumentBinding::OutputStreamLease { lower, .. } => lower,
      _ => {
        return Err(OhosEngineError::InvalidObjectReceiver {
          operation_id: operation.operation_id,
        })
      }
    };
    lowerings.push(quote! {
      let #name = match #lower(#name) {
        Ok(value) => value,
        Err(error) => return napi_ohos_uniffi_engine::OhosCallResult::Error(error),
      };
    });
  }

  let host_arg = Ident::new("arg0", Span::call_site());
  let first_operation_arg = usize::from(requires_host) + usize::from(operation.receiver.is_some());
  let mut pre_call = TokenStream::new();
  for (argument_index, argument) in operation.arguments.iter().enumerate() {
    let name = &argument.name;
    let carrier_type = argument.binding.carrier_type();
    builder = builder.argument(NapiFnArg {
      kind: NapiFnArgKind::PatType(Box::new(syn::parse_quote!(#name: #carrier_type))),
      ts_arg_type: None,
    });
    argument_names.push(name.clone());
    match &argument.binding {
      OhosArgumentBinding::Direct { .. } => {}
      OhosArgumentBinding::I64BigInt => lowerings.push(quote! {
        let (__uniffi_value, __uniffi_lossless) = #name.get_i64();
        let #name = match napi_ohos_uniffi_engine::napi_family_core::require_lossless_i64(
          __uniffi_value,
          __uniffi_lossless,
        ) {
          Ok(value) => value,
          Err(error) => return napi_ohos_uniffi_engine::OhosCallResult::Error(
            napi_ohos_uniffi_engine::BridgeErrorDescriptor::validation(error.to_string()),
          ),
        };
      }),
      OhosArgumentBinding::U64BigInt => lowerings.push(quote! {
        let (__uniffi_negative, __uniffi_value, __uniffi_lossless) = #name.get_u64();
        let #name = match napi_ohos_uniffi_engine::napi_family_core::require_lossless_u64(
          __uniffi_negative,
          __uniffi_value,
          __uniffi_lossless,
        ) {
          Ok(value) => value,
          Err(error) => return napi_ohos_uniffi_engine::OhosCallResult::Error(
            napi_ohos_uniffi_engine::BridgeErrorDescriptor::validation(error.to_string()),
          ),
        };
      }),
      OhosArgumentBinding::LowerWith { lower, .. }
      | OhosArgumentBinding::ObjectLease { lower, .. }
      | OhosArgumentBinding::OutputStreamLease { lower, .. } => lowerings.push(quote! {
        let #name = match #lower(#name) {
          Ok(value) => value,
          Err(error) => return napi_ohos_uniffi_engine::OhosCallResult::Error(error),
        };
      }),
      OhosArgumentBinding::CallbackProxy { build, .. } => {
        let callback = family
          .callbacks
          .iter()
          .find(|use_site| {
            matches!(
              use_site.path.segments(),
              [ValuePathSegment::Argument(index)] if *index as usize == argument_index
            )
          })
          .ok_or(OhosEngineError::MissingStructuredUseSite {
            operation_id: operation.operation_id,
            argument: argument_index,
            role: "callback",
          })?;
        let callback_type_id = callback.callback_type_id;
        let contract = callback_contract_tokens(callback, argument_index as u32);
        let wrapper_arg = Ident::new(
          &format!("arg{}", first_operation_arg + argument_index),
          Span::call_site(),
        );
        pre_call.extend(quote! {
          let #wrapper_arg = #build(
            &#host_arg,
            #callback_type_id,
            #wrapper_arg,
            #contract,
          )?;
        });
      }
      OhosArgumentBinding::InputStreamProxy { build, .. } => {
        let found = family.streams.iter().any(|use_site| {
          use_site.direction == StreamDirection::Input
            && matches!(
              use_site.path.segments(),
              [ValuePathSegment::Argument(index)] if *index as usize == argument_index
            )
        });
        if !found {
          return Err(OhosEngineError::MissingStructuredUseSite {
            operation_id: operation.operation_id,
            argument: argument_index,
            role: "input stream",
          });
        }
        let wrapper_arg = Ident::new(
          &format!("arg{}", first_operation_arg + argument_index),
          Span::call_site(),
        );
        pre_call.extend(quote! {
          let #wrapper_arg = #build(&#host_arg, #wrapper_arg)?;
        });
      }
    }
  }

  let invoke = if family.async_kind == AsyncKind::Async {
    quote!(#call(#(#argument_names),*).await)
  } else {
    quote!(#call(#(#argument_names),*))
  };
  let value = match &operation.error_binding {
    OhosErrorBinding::Infallible => quote!(let __uniffi_value = #invoke;),
    OhosErrorBinding::Descriptor { map } => quote! {
      let __uniffi_value = match #invoke {
        Ok(value) => value,
        Err(error) => return napi_ohos_uniffi_engine::OhosCallResult::Error(#map(error)),
      };
    },
  };
  let lift = match &operation.return_binding {
    OhosReturnBinding::Unit | OhosReturnBinding::Direct { .. } => quote!(__uniffi_value),
    OhosReturnBinding::I64BigInt | OhosReturnBinding::U64BigInt => quote!({
      let parts = napi_ohos_uniffi_engine::napi_family_core::BigIntWords::from(__uniffi_value);
      napi_ohos::bindgen_prelude::BigInt {
        sign_bit: parts.negative,
        words: parts.words,
      }
    }),
    OhosReturnBinding::LiftWith { lift, .. }
    | OhosReturnBinding::ObjectLease { lift, .. }
    | OhosReturnBinding::CallbackLease { lift, .. }
    | OhosReturnBinding::OutputStreamLease { lift, .. } => quote!({
      match #lift(__uniffi_value) {
        Ok(value) => value,
        Err(error) => return napi_ohos_uniffi_engine::OhosCallResult::Error(error),
      }
    }),
  };
  let async_token = (family.async_kind == AsyncKind::Async).then(|| quote!(async));
  let receiver_declaration = operation.receiver.iter().map(|receiver| {
    let name = &receiver.name;
    let ty = receiver.binding.rust_parameter_type();
    quote!(#name: #ty,)
  });
  let argument_declarations = operation.arguments.iter().map(|argument| {
    let name = &argument.name;
    let ty = argument.binding.rust_parameter_type();
    quote!(#name: #ty)
  });
  let body = quote! {
    #[doc(hidden)]
    #[allow(clippy::all)]
    #async_token fn #function_name(
      #(#receiver_declaration)*
      #(#argument_declarations),*
    ) -> napi_ohos_uniffi_engine::OhosCallResult<#return_carrier> {
      #(#lowerings)*
      #value
      napi_ohos_uniffi_engine::OhosCallResult::Value(#lift)
    }
  };
  let function = builder
    .return_type(syn::parse_quote!(
      napi_ohos_uniffi_engine::OhosCallResult<#return_carrier>
    ))
    .pre_call(pre_call)
    .leading_wrapper_args(usize::from(requires_host))
    .asynchronous(family.async_kind == AsyncKind::Async)
    .strict(true)
    .skip_typescript(true)
    .private(true)
    .register_name(register_name)
    .build();
  Ok(GeneratedOperation {
    body,
    function,
    callback_factory,
  })
}

fn callback_contract_tokens(
  callback: &napi_family_core::CallbackUseSite,
  argument_index: u32,
) -> TokenStream {
  let callback_type_id = callback.callback_type_id;
  let retention = match callback.contract.retention {
    CallbackRetention::Scoped => {
      quote!(napi_ohos_uniffi_engine::SessionCallbackRetention::Scoped)
    }
    CallbackRetention::Retained => {
      quote!(napi_ohos_uniffi_engine::SessionCallbackRetention::Retained)
    }
  };
  let threading = match callback.contract.threading {
    CallbackThreading::CallingThread => {
      quote!(napi_ohos_uniffi_engine::SessionCallbackThreading::CallingThread)
    }
    CallbackThreading::MayCrossThread => {
      quote!(napi_ohos_uniffi_engine::SessionCallbackThreading::MayCrossThread)
    }
  };
  let reentrancy = match callback.contract.reentrancy {
    CallbackReentrancy::Allowed => {
      quote!(napi_ohos_uniffi_engine::SessionCallbackReentrancy::Allowed)
    }
    CallbackReentrancy::Forbidden => {
      quote!(napi_ohos_uniffi_engine::SessionCallbackReentrancy::Forbidden)
    }
  };
  quote! {
    napi_ohos_uniffi_engine::SessionCallbackArgument {
      argument_index: #argument_index,
      callback_type_id: #callback_type_id,
      retention: #retention,
      threading: #threading,
      reentrancy: #reentrancy,
    }
  }
}

struct GeneratedResourceCallbacks {
  release_object: Option<Ident>,
  cancel_output_stream: Option<Ident>,
  release_output_stream: Option<Ident>,
}

fn generate_resource_callbacks(
  hooks: &OhosResourceHooks,
  source: &mut TokenStream,
) -> Result<GeneratedResourceCallbacks, OhosEngineError> {
  Ok(GeneratedResourceCallbacks {
    release_object: hooks
      .release_object
      .as_ref()
      .map(|hook| generate_resource_callback("release_object", hook, false, source))
      .transpose()?,
    cancel_output_stream: hooks
      .cancel_output_stream
      .as_ref()
      .map(|hook| generate_resource_callback("cancel_output_stream", hook, true, source))
      .transpose()?,
    release_output_stream: hooks
      .release_output_stream
      .as_ref()
      .map(|hook| generate_resource_callback("release_output_stream", hook, false, source))
      .transpose()?,
  })
}

fn generate_resource_callback(
  name: &str,
  hook: &OhosResourceHook,
  asynchronous: bool,
  source: &mut TokenStream,
) -> Result<Ident, OhosEngineError> {
  let function_name = format_ident!("__uniffi_{name}");
  let callback_factory = format_ident!("_napi_rs_internal_register___uniffi_{name}");
  let register_name = format_ident!("__napi_register_uniffi_{name}");
  let carrier_type = &hook.carrier_type;
  let call = &hook.call;
  let invoke = if asynchronous {
    quote!(#call(handle).await)
  } else {
    quote!(#call(handle))
  };
  let async_token = asynchronous.then(|| quote!(async));
  source.extend(quote! {
    #[doc(hidden)]
    #async_token fn #function_name(
      handle: #carrier_type,
    ) -> napi_ohos::Result<()> {
      match #invoke {
        Ok(()) => Ok(()),
        Err(error) => Err(napi_ohos::Error::new(
          napi_ohos::Status::GenericFailure,
          error.to_string(),
        )),
      }
    }
  });
  let function = NapiFnBuilder::new(function_name.clone(), function_name.to_string())
    .argument(NapiFnArg {
      kind: NapiFnArgKind::PatType(Box::new(syn::parse_quote!(handle: #carrier_type))),
      ts_arg_type: None,
    })
    .result_return_type(syn::parse_quote!(()))
    .asynchronous(asynchronous)
    .strict(true)
    .skip_typescript(true)
    .private(true)
    .register_name(register_name)
    .build();
  function
    .try_to_tokens(source)
    .map_err(|error| OhosEngineError::Codegen(format!("{error:?}")))?;
  Ok(callback_factory)
}

fn optional_callback_factory(callback: Option<&Ident>) -> TokenStream {
  callback.map_or_else(
    || quote!(None),
    |callback| quote!(Some(unsafe { #callback(env.raw())? })),
  )
}

fn session_descriptor_tokens(
  family_operation: &napi_family_core::FamilyOperation,
  operation: &OhosOperationPlan,
  callback: Option<&Ident>,
) -> Result<TokenStream, OhosEngineError> {
  let operation_id = family_operation.id;
  let dispatch = match family_operation.target {
    FamilyOperationTarget::Native => match family_operation.async_kind {
      AsyncKind::Sync => quote!(napi_ohos_uniffi_engine::SessionOperationDispatch::NativeSync),
      AsyncKind::Async => quote!(napi_ohos_uniffi_engine::SessionOperationDispatch::NativeAsync),
    },
    FamilyOperationTarget::CallbackHost {
      callback_type_id,
      method_id,
    } => {
      let error_style = if family_operation.fallible {
        quote!(napi_ohos_uniffi_engine::SessionCallbackErrorStyle::Fallible)
      } else {
        quote!(napi_ohos_uniffi_engine::SessionCallbackErrorStyle::Infallible)
      };
      match family_operation.async_kind {
        AsyncKind::Sync => quote! {
          napi_ohos_uniffi_engine::SessionOperationDispatch::CallbackHostSync {
            callback_type_id: #callback_type_id,
            method_id: #method_id,
            error_style: #error_style,
          }
        },
        AsyncKind::Async => quote! {
          napi_ohos_uniffi_engine::SessionOperationDispatch::CallbackHostAsync {
            callback_type_id: #callback_type_id,
            method_id: #method_id,
            error_style: #error_style,
          }
        },
      }
    }
    FamilyOperationTarget::InputStreamHostPull => {
      if family_operation.async_kind != AsyncKind::Async {
        return Err(OhosEngineError::Codegen(
          "input-stream pull must be async".into(),
        ));
      }
      quote!(napi_ohos_uniffi_engine::SessionOperationDispatch::InputStreamHostPull)
    }
    FamilyOperationTarget::InputStreamHostCancel => {
      if family_operation.async_kind != AsyncKind::Async {
        return Err(OhosEngineError::Codegen(
          "input-stream cancel must be async".into(),
        ));
      }
      quote!(napi_ohos_uniffi_engine::SessionOperationDispatch::InputStreamHostCancel)
    }
  };
  let callback = match (family_operation.target, callback) {
    (FamilyOperationTarget::Native, Some(callback)) => {
      quote!(Some(unsafe { #callback(env.raw())? }))
    }
    (FamilyOperationTarget::Native, None) => {
      return Err(OhosEngineError::Codegen(format!(
        "native operation {operation_id} has no callback"
      )))
    }
    (_, None) => quote!(None),
    (_, Some(_)) => {
      return Err(OhosEngineError::Codegen(format!(
        "host operation {operation_id} has a native callback"
      )))
    }
  };
  let receiver = match operation_receiver(family_operation) {
    None => quote!(None),
    Some(SessionResourceReceiver::Object) => quote!(Some(
      napi_ohos_uniffi_engine::SessionResourceReceiver::Object
    )),
    Some(SessionResourceReceiver::OutputStream) => quote!(Some(
      napi_ohos_uniffi_engine::SessionResourceReceiver::OutputStream
    )),
  };
  let result_receiver = match operation_result_receiver(family_operation) {
    None => quote!(None),
    Some(SessionResourceReceiver::Object) => quote!(Some(
      napi_ohos_uniffi_engine::SessionResourceReceiver::Object
    )),
    Some(SessionResourceReceiver::OutputStream) => quote!(Some(
      napi_ohos_uniffi_engine::SessionResourceReceiver::OutputStream
    )),
  };
  let native_call = if operation.arguments.iter().any(|argument| {
    matches!(
      argument.binding,
      OhosArgumentBinding::CallbackProxy { .. } | OhosArgumentBinding::InputStreamProxy { .. }
    )
  }) {
    quote!(napi_ohos_uniffi_engine::SessionNativeCall::HostAndArguments)
  } else {
    quote!(napi_ohos_uniffi_engine::SessionNativeCall::ArgumentsOnly)
  };
  let callback_arguments = family_operation
    .callbacks
    .iter()
    .map(|use_site| {
      let [ValuePathSegment::Argument(argument_index)] = use_site.path.segments() else {
        return Err(OhosEngineError::Codegen(format!(
          "unsupported callback path for operation {operation_id}: {}",
          use_site.path
        )));
      };
      let callback_type_id = use_site.callback_type_id;
      let retention = match use_site.contract.retention {
        CallbackRetention::Scoped => {
          quote!(napi_ohos_uniffi_engine::SessionCallbackRetention::Scoped)
        }
        CallbackRetention::Retained => {
          quote!(napi_ohos_uniffi_engine::SessionCallbackRetention::Retained)
        }
      };
      let threading = match use_site.contract.threading {
        CallbackThreading::CallingThread => {
          quote!(napi_ohos_uniffi_engine::SessionCallbackThreading::CallingThread)
        }
        CallbackThreading::MayCrossThread => {
          quote!(napi_ohos_uniffi_engine::SessionCallbackThreading::MayCrossThread)
        }
      };
      let reentrancy = match use_site.contract.reentrancy {
        CallbackReentrancy::Allowed => {
          quote!(napi_ohos_uniffi_engine::SessionCallbackReentrancy::Allowed)
        }
        CallbackReentrancy::Forbidden => {
          quote!(napi_ohos_uniffi_engine::SessionCallbackReentrancy::Forbidden)
        }
      };
      Ok(quote! {
        napi_ohos_uniffi_engine::SessionCallbackArgument {
          argument_index: #argument_index,
          callback_type_id: #callback_type_id,
          retention: #retention,
          threading: #threading,
          reentrancy: #reentrancy,
        }
      })
    })
    .collect::<Result<Vec<_>, _>>()?;
  let stream_arguments = family_operation
    .streams
    .iter()
    .filter(|use_site| use_site.direction == StreamDirection::Input)
    .map(|use_site| {
      let [ValuePathSegment::Argument(argument_index)] = use_site.path.segments() else {
        return Err(OhosEngineError::Codegen(format!(
          "unsupported input-stream path for operation {operation_id}: {}",
          use_site.path
        )));
      };
      Ok(quote! {
        napi_ohos_uniffi_engine::SessionStreamArgument {
          argument_index: #argument_index,
          direction: napi_ohos_uniffi_engine::SessionStreamDirection::Input,
        }
      })
    })
    .collect::<Result<Vec<_>, _>>()?;
  Ok(quote! {
    napi_ohos_uniffi_engine::SessionOperationDescriptor {
      dispatch: #dispatch,
      callback: #callback,
      native_call: #native_call,
      receiver: #receiver,
      result_receiver: #result_receiver,
      callback_arguments: vec![#(#callback_arguments),*],
      stream_arguments: vec![#(#stream_arguments),*],
    }
  })
}

fn operation_receiver(
  operation: &napi_family_core::FamilyOperation,
) -> Option<SessionResourceReceiver> {
  operation.receiver.as_ref().map(|_| {
    if matches!(
      operation.kind,
      OperationKind::OutputStreamNext | OperationKind::OutputStreamCancel
    ) {
      SessionResourceReceiver::OutputStream
    } else {
      SessionResourceReceiver::Object
    }
  })
}

fn operation_result_receiver(
  operation: &napi_family_core::FamilyOperation,
) -> Option<SessionResourceReceiver> {
  match operation.result.map(|resource| resource.kind) {
    Some(napi_family_core::ResourceKind::Object) => Some(SessionResourceReceiver::Object),
    Some(napi_family_core::ResourceKind::OutputStream) => {
      Some(SessionResourceReceiver::OutputStream)
    }
    _ => operation.streams.iter().find_map(|stream| {
      (stream.direction == StreamDirection::Output).then_some(SessionResourceReceiver::OutputStream)
    }),
  }
}

#[derive(Debug, PartialEq, Eq)]
pub enum OhosEngineError {
  FamilyPlan(FamilyPlanError),
  DuplicateRustOperation {
    id: u32,
  },
  OperationCount {
    expected: usize,
    actual: usize,
  },
  TooManyOperations,
  MissingRustOperation {
    id: u32,
  },
  ArgumentCount {
    operation_id: u32,
    expected: usize,
    actual: usize,
  },
  DuplicateRustArgument {
    operation_id: u32,
    name: String,
  },
  InvalidArgumentBinding {
    operation_id: u32,
    argument: usize,
    expected: &'static str,
  },
  InvalidReturnBinding {
    operation_id: u32,
    expected: &'static str,
  },
  InvalidResourceResult {
    operation_id: u32,
  },
  InvalidStructuredBinding {
    operation_id: u32,
    argument: usize,
    role: &'static str,
  },
  MissingErrorDescriptor {
    operation_id: u32,
  },
  UnexpectedErrorDescriptor {
    operation_id: u32,
  },
  InvalidOperationTarget {
    operation_id: u32,
    kind: OperationKind,
  },
  HostOperationHasRustBindings {
    operation_id: u32,
  },
  MissingObjectReceiver {
    operation_id: u32,
  },
  UnexpectedObjectReceiver {
    operation_id: u32,
  },
  InvalidObjectReceiver {
    operation_id: u32,
  },
  MissingStructuredUseSite {
    operation_id: u32,
    argument: usize,
    role: &'static str,
  },
  MissingResourceHook {
    role: &'static str,
  },
  NonDenseOperation {
    expected: u32,
    actual: u32,
  },
  DispatchMismatch {
    operation_id: u32,
  },
  UnknownOperation {
    operation_id: u32,
  },
  Closed,
  Host(OhosError),
  ReentrancyForbidden {
    callback_type: u32,
    callback_id: u32,
  },
  InvalidValue {
    expected: &'static str,
  },
  Codegen(String),
}

impl fmt::Display for OhosEngineError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::FamilyPlan(error) => write!(formatter, "{error}"),
      Self::DuplicateRustOperation { id } => write!(formatter, "duplicate Rust operation ID {id}"),
      Self::OperationCount { expected, actual } => {
        write!(
          formatter,
          "OHOS operation table requires {expected} entries, found {actual}"
        )
      }
      Self::TooManyOperations => formatter.write_str("OHOS operation table exceeds u32"),
      Self::MissingRustOperation { id } => write!(formatter, "missing Rust operation ID {id}"),
      Self::ArgumentCount {
        operation_id,
        expected,
        actual,
      } => write!(
        formatter,
        "operation {operation_id} has {actual} Rust arguments; bridge requires {expected}"
      ),
      Self::DuplicateRustArgument { operation_id, name } => write!(
        formatter,
        "operation {operation_id} repeats Rust argument name {name:?}"
      ),
      Self::InvalidArgumentBinding {
        operation_id,
        argument,
        expected,
      } => write!(
        formatter,
        "operation {operation_id} argument {argument} requires {expected}"
      ),
      Self::InvalidReturnBinding {
        operation_id,
        expected,
      } => write!(
        formatter,
        "operation {operation_id} return requires {expected}"
      ),
      Self::InvalidResourceResult { operation_id } => write!(
        formatter,
        "operation {operation_id} has an incompatible resource result binding"
      ),
      Self::InvalidStructuredBinding {
        operation_id,
        argument,
        role,
      } => write!(
        formatter,
        "operation {operation_id} argument {argument} requires a structured {role} binding"
      ),
      Self::MissingErrorDescriptor { operation_id } => write!(
        formatter,
        "fallible operation {operation_id} has no error descriptor mapper"
      ),
      Self::UnexpectedErrorDescriptor { operation_id } => write!(
        formatter,
        "infallible operation {operation_id} unexpectedly has an error descriptor mapper"
      ),
      Self::InvalidOperationTarget { operation_id, kind } => write!(
        formatter,
        "operation {operation_id} has a Rust target incompatible with {kind:?}"
      ),
      Self::HostOperationHasRustBindings { operation_id } => write!(
        formatter,
        "host operation {operation_id} must not contain Rust call arguments or a receiver"
      ),
      Self::MissingObjectReceiver { operation_id } => {
        write!(
          formatter,
          "object operation {operation_id} has no resource receiver"
        )
      }
      Self::UnexpectedObjectReceiver { operation_id } => write!(
        formatter,
        "non-object operation {operation_id} unexpectedly has a resource receiver"
      ),
      Self::InvalidObjectReceiver { operation_id } => write!(
        formatter,
        "object operation {operation_id} requires a borrowed structured resource receiver"
      ),
      Self::MissingStructuredUseSite {
        operation_id,
        argument,
        role,
      } => write!(
        formatter,
        "operation {operation_id} argument {argument} has no structured {role} use-site contract"
      ),
      Self::MissingResourceHook { role } => {
        write!(formatter, "OHOS bridge plan has no structured {role} hook")
      }
      Self::NonDenseOperation { expected, actual } => {
        write!(
          formatter,
          "OHOS operation table is not dense: expected {expected}, found {actual}"
        )
      }
      Self::DispatchMismatch { operation_id } => {
        write!(
          formatter,
          "OHOS operation {operation_id} dispatch does not match family plan"
        )
      }
      Self::UnknownOperation { operation_id } => {
        write!(formatter, "unknown OHOS operation {operation_id}")
      }
      Self::Closed => formatter.write_str("OHOS backend session is closed"),
      Self::Host(error) => write!(formatter, "OHOS host error: {error}"),
      Self::ReentrancyForbidden {
        callback_type,
        callback_id,
      } => write!(
        formatter,
        "callback {callback_type}:{callback_id} is already active and forbids reentrancy"
      ),
      Self::InvalidValue { expected } => write!(formatter, "expected {expected}"),
      Self::Codegen(error) => write!(formatter, "OHOS N-API codegen failed: {error}"),
    }
  }
}

impl Error for OhosEngineError {}

impl From<OhosError> for OhosEngineError {
  fn from(error: OhosError) -> Self {
    Self::Host(error)
  }
}

#[derive(Default)]
struct SessionState {
  closed: bool,
  retained_callbacks: BTreeSet<(u32, u32)>,
  active_callbacks: BTreeSet<(u32, u32)>,
  input_streams: BTreeSet<u32>,
  output_streams: BTreeSet<u32>,
  cancelled_output_streams: BTreeSet<u32>,
  objects: BTreeSet<u32>,
}

/// A real operation-ID session bound to one Ark Host instance.
pub struct OhosBackendSession<H: OhosHost> {
  host: Arc<Mutex<H>>,
  family: FamilyPlan,
  state: Arc<Mutex<SessionState>>,
}

impl<H: OhosHost> OhosBackendSession<H> {
  fn new(module: &GeneratedOhosModule, host: H) -> Self {
    Self {
      host: Arc::new(Mutex::new(host)),
      family: module.family.clone(),
      state: Arc::new(Mutex::new(SessionState::default())),
    }
  }

  pub fn family(&self) -> &FamilyPlan {
    &self.family
  }

  pub fn invoke_sync(
    &self,
    operation_id: u32,
    args: &[OhosValue],
  ) -> Result<OhosValue, OhosEngineError> {
    self.ensure_open()?;
    let operation = self.operation(operation_id)?;
    self.track_inputs_and_objects(operation, args)?;
    if operation.async_kind != AsyncKind::Sync {
      return Err(OhosEngineError::InvalidValue {
        expected: "sync operation",
      });
    }
    let result = match operation.target {
      FamilyOperationTarget::Native => self
        .host
        .lock()
        .map_err(|_| OhosError::new("OHOS host mutex poisoned"))?
        .invoke_sync(operation_id, args)
        .map_err(Into::into),
      FamilyOperationTarget::CallbackHost {
        callback_type_id,
        method_id,
      } => {
        let callback = callback_id(args)?;
        self.invoke_callback_guarded(
          callback_type_id,
          callback,
          method_id,
          args,
          self.callback_reentrancy_for_method(operation, callback_type_id),
        )
      }
      FamilyOperationTarget::InputStreamHostPull => {
        let stream = stream_id(args)?;
        self
          .host
          .lock()
          .map_err(|_| OhosError::new("OHOS host mutex poisoned"))?
          .pull_input_stream(stream)
          .map_err(Into::into)
      }
      FamilyOperationTarget::InputStreamHostCancel => {
        let stream = stream_id(args)?;
        self
          .host
          .lock()
          .map_err(|_| OhosError::new("OHOS host mutex poisoned"))?
          .cancel_input_stream(stream)
          .map(|_| OhosValue::Unit)
          .map_err(Into::into)
      }
    };
    if let Ok(value) = &result {
      self.track_return_value(operation, value)?;
    }
    result
  }

  pub fn invoke_async(
    &self,
    operation_id: u32,
    args: Vec<OhosValue>,
  ) -> Result<OhosFuture<'static>, OhosEngineError> {
    self.ensure_open()?;
    let operation = self.operation(operation_id)?;
    self.track_inputs_and_objects(operation, &args)?;
    if operation.async_kind != AsyncKind::Async {
      return Err(OhosEngineError::InvalidValue {
        expected: "async operation",
      });
    }
    let host = Arc::clone(&self.host);
    let state = Arc::clone(&self.state);
    let target = operation.target;
    let callback_guard = match target {
      FamilyOperationTarget::CallbackHost {
        callback_type_id,
        method_id: _method_id,
      } => {
        let callback = callback_id(&args)?;
        let reentrancy = self.callback_reentrancy_for_method(operation, callback_type_id);
        if reentrancy == CallbackReentrancy::Forbidden {
          let mut locked = self
            .state
            .lock()
            .map_err(|_| OhosError::new("OHOS session mutex poisoned"))?;
          if !locked.active_callbacks.insert((callback_type_id, callback)) {
            return Err(OhosEngineError::ReentrancyForbidden {
              callback_type: callback_type_id,
              callback_id: callback,
            });
          }
        }
        Some((callback_type_id, callback, reentrancy))
      }
      _ => None,
    };
    Ok(Box::pin(async move {
      let result = match target {
        FamilyOperationTarget::Native => {
          host
            .lock()
            .map_err(|_| OhosError::new("OHOS host mutex poisoned"))?
            .invoke_async(operation_id, args)
            .await
        }
        FamilyOperationTarget::InputStreamHostPull => {
          let stream = stream_id(&args).map_err(|error| OhosError::new(error.to_string()))?;
          host
            .lock()
            .map_err(|_| OhosError::new("OHOS host mutex poisoned"))?
            .pull_input_stream(stream)
        }
        FamilyOperationTarget::InputStreamHostCancel => {
          let stream = stream_id(&args).map_err(|error| OhosError::new(error.to_string()))?;
          host
            .lock()
            .map_err(|_| OhosError::new("OHOS host mutex poisoned"))?
            .cancel_input_stream(stream)
            .map(|_| OhosValue::Unit)
        }
        FamilyOperationTarget::CallbackHost {
          callback_type_id,
          method_id,
        } => {
          // Async callbacks are dispatched by the Ark priority TSFN.  The
          // session guard remains authoritative for reentrancy.
          let callback = callback_id(&args).map_err(|error| OhosError::new(error.to_string()))?;
          host
            .lock()
            .map_err(|_| OhosError::new("OHOS host mutex poisoned"))?
            .invoke_callback(callback_type_id, callback, method_id, &args)
        }
      };
      if let Ok(value) = &result {
        if let Ok(mut locked) = state.lock() {
          match value {
            OhosValue::Object(id) => {
              locked.objects.insert(*id);
            }
            OhosValue::OutputStream(id) => {
              locked.output_streams.insert(*id);
              locked.cancelled_output_streams.remove(id);
            }
            _ => {}
          }
        }
      }
      if let Some((callback_type, callback_id, reentrancy)) = callback_guard {
        if reentrancy == CallbackReentrancy::Forbidden {
          if let Ok(mut locked) = state.lock() {
            locked
              .active_callbacks
              .remove(&(callback_type, callback_id));
          }
        }
      }
      result
    }))
  }

  pub fn invoke_callback(
    &self,
    callback_type: u32,
    callback_id: u32,
    method_id: u32,
    args: &[OhosValue],
  ) -> Result<OhosValue, OhosEngineError> {
    self.ensure_open()?;
    self.invoke_callback_guarded(
      callback_type,
      callback_id,
      method_id,
      args,
      self.callback_reentrancy_for_type(callback_type),
    )
  }

  fn invoke_callback_guarded(
    &self,
    callback_type: u32,
    callback_id: u32,
    method_id: u32,
    args: &[OhosValue],
    reentrancy: CallbackReentrancy,
  ) -> Result<OhosValue, OhosEngineError> {
    {
      let mut state = self
        .state
        .lock()
        .map_err(|_| OhosError::new("OHOS session mutex poisoned"))?;
      if reentrancy == CallbackReentrancy::Forbidden
        && !state.active_callbacks.insert((callback_type, callback_id))
      {
        return Err(OhosEngineError::ReentrancyForbidden {
          callback_type,
          callback_id,
        });
      }
    }
    let result = self
      .host
      .lock()
      .map_err(|_| OhosError::new("OHOS host mutex poisoned"))?
      .invoke_callback(callback_type, callback_id, method_id, args)
      .map_err(Into::into);
    if reentrancy == CallbackReentrancy::Forbidden {
      if let Ok(mut state) = self.state.lock() {
        state.active_callbacks.remove(&(callback_type, callback_id));
      }
    }
    result
  }

  pub fn retain_callback(
    &self,
    callback_type: u32,
    callback_id: u32,
  ) -> Result<(), OhosEngineError> {
    self.ensure_open()?;
    let inserted = self
      .state
      .lock()
      .map_err(|_| OhosError::new("OHOS session mutex poisoned"))?
      .retained_callbacks
      .insert((callback_type, callback_id));
    if inserted {
      self
        .host
        .lock()
        .map_err(|_| OhosError::new("OHOS host mutex poisoned"))?
        .retain_callback(callback_type, callback_id)?;
    }
    Ok(())
  }

  pub fn release_callback(
    &self,
    callback_type: u32,
    callback_id: u32,
  ) -> Result<(), OhosEngineError> {
    self.ensure_open()?;
    let removed = self
      .state
      .lock()
      .map_err(|_| OhosError::new("OHOS session mutex poisoned"))?
      .retained_callbacks
      .remove(&(callback_type, callback_id));
    if removed {
      self
        .host
        .lock()
        .map_err(|_| OhosError::new("OHOS host mutex poisoned"))?
        .release_callback(callback_type, callback_id)?;
    }
    Ok(())
  }

  pub fn cancel_input_stream(&self, stream_id: u32) -> Result<(), OhosEngineError> {
    self.ensure_open()?;
    self
      .host
      .lock()
      .map_err(|_| OhosError::new("OHOS host mutex poisoned"))?
      .cancel_input_stream(stream_id)?;
    Ok(())
  }

  pub fn release_input_stream(&self, stream_id: u32) -> Result<(), OhosEngineError> {
    self.ensure_open()?;
    let removed = self
      .state
      .lock()
      .map_err(|_| OhosError::new("OHOS session mutex poisoned"))?
      .input_streams
      .remove(&stream_id);
    if removed {
      self
        .host
        .lock()
        .map_err(|_| OhosError::new("OHOS host mutex poisoned"))?
        .release_input_stream(stream_id)?;
    }
    Ok(())
  }

  pub fn next_output_stream(&self, stream_id: u32) -> Result<OhosValue, OhosEngineError> {
    self.ensure_open()?;
    self
      .host
      .lock()
      .map_err(|_| OhosError::new("OHOS host mutex poisoned"))?
      .next_output_stream(stream_id)
      .map_err(Into::into)
  }

  pub fn cancel_output_stream(&self, stream_id: u32) -> Result<(), OhosEngineError> {
    self.ensure_open()?;
    let should_cancel = self
      .state
      .lock()
      .map_err(|_| OhosError::new("OHOS session mutex poisoned"))?
      .cancelled_output_streams
      .insert(stream_id);
    if should_cancel {
      self
        .host
        .lock()
        .map_err(|_| OhosError::new("OHOS host mutex poisoned"))?
        .cancel_output_stream(stream_id)?;
    }
    Ok(())
  }

  pub fn release_output_stream(&self, stream_id: u32) -> Result<(), OhosEngineError> {
    self.ensure_open()?;
    let removed = {
      let mut state = self
        .state
        .lock()
        .map_err(|_| OhosError::new("OHOS session mutex poisoned"))?;
      let removed = state.output_streams.remove(&stream_id);
      state.cancelled_output_streams.remove(&stream_id);
      removed
    };
    if removed {
      self
        .host
        .lock()
        .map_err(|_| OhosError::new("OHOS host mutex poisoned"))?
        .release_output_stream(stream_id)?;
    }
    Ok(())
  }

  pub fn release_object(&self, object_id: u32) -> Result<(), OhosEngineError> {
    self.ensure_open()?;
    let removed = self
      .state
      .lock()
      .map_err(|_| OhosError::new("OHOS session mutex poisoned"))?
      .objects
      .remove(&object_id);
    if removed {
      self
        .host
        .lock()
        .map_err(|_| OhosError::new("OHOS host mutex poisoned"))?
        .release_object(object_id)?;
    }
    Ok(())
  }

  pub fn close(&self) -> Result<(), OhosEngineError> {
    let (callbacks, input, output, cancelled_output, objects) = {
      let mut state = self
        .state
        .lock()
        .map_err(|_| OhosError::new("OHOS session mutex poisoned"))?;
      if state.closed {
        return Ok(());
      }
      state.closed = true;
      (
        std::mem::take(&mut state.retained_callbacks),
        std::mem::take(&mut state.input_streams),
        std::mem::take(&mut state.output_streams),
        std::mem::take(&mut state.cancelled_output_streams),
        std::mem::take(&mut state.objects),
      )
    };
    let mut host = self
      .host
      .lock()
      .map_err(|_| OhosError::new("OHOS host mutex poisoned"))?;
    for (ty, id) in callbacks {
      host.release_callback(ty, id)?;
    }
    for id in input {
      host.cancel_input_stream(id)?;
      host.release_input_stream(id)?;
    }
    for id in output {
      if !cancelled_output.contains(&id) {
        host.cancel_output_stream(id)?;
      }
      host.release_output_stream(id)?;
    }
    for id in objects {
      host.release_object(id)?;
    }
    Ok(())
  }

  fn ensure_open(&self) -> Result<(), OhosEngineError> {
    if self
      .state
      .lock()
      .map_err(|_| OhosError::new("OHOS session mutex poisoned"))?
      .closed
    {
      Err(OhosEngineError::Closed)
    } else {
      Ok(())
    }
  }

  fn operation(
    &self,
    operation_id: u32,
  ) -> Result<&napi_family_core::FamilyOperation, OhosEngineError> {
    self
      .family
      .operations()
      .iter()
      .find(|operation| operation.id == operation_id)
      .ok_or(OhosEngineError::UnknownOperation { operation_id })
  }

  fn callback_reentrancy_for_type(&self, callback_type_id: u32) -> CallbackReentrancy {
    self
      .family
      .operations()
      .iter()
      .flat_map(|operation| operation.callbacks.iter())
      .filter(|use_site| use_site.callback_type_id == callback_type_id)
      .map(|use_site| use_site.contract.reentrancy)
      .find(|policy| *policy == CallbackReentrancy::Forbidden)
      .unwrap_or(CallbackReentrancy::Allowed)
  }

  /// Callback methods have their async/error shape on the method operation,
  /// while reentrancy is a use-site lifecycle policy.  A callback method
  /// operation has no callback argument itself, so never default to Allowed
  /// merely because its local callback list is empty.  Aggregate all known
  /// use-sites for the callback type and let a Forbidden policy win; this
  /// avoids first-use-site registry selection and preserves overlap safety.
  fn callback_reentrancy_for_method(
    &self,
    operation: &napi_family_core::FamilyOperation,
    callback_type_id: u32,
  ) -> CallbackReentrancy {
    let operation_policy = operation
      .callbacks
      .iter()
      .filter(|use_site| use_site.callback_type_id == callback_type_id)
      .map(|use_site| use_site.contract.reentrancy)
      .find(|policy| *policy == CallbackReentrancy::Forbidden);
    if operation_policy.is_some() {
      return CallbackReentrancy::Forbidden;
    }
    if operation
      .callbacks
      .iter()
      .any(|use_site| use_site.callback_type_id == callback_type_id)
    {
      return CallbackReentrancy::Allowed;
    }
    self
      .family
      .operations()
      .iter()
      .flat_map(|candidate| candidate.callbacks.iter())
      .filter(|use_site| use_site.callback_type_id == callback_type_id)
      .map(|use_site| use_site.contract.reentrancy)
      .find(|policy| *policy == CallbackReentrancy::Forbidden)
      .unwrap_or(CallbackReentrancy::Allowed)
  }

  fn track_inputs_and_objects(
    &self,
    operation: &napi_family_core::FamilyOperation,
    args: &[OhosValue],
  ) -> Result<(), OhosEngineError> {
    let receiver_offset = usize::from(operation.receiver.is_some());
    let mut state = self
      .state
      .lock()
      .map_err(|_| OhosError::new("OHOS session mutex poisoned"))?;
    for stream in &operation.streams {
      if stream.direction != StreamDirection::Input {
        continue;
      }
      let Some(value) = args.get(receiver_offset) else {
        continue;
      };
      if let OhosValue::InputStream(id) = value {
        state.input_streams.insert(*id);
      }
    }
    if operation.receiver.is_some() {
      if let Some(OhosValue::Object(id) | OhosValue::OutputStream(id)) = args.first() {
        if operation.kind == OperationKind::OutputStreamNext
          || operation.kind == OperationKind::OutputStreamCancel
        {
          state.output_streams.insert(*id);
          state.cancelled_output_streams.remove(id);
        } else {
          state.objects.insert(*id);
        }
      }
    }
    Ok(())
  }

  fn track_return_value(
    &self,
    operation: &napi_family_core::FamilyOperation,
    value: &OhosValue,
  ) -> Result<(), OhosEngineError> {
    let mut state = self
      .state
      .lock()
      .map_err(|_| OhosError::new("OHOS session mutex poisoned"))?;
    match (operation.result.map(|resource| resource.kind), value) {
      (Some(napi_family_core::ResourceKind::Object), OhosValue::Object(id)) => {
        state.objects.insert(*id);
      }
      (Some(napi_family_core::ResourceKind::OutputStream), OhosValue::OutputStream(id)) => {
        state.output_streams.insert(*id);
        state.cancelled_output_streams.remove(id);
      }
      _ => {}
    }
    Ok(())
  }
}

/// The generated factory binds one Host and returns the same concrete session
/// type for every invocation.  There is intentionally no second public export.
#[derive(Clone)]
pub struct OhosBackendFactory {
  module: Arc<GeneratedOhosModule>,
}

impl OhosBackendFactory {
  pub fn new(module: GeneratedOhosModule) -> Self {
    Self {
      module: Arc::new(module),
    }
  }

  pub fn module(&self) -> &GeneratedOhosModule {
    &self.module
  }

  pub fn open<H: OhosHost>(&self, host: H) -> OhosBackendSession<H> {
    OhosBackendSession::new(&self.module, host)
  }
}

fn callback_id(args: &[OhosValue]) -> Result<u32, OhosEngineError> {
  args
    .first()
    .and_then(|value| match value {
      OhosValue::Callback(id) => Some(*id),
      _ => None,
    })
    .ok_or(OhosEngineError::InvalidValue {
      expected: "callback ID",
    })
}

fn stream_id(args: &[OhosValue]) -> Result<u32, OhosEngineError> {
  args
    .first()
    .and_then(|value| match value {
      OhosValue::InputStream(id) | OhosValue::OutputStream(id) => Some(*id),
      OhosValue::Number(id) if *id >= 0.0 => Some(*id as u32),
      _ => None,
    })
    .ok_or(OhosEngineError::InvalidValue {
      expected: "stream ID",
    })
}

#[cfg(test)]
mod tests;
