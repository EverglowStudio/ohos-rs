//! Native implementation of the private BackendSession boundary.
//!
//! The generated factory binds exactly one JavaScript Host object and keeps
//! operation callbacks behind native references.  Only the session methods
//! below are observable from JavaScript; raw callbacks never enter the module
//! export table or become properties of the returned session.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{c_void, CString};
use std::ptr;
use std::rc::Rc;
use std::sync::{
  atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering},
  Arc, Mutex, OnceLock, Weak,
};

use napi_ohos::bindgen_prelude::{JsValue, Object, PromiseRaw, Unknown};
use napi_ohos::{sys, Env, Error, Result, Status};

/// Reference-counted lifetime gate shared by every pending settlement and
/// invocation Host proxy.  Settlement contexts never retain a bare
/// `SessionState` pointer: they consult this gate and observe `None` once the
/// deadline (or the N-API finalizer) detaches the state.
struct LifecycleGate {
  env: sys::napi_env,
  /// Raw Ark N-API references may only be touched on the thread owning the
  /// environment.  The marker lets callback invokers reject an off-thread
  /// returned retain before resolving the state pointer.
  owner_thread: std::thread::ThreadId,
  state: AtomicPtr<SessionState>,
  detached: AtomicBool,
  /// Callback proxies may outlive the public session object. Revoke their
  /// invocation capability as soon as close/deadline starts teardown.
  invocations_open: AtomicBool,
  active_settlements: AtomicU32,
  close_deferred: AtomicPtr<sys::napi_deferred__>,
  settlement_refs: Mutex<Vec<Arc<PendingSessionRef>>>,
  host_leases: Mutex<Vec<Weak<InvocationHostLease>>>,
}

struct PendingSessionRef {
  env: sys::napi_env,
  reference: AtomicPtr<sys::napi_ref__>,
}

impl PendingSessionRef {
  fn new(env: sys::napi_env, reference: sys::napi_ref) -> Arc<Self> {
    Arc::new(Self {
      env,
      reference: AtomicPtr::new(reference),
    })
  }

  fn delete(&self) {
    let reference = self.reference.swap(ptr::null_mut(), Ordering::AcqRel);
    delete_reference(self.env, reference);
  }
}

/// Per-native-invocation Host binding.  The lease is independent for each
/// invocation so concurrent HostAndArguments futures cannot share or replace a
/// session-level "current invocation" slot.
struct InvocationHostLease {
  gate: Arc<LifecycleGate>,
  generation: u32,
  active: AtomicBool,
}

impl InvocationHostLease {
  fn is_active(&self) -> bool {
    let _generation = self.generation;
    self.active.load(Ordering::Acquire) && !self.gate.detached.load(Ordering::Acquire)
  }
}

impl LifecycleGate {
  fn new(env: sys::napi_env) -> Arc<Self> {
    Arc::new(Self {
      env,
      owner_thread: std::thread::current().id(),
      state: AtomicPtr::new(ptr::null_mut()),
      detached: AtomicBool::new(false),
      invocations_open: AtomicBool::new(true),
      active_settlements: AtomicU32::new(0),
      close_deferred: AtomicPtr::new(ptr::null_mut()),
      settlement_refs: Mutex::new(Vec::new()),
      host_leases: Mutex::new(Vec::new()),
    })
  }

  fn install_state(&self, state: *mut SessionState) -> bool {
    if self.detached.load(Ordering::Acquire) {
      return false;
    }
    self.state.store(state, Ordering::Release);
    !self.detached.load(Ordering::Acquire)
  }

  fn state_ptr(&self) -> *mut SessionState {
    self.state.load(Ordering::Acquire)
  }

  fn state(&self) -> Option<&'static SessionState> {
    let state = self.state_ptr();
    (!state.is_null()).then(|| unsafe { &*state })
  }

  fn register_settlement(&self) -> Result<()> {
    loop {
      if self.detached.load(Ordering::Acquire) {
        return Err(Error::new(
          Status::GenericFailure,
          "UniFFI session lifecycle is detached",
        ));
      }
      let current = self.active_settlements.load(Ordering::Acquire);
      let next = current.checked_add(1).ok_or_else(|| {
        Error::new(
          Status::GenericFailure,
          "too many pending UniFFI settlements",
        )
      })?;
      if self
        .active_settlements
        .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
      {
        if self.detached.load(Ordering::Acquire) {
          self.active_settlements.fetch_sub(1, Ordering::AcqRel);
          return Err(Error::new(
            Status::GenericFailure,
            "UniFFI session lifecycle detached during settlement registration",
          ));
        }
        return Ok(());
      }
    }
  }

  fn finish_settlement(&self) {
    let _ = self
      .active_settlements
      .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
        value.checked_sub(1)
      });
  }

  fn set_close_deferred(&self, deferred: sys::napi_deferred) {
    self.close_deferred.store(deferred, Ordering::Release);
    if self.detached.load(Ordering::Acquire) {
      self.resolve_close();
    }
  }

  fn resolve_close(&self) {
    let deferred = self.close_deferred.swap(ptr::null_mut(), Ordering::AcqRel);
    if deferred.is_null() || self.env.is_null() {
      return;
    }
    clear_pending_exception(self.env);
    let _ = unsafe {
      sys::napi_resolve_deferred(
        self.env,
        deferred,
        js_undefined(self.env).unwrap_or(ptr::null_mut()),
      )
    };
  }

  fn register_settlement_ref(&self, reference: Arc<PendingSessionRef>) -> bool {
    let mut refs = self
      .settlement_refs
      .lock()
      .expect("lifecycle settlement refs poisoned");
    if self.detached.load(Ordering::Acquire) {
      drop(refs);
      reference.delete();
      false
    } else {
      refs.push(reference);
      true
    }
  }

  fn unregister_settlement_ref(&self, reference: &Arc<PendingSessionRef>) {
    let mut refs = self
      .settlement_refs
      .lock()
      .expect("lifecycle settlement refs poisoned");
    refs.retain(|candidate| !Arc::ptr_eq(candidate, reference));
  }

  fn detach_settlement_refs(&self) {
    let refs = std::mem::take(
      &mut *self
        .settlement_refs
        .lock()
        .expect("lifecycle settlement refs poisoned"),
    );
    for reference in refs {
      reference.delete();
    }
  }

  fn new_host_lease(self: &Arc<Self>, generation: u32) -> Arc<InvocationHostLease> {
    let lease = Arc::new(InvocationHostLease {
      gate: self.clone(),
      generation,
      active: AtomicBool::new(true),
    });
    self
      .host_leases
      .lock()
      .expect("lifecycle Host leases poisoned")
      .push(Arc::downgrade(&lease));
    lease
  }

  fn detach_host_leases(&self) {
    let mut leases = self
      .host_leases
      .lock()
      .expect("lifecycle Host leases poisoned");
    leases.retain(|weak| {
      weak.upgrade().is_some_and(|lease| {
        lease.active.store(false, Ordering::Release);
        false
      })
    });
  }

  /// Atomically invalidate all gate users before any N-API references are
  /// released.  The returned pointer is used only by the current JS-thread
  /// teardown path; settlement contexts never dereference it after this call.
  fn detach(&self) -> *mut SessionState {
    self.invalidate_invocations();
    self.detached.store(true, Ordering::Release);
    self.detach_host_leases();
    let state = self.state.swap(ptr::null_mut(), Ordering::AcqRel);
    self.detach_settlement_refs();
    self.resolve_close();
    state
  }

  fn invalidate_invocations(&self) {
    self.invocations_open.store(false, Ordering::Release);
  }

  fn invocations_are_open(&self) -> bool {
    self.invocations_open.load(Ordering::Acquire) && !self.detached.load(Ordering::Acquire)
  }

  fn ensure_owner_thread(&self) -> Result<()> {
    if self.is_owner_thread() {
      Ok(())
    } else {
      Err(Error::new(
        Status::GenericFailure,
        "UniFFI callback invoker must run on the owning ArkVM thread",
      ))
    }
  }

  fn is_owner_thread(&self) -> bool {
    std::thread::current().id() == self.owner_thread
  }
}

// N-API callback contexts are only entered on their owning JS environment
// thread.  The Arc itself can be dropped by a worker-thread future; dropping
// it performs no N-API calls, so these marker impls are sound for the gate's
// atomics/mutexes.
unsafe impl Send for LifecycleGate {}
unsafe impl Sync for LifecycleGate {}
unsafe impl Send for PendingSessionRef {}
unsafe impl Sync for PendingSessionRef {}

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
pub enum SessionCallbackErrorStyle {
  Infallible,
  Fallible,
}

/// Whether the generated host callback proxy may be entered again before the
/// current invocation returns.
///
/// Proxy builders must enforce this use-site contract when it is `Forbidden`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionCallbackReentrancy {
  Allowed,
  Forbidden,
}

/// Session-owned callback invocation capability. All generated scoped and
/// retained proxies receive a clone of this value; clones share the one
/// session allocator and lifecycle gate.
#[derive(Clone)]
pub struct SessionCallbackInvoker {
  inner: Arc<CallbackInvokerInner>,
}

struct CallbackInvokerInner {
  next_invocation_id: AtomicU32,
  gate: Arc<LifecycleGate>,
}

impl SessionCallbackInvoker {
  fn new(gate: Arc<LifecycleGate>) -> Self {
    Self {
      inner: Arc::new(CallbackInvokerInner {
        next_invocation_id: AtomicU32::new(0),
        gate,
      }),
    }
  }

  /// Check that a callback proxy may enter the Host without allocating an
  /// invocation ID. Synchronous callback methods call this on every entry.
  pub fn check_open(&self) -> Result<()> {
    self.inner.gate.ensure_owner_thread()?;
    if !self.inner.gate.invocations_are_open() {
      return Err(Error::new(
        Status::GenericFailure,
        "UniFFI callback invoker is closed",
      ));
    }
    Ok(())
  }

  /// Return whether the current thread owns this session's ArkVM
  /// environment. This checks only the immutable thread marker; it never
  /// resolves SessionState or touches an N-API reference. Callers must still
  /// use `check_open`/`with_host` before entering the Host.
  pub fn is_owner_thread(&self) -> bool {
    self.inner.gate.is_owner_thread()
  }

  /// Resolve the active session Host and Env for one owner-thread operation.
  ///
  /// The callback receives borrowed handles that are valid only for this
  /// call. Callers must not move either value into a proxy/future or retain
  /// their raw N-API pointers. The Host is resolved from the SessionState's
  /// persistent reference, rather than from a finalized per-invocation proxy.
  pub fn with_host<R>(&self, callback: impl FnOnce(&Env, &Object<'static>) -> R) -> Result<R> {
    self.check_open()?;
    let state = self.inner.gate.state_ptr();
    if state.is_null() || !self.inner.gate.invocations_are_open() {
      return Err(Error::new(
        Status::GenericFailure,
        "UniFFI callback invoker is closed",
      ));
    }
    // Safety: `state` is published by `install_state` and cleared before
    // state-owned N-API references are released. Owner-thread/open checks
    // above prevent a revoked state from being dereferenced.
    let state = unsafe { &*state };
    state.with_host(callback)
  }

  /// Retain a callback returned by a callback method on the owning ArkVM
  /// thread. The lease is backed by the existing SessionState registry and
  /// release queue; only the Arc token crosses to a native worker.
  pub fn retain_returned_callback(
    &self,
    callback_type_id: u32,
    callback_id: u32,
  ) -> Result<SessionCallbackLease> {
    self.check_open()?;
    let state = self.inner.gate.state_ptr();
    if state.is_null() || !self.inner.gate.invocations_are_open() {
      return Err(Error::new(
        Status::GenericFailure,
        "UniFFI callback invoker is closed",
      ));
    }
    // Safety: install_state publishes this pointer and detach clears it
    // before state-owned Ark references are released. The open/thread checks
    // above prevent dereferencing a revoked state; new_callback_lease repeats
    // the check around the Host hook for a close/reentrancy race.
    let state = unsafe { &*state };
    state.new_callback_lease(CallbackKey {
      callback_type_id,
      callback_id,
    })
  }

  /// Allocate a session-local async callback invocation ID. The lifecycle
  /// check is repeated after the atomic allocation so close/deadline cannot
  /// publish a newly allocated ID to a callback entering after revocation.
  pub fn next_invocation_id(&self) -> Result<u32> {
    if !self.inner.gate.invocations_are_open() {
      return Err(Error::new(
        Status::GenericFailure,
        "UniFFI callback invoker is closed",
      ));
    }
    loop {
      let invocation_id = self.inner.next_invocation_id.load(Ordering::Acquire);
      let successor = invocation_id.checked_add(1).ok_or_else(|| {
        Error::new(
          Status::GenericFailure,
          "callback invocation ID space exhausted",
        )
      })?;
      if self
        .inner
        .next_invocation_id
        .compare_exchange(
          invocation_id,
          successor,
          Ordering::AcqRel,
          Ordering::Acquire,
        )
        .is_ok()
      {
        if self.inner.gate.invocations_are_open() {
          return Ok(invocation_id);
        }
        return Err(Error::new(
          Status::GenericFailure,
          "UniFFI callback invoker is closed",
        ));
      }
      if !self.inner.gate.invocations_are_open() {
        return Err(Error::new(
          Status::GenericFailure,
          "UniFFI callback invoker is closed",
        ));
      }
    }
  }
}

/// An explicit retained callback transfer.  Generated callback proxies own a
/// clone of this token; the final clone enqueues one release request.  The
/// queue is drained on the JS thread by the owning session, so a proxy may be
/// dropped by a cross-thread Rust future without touching N-API directly.
#[derive(Clone)]
pub struct SessionCallbackLease {
  inner: Arc<CallbackLeaseInner>,
}

struct CallbackLeaseInner {
  token: u64,
  queue: Arc<CallbackReleaseQueue>,
}

struct CallbackReleaseState {
  next_token: u64,
  active: BTreeMap<u64, CallbackKey>,
  pending: Vec<CallbackKey>,
}

struct CallbackReleaseQueue {
  // The active map is the logical lease registry.  It outlives every
  // SessionCallbackLease Arc, closing the strong-count-to-zero/Drop-before-
  // mutex window where a Weak-only session registry could miss a release.
  // `pending` is protected by the same mutex so claim and close are atomic.
  state: Mutex<CallbackReleaseState>,
  // A raw N-API TSFN is kept behind an atomic pointer because the queue is
  // shared by callback proxy tokens that may be dropped on worker threads.
  // `tsfn_guard` serializes calls with teardown; an atomic pointer alone would
  // allow a worker to race `napi_release_threadsafe_function` during session
  // finalization.
  tsfn: std::sync::atomic::AtomicPtr<sys::napi_threadsafe_function__>,
  tsfn_guard: Mutex<()>,
  closed: AtomicBool,
}

/// Context owned by N-API's TSFN rather than by `SessionState`.  N-API may
/// still invoke `call_js_cb` while an aborting TSFN is being drained, so this
/// context must remain valid until the TSFN finalize callback runs.  It keeps
/// its own Host reference and never dereferences the wrapped session pointer.
struct CallbackReleaseTsfnContext {
  host: sys::napi_ref,
  release_callback: sys::napi_ref,
  queue: Arc<CallbackReleaseQueue>,
}

impl CallbackLeaseInner {
  fn release(&self) {
    // Claiming the logical token and publishing its release are one mutex
    // operation. A close racing this Drop therefore either claims the token
    // itself or observes it already pending, never both.
    if self.queue.claim(self.token) {
      self.queue.wake();
    }
  }
}

impl CallbackReleaseQueue {
  fn register(&self, key: CallbackKey) -> Result<u64> {
    let mut state = self.state.lock().expect("callback release queue poisoned");
    let token = state.next_token;
    state.next_token = token.checked_add(1).ok_or_else(|| {
      Error::new(
        Status::GenericFailure,
        "callback lease token space exhausted",
      )
    })?;
    state.active.insert(token, key);
    Ok(token)
  }

  fn claim(&self, token: u64) -> bool {
    let mut state = self.state.lock().expect("callback release queue poisoned");
    let Some(key) = state.active.remove(&token) else {
      return false;
    };
    state.pending.push(key);
    true
  }

  fn claim_all_active(&self) {
    let mut state = self.state.lock().expect("callback release queue poisoned");
    let active = std::mem::take(&mut state.active);
    state.pending.extend(active.into_values());
  }

  fn take_pending(&self) -> Vec<CallbackKey> {
    let mut state = self.state.lock().expect("callback release queue poisoned");
    std::mem::take(&mut state.pending)
  }

  fn wake(&self) {
    // Keep the TSFN alive while napi_call_threadsafe_function executes.  The
    // finalizer sets `closed` first, then waits for this lock before releasing
    // the TSFN, preventing a use-after-release from a worker-thread Drop.
    let _guard = self
      .tsfn_guard
      .lock()
      .expect("callback TSFN guard poisoned");
    if self.closed.load(Ordering::Acquire) {
      return;
    }
    let tsfn = self.tsfn.load(Ordering::Acquire);
    if !tsfn.is_null() {
      #[cfg(target_env = "ohos")]
      let status = unsafe {
        sys::napi_call_threadsafe_function_with_priority(
          tsfn,
          ptr::null_mut(),
          sys::napi_task_priority::napi_priority_high,
          true,
        )
      };
      #[cfg(not(target_env = "ohos"))]
      let status = unsafe {
        sys::napi_call_threadsafe_function(
          tsfn,
          ptr::null_mut(),
          sys::ThreadsafeFunctionCallMode::nonblocking,
        )
      };
      // The TSFN only wakes the JavaScript-thread callback; release requests
      // remain in `pending` until that callback drains them.
      let _ = status;
    }
  }

  fn install(&self, tsfn: sys::napi_threadsafe_function) -> bool {
    let _guard = self
      .tsfn_guard
      .lock()
      .expect("callback TSFN guard poisoned");
    if self.closed.load(Ordering::Acquire) {
      if !tsfn.is_null() {
        let _ = unsafe {
          sys::napi_release_threadsafe_function(tsfn, sys::ThreadsafeFunctionReleaseMode::abort)
        };
      }
      false
    } else {
      self.tsfn.store(tsfn, Ordering::Release);
      true
    }
  }

  fn shutdown(&self) {
    self.closed.store(true, Ordering::Release);
    let _guard = self
      .tsfn_guard
      .lock()
      .expect("callback TSFN guard poisoned");
    let tsfn = self.tsfn.swap(ptr::null_mut(), Ordering::AcqRel);
    if !tsfn.is_null() {
      let _ = unsafe {
        sys::napi_release_threadsafe_function(tsfn, sys::ThreadsafeFunctionReleaseMode::abort)
      };
    }
  }

  /// Mark the queue detached after N-API has finalized the TSFN itself.  This
  /// path deliberately does not call `napi_release_threadsafe_function`: the
  /// runtime already owns that release and the pointer may no longer refer to
  /// a live TSFN while a wrapped session finalizer is running.
  fn mark_finalized_by_env(&self) {
    self.closed.store(true, Ordering::Release);
    let _guard = self
      .tsfn_guard
      .lock()
      .expect("callback TSFN guard poisoned");
    self.tsfn.store(ptr::null_mut(), Ordering::Release);
  }
}

fn drain_callback_releases_with_callback(
  env: sys::napi_env,
  host_reference: sys::napi_ref,
  callback_reference: sys::napi_ref,
  queue: &CallbackReleaseQueue,
) {
  if env.is_null() || callback_reference.is_null() {
    return;
  }
  clear_pending_exception(env);
  let callbacks = queue.take_pending();
  let host = reference_value(env, host_reference, "Host").ok();
  let Ok(callback) = reference_value(env, callback_reference, "releaseCallback") else {
    return;
  };
  for item in callbacks {
    let (Ok(callback_type), Ok(callback_id)) = (
      js_u32(env, item.callback_type_id),
      js_u32(env, item.callback_id),
    ) else {
      continue;
    };
    clear_pending_exception(env);
    let receiver = host.unwrap_or_else(|| js_undefined(env).unwrap_or(ptr::null_mut()));
    let result = call_function(env, receiver, callback, &[callback_type, callback_id]);
    if result.is_err() {
      clear_pending_exception(env);
    }
  }
}

impl Drop for CallbackLeaseInner {
  fn drop(&mut self) {
    self.release();
  }
}

#[derive(Clone)]
pub struct SessionCallbackTransfers {
  use_sites: Arc<Vec<Vec<SessionCallbackLease>>>,
  invoker: Option<SessionCallbackInvoker>,
}

impl SessionCallbackTransfers {
  pub fn empty() -> Self {
    Self {
      use_sites: Arc::new(Vec::new()),
      invoker: None,
    }
  }

  /// Resolve the engine-owned callback invocation capability associated with
  /// this operation's private transfer carrier.
  pub fn invoker(&self) -> Result<SessionCallbackInvoker> {
    self.invoker.clone().ok_or_else(|| {
      Error::new(
        Status::InvalidArg,
        "callback invocation capability is unavailable",
      )
    })
  }

  pub fn lease(&self, use_site_index: usize, value_index: usize) -> Result<SessionCallbackLease> {
    self
      .use_sites
      .get(use_site_index)
      .and_then(|leases| leases.get(value_index))
      .cloned()
      .ok_or_else(|| {
        Error::new(
          Status::InvalidArg,
          format!("callback lease transfer {use_site_index}/{value_index} is unavailable"),
        )
      })
  }
}

struct CallbackTransferRegistry {
  next_id: u32,
  transfers: BTreeMap<(u32, u32), SessionCallbackTransfers>,
}

static CALLBACK_TRANSFER_REGISTRY: OnceLock<Mutex<CallbackTransferRegistry>> = OnceLock::new();
static SESSION_GENERATIONS: OnceLock<Mutex<u32>> = OnceLock::new();

fn callback_transfer_registry() -> &'static Mutex<CallbackTransferRegistry> {
  CALLBACK_TRANSFER_REGISTRY.get_or_init(|| {
    Mutex::new(CallbackTransferRegistry {
      next_id: 0,
      transfers: BTreeMap::new(),
    })
  })
}

fn allocate_session_generation() -> Result<u32> {
  let generations = SESSION_GENERATIONS.get_or_init(|| Mutex::new(0));
  let mut next = generations
    .lock()
    .expect("session generation registry poisoned");
  let generation = *next;
  *next = generation
    .checked_add(1)
    .ok_or_else(|| Error::new(Status::GenericFailure, "session generation space exhausted"))?;
  Ok(generation)
}

fn allocate_callback_transfer_id() -> Result<u32> {
  let mut registry = callback_transfer_registry()
    .lock()
    .expect("callback transfer registry poisoned");
  let id = registry.next_id;
  registry.next_id = registry.next_id.checked_add(1).ok_or_else(|| {
    Error::new(
      Status::GenericFailure,
      "callback transfer ID space exhausted",
    )
  })?;
  Ok(id)
}

fn register_callback_transfer(generation: u32, id: u32, transfers: SessionCallbackTransfers) {
  callback_transfer_registry()
    .lock()
    .expect("callback transfer registry poisoned")
    .transfers
    .insert((generation, id), transfers);
}

fn discard_callback_transfer(generation: u32, id: u32) {
  let _ = callback_transfer_registry()
    .lock()
    .expect("callback transfer registry poisoned")
    .transfers
    .remove(&(generation, id));
}

/// Extract the transfer token injected by the session for one retained
/// callback argument.  This is intentionally private to generated engine
/// code; public facades never expose the token as a JS value.
pub fn take_session_callback_transfers(
  generation: u32,
  id: u32,
) -> Result<SessionCallbackTransfers> {
  callback_transfer_registry()
    .lock()
    .expect("callback transfer registry poisoned")
    .transfers
    .remove(&(generation, id))
    .ok_or_else(|| {
      Error::new(
        Status::GenericFailure,
        format!("unknown callback transfer {generation}/{id}"),
      )
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCallbackArgument {
  /// Complete canonical value path. Runtime never reduces this to an
  /// argument index so nested record/enum/container use-sites remain stable.
  pub path: Vec<SessionValuePathSegment>,
  pub callback_type_id: u32,
  pub retention: SessionCallbackRetention,
  pub threading: SessionCallbackThreading,
  pub reentrancy: SessionCallbackReentrancy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionValuePathSegment {
  Argument(u32),
  Return,
  StreamItem,
  StreamError,
  Field(String),
  Variant(String),
  Optional,
  SequenceElement,
  MapKey,
  MapValue,
  SetElement,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionStreamDirection {
  Input,
  Output,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionStreamArgument {
  pub path: Vec<SessionValuePathSegment>,
  pub use_site_id: u32,
  pub direction: SessionStreamDirection,
}

/// Return resources are described by complete, return-rooted paths.  The
/// session only executes this mechanical path and never needs the UniFFI type
/// graph that produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionResultResourceUseSite {
  pub path: Vec<SessionValuePathSegment>,
  pub kind: SessionResourceReceiver,
  pub ownership: SessionResourceOwnership,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionResourceReceiver {
  Object,
  InputStream,
  OutputStream,
}

/// Ownership carried mechanically with every generated return resource
/// use-site. Family validation currently permits only `Owned` results, but
/// preserving this field keeps the descriptor contract explicit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionResourceOwnership {
  Owned,
  Borrowed,
  ByArc,
}

/// Session-side receiver classification.  Value receivers occupy argument
/// slot zero but are passed through unchanged and never enter resource lease
/// tracking; resource receivers retain the existing handle/lease behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionReceiver {
  Value,
  Resource(SessionResourceReceiver),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionOperationDispatch {
  NativeSync,
  NativeAsync,
  CallbackHostSync {
    callback_type_id: u32,
    method_id: u32,
    error_style: SessionCallbackErrorStyle,
  },
  CallbackHostAsync {
    callback_type_id: u32,
    method_id: u32,
    error_style: SessionCallbackErrorStyle,
  },
  InputStreamHostPull,
  InputStreamHostCancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionNativeCall {
  ArgumentsOnly,
  HostAndArguments,
}

/// One dense operation slot supplied by generated code.
pub struct SessionOperationDescriptor {
  pub dispatch: SessionOperationDispatch,
  pub callback: Option<sys::napi_value>,
  pub native_call: SessionNativeCall,
  pub receiver: Option<SessionReceiver>,
  pub result_resources: Vec<SessionResultResourceUseSite>,
  pub callback_transfer: bool,
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
  // A direct reference to the releaseInputStream function lets a late result
  // be cleaned up after the Host object has been revoked at deadline.  It is
  // invoked with `undefined` as receiver and never re-enters the Host proxy.
  release_input_stream: Cell<sys::napi_ref>,
  cancel_output_stream: Cell<sys::napi_ref>,
  release_output_stream: Cell<sys::napi_ref>,
}

struct TrackedResource {
  reference: sys::napi_ref,
  kind: SessionResourceReceiver,
  cancel_called: bool,
  cancel_pending: bool,
  cancel_promise: sys::napi_ref,
  release_called: bool,
  release_pending: bool,
}

struct SessionOperation {
  dispatch: SessionOperationDispatch,
  callback: Cell<sys::napi_ref>,
  native_call: SessionNativeCall,
  receiver: Option<SessionReceiver>,
  result_resources: Vec<SessionResultResourceUseSite>,
  callback_transfer: bool,
  callback_arguments: Vec<SessionCallbackArgument>,
  stream_arguments: Vec<SessionStreamArgument>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CallbackKey {
  callback_type_id: u32,
  callback_id: u32,
}

#[derive(Clone, Copy)]
struct CallbackRegistrationToken {
  key: CallbackKey,
  token: u64,
}

#[derive(Clone)]
struct RegisteredCallbackContract {
  token: Option<u64>,
  contract: SessionCallbackArgument,
}

struct CallbackGuardLease {
  gate: Arc<LifecycleGate>,
  session_reference: Arc<PendingSessionRef>,
  key: CallbackKey,
  finished: Cell<bool>,
}

impl CallbackGuardLease {
  fn finish(&self) {
    if self.finished.replace(true) {
      return;
    }
    if let Some(state) = self.gate.state() {
      release_callback_guard(&state.active_callbacks, self.key);
    }
    self.session_reference.delete();
    self.gate.unregister_settlement_ref(&self.session_reference);
  }
}

impl Drop for CallbackGuardLease {
  fn drop(&mut self) {
    self.finish();
  }
}

fn callback_reentrancy_for_operation(
  callback_arguments: &[SessionCallbackArgument],
  callback_type_id: u32,
) -> SessionCallbackReentrancy {
  callback_arguments
    .iter()
    .find(|argument| argument.callback_type_id == callback_type_id)
    .map(|argument| argument.reentrancy)
    .unwrap_or(SessionCallbackReentrancy::Allowed)
}

fn release_callback_guard(active: &RefCell<BTreeSet<CallbackKey>>, key: CallbackKey) {
  active.borrow_mut().remove(&key);
}

fn clear_callback_guard_on_error(
  active: &RefCell<BTreeSet<CallbackKey>>,
  guarded: bool,
  key: CallbackKey,
) {
  if guarded {
    release_callback_guard(active, key);
  }
}

struct SessionState {
  env: sys::napi_env,
  gate: Arc<LifecycleGate>,
  session_generation: u32,
  host: Cell<sys::napi_ref>,
  close_policy: napi_family_core::ClosePolicy,
  deadline_timer: Cell<sys::napi_ref>,
  release_callback: Cell<sys::napi_ref>,
  operations: Vec<SessionOperation>,
  closing: Cell<bool>,
  closed: Cell<bool>,
  callback_invoker: SessionCallbackInvoker,
  pending_work: Cell<u32>,
  close_deferred: Cell<sys::napi_deferred>,
  close_promise: Cell<sys::napi_ref>,
  callback_leases: RefCell<Vec<Weak<CallbackLeaseInner>>>,
  callback_owners: RefCell<Vec<SessionCallbackLease>>,
  callback_transfers: RefCell<Vec<u32>>,
  callback_release_queue: Arc<CallbackReleaseQueue>,
  callback_methods: BTreeMap<(u32, u32), SessionCallbackErrorStyle>,
  next_callback_registration_id: Cell<u64>,
  callback_contracts: RefCell<BTreeMap<CallbackKey, Vec<RegisteredCallbackContract>>>,
  active_callbacks: RefCell<BTreeSet<CallbackKey>>,
  input_streams: RefCell<BTreeSet<u32>>,
  /// Input stream IDs are monotonic resource identities.  Keep a tombstone
  /// after terminal done/error/cancel so a late cancel (or duplicate promise
  /// settlement) cannot re-register and release the same host stream twice.
  released_input_streams: RefCell<BTreeSet<u32>>,
  resource_references: RefCell<Vec<TrackedResource>>,
  resource_callbacks: ResourceCallbacks,
  late_result_cleanup: Arc<LateResultCleanup>,
}

#[derive(Clone)]
struct InvocationSnapshot {
  callback_len: usize,
  callback_owner_len: usize,
  callback_transfer_len: usize,
  streams: BTreeSet<u32>,
  resource_len: usize,
}

struct RetainedInvocation {
  transfer_id: Option<u32>,
  scoped_callbacks: Vec<CallbackRegistrationToken>,
}

impl SessionState {
  fn host_value(&self) -> Result<sys::napi_value> {
    reference_value(self.env, self.host.get(), "Host")
  }

  fn with_host<R>(&self, callback: impl FnOnce(&Env, &Object<'static>) -> R) -> Result<R> {
    self.callback_invoker.check_open()?;
    let host = Object::from_raw(self.env, self.host_value()?);
    let env = Env::from_raw(self.env);
    Ok(callback(&env, &host))
  }

  fn ensure_open(&self) -> Result<()> {
    if self.closing.get() || self.closed.get() || self.gate.detached.load(Ordering::Acquire) {
      Err(Error::new(
        Status::GenericFailure,
        "UniFFI backend session is closed",
      ))
    } else {
      Ok(())
    }
  }

  fn begin_pending_work(&self) -> Result<()> {
    self.gate.register_settlement()?;
    let pending = self.pending_work.get();
    let next = pending.checked_add(1).ok_or_else(|| {
      self.gate.finish_settlement();
      Error::new(
        Status::GenericFailure,
        "too many pending UniFFI backend operations",
      )
    })?;
    self.pending_work.set(next);
    Ok(())
  }

  fn finish_pending_work(&self) {
    let pending = self.pending_work.get();
    if pending == 0 {
      return;
    }
    self.pending_work.set(pending - 1);
    self.gate.finish_settlement();
    if pending == 1 {
      self.resolve_close_deferred();
    }
  }

  fn resolve_close_deferred(&self) {
    self.clear_deadline_timer();
    if self.closing.get()
      && !self.closed.get()
      && !self.gate.detached.load(Ordering::Acquire)
      && self.pending_work.get() == 0
    {
      self.finalize_natural_close();
    }
    self.gate.resolve_close();
    self.close_deferred.set(ptr::null_mut());
  }

  fn snapshot(&self) -> InvocationSnapshot {
    InvocationSnapshot {
      callback_len: self.callback_leases.borrow().len(),
      callback_owner_len: self.callback_owners.borrow().len(),
      callback_transfer_len: self.callback_transfers.borrow().len(),
      streams: self.input_streams.borrow().clone(),
      resource_len: self.resource_references.borrow().len(),
    }
  }

  fn rollback(&self, snapshot: InvocationSnapshot) {
    let callbacks = {
      let mut current = self.callback_leases.borrow_mut();
      if current.len() <= snapshot.callback_len {
        Vec::new()
      } else {
        current.split_off(snapshot.callback_len)
      }
    };
    for callback in callbacks {
      if let Some(callback) = callback.upgrade() {
        callback.release();
      }
    }
    let owners = {
      let mut current = self.callback_owners.borrow_mut();
      if current.len() <= snapshot.callback_owner_len {
        Vec::new()
      } else {
        current.split_off(snapshot.callback_owner_len)
      }
    };
    for owner in owners {
      owner.inner.release();
    }
    let transfers = {
      let mut current = self.callback_transfers.borrow_mut();
      if current.len() <= snapshot.callback_transfer_len {
        Vec::new()
      } else {
        current.split_off(snapshot.callback_transfer_len)
      }
    };
    for transfer_id in transfers {
      discard_callback_transfer(self.session_generation, transfer_id);
    }
    self.drain_callback_releases();
    let streams = {
      let mut current = self.input_streams.borrow_mut();
      let added = current
        .difference(&snapshot.streams)
        .copied()
        .collect::<Vec<_>>();
      for id in &added {
        current.remove(id);
      }
      self
        .released_input_streams
        .borrow_mut()
        .extend(added.iter().copied());
      added
    };
    for id in streams {
      if let Ok(value) = js_u32(self.env, id) {
        clear_pending_exception(self.env);
        let _ = self.call_host("releaseInputStream", &[value]);
      }
    }
    loop {
      let Some((resource, kind, reference)) = ({
        let references = self.resource_references.borrow();
        if references.len() <= snapshot.resource_len {
          None
        } else {
          let tracked = references.last().expect("resource length checked");
          Some((
            reference_value(self.env, tracked.reference, "resource lease").ok(),
            tracked.kind,
            tracked.reference,
          ))
        }
      }) else {
        break;
      };
      if let Some(resource) = resource {
        let _ = self.release_resource(resource, kind, false);
      } else {
        let mut references = self.resource_references.borrow_mut();
        if references.len() > snapshot.resource_len {
          references.pop();
          delete_reference(self.env, reference);
        }
      }
    }
  }

  fn drain_callback_releases(&self) {
    drain_callback_releases_with_callback(
      self.env,
      self.host.get(),
      self.release_callback.get(),
      &self.callback_release_queue,
    );
  }

  fn operation(&self, id: u32) -> Result<&SessionOperation> {
    self.operations.get(id as usize).ok_or_else(|| {
      Error::new(
        Status::InvalidArg,
        format!("unknown UniFFI operation ID {id}"),
      )
    })
  }

  fn release_scoped_callback_tokens(&self, tokens: &[CallbackRegistrationToken]) {
    if tokens.is_empty() {
      return;
    }
    let mut contracts = self.callback_contracts.borrow_mut();
    for token in tokens {
      let Some(entries) = contracts.get_mut(&token.key) else {
        continue;
      };
      entries.retain(|entry| entry.token != Some(token.token));
      if entries.is_empty() {
        contracts.remove(&token.key);
      }
    }
  }

  fn register_callback_contract(
    &self,
    key: CallbackKey,
    contract: &SessionCallbackArgument,
    scoped: &mut Vec<CallbackRegistrationToken>,
  ) {
    let mut contracts = self.callback_contracts.borrow_mut();
    let entries = contracts.entry(key).or_default();
    if contract.retention == SessionCallbackRetention::Retained {
      if !entries
        .iter()
        .any(|entry| entry.token.is_none() && entry.contract == *contract)
      {
        entries.push(RegisteredCallbackContract {
          token: None,
          contract: contract.clone(),
        });
      }
    } else {
      let token = self.next_callback_registration_id.get();
      self
        .next_callback_registration_id
        .set(token.wrapping_add(1));
      entries.push(RegisteredCallbackContract {
        token: Some(token),
        contract: contract.clone(),
      });
      scoped.push(CallbackRegistrationToken { key, token });
    }
  }

  /// Resolve all values at a canonical use-site path. Sequence/set/map
  /// segments fan out to every contained value; this is important for
  /// retained callbacks nested inside records rather than silently selecting
  /// element zero. A malformed shape is a backend protocol error.
  fn values_at_path(
    &self,
    args: &[sys::napi_value],
    receiver_offset: usize,
    path: &[SessionValuePathSegment],
    role: &str,
  ) -> Result<Vec<sys::napi_value>> {
    self.values_at_path_with_return(args, receiver_offset, path, None, role)
  }

  fn values_at_path_with_return(
    &self,
    args: &[sys::napi_value],
    receiver_offset: usize,
    path: &[SessionValuePathSegment],
    return_value: Option<sys::napi_value>,
    role: &str,
  ) -> Result<Vec<sys::napi_value>> {
    let Some((root, rest)) = path.split_first() else {
      return Err(Error::new(
        Status::InvalidArg,
        format!("{role} use-site path is empty"),
      ));
    };
    let mut values = match root {
      SessionValuePathSegment::Argument(index) => {
        let position = receiver_offset + *index as usize;
        vec![*args.get(position).ok_or_else(|| {
          Error::new(
            Status::InvalidArg,
            format!("{role} argument path index {index} is out of range"),
          )
        })?]
      }
      SessionValuePathSegment::Return => {
        vec![return_value.ok_or_else(|| {
          Error::new(
            Status::InvalidArg,
            "return path cannot be resolved before a native result",
          )
        })?]
      }
      _ => {
        return Err(Error::new(
          Status::InvalidArg,
          format!("{role} use-site path has an invalid root"),
        ))
      }
    };
    for segment in rest {
      let mut next = Vec::new();
      for value in values {
        match segment {
          SessionValuePathSegment::Field(name) => {
            next.push(named_property(self.env, value, name)?);
          }
          SessionValuePathSegment::StreamItem | SessionValuePathSegment::StreamError => {
            if let Some(value) = stream_step_payload(self.env, value, segment, role)? {
              next.push(value);
            }
          }
          SessionValuePathSegment::Variant(name) => {
            if let Some(value) = resolve_variant(self.env, value, name)? {
              next.push(value);
            }
          }
          SessionValuePathSegment::Optional => {
            if is_null_value(self.env, value)? {
              continue;
            }
            next.push(value);
          }
          SessionValuePathSegment::SequenceElement => {
            let mut is_array = false;
            napi_ohos::check_status!(unsafe {
              sys::napi_is_array(self.env, value, &mut is_array)
            })?;
            if !is_array {
              return Err(Error::new(
                Status::InvalidArg,
                format!("{role} path expected an array sequence/set"),
              ));
            }
            let mut length = 0;
            napi_ohos::check_status!(unsafe {
              sys::napi_get_array_length(self.env, value, &mut length)
            })?;
            for index in 0..length {
              let mut element = ptr::null_mut();
              napi_ohos::check_status!(unsafe {
                sys::napi_get_element(self.env, value, index, &mut element)
              })?;
              next.push(element);
            }
          }
          SessionValuePathSegment::SetElement => {
            next.extend(iterable_values(self.env, value, "values")?);
          }
          SessionValuePathSegment::MapKey => {
            next.extend(
              iterable_entries(self.env, value)?
                .into_iter()
                .map(|(key, _)| key),
            );
          }
          SessionValuePathSegment::MapValue => {
            next.extend(
              iterable_entries(self.env, value)?
                .into_iter()
                .map(|(_, value)| value),
            );
          }
          SessionValuePathSegment::Argument(_) | SessionValuePathSegment::Return => {
            return Err(Error::new(
              Status::InvalidArg,
              format!("{role} use-site path contains a nested root"),
            ));
          }
        }
      }
      values = next;
    }
    Ok(values)
  }

  fn new_callback_lease(&self, key: CallbackKey) -> Result<SessionCallbackLease> {
    self.callback_invoker.check_open()?;
    let callback_type = js_u32(self.env, key.callback_type_id)?;
    let callback_id = js_u32(self.env, key.callback_id)?;
    // Register the logical token before entering user Host code. A
    // re-entrant close can claim and drain this token while retainCallback is
    // running; the post-hook check then observes revocation without a second
    // release.
    let token = match self.callback_release_queue.register(key) {
      Ok(token) => token,
      Err(error) => {
        clear_pending_exception(self.env);
        return Err(error);
      }
    };
    let inner = Arc::new(CallbackLeaseInner {
      token,
      queue: self.callback_release_queue.clone(),
    });
    let lease = SessionCallbackLease { inner };
    self
      .callback_leases
      .borrow_mut()
      .push(Arc::downgrade(&lease.inner));
    if let Err(error) = self.call_host("retainCallback", &[callback_type, callback_id]) {
      // A host retain hook may have side effects before reporting an error;
      // claim the pre-registered token so the attempted retain is balanced.
      clear_pending_exception(self.env);
      lease.inner.release();
      return Err(error);
    }
    if let Err(error) = self.callback_invoker.check_open() {
      // retainCallback may synchronously trigger lifecycle teardown. The
      // token is either claimed by close or by this release, never both.
      clear_pending_exception(self.env);
      lease.inner.release();
      return Err(error);
    }
    Ok(lease)
  }

  fn retain_result_callbacks(
    &self,
    callback_arguments: &[SessionCallbackArgument],
    result_value: sys::napi_value,
  ) -> Result<()> {
    for callback in callback_arguments {
      if !matches!(callback.path.first(), Some(SessionValuePathSegment::Return)) {
        continue;
      }
      let values = self.values_at_path_with_return(
        &[],
        0,
        &callback.path,
        Some(result_value),
        "callback result",
      )?;
      for value in values {
        let callback_id = value_u32(self.env, value, "callback ID")?;
        if callback.retention != SessionCallbackRetention::Retained {
          continue;
        }
        let key = CallbackKey {
          callback_type_id: callback.callback_type_id,
          callback_id,
        };
        let mut no_scoped = Vec::new();
        self.register_callback_contract(key, callback, &mut no_scoped);
        let lease = self.new_callback_lease(key)?;
        self.callback_owners.borrow_mut().push(lease);
      }
    }
    Ok(())
  }

  fn retain_argument_resources(
    &self,
    operation: &SessionOperation,
    args: &[sys::napi_value],
  ) -> Result<RetainedInvocation> {
    let mut use_site_leases = Vec::with_capacity(operation.callback_arguments.len());
    let mut scoped_callbacks = Vec::new();
    let receiver_offset = usize::from(operation.receiver.is_some());
    for callback in &operation.callback_arguments {
      // Return-rooted callbacks are retained when the native result settles;
      // there is no argument value to inspect during invocation.  Preserve an
      // empty slot so argument callback transfer indexes remain canonical.
      if !matches!(
        callback.path.first(),
        Some(SessionValuePathSegment::Argument(_))
      ) {
        use_site_leases.push(Vec::new());
        continue;
      }
      let mut leases = Vec::new();
      let values = self.values_at_path(args, receiver_offset, &callback.path, "callback")?;
      for value in values {
        let callback_id = value_u32(self.env, value, "callback ID")?;
        if callback.retention == SessionCallbackRetention::Retained {
          let key = CallbackKey {
            callback_type_id: callback.callback_type_id,
            callback_id,
          };
          self.register_callback_contract(key, callback, &mut scoped_callbacks);
          leases.push(self.new_callback_lease(key)?);
        }
      }
      use_site_leases.push(leases);
    }
    for stream in &operation.stream_arguments {
      if stream.direction != SessionStreamDirection::Input {
        continue;
      }
      let values = self.values_at_path(args, receiver_offset, &stream.path, "input stream")?;
      for value in values {
        let id = stream_id(self.env, value, "input stream ID")?;
        if !self.released_input_streams.borrow().contains(&id) {
          self.input_streams.borrow_mut().insert(id);
        }
      }
    }
    if let Some(SessionReceiver::Resource(receiver)) = operation.receiver {
      let resource = *args
        .first()
        .ok_or_else(|| Error::new(Status::InvalidArg, "missing resource receiver"))?;
      match receiver {
        SessionResourceReceiver::InputStream => {
          let id = stream_id(self.env, resource, "input stream ID")?;
          if !self.released_input_streams.borrow().contains(&id) {
            self.input_streams.borrow_mut().insert(id);
          }
        }
        SessionResourceReceiver::Object | SessionResourceReceiver::OutputStream => {
          self.retain_resource(resource, receiver)?;
        }
      }
    }
    let transfer_id = if operation.callback_transfer {
      let transfer_id = allocate_callback_transfer_id()?;
      register_callback_transfer(
        self.session_generation,
        transfer_id,
        SessionCallbackTransfers {
          use_sites: Arc::new(use_site_leases),
          invoker: Some(self.callback_invoker.clone()),
        },
      );
      self.callback_transfers.borrow_mut().push(transfer_id);
      Some(transfer_id)
    } else {
      None
    };
    Ok(RetainedInvocation {
      transfer_id,
      scoped_callbacks,
    })
  }

  fn retain_resource(
    &self,
    resource: sys::napi_value,
    kind: SessionResourceReceiver,
  ) -> Result<()> {
    match kind {
      SessionResourceReceiver::InputStream => {
        let _ = stream_id(self.env, resource, "input stream resource ID")?;
      }
      SessionResourceReceiver::Object | SessionResourceReceiver::OutputStream => {
        let handle = named_property(self.env, resource, "handle")?;
        let _ = value_u32(self.env, handle, "resource handle")?;
      }
    }
    let already_tracked = self.resource_references.borrow().iter().any(|tracked| {
      tracked.kind == kind
        && reference_value(self.env, tracked.reference, "resource lease")
          .ok()
          .is_some_and(|existing| strict_equals(self.env, existing, resource))
    });
    if !already_tracked {
      self.resource_references.borrow_mut().push(TrackedResource {
        reference: create_reference(self.env, resource, "resource lease")?,
        kind,
        cancel_called: false,
        cancel_pending: false,
        cancel_promise: ptr::null_mut(),
        release_called: false,
        release_pending: false,
      });
    }
    Ok(())
  }

  fn release_resource(
    &self,
    resource: sys::napi_value,
    kind: SessionResourceReceiver,
    cancel: bool,
  ) -> Result<Option<sys::napi_value>> {
    // Mark the requested hook before invoking user code. Host release hooks
    // are allowed to throw; retaining an unmarked lease would make an
    // explicit cleanup followed by close invoke the hook twice. Output
    // streams keep the lease after cancel so their separate release hook can
    // run once during the normal cancel/finalize sequence.
    let tracked_reference = {
      let mut references = self.resource_references.borrow_mut();
      let Some(index) = references.iter().position(|tracked| {
        tracked.kind == kind
          && reference_value(self.env, tracked.reference, "resource lease")
            .ok()
            .is_some_and(|existing| strict_equals(self.env, existing, resource))
      }) else {
        return Ok(None);
      };
      let tracked = &mut references[index];
      if cancel {
        if tracked.cancel_called {
          // A second cancel while the first hook is in flight must observe
          // that same Promise.  Returning a fresh resolved Promise would let
          // callers (and close()) observe completion before the hook settles.
          if !tracked.cancel_promise.is_null() {
            return Ok(reference_value(self.env, tracked.cancel_promise, "cancel promise").ok());
          }
          return Ok(None);
        }
        tracked.cancel_called = true;
      } else {
        if tracked.release_called {
          return Ok(None);
        }
        if kind == SessionResourceReceiver::OutputStream && tracked.cancel_pending {
          // A release requested while the asynchronous cancel hook is still
          // pending is deferred until that hook settles.  Keep the native
          // lease tracked so close and settlement share one release claim.
          tracked.release_pending = true;
          return Ok(None);
        }
        tracked.release_called = true;
        tracked.release_pending = false;
      }
      let reference = tracked.reference;
      let cancel_promise = tracked.cancel_promise;
      if !cancel || kind != SessionResourceReceiver::OutputStream {
        references.swap_remove(index);
      }
      (reference, cancel_promise)
    };
    if !cancel || kind != SessionResourceReceiver::OutputStream {
      delete_reference(self.env, tracked_reference.0);
      delete_reference(self.env, tracked_reference.1);
    }
    let callback = match (kind, cancel) {
      (SessionResourceReceiver::Object, _) => self.resource_callbacks.release_object.get(),
      (SessionResourceReceiver::InputStream, _) => ptr::null_mut(),
      (SessionResourceReceiver::OutputStream, true) => {
        self.resource_callbacks.cancel_output_stream.get()
      }
      (SessionResourceReceiver::OutputStream, false) => {
        self.resource_callbacks.release_output_stream.get()
      }
    };
    let result = if callback.is_null() {
      None
    } else {
      // A failed user hook leaves a pending JS exception on the N-API
      // environment.  Clear it before invoking a cleanup hook so explicit
      // rollback/close can still release every other lease exactly once.
      clear_pending_exception(self.env);
      let callback = reference_value(self.env, callback, "resource callback")?;
      let handle = named_property(self.env, resource, "handle")?;
      Some(call_function(self.env, resource, callback, &[handle])?)
    };
    Ok(result)
  }

  /// Dispose a native object/output result that arrived after close().  The
  /// normal tracked-resource path cannot be used for callback/input values:
  /// close() has already drained the public lease table and shut down the
  /// callback release TSFN.  Object results can invoke their native release
  /// hook directly.  Output results must first enter the same cancel
  /// settlement state machine as a public lease so an asynchronous producer is
  /// stopped before its final release.
  fn release_late_result(
    &self,
    result: sys::napi_value,
    result_resources: &[SessionResultResourceUseSite],
    session_value: sys::napi_value,
  ) {
    let call_kind = match named_property(self.env, result, "kind") {
      Ok(value) => value,
      Err(_) => {
        clear_pending_exception(self.env);
        return;
      }
    };
    if string_value(self.env, call_kind).ok().flatten().as_deref() != Some("value") {
      clear_pending_exception(self.env);
      return;
    }
    let value = match named_property(self.env, result, "value") {
      Ok(value) => value,
      Err(_) => {
        clear_pending_exception(self.env);
        return;
      }
    };
    let mut released = Vec::new();
    for resource in result_resources {
      let Ok(values) =
        self.values_at_path_with_return(&[], 0, &resource.path, Some(value), "result resource")
      else {
        clear_pending_exception(self.env);
        continue;
      };
      for value in values {
        if released.iter().any(|(kind, existing)| {
          *kind == resource.kind && strict_equals(self.env, *existing, value)
        }) {
          continue;
        }
        released.push((resource.kind, value));
        match resource.kind {
          SessionResourceReceiver::Object => self.release_untracked_resource(value, resource.kind),
          SessionResourceReceiver::OutputStream => self.cancel_late_output(value, session_value),
          SessionResourceReceiver::InputStream => {
            if let Ok(id) = stream_id(self.env, value, "late input stream result ID") {
              clear_pending_exception(self.env);
              if let Ok(stream_id) = js_u32(self.env, id) {
                clear_pending_exception(self.env);
                if let Ok(callback) = reference_value(
                  self.env,
                  self.resource_callbacks.release_input_stream.get(),
                  "late input stream release callback",
                ) {
                  if let Ok(receiver) = js_undefined(self.env) {
                    if let Ok(release) = call_function(self.env, receiver, callback, &[stream_id]) {
                      if is_thenable(self.env, release).unwrap_or(false) {
                        let _ = self.schedule_cleanup_promise(release);
                      }
                    }
                  }
                }
              }
            }
            clear_pending_exception(self.env);
          }
        }
      }
    }
  }

  fn release_untracked_resource(&self, resource: sys::napi_value, kind: SessionResourceReceiver) {
    let callback = match kind {
      SessionResourceReceiver::Object => self.resource_callbacks.release_object.get(),
      SessionResourceReceiver::OutputStream => self.resource_callbacks.release_output_stream.get(),
      SessionResourceReceiver::InputStream => ptr::null_mut(),
    };
    if callback.is_null() {
      return;
    }
    clear_pending_exception(self.env);
    let callback = match reference_value(self.env, callback, "resource callback") {
      Ok(callback) => callback,
      Err(_) => {
        clear_pending_exception(self.env);
        return;
      }
    };
    let handle = match named_property(self.env, resource, "handle") {
      Ok(handle) => handle,
      Err(_) => {
        clear_pending_exception(self.env);
        return;
      }
    };
    let _ = call_function(self.env, resource, callback, &[handle]);
    clear_pending_exception(self.env);
  }

  fn cancel_late_output(&self, resource: sys::napi_value, session_value: sys::napi_value) {
    let reference = match create_reference(self.env, resource, "late output lease") {
      Ok(reference) => reference,
      Err(_) => {
        clear_pending_exception(self.env);
        return;
      }
    };
    self.resource_references.borrow_mut().push(TrackedResource {
      reference,
      kind: SessionResourceReceiver::OutputStream,
      cancel_called: false,
      cancel_pending: false,
      cancel_promise: ptr::null_mut(),
      release_called: false,
      release_pending: false,
    });
    let cancel_result =
      match self.release_resource(resource, SessionResourceReceiver::OutputStream, true) {
        Ok(Some(result)) => Some(result),
        Ok(None) | Err(_) => None,
      };
    clear_pending_exception(self.env);
    let Some(cancel_result) = cancel_result else {
      let _ = self.release_resource(resource, SessionResourceReceiver::OutputStream, false);
      clear_pending_exception(self.env);
      return;
    };
    let is_pending = is_thenable(self.env, cancel_result).unwrap_or(false);
    clear_pending_exception(self.env);
    if !is_pending {
      let _ = self.release_resource(resource, SessionResourceReceiver::OutputStream, false);
      clear_pending_exception(self.env);
      return;
    }
    if self.gate.detached.load(Ordering::Acquire) {
      // The close Promise has already resolved, so a normal settlement would
      // be rejected by the detached lifecycle gate.  Keep cancellation and
      // release ordered with a gate-independent Promise reaction instead.
      if self
        .schedule_late_output_release(
          cancel_result,
          resource,
          named_property(self.env, resource, "handle").unwrap_or(ptr::null_mut()),
        )
        .is_err()
      {
        clear_pending_exception(self.env);
        let _ = self.release_resource(resource, SessionResourceReceiver::OutputStream, false);
        clear_pending_exception(self.env);
      }
      return;
    }
    if self
      .remember_cancel_promise(resource, cancel_result)
      .is_err()
    {
      clear_pending_exception(self.env);
      let _ = self.release_resource(resource, SessionResourceReceiver::OutputStream, false);
      clear_pending_exception(self.env);
      return;
    }
    if self
      .begin_output_cancel(resource, cancel_result, session_value)
      .is_err()
    {
      self.forget_cancel_promise(resource);
      clear_pending_exception(self.env);
      let _ = self.release_resource(resource, SessionResourceReceiver::OutputStream, false);
      clear_pending_exception(self.env);
    }
  }

  fn schedule_late_output_release(
    &self,
    cancel_promise: sys::napi_value,
    resource: sys::napi_value,
    handle: sys::napi_value,
  ) -> Result<()> {
    let release_callback = self.resource_callbacks.release_output_stream.get();
    if release_callback.is_null() {
      let _ = self.release_resource(resource, SessionResourceReceiver::OutputStream, false);
      return Ok(());
    }
    let resource_ref = PendingSessionRef::new(
      self.env,
      create_reference(self.env, resource, "late output cleanup resource")?,
    );
    let handle_ref = Some(PendingSessionRef::new(
      self.env,
      retain_late_cleanup_value(self.env, handle, "late output cleanup handle")?,
    ));
    let release_callback_value =
      reference_value(self.env, release_callback, "output release callback")?;
    let release_callback_ref = match create_reference(
      self.env,
      release_callback_value,
      "late output cleanup callback",
    ) {
      Ok(reference) => PendingSessionRef::new(self.env, reference),
      Err(error) => {
        resource_ref.delete();
        if let Some(handle_ref) = handle_ref.as_ref() {
          handle_ref.delete();
        }
        return Err(error);
      }
    };
    let cleanup = Arc::new(LateOutputRelease {
      resource: resource_ref,
      handle: handle_ref,
      release_callback: release_callback_ref,
      settled: AtomicBool::new(false),
    });
    let fulfilled_context = Box::into_raw(Box::new(LateOutputReleaseContext {
      cleanup: cleanup.clone(),
    }));
    let rejected_context = Box::into_raw(Box::new(LateOutputReleaseContext {
      cleanup: cleanup.clone(),
    }));
    let fulfilled = match create_callback_function(
      self.env,
      "uniffi_late_output_cancel_fulfilled",
      late_output_release_fulfilled,
      fulfilled_context.cast(),
      Some(finalize_late_output_release_context),
    ) {
      Ok(value) => value,
      Err(error) => {
        unsafe {
          drop(Box::from_raw(fulfilled_context));
          drop(Box::from_raw(rejected_context));
        }
        cleanup.resource.delete();
        if let Some(handle_ref) = cleanup.handle.as_ref() {
          handle_ref.delete();
        }
        cleanup.release_callback.delete();
        return Err(error);
      }
    };
    let rejected = match create_callback_function(
      self.env,
      "uniffi_late_output_cancel_rejected",
      late_output_release_rejected,
      rejected_context.cast(),
      Some(finalize_late_output_release_context),
    ) {
      Ok(value) => value,
      Err(error) => {
        unsafe {
          drop(Box::from_raw(rejected_context));
        }
        cleanup.resource.delete();
        if let Some(handle_ref) = cleanup.handle.as_ref() {
          handle_ref.delete();
        }
        cleanup.release_callback.delete();
        return Err(error);
      }
    };
    let then = match named_property(self.env, cancel_promise, "then") {
      Ok(value) => value,
      Err(error) => {
        cleanup.resource.delete();
        if let Some(handle_ref) = cleanup.handle.as_ref() {
          handle_ref.delete();
        }
        cleanup.release_callback.delete();
        return Err(error);
      }
    };
    if let Err(error) = call_function(self.env, cancel_promise, then, &[fulfilled, rejected]) {
      cleanup.resource.delete();
      if let Some(handle_ref) = cleanup.handle.as_ref() {
        handle_ref.delete();
      }
      cleanup.release_callback.delete();
      return Err(error);
    }
    // The tracked lease has transferred ownership to `cleanup`; remove the
    // original reference so finalizer cleanup cannot invoke release twice.
    let tracked_index = {
      let references = self.resource_references.borrow();
      references.iter().position(|tracked| {
        tracked.kind == SessionResourceReceiver::OutputStream
          && reference_value(self.env, tracked.reference, "resource lease")
            .ok()
            .is_some_and(|existing| strict_equals(self.env, existing, resource))
      })
    };
    if let Some(index) = tracked_index {
      let tracked = self.resource_references.borrow_mut().swap_remove(index);
      delete_reference(self.env, tracked.reference);
      delete_reference(self.env, tracked.cancel_promise);
    }
    Ok(())
  }

  fn begin_output_cancel(
    &self,
    resource: sys::napi_value,
    promise: sys::napi_value,
    session_value: sys::napi_value,
  ) -> Result<()> {
    let thenable = is_thenable(self.env, promise)?;
    if !thenable {
      return Ok(());
    }
    let resource_reference = {
      let mut references = self.resource_references.borrow_mut();
      let Some(tracked) = references.iter_mut().find(|tracked| {
        tracked.kind == SessionResourceReceiver::OutputStream
          && reference_value(self.env, tracked.reference, "resource lease")
            .ok()
            .is_some_and(|existing| strict_equals(self.env, existing, resource))
      }) else {
        return Err(Error::new(
          Status::GenericFailure,
          "output stream cancel lease disappeared before settlement",
        ));
      };
      tracked.cancel_pending = true;
      tracked.reference
    };
    if let Err(error) = self.begin_pending_work() {
      if let Some(tracked) = self
        .resource_references
        .borrow_mut()
        .iter_mut()
        .find(|tracked| tracked.reference == resource_reference)
      {
        tracked.cancel_pending = false;
      }
      return Err(error);
    }
    if let Err(error) =
      self.attach_output_cancel_settlement(promise, resource_reference, session_value)
    {
      self.finish_pending_work();
      if let Some(tracked) = self
        .resource_references
        .borrow_mut()
        .iter_mut()
        .find(|tracked| tracked.reference == resource_reference)
      {
        tracked.cancel_pending = false;
      }
      return Err(error);
    }
    Ok(())
  }

  fn remember_cancel_promise(
    &self,
    resource: sys::napi_value,
    promise: sys::napi_value,
  ) -> Result<()> {
    let promise_reference = create_reference(self.env, promise, "output cancel promise")?;
    let mut references = self.resource_references.borrow_mut();
    let Some(tracked) = references.iter_mut().find(|tracked| {
      tracked.kind == SessionResourceReceiver::OutputStream
        && reference_value(self.env, tracked.reference, "resource lease")
          .ok()
          .is_some_and(|existing| strict_equals(self.env, existing, resource))
    }) else {
      delete_reference(self.env, promise_reference);
      return Err(Error::new(
        Status::GenericFailure,
        "output stream cancel lease disappeared before Promise retention",
      ));
    };
    if tracked.cancel_promise.is_null() {
      tracked.cancel_promise = promise_reference;
    } else {
      delete_reference(self.env, promise_reference);
    }
    Ok(())
  }

  fn forget_cancel_promise(&self, resource: sys::napi_value) {
    let promise_reference = self
      .resource_references
      .borrow_mut()
      .iter_mut()
      .find(|tracked| {
        tracked.kind == SessionResourceReceiver::OutputStream
          && reference_value(self.env, tracked.reference, "resource lease")
            .ok()
            .is_some_and(|existing| strict_equals(self.env, existing, resource))
      })
      .map(|tracked| {
        let reference = tracked.cancel_promise;
        tracked.cancel_promise = ptr::null_mut();
        reference
      })
      .unwrap_or(ptr::null_mut());
    delete_reference(self.env, promise_reference);
  }

  fn output_cancel_is_new(&self, resource: sys::napi_value) -> bool {
    self.resource_references.borrow().iter().any(|tracked| {
      tracked.kind == SessionResourceReceiver::OutputStream
        && !tracked.cancel_called
        && reference_value(self.env, tracked.reference, "resource lease")
          .ok()
          .is_some_and(|existing| strict_equals(self.env, existing, resource))
    })
  }

  /// Retain a session object for a settlement callback when one is
  /// available.  Cleanup paths invoked from a finalizer intentionally pass a
  /// null/primitive sentinel; N-API cannot create a strong reference to that
  /// value, but the callback does not need the session object in that path.
  fn new_settlement_ref(
    &self,
    session_value: sys::napi_value,
    role: &str,
  ) -> Result<Arc<PendingSessionRef>> {
    if session_value.is_null() {
      return Ok(PendingSessionRef::new(self.env, ptr::null_mut()));
    }
    let mut value_type = sys::ValueType::napi_undefined;
    napi_ohos::check_status!(unsafe {
      sys::napi_typeof(self.env, session_value, &mut value_type)
    })?;
    if !matches!(
      value_type,
      sys::ValueType::napi_object | sys::ValueType::napi_function
    ) {
      return Ok(PendingSessionRef::new(self.env, ptr::null_mut()));
    }
    let reference =
      PendingSessionRef::new(self.env, create_reference(self.env, session_value, role)?);
    if !self.gate.register_settlement_ref(reference.clone()) {
      return Err(Error::new(
        Status::GenericFailure,
        "session detached before Promise settlement registration",
      ));
    }
    Ok(reference)
  }

  fn new_settlement_shared(
    &self,
    snapshot: Option<InvocationSnapshot>,
    session_value: sys::napi_value,
  ) -> Result<(
    Rc<SettlementShared>,
    Arc<PendingSessionRef>,
    Arc<PendingSessionRef>,
  )> {
    let fulfilled_reference = self.new_settlement_ref(session_value, "pending session")?;
    let rejected_reference = match self.new_settlement_ref(session_value, "pending session") {
      Ok(reference) => reference,
      Err(error) => {
        fulfilled_reference.delete();
        self.gate.unregister_settlement_ref(&fulfilled_reference);
        return Err(error);
      }
    };
    let shared = Rc::new(SettlementShared {
      gate: self.gate.clone(),
      settled: Cell::new(false),
      snapshot: RefCell::new(snapshot),
      session_references: RefCell::new(vec![
        fulfilled_reference.clone(),
        rejected_reference.clone(),
      ]),
      late_result_cleanup: self.late_result_cleanup.clone(),
    });
    Ok((shared, fulfilled_reference, rejected_reference))
  }

  fn attach_output_cancel_settlement(
    &self,
    promise: sys::napi_value,
    resource_reference: sys::napi_ref,
    session_value: sys::napi_value,
  ) -> Result<()> {
    let (shared, fulfilled_reference, rejected_reference) =
      self.new_settlement_shared(None, session_value)?;
    let fulfilled_context = Box::into_raw(Box::new(OutputCancelContext {
      shared: shared.clone(),
      session_reference: fulfilled_reference,
      resource_reference,
    }));
    let rejected_context = Box::into_raw(Box::new(OutputCancelContext {
      shared: shared.clone(),
      session_reference: rejected_reference,
      resource_reference,
    }));
    let fulfilled = create_callback_function(
      self.env,
      "uniffi_output_cancel_fulfilled",
      output_cancel_fulfilled,
      fulfilled_context.cast(),
      Some(finalize_output_cancel_context),
    )
    .map_err(|error| {
      shared.settled.set(true);
      unsafe {
        let fulfilled = Box::from_raw(fulfilled_context);
        let rejected = Box::from_raw(rejected_context);
        fulfilled.session_reference.delete();
        rejected.session_reference.delete();
      }
      error
    })?;
    let rejected = create_callback_function(
      self.env,
      "uniffi_output_cancel_rejected",
      output_cancel_rejected,
      rejected_context.cast(),
      Some(finalize_output_cancel_context),
    )
    .map_err(|error| {
      shared.settled.set(true);
      unsafe {
        let rejected = Box::from_raw(rejected_context);
        rejected.session_reference.delete();
      }
      error
    })?;
    let then = match named_property(self.env, promise, "then") {
      Ok(then) => then,
      Err(error) => {
        shared.settled.set(true);
        return Err(error);
      }
    };
    if let Err(error) = call_function(self.env, promise, then, &[fulfilled, rejected]) {
      shared.settled.set(true);
      return Err(error);
    }
    Ok(())
  }

  fn complete_output_cancel(&self, resource_reference: sys::napi_ref) -> Result<()> {
    let Some(resource) = reference_value(self.env, resource_reference, "resource lease").ok()
    else {
      return Ok(());
    };
    let release = {
      let mut references = self.resource_references.borrow_mut();
      let Some(tracked) = references
        .iter_mut()
        .find(|tracked| tracked.reference == resource_reference)
      else {
        return Ok(());
      };
      tracked.cancel_pending = false;
      // A successful explicit cancel owns the subsequent release.  A close
      // racing that cancel marks `release_pending`; both paths share this
      // same exactly-once claim.
      let release = !tracked.release_called;
      release
    };
    if release {
      let _ = self.release_resource(resource, SessionResourceReceiver::OutputStream, false);
    }
    Ok(())
  }

  fn call_host(&self, method: &str, args: &[sys::napi_value]) -> Result<sys::napi_value> {
    let host = self.host_value()?;
    call_named(self.env, host, method, args)
  }

  fn track_result_value(
    &self,
    result: sys::napi_value,
    result_resources: &[SessionResultResourceUseSite],
    callback_arguments: &[SessionCallbackArgument],
  ) -> Result<()> {
    let call_kind = named_property(self.env, result, "kind")?;
    if string_value(self.env, call_kind)?.as_deref() != Some("value") {
      return Ok(());
    }
    let value = named_property(self.env, result, "value")?;
    self.retain_result_callbacks(callback_arguments, value)?;
    for resource in result_resources {
      let values =
        self.values_at_path_with_return(&[], 0, &resource.path, Some(value), "result resource")?;
      for value in values {
        match resource.kind {
          SessionResourceReceiver::InputStream => {
            let id = stream_id(self.env, value, "input stream result ID")?;
            if !self.released_input_streams.borrow().contains(&id) {
              self.input_streams.borrow_mut().insert(id);
            }
          }
          SessionResourceReceiver::Object | SessionResourceReceiver::OutputStream => {
            self.retain_resource(value, resource.kind)?;
          }
        }
      }
    }
    Ok(())
  }

  fn track_operation_result(
    &self,
    result: sys::napi_value,
    operation: &SessionOperation,
    snapshot: InvocationSnapshot,
    session_value: sys::napi_value,
    scoped_callbacks: Vec<CallbackRegistrationToken>,
  ) -> Result<sys::napi_value> {
    let result_resources = operation.result_resources.as_slice();
    if is_thenable(self.env, result)? {
      if let Err(error) = self.begin_pending_work() {
        self.release_scoped_callback_tokens(&scoped_callbacks);
        self.rollback(snapshot);
        return Err(error);
      }
      if let Err(error) = self.attach_promise_settlement(
        result,
        result_resources,
        operation.callback_arguments.clone(),
        snapshot.clone(),
        session_value,
        scoped_callbacks.clone(),
      ) {
        self.finish_pending_work();
        self.release_scoped_callback_tokens(&scoped_callbacks);
        self.rollback(snapshot);
        return Err(error);
      }
    } else if call_result_is_error(self.env, result)? {
      self.release_scoped_callback_tokens(&scoped_callbacks);
      self.rollback(snapshot);
    } else if self.closing.get() || self.closed.get() || self.gate.detached.load(Ordering::Acquire)
    {
      // A synchronous native invocation may re-enter close() before returning.
      // Its primitive result is still delivered, but resource/callback/input
      // tracking must not create a fresh lease after teardown has begun.
      self.release_scoped_callback_tokens(&scoped_callbacks);
      self.release_late_result(result, result_resources, session_value);
    } else if let Err(error) =
      self.track_result_value(result, result_resources, &operation.callback_arguments)
    {
      self.release_scoped_callback_tokens(&scoped_callbacks);
      self.rollback(snapshot);
      return Err(error);
    } else {
      self.release_scoped_callback_tokens(&scoped_callbacks);
    }
    Ok(result)
  }

  fn attach_promise_settlement(
    &self,
    promise: sys::napi_value,
    result_resources: &[SessionResultResourceUseSite],
    callback_arguments: Vec<SessionCallbackArgument>,
    snapshot: InvocationSnapshot,
    session_value: sys::napi_value,
    scoped_callbacks: Vec<CallbackRegistrationToken>,
  ) -> Result<()> {
    let (shared, fulfilled_reference, rejected_reference) =
      self.new_settlement_shared(Some(snapshot), session_value)?;
    let fulfilled_context = Box::into_raw(Box::new(AsyncResultContext {
      shared: shared.clone(),
      session_reference: fulfilled_reference,
      result_resources: result_resources.to_vec(),
      callback_arguments: callback_arguments.clone(),
      scoped_callbacks: scoped_callbacks.clone(),
    }));
    let rejected_context = Box::into_raw(Box::new(AsyncResultContext {
      shared: shared.clone(),
      session_reference: rejected_reference,
      result_resources: Vec::new(),
      callback_arguments,
      scoped_callbacks,
    }));
    let fulfilled = create_callback_function(
      self.env,
      "uniffi_async_fulfilled",
      async_result_fulfilled,
      fulfilled_context.cast(),
      Some(finalize_async_result_context),
    )
    .map_err(|error| {
      shared.settled.set(true);
      shared.snapshot.borrow_mut().take();
      unsafe {
        let fulfilled = Box::from_raw(fulfilled_context);
        let rejected = Box::from_raw(rejected_context);
        fulfilled.session_reference.delete();
        rejected.session_reference.delete();
      }
      error
    })?;
    let rejected = create_callback_function(
      self.env,
      "uniffi_async_rejected",
      async_result_rejected,
      rejected_context.cast(),
      Some(finalize_async_result_context),
    )
    .map_err(|error| {
      shared.settled.set(true);
      shared.snapshot.borrow_mut().take();
      unsafe {
        let rejected = Box::from_raw(rejected_context);
        rejected.session_reference.delete();
      }
      error
    })?;
    let then = match named_property(self.env, promise, "then") {
      Ok(then) => then,
      Err(error) => {
        shared.settled.set(true);
        shared.snapshot.borrow_mut().take();
        return Err(error);
      }
    };
    if let Err(error) = call_function(self.env, promise, then, &[fulfilled, rejected]) {
      shared.settled.set(true);
      shared.snapshot.borrow_mut().take();
      return Err(error);
    }
    Ok(())
  }

  fn track_input_stream_result(
    &self,
    result: sys::napi_value,
    stream_id: sys::napi_value,
    cancel: bool,
    session_value: sys::napi_value,
  ) -> Result<sys::napi_value> {
    let id = value_u32(self.env, stream_id, "input stream ID")?;
    if is_thenable(self.env, result)? {
      if let Err(error) = self.begin_pending_work() {
        return Err(error);
      }
      if let Err(error) = self.attach_input_settlement(result, id, cancel, session_value) {
        self.finish_pending_work();
        return Err(error);
      }
    } else if cancel || input_step_is_terminal(self.env, result)? {
      if let Some(release) = self.release_input_stream(id)? {
        if is_thenable(self.env, release)? {
          self.schedule_cleanup_promise(release)?;
        }
      }
    }
    Ok(result)
  }

  fn release_input_stream(&self, id: u32) -> Result<Option<sys::napi_value>> {
    if !self.input_streams.borrow_mut().remove(&id) {
      return Ok(None);
    }
    self.released_input_streams.borrow_mut().insert(id);
    let stream_id = js_u32(self.env, id)?;
    Ok(Some(self.call_host("releaseInputStream", &[stream_id])?))
  }

  fn attach_input_settlement(
    &self,
    promise: sys::napi_value,
    stream_id: u32,
    cancel: bool,
    session_value: sys::napi_value,
  ) -> Result<()> {
    let (shared, fulfilled_reference, rejected_reference) =
      self.new_settlement_shared(None, session_value)?;
    let fulfilled_context = Box::into_raw(Box::new(InputSettlementContext {
      shared: shared.clone(),
      session_reference: fulfilled_reference,
      stream_id,
      cancel,
    }));
    let rejected_context = Box::into_raw(Box::new(InputSettlementContext {
      shared: shared.clone(),
      session_reference: rejected_reference,
      stream_id,
      cancel: true,
    }));
    let fulfilled = create_callback_function(
      self.env,
      "uniffi_input_fulfilled",
      input_settlement_fulfilled,
      fulfilled_context.cast(),
      Some(finalize_input_settlement_context),
    )
    .map_err(|error| {
      shared.settled.set(true);
      unsafe {
        let fulfilled = Box::from_raw(fulfilled_context);
        let rejected = Box::from_raw(rejected_context);
        fulfilled.session_reference.delete();
        rejected.session_reference.delete();
      }
      error
    })?;
    let rejected = create_callback_function(
      self.env,
      "uniffi_input_rejected",
      input_settlement_rejected,
      rejected_context.cast(),
      Some(finalize_input_settlement_context),
    )
    .map_err(|error| {
      shared.settled.set(true);
      unsafe {
        let rejected = Box::from_raw(rejected_context);
        rejected.session_reference.delete();
      }
      error
    })?;
    let then = match named_property(self.env, promise, "then") {
      Ok(then) => then,
      Err(error) => {
        shared.settled.set(true);
        return Err(error);
      }
    };
    if let Err(error) = call_function(self.env, promise, then, &[fulfilled, rejected]) {
      shared.settled.set(true);
      return Err(error);
    }
    Ok(())
  }

  fn dispatch(
    &self,
    operation_id: u32,
    args: Vec<sys::napi_value>,
    asynchronous: bool,
    this: sys::napi_value,
  ) -> Result<sys::napi_value> {
    self.ensure_open()?;
    self.drain_callback_releases();
    let operation = self.operation(operation_id)?;
    let is_async = matches!(
      operation.dispatch,
      SessionOperationDispatch::NativeAsync
        | SessionOperationDispatch::CallbackHostAsync { .. }
        | SessionOperationDispatch::InputStreamHostPull
        | SessionOperationDispatch::InputStreamHostCancel
    );
    if asynchronous != is_async {
      return Err(Error::new(
        Status::InvalidArg,
        format!(
          "operation {operation_id} is {}, but was invoked through {}",
          if is_async { "async" } else { "sync" },
          if asynchronous {
            "invokeAsync"
          } else {
            "invokeSync"
          }
        ),
      ));
    }
    let snapshot = self.snapshot();
    let retained = match self.retain_argument_resources(operation, &args) {
      Ok(retained) => retained,
      Err(error) => {
        self.rollback(snapshot);
        return Err(error);
      }
    };
    if operation.callback_transfer && retained.transfer_id.is_none() {
      self.rollback(snapshot);
      return Err(Error::new(
        Status::GenericFailure,
        "missing callback transfer carrier",
      ));
    }

    match operation.dispatch {
      SessionOperationDispatch::NativeSync | SessionOperationDispatch::NativeAsync => {
        let raw_result = (|| {
          let callback = reference_value(self.env, operation.callback.get(), "operation callback")?;
          let mut args = args;
          if matches!(operation.receiver, Some(SessionReceiver::Resource(_))) {
            args[0] = named_property(self.env, args[0], "handle")?;
          }
          let mut native_args = Vec::with_capacity(
            args.len()
              + usize::from(operation.native_call == SessionNativeCall::HostAndArguments)
              + usize::from(operation.callback_transfer) * 2,
          );
          if operation.native_call == SessionNativeCall::HostAndArguments {
            let lease = self.gate.new_host_lease(self.session_generation);
            native_args.push(create_invocation_host_proxy(
              self.env,
              self.host_value()?,
              lease,
            )?);
          }
          if operation.callback_transfer {
            native_args.push(js_u32(self.env, self.session_generation)?);
            native_args.push(js_u32(
              self.env,
              retained
                .transfer_id
                .expect("callback transfer presence checked above"),
            )?);
          }
          native_args.extend(args);
          call_function(self.env, this, callback, &native_args)
        })();
        let result = match raw_result {
          Ok(result) => result,
          Err(error) => {
            self.release_scoped_callback_tokens(&retained.scoped_callbacks);
            self.rollback(snapshot);
            return Err(error);
          }
        };
        self.track_operation_result(result, operation, snapshot, this, retained.scoped_callbacks)
      }
      SessionOperationDispatch::CallbackHostSync {
        callback_type_id,
        method_id,
        error_style,
      } => match self.dispatch_callback_host(
        callback_type_id,
        method_id,
        error_style,
        args,
        false,
        &operation.callback_arguments,
        this,
      ) {
        Ok(result) => {
          self.release_scoped_callback_tokens(&retained.scoped_callbacks);
          Ok(result)
        }
        Err(error) => {
          self.release_scoped_callback_tokens(&retained.scoped_callbacks);
          self.rollback(snapshot);
          Err(error)
        }
      },
      SessionOperationDispatch::CallbackHostAsync {
        callback_type_id,
        method_id,
        error_style,
      } => {
        let result = match self.dispatch_callback_host(
          callback_type_id,
          method_id,
          error_style,
          args,
          true,
          &operation.callback_arguments,
          this,
        ) {
          Ok(result) => result,
          Err(error) => {
            self.release_scoped_callback_tokens(&retained.scoped_callbacks);
            self.rollback(snapshot);
            return Err(error);
          }
        };
        if is_thenable(self.env, result)? {
          if let Err(error) = self.begin_pending_work() {
            self.release_scoped_callback_tokens(&retained.scoped_callbacks);
            self.rollback(snapshot);
            return Err(error);
          }
          if let Err(error) = self.attach_promise_settlement(
            result,
            &[],
            Vec::new(),
            snapshot.clone(),
            this,
            retained.scoped_callbacks.clone(),
          ) {
            self.finish_pending_work();
            self.release_scoped_callback_tokens(&retained.scoped_callbacks);
            self.rollback(snapshot);
            return Err(error);
          }
        } else {
          self.release_scoped_callback_tokens(&retained.scoped_callbacks);
        }
        Ok(result)
      }
      SessionOperationDispatch::InputStreamHostPull => {
        let raw_result = (|| {
          let stream_value = *args
            .first()
            .ok_or_else(|| Error::new(Status::InvalidArg, "pull requires stream ID"))?;
          let id = stream_id(self.env, stream_value, "input stream ID")?;
          let id_value = js_u32(self.env, id)?;
          let result = self.call_host("pullInputStream", &[id_value])?;
          self.track_input_stream_result(result, id_value, false, this)
        })();
        match raw_result {
          Ok(result) => {
            self.release_scoped_callback_tokens(&retained.scoped_callbacks);
            Ok(result)
          }
          Err(error) => {
            self.release_scoped_callback_tokens(&retained.scoped_callbacks);
            self.rollback(snapshot);
            Err(error)
          }
        }
      }
      SessionOperationDispatch::InputStreamHostCancel => {
        let raw_result = (|| {
          let stream_value = *args
            .first()
            .ok_or_else(|| Error::new(Status::InvalidArg, "cancel requires stream ID"))?;
          let id = stream_id(self.env, stream_value, "input stream ID")?;
          let id_value = js_u32(self.env, id)?;
          let result = self.call_host("cancelInputStream", &[id_value])?;
          self.track_input_stream_result(result, id_value, true, this)
        })();
        match raw_result {
          Ok(result) => {
            self.release_scoped_callback_tokens(&retained.scoped_callbacks);
            Ok(result)
          }
          Err(error) => {
            self.release_scoped_callback_tokens(&retained.scoped_callbacks);
            self.rollback(snapshot);
            Err(error)
          }
        }
      }
    }
  }

  fn dispatch_callback_host(
    &self,
    callback_type_id: u32,
    method_id: u32,
    error_style: SessionCallbackErrorStyle,
    args: Vec<sys::napi_value>,
    asynchronous: bool,
    callback_arguments: &[SessionCallbackArgument],
    session: sys::napi_value,
  ) -> Result<sys::napi_value> {
    if !self
      .callback_methods
      .contains_key(&(callback_type_id, method_id))
    {
      return Err(Error::new(
        Status::InvalidArg,
        format!("unknown callback method {callback_type_id}:{method_id}"),
      ));
    }
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
    let registered = self
      .callback_contracts
      .borrow()
      .get(&key)
      .cloned()
      .unwrap_or_default();
    let operation_reentrancy =
      callback_reentrancy_for_operation(callback_arguments, callback_type_id);
    let guarded = operation_reentrancy == SessionCallbackReentrancy::Forbidden
      || registered
        .iter()
        .any(|entry| entry.contract.reentrancy == SessionCallbackReentrancy::Forbidden);
    if !asynchronous
      && registered
        .iter()
        .any(|entry| entry.contract.threading == SessionCallbackThreading::MayCrossThread)
    {
      return Err(Error::new(
        Status::InvalidArg,
        "synchronous callbacks cannot use the may-cross-thread policy",
      ));
    }
    if guarded && !self.active_callbacks.borrow_mut().insert(key) {
      return Err(Error::new(
        Status::InvalidArg,
        "callback reentrancy is forbidden",
      ));
    }
    let callback_args = js_array(self.env, &args[1..])?;
    let mut host_args = vec![
      js_u32(self.env, callback_type_id)?,
      callback_id_value,
      js_u32(self.env, method_id)?,
    ];
    let method = if asynchronous {
      let invocation_id = match self.callback_invoker.next_invocation_id() {
        Ok(value) => value,
        Err(error) => {
          clear_callback_guard_on_error(&self.active_callbacks, guarded, key);
          return Err(error);
        }
      };
      host_args.push(js_u32(self.env, invocation_id)?);
      "invokeCallbackAsync"
    } else {
      self.callback_invoker.check_open()?;
      "invokeCallbackSync"
    };
    host_args.push(callback_args);
    let result = match self.call_host(method, &host_args) {
      Ok(value) => value,
      Err(error) => {
        clear_callback_guard_on_error(&self.active_callbacks, guarded, key);
        let reason =
          take_pending_exception_message(self.env).unwrap_or_else(|| error.reason.clone());
        return Err(match error_style {
          SessionCallbackErrorStyle::Infallible => Error::new(
            Status::GenericFailure,
            format!("infallible callback method {method_id} failed: {reason}"),
          ),
          SessionCallbackErrorStyle::Fallible => Error::new(Status::GenericFailure, reason),
        });
      }
    };
    let result_is_promise = match is_thenable(self.env, result) {
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
      self.release_callback_guard_after_promise(session, result, key)
    } else {
      if guarded {
        release_callback_guard(&self.active_callbacks, key);
      }
      Ok(result)
    }
  }

  fn release_callback_guard_after_promise(
    &self,
    session: sys::napi_value,
    promise: sys::napi_value,
    key: CallbackKey,
  ) -> Result<sys::napi_value> {
    let session_reference = PendingSessionRef::new(
      self.env,
      create_reference(self.env, session, "callback session")?,
    );
    if !self.gate.register_settlement_ref(session_reference.clone()) {
      session_reference.delete();
      return Err(Error::new(
        Status::GenericFailure,
        "callback session detached before Promise finalization",
      ));
    }
    let lease = Rc::new(CallbackGuardLease {
      gate: self.gate.clone(),
      session_reference,
      key,
      finished: Cell::new(false),
    });
    let mut promise = PromiseRaw::<Unknown<'static>>::new(self.env, promise);
    let finally_lease = Rc::clone(&lease);
    let settled = match promise.finally::<(), _>(move |_| {
      finally_lease.finish();
      Ok(())
    }) {
      Ok(value) => value,
      Err(error) => {
        if let Some(message) = take_pending_exception_message(self.env) {
          return Err(Error::new(Status::GenericFailure, message));
        }
        return Err(error);
      }
    };
    Ok(settled.raw())
  }

  fn close(&self, session_value: sys::napi_value) {
    if self.closing.replace(true) {
      return;
    }
    self.gate.invalidate_invocations();
    let callbacks = std::mem::take(&mut *self.callback_leases.borrow_mut());
    for callback in callbacks {
      if let Some(callback) = callback.upgrade() {
        callback.release();
      }
    }
    let owners = std::mem::take(&mut *self.callback_owners.borrow_mut());
    for owner in owners {
      owner.inner.release();
    }
    let transfers = std::mem::take(&mut *self.callback_transfers.borrow_mut());
    for transfer_id in transfers {
      discard_callback_transfer(self.session_generation, transfer_id);
    }
    // Claim every logical lease, including a proxy whose final Arc is between
    // strong-count zero and entering Drop. The queue mutex makes this atomic
    // with a racing Drop; the loser then observes an already-claimed token.
    self.callback_release_queue.claim_all_active();
    self.drain_callback_releases();
    let streams = std::mem::take(&mut *self.input_streams.borrow_mut());
    for stream_id in streams {
      if let Ok(stream_id) = js_u32(self.env, stream_id) {
        clear_pending_exception(self.env);
        if let Ok(result) = self.call_host("releaseInputStream", &[stream_id]) {
          if is_thenable(self.env, result).unwrap_or(false) {
            let _ = self.schedule_cleanup_promise(result);
          }
        }
        clear_pending_exception(self.env);
      }
    }
    let resources = self
      .resource_references
      .borrow()
      .iter()
      .filter_map(|tracked| {
        reference_value(self.env, tracked.reference, "resource lease")
          .ok()
          .map(|value| (value, tracked.kind))
      })
      .collect::<Vec<_>>();
    for (resource, kind) in resources {
      if kind == SessionResourceReceiver::OutputStream
        && !self.resource_callbacks.cancel_output_stream.get().is_null()
        && self.output_cancel_is_new(resource)
      {
        match self.release_resource(resource, kind, true) {
          Ok(Some(cancel)) if is_thenable(self.env, cancel).unwrap_or(false) => {
            let started = self
              .remember_cancel_promise(resource, cancel)
              .and_then(|_| self.begin_output_cancel(resource, cancel, session_value));
            if started.is_err() {
              self.forget_cancel_promise(resource);
              let _ = self.release_resource(resource, kind, false);
            }
          }
          Ok(_) | Err(_) => {
            let _ = self.release_resource(resource, kind, false);
          }
        }
      } else {
        let _ = self.release_resource(resource, kind, false);
      }
      clear_pending_exception(self.env);
    }
  }

  fn schedule_cleanup_promise(&self, promise: sys::napi_value) -> Result<()> {
    self.begin_pending_work()?;
    let undefined = js_undefined(self.env)?;
    if let Err(error) = self.attach_input_settlement(promise, 0, true, undefined) {
      self.finish_pending_work();
      return Err(error);
    }
    Ok(())
  }

  fn close_promise(&self) -> Result<sys::napi_value> {
    let existing = self.close_promise.get();
    if !existing.is_null() {
      return reference_value(self.env, existing, "close promise");
    }

    let mut deferred = ptr::null_mut();
    let mut promise = ptr::null_mut();
    napi_ohos::check_status!(unsafe {
      sys::napi_create_promise(self.env, &mut deferred, &mut promise)
    })?;
    let promise_reference = match create_reference(self.env, promise, "close promise") {
      Ok(reference) => reference,
      Err(error) => {
        // There is no JS-visible promise to reject if creating its native
        // reference fails. Keep the deferred unreachable and report the
        // allocation error to the caller instead.
        return Err(error);
      }
    };
    self.close_promise.set(promise_reference);
    self.close_deferred.set(deferred);
    self.gate.set_close_deferred(deferred);
    self.start_deadline_timer()?;
    if self.pending_work.get() == 0 {
      self.resolve_close_deferred();
    }
    Ok(promise)
  }

  fn cleanup_references(&self) {
    if !self.closing.get() && !self.gate.detached.load(Ordering::Acquire) {
      self.close(ptr::null_mut());
    }
    if !self.gate.detached.load(Ordering::Acquire) {
      let _ = self.gate.detach();
      self.closed.set(true);
      self.clear_deadline_timer();
    } else {
      self.clear_deadline_timer();
    }
    // The TSFN context owns all data needed by its callback.  During ordinary
    // explicit close the JS method shuts the TSFN down below; during Node
    // environment cleanup the TSFN finalizer marks the queue detached first,
    // so this finalizer never releases an already-finalized TSFN.
    self.callback_release_queue.shutdown();
    for operation in &self.operations {
      let reference = operation.callback.replace(ptr::null_mut());
      delete_reference(self.env, reference);
    }
    let host = self.host.replace(ptr::null_mut());
    delete_reference(self.env, host);
    delete_reference(self.env, self.release_callback.replace(ptr::null_mut()));
    delete_reference(self.env, self.close_promise.replace(ptr::null_mut()));
    self.close_deferred.set(ptr::null_mut());
    for callback in [
      &self.resource_callbacks.release_object,
      &self.resource_callbacks.release_input_stream,
      &self.resource_callbacks.cancel_output_stream,
      &self.resource_callbacks.release_output_stream,
    ] {
      delete_reference(self.env, callback.replace(ptr::null_mut()));
    }
  }

  fn start_deadline_timer(&self) -> Result<()> {
    if !self.deadline_timer.get().is_null() {
      return Ok(());
    }
    let global = get_global(self.env)?;
    let set_timeout = named_property(self.env, global, "setTimeout")?;
    let context = Box::into_raw(Box::new(DeadlineContext {
      gate: self.gate.clone(),
    }));
    let callback = match create_callback_function(
      self.env,
      "uniffi_close_deadline",
      deadline_timer_callback,
      context.cast(),
      Some(finalize_deadline_context),
    ) {
      Ok(callback) => callback,
      Err(error) => {
        unsafe {
          drop(Box::from_raw(context));
        }
        return Err(error);
      }
    };
    let delay = js_u32(self.env, self.close_policy.grace_ms)?;
    let timer = call_function(self.env, global, set_timeout, &[callback, delay])?;
    // Ark's setTimeout returns a numeric handle; keep the timer entirely on
    // the owning JS event-loop thread.
    self
      .deadline_timer
      .set(create_reference(self.env, timer, "close deadline timer")?);
    Ok(())
  }

  fn clear_deadline_timer(&self) {
    let reference = self.deadline_timer.replace(ptr::null_mut());
    if reference.is_null() {
      return;
    }
    if let Ok(timer) = reference_value(self.env, reference, "close deadline timer") {
      if let Ok(global) = get_global(self.env) {
        if let Ok(clear_timeout) = named_property(self.env, global, "clearTimeout") {
          clear_pending_exception(self.env);
          let _ = call_function(self.env, global, clear_timeout, &[timer]);
          clear_pending_exception(self.env);
        }
      }
    }
    delete_reference(self.env, reference);
  }

  fn capture_pending_output_handles(&self) -> Vec<(sys::napi_ref, sys::napi_value)> {
    self
      .resource_references
      .borrow()
      .iter()
      .filter_map(|tracked| {
        if tracked.kind != SessionResourceReceiver::OutputStream || !tracked.cancel_pending {
          return None;
        }
        let resource = reference_value(self.env, tracked.reference, "resource lease").ok();
        let handle =
          resource.and_then(|resource| named_property(self.env, resource, "handle").ok());
        clear_pending_exception(self.env);
        handle.map(|handle| (tracked.reference, handle))
      })
      .collect()
  }

  fn deadline_detach(&self) {
    if self.gate.state_ptr() != (self as *const SessionState).cast_mut() {
      return;
    }
    let pending_output_handles = self.capture_pending_output_handles();
    // `detach` atomically clears the pointer observed by every settlement and
    // Host lease before this method drops any N-API references.
    let _ = self.gate.detach();
    self.closing.set(true);
    self.closed.set(true);
    self.clear_deadline_timer();
    self.callback_release_queue.claim_all_active();
    self.callback_release_queue.take_pending();
    self.callback_release_queue.shutdown();
    let transfers = std::mem::take(&mut *self.callback_transfers.borrow_mut());
    for transfer_id in transfers {
      discard_callback_transfer(self.session_generation, transfer_id);
    }
    self.callback_leases.borrow_mut().clear();
    self.callback_owners.borrow_mut().clear();
    self.callback_contracts.borrow_mut().clear();
    self.active_callbacks.borrow_mut().clear();
    self.input_streams.borrow_mut().clear();
    let resources = std::mem::take(&mut *self.resource_references.borrow_mut());
    for tracked in resources {
      let resource = reference_value(self.env, tracked.reference, "resource lease").ok();
      // Resolve the Promise value before deleting the state-owned reference;
      // after detach the normal OutputCancelContext cannot re-enter state, so
      // the independent reaction below needs this value to preserve
      // cancel-before-release ordering.
      let cancel_promise = if tracked.cancel_pending && !tracked.cancel_promise.is_null() {
        reference_value(self.env, tracked.cancel_promise, "cancel promise").ok()
      } else {
        None
      };
      if let Some(resource) = resource {
        match tracked.kind {
          SessionResourceReceiver::Object => {
            self.release_untracked_resource(resource, tracked.kind)
          }
          SessionResourceReceiver::OutputStream if tracked.cancel_pending => {
            // The normal OutputCancelContext is tied to the lifecycle gate;
            // after deadline detach its callback can no longer reach
            // SessionState. Transfer the still-pending cancel Promise and
            // resource to a gate-independent reaction before dropping the
            // state-owned references, so cancel -> release ordering remains
            // intact without reviving the Host.
            let handle = pending_output_handles
              .iter()
              .find(|(reference, _)| *reference == tracked.reference)
              .map(|(_, handle)| *handle);
            let scheduled = cancel_promise
              .zip(handle)
              .and_then(|(promise, handle)| {
                match self.schedule_late_output_release(promise, resource, handle) {
                  Ok(()) => Some(()),
                  Err(_error) => None,
                }
              })
              .is_some();
            if !scheduled {
              clear_pending_exception(self.env);
              // No usable Promise remains to order the cleanup. Avoid
              // leaking the native lease; this fallback is only reached for
              // an already-failed scheduling path.
              self.release_untracked_resource(resource, tracked.kind);
            }
          }
          SessionResourceReceiver::OutputStream => {
            self.release_untracked_resource(resource, tracked.kind)
          }
          SessionResourceReceiver::InputStream => {}
        }
      }
      delete_reference(self.env, tracked.reference);
      delete_reference(self.env, tracked.cancel_promise);
    }
    for operation in &self.operations {
      delete_reference(self.env, operation.callback.replace(ptr::null_mut()));
    }
    delete_reference(self.env, self.host.replace(ptr::null_mut()));
    delete_reference(self.env, self.release_callback.replace(ptr::null_mut()));
    // Keep the native close Promise reference until the wrapped session's
    // finalizer so a repeated close() after deadline returns the exact same
    // Promise object.
    self.close_deferred.set(ptr::null_mut());
    // Keep resource hook references until cleanup_references() so late
    // object/output results can still run their native release hooks after
    // detach.  The session finalizer deletes them exactly once.
  }

  /// Natural close completion follows the same detach transition as the
  /// deadline path, but is reached only after every counted settlement has
  /// finished.  Keeping this transition explicit revokes all retained Host
  /// proxies and drops native references before resolving close().
  fn finalize_natural_close(&self) {
    if self.gate.state_ptr() != (self as *const SessionState).cast_mut() {
      return;
    }
    let _ = self.gate.detach();
    self.closing.set(true);
    self.closed.set(true);
    self.callback_release_queue.claim_all_active();
    self.callback_release_queue.take_pending();
    self.callback_release_queue.shutdown();
    let transfers = std::mem::take(&mut *self.callback_transfers.borrow_mut());
    for transfer_id in transfers {
      discard_callback_transfer(self.session_generation, transfer_id);
    }
    self.callback_leases.borrow_mut().clear();
    self.callback_owners.borrow_mut().clear();
    self.callback_contracts.borrow_mut().clear();
    self.active_callbacks.borrow_mut().clear();
    self.input_streams.borrow_mut().clear();
    let resources = std::mem::take(&mut *self.resource_references.borrow_mut());
    for tracked in resources {
      let resource = reference_value(self.env, tracked.reference, "resource lease").ok();
      delete_reference(self.env, tracked.reference);
      delete_reference(self.env, tracked.cancel_promise);
      if let Some(resource) = resource {
        if tracked.kind == SessionResourceReceiver::Object
          || (tracked.kind == SessionResourceReceiver::OutputStream && !tracked.cancel_pending)
        {
          self.release_untracked_resource(resource, tracked.kind);
        }
      }
    }
    for operation in &self.operations {
      delete_reference(self.env, operation.callback.replace(ptr::null_mut()));
    }
    delete_reference(self.env, self.host.replace(ptr::null_mut()));
    delete_reference(self.env, self.release_callback.replace(ptr::null_mut()));
    // Keep resource hook references until cleanup_references() so late
    // object/output results can still run their native release hooks after
    // detach.  The session finalizer deletes them exactly once.
  }
}

/// Build the session object returned by the sole generated factory export.
pub fn create_backend_session(
  env: &Env,
  host: Object<'static>,
  close_policy: napi_family_core::ClosePolicy,
  descriptors: Vec<SessionOperationDescriptor>,
  resource_callbacks: SessionResourceCallbacks,
) -> Result<Object<'static>> {
  let session_generation = allocate_session_generation()?;
  let gate = LifecycleGate::new(env.raw());
  let host_reference = create_reference(env.raw(), host.raw(), "Host")?;
  let requires_input_release = descriptors.iter().any(|descriptor| {
    matches!(
      descriptor.receiver,
      Some(SessionReceiver::Resource(
        SessionResourceReceiver::InputStream
      ))
    ) || descriptor
      .result_resources
      .iter()
      .any(|resource| resource.kind == SessionResourceReceiver::InputStream)
      || descriptor
        .stream_arguments
        .iter()
        .any(|stream| stream.direction == SessionStreamDirection::Input)
      || matches!(
        descriptor.dispatch,
        SessionOperationDispatch::InputStreamHostPull
          | SessionOperationDispatch::InputStreamHostCancel
      )
  });
  let release_input_stream = if requires_input_release {
    Some(named_property(env.raw(), host.raw(), "releaseInputStream")?)
  } else {
    None
  };
  let release_callback_reference = create_reference(
    env.raw(),
    named_property(env.raw(), host.raw(), "releaseCallback")?,
    "releaseCallback",
  )?;
  let mut operations = Vec::with_capacity(descriptors.len());
  let mut callback_methods = BTreeMap::new();
  for (id, descriptor) in descriptors.into_iter().enumerate() {
    match descriptor.dispatch {
      SessionOperationDispatch::CallbackHostSync {
        callback_type_id,
        method_id,
        error_style,
      }
      | SessionOperationDispatch::CallbackHostAsync {
        callback_type_id,
        method_id,
        error_style,
      } => {
        callback_methods.insert((callback_type_id, method_id), error_style);
      }
      _ => {}
    }
    if descriptor
      .result_resources
      .iter()
      .any(|resource| resource.ownership != SessionResourceOwnership::Owned)
    {
      return Err(Error::new(
        Status::InvalidArg,
        format!("result resource use-site in operation {id} must be owned"),
      ));
    }
    let callback = match (descriptor.dispatch, descriptor.callback) {
      (
        SessionOperationDispatch::NativeSync | SessionOperationDispatch::NativeAsync,
        Some(value),
      ) => create_reference(env.raw(), value, "operation callback")?,
      (SessionOperationDispatch::NativeSync | SessionOperationDispatch::NativeAsync, None) => {
        return Err(Error::new(
          Status::InvalidArg,
          format!("native UniFFI operation slot {id} has no callback"),
        ));
      }
      (_, Some(_)) => {
        return Err(Error::new(
          Status::InvalidArg,
          format!("host UniFFI operation slot {id} unexpectedly has a native callback"),
        ));
      }
      (_, None) => ptr::null_mut(),
    };
    operations.push(SessionOperation {
      dispatch: descriptor.dispatch,
      callback: Cell::new(callback),
      native_call: descriptor.native_call,
      receiver: descriptor.receiver,
      result_resources: descriptor.result_resources,
      callback_transfer: descriptor.callback_transfer,
      callback_arguments: descriptor.callback_arguments,
      stream_arguments: descriptor.stream_arguments,
    });
  }

  let resource_callbacks = ResourceCallbacks {
    release_object: Cell::new(optional_reference(
      env.raw(),
      resource_callbacks.release_object,
      "object release callback",
    )?),
    release_input_stream: Cell::new(optional_reference(
      env.raw(),
      release_input_stream,
      "input-stream release callback",
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
  };
  let late_result_cleanup = LateResultCleanup::new(env.raw(), &resource_callbacks)?;
  let state = Box::new(SessionState {
    env: env.raw(),
    gate: gate.clone(),
    session_generation,
    host: Cell::new(host_reference),
    release_callback: Cell::new(release_callback_reference),
    operations,
    closing: Cell::new(false),
    closed: Cell::new(false),
    close_policy,
    deadline_timer: Cell::new(ptr::null_mut()),
    callback_invoker: SessionCallbackInvoker::new(gate.clone()),
    pending_work: Cell::new(0),
    close_deferred: Cell::new(ptr::null_mut()),
    close_promise: Cell::new(ptr::null_mut()),
    callback_leases: RefCell::new(Vec::new()),
    callback_owners: RefCell::new(Vec::new()),
    callback_transfers: RefCell::new(Vec::new()),
    callback_release_queue: Arc::new(CallbackReleaseQueue {
      state: Mutex::new(CallbackReleaseState {
        next_token: 0,
        active: BTreeMap::new(),
        pending: Vec::new(),
      }),
      tsfn: std::sync::atomic::AtomicPtr::new(ptr::null_mut()),
      tsfn_guard: Mutex::new(()),
      closed: AtomicBool::new(false),
    }),
    callback_methods,
    next_callback_registration_id: Cell::new(0),
    callback_contracts: RefCell::new(BTreeMap::new()),
    active_callbacks: RefCell::new(BTreeSet::new()),
    input_streams: RefCell::new(BTreeSet::new()),
    released_input_streams: RefCell::new(BTreeSet::new()),
    resource_references: RefCell::new(Vec::new()),
    resource_callbacks,
    late_result_cleanup,
  });
  let session = Object::new(env)?;
  let raw_session = session.raw();
  let state = Box::into_raw(state);
  if !gate.install_state(state) {
    let state = unsafe { Box::from_raw(state) };
    state.cleanup_references();
    return Err(Error::new(
      Status::GenericFailure,
      "session lifecycle gate detached during creation",
    ));
  }
  let callback_release_queue = unsafe { (&*state).callback_release_queue.clone() };
  let callback_release_tsfn =
    match create_callback_release_tsfn(env.raw(), host.raw(), callback_release_queue.clone()) {
      Ok(tsfn) => tsfn,
      Err(error) => {
        let state = unsafe { Box::from_raw(state) };
        state.cleanup_references();
        return Err(error);
      }
    };
  if !callback_release_queue.install(callback_release_tsfn) {
    let state = unsafe { Box::from_raw(state) };
    state.cleanup_references();
    return Err(Error::new(
      Status::GenericFailure,
      "callback release scheduler closed during session creation",
    ));
  }
  napi_ohos::check_status!(unsafe {
    sys::napi_wrap(
      env.raw(),
      raw_session,
      state.cast(),
      Some(finalize_session),
      ptr::null_mut(),
      ptr::null_mut(),
    )
  })
  .map_err(|error| {
    callback_release_queue.shutdown();
    let state = unsafe { Box::from_raw(state) };
    state.cleanup_references();
    error
  })?;
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

fn create_callback_release_tsfn(
  env: sys::napi_env,
  host: sys::napi_value,
  queue: Arc<CallbackReleaseQueue>,
) -> Result<sys::napi_threadsafe_function> {
  let name = CString::new("uniffi_callback_release")?;
  let mut async_resource_name = ptr::null_mut();
  napi_ohos::check_status!(unsafe {
    sys::napi_create_string_utf8(
      env,
      name.as_ptr().cast(),
      name.as_bytes().len() as isize,
      &mut async_resource_name,
    )
  })?;
  let mut tsfn = ptr::null_mut();
  let release_callback = named_property(env, host, "releaseCallback")?;
  let host = create_reference(env, host, "callback release Host")?;
  let release_callback = match create_reference(env, release_callback, "releaseCallback") {
    Ok(reference) => reference,
    Err(error) => {
      delete_reference(env, host);
      return Err(error);
    }
  };
  let context = Box::into_raw(Box::new(CallbackReleaseTsfnContext {
    host,
    release_callback,
    queue,
  }));
  let callback = match create_callback_function(
    env,
    "uniffi_callback_release",
    callback_release_js,
    context.cast(),
    None,
  ) {
    Ok(callback) => callback,
    Err(error) => {
      let context = unsafe { Box::from_raw(context) };
      delete_reference(env, context.host);
      delete_reference(env, context.release_callback);
      return Err(error);
    }
  };
  if let Err(error) = napi_ohos::check_status!(unsafe {
    sys::napi_create_threadsafe_function(
      env,
      callback,
      ptr::null_mut(),
      async_resource_name,
      0,
      1,
      context.cast(),
      Some(finalize_callback_release_tsfn),
      context.cast(),
      Some(callback_release_tsfn),
      &mut tsfn,
    )
  }) {
    let context = unsafe { Box::from_raw(context) };
    delete_reference(env, context.host);
    delete_reference(env, context.release_callback);
    return Err(error);
  }
  // The scheduler is only a wake-up mechanism for this session.  It must not
  // keep the Node event loop alive on its own; the owning JS session and its
  // finalizer determine its lifetime.
  if let Err(error) =
    napi_ohos::check_status!(unsafe { sys::napi_unref_threadsafe_function(env, tsfn) })
  {
    let _ = unsafe {
      sys::napi_release_threadsafe_function(tsfn, sys::ThreadsafeFunctionReleaseMode::abort)
    };
    return Err(error);
  }
  Ok(tsfn)
}

unsafe extern "C" fn callback_release_js(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  let mut argc = 0;
  let mut this = ptr::null_mut();
  let mut data = ptr::null_mut();
  let status =
    unsafe { sys::napi_get_cb_info(env, info, &mut argc, ptr::null_mut(), &mut this, &mut data) };
  if status == sys::Status::napi_ok && !data.is_null() {
    let context = unsafe { &*data.cast::<CallbackReleaseTsfnContext>() };
    drain_callback_releases_with_callback(
      env,
      context.host,
      context.release_callback,
      &context.queue,
    );
  }
  js_undefined(env).unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn callback_release_tsfn(
  env: sys::napi_env,
  js_callback: sys::napi_value,
  context: *mut c_void,
  _data: *mut c_void,
) {
  if !env.is_null() && !js_callback.is_null() {
    let receiver = js_undefined(env).unwrap_or(ptr::null_mut());
    let mut result = ptr::null_mut();
    let status =
      unsafe { sys::napi_call_function(env, receiver, js_callback, 0, ptr::null(), &mut result) };
    if status != sys::Status::napi_ok {
      clear_pending_exception(env);
    }
  }
  if env.is_null() || context.is_null() {
    return;
  }
  // The TSFN's regular JS callback (`callback_release_js`) owns the actual
  // drain.  This hook intentionally does not dereference the context: N-API
  // can invoke it with a null env/callback while an aborting TSFN is closing.
}

unsafe extern "C" fn finalize_callback_release_tsfn(
  env: sys::napi_env,
  data: *mut c_void,
  _hint: *mut c_void,
) {
  if data.is_null() {
    return;
  }
  let context = unsafe { Box::from_raw(data.cast::<CallbackReleaseTsfnContext>()) };
  context.queue.mark_finalized_by_env();
  // The host reference is owned by this TSFN context.  If N-API is already
  // tearing down the environment there is no legal API call left to make;
  // otherwise delete it exactly once here, after the final possible
  // `call_js_cb` invocation.
  if !env.is_null() {
    delete_reference(env, context.host);
    delete_reference(env, context.release_callback);
  }
}

unsafe extern "C" fn session_invoke_sync(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  callback_result(env, || {
    let (this, args) = callback_args(env, info, 2)?;
    let operation_id = value_u32(env, args[0], "operation ID")?;
    let operation_args = array_values(env, args[1])?;
    state(env, this)?.dispatch(operation_id, operation_args, false, this)
  })
}

unsafe extern "C" fn session_invoke_async(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  callback_result(env, || {
    let (this, args) = callback_args(env, info, 2)?;
    let operation_id = value_u32(env, args[0], "operation ID")?;
    let operation_args = array_values(env, args[1])?;
    state(env, this)?.dispatch(operation_id, operation_args, true, this)
  })
}

unsafe extern "C" fn session_release_object(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  // The ABI requires release to be non-throwing and idempotent.  Invalid or
  // already-released leases therefore collapse to a no-op.
  let result = (|| {
    let (this, args) = callback_args(env, info, 1)?;
    let _ = state(env, this)?.release_resource(args[0], SessionResourceReceiver::Object, false);
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
    let state = state(env, this)?;
    let is_new_cancel = state.output_cancel_is_new(args[0]);
    let has_cancel_hook = !state
      .resource_callbacks
      .cancel_output_stream
      .get()
      .is_null();
    let result = state
      .release_resource(args[0], SessionResourceReceiver::OutputStream, true)?
      .unwrap_or(resolved_promise(env)?);
    if is_new_cancel {
      state.remember_cancel_promise(args[0], result)?;
      if has_cancel_hook {
        if let Err(error) = state.begin_output_cancel(args[0], result, this) {
          state.forget_cancel_promise(args[0]);
          return Err(error);
        }
      }
    }
    Ok(result)
  })
}

unsafe extern "C" fn session_release_output_stream(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  let result = (|| {
    let (this, args) = callback_args(env, info, 1)?;
    let _ =
      state(env, this)?.release_resource(args[0], SessionResourceReceiver::OutputStream, false);
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
    let state = state(env, this)?;
    state.close(this);
    state.close_promise()
  })
}

fn state(env: sys::napi_env, this: sys::napi_value) -> Result<&'static SessionState> {
  let mut data = ptr::null_mut();
  napi_ohos::check_status!(unsafe { sys::napi_unwrap(env, this, &mut data) })?;
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
  napi_ohos::check_status!(unsafe {
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

fn array_values(env: sys::napi_env, array: sys::napi_value) -> Result<Vec<sys::napi_value>> {
  let mut is_array = false;
  napi_ohos::check_status!(unsafe { sys::napi_is_array(env, array, &mut is_array) })?;
  if !is_array {
    return Err(Error::new(
      Status::InvalidArg,
      "UniFFI invocation arguments must be an array",
    ));
  }
  let mut length = 0;
  napi_ohos::check_status!(unsafe { sys::napi_get_array_length(env, array, &mut length) })?;
  let mut result = Vec::with_capacity(length as usize);
  for index in 0..length {
    let mut value = ptr::null_mut();
    napi_ohos::check_status!(unsafe { sys::napi_get_element(env, array, index, &mut value) })?;
    result.push(value);
  }
  Ok(result)
}

fn add_method(
  env: sys::napi_env,
  object: sys::napi_value,
  name: &str,
  callback: unsafe extern "C" fn(sys::napi_env, sys::napi_callback_info) -> sys::napi_value,
) -> Result<()> {
  let name = CString::new(name)?;
  let mut function = ptr::null_mut();
  napi_ohos::check_status!(unsafe {
    sys::napi_create_function(
      env,
      name.as_ptr(),
      name.as_bytes().len() as isize,
      Some(callback),
      ptr::null_mut(),
      &mut function,
    )
  })?;
  napi_ohos::check_status!(unsafe {
    sys::napi_set_named_property(env, object, name.as_ptr(), function)
  })
}

fn call_named(
  env: sys::napi_env,
  this: sys::napi_value,
  name: &str,
  args: &[sys::napi_value],
) -> Result<sys::napi_value> {
  let name = CString::new(name)?;
  let mut function = ptr::null_mut();
  let status = unsafe { sys::napi_get_named_property(env, this, name.as_ptr(), &mut function) };
  if status != sys::Status::napi_ok {
    return Err(Error::new(Status::from(status), "get host method failed"));
  }
  let mut value_type = sys::ValueType::napi_undefined;
  let status = unsafe { sys::napi_typeof(env, function, &mut value_type) };
  if status != sys::Status::napi_ok {
    return Err(Error::new(Status::from(status), "host method type failed"));
  }
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
  napi_ohos::check_status!(unsafe {
    sys::napi_get_named_property(env, object, name.as_ptr(), &mut value)
  })?;
  Ok(value)
}

fn get_property(
  env: sys::napi_env,
  object: sys::napi_value,
  key: sys::napi_value,
) -> Result<sys::napi_value> {
  let mut value = ptr::null_mut();
  napi_ohos::check_status!(unsafe { sys::napi_get_property(env, object, key, &mut value) })?;
  Ok(value)
}

fn set_property(
  env: sys::napi_env,
  object: sys::napi_value,
  name: &str,
  value: sys::napi_value,
) -> Result<()> {
  let name = CString::new(name)?;
  let mut key = ptr::null_mut();
  napi_ohos::check_status!(unsafe {
    sys::napi_create_string_utf8(env, name.as_ptr(), name.as_bytes().len() as isize, &mut key)
  })?;
  napi_ohos::check_status!(unsafe { sys::napi_set_property(env, object, key, value) })
}

fn call_function(
  env: sys::napi_env,
  this: sys::napi_value,
  function: sys::napi_value,
  args: &[sys::napi_value],
) -> Result<sys::napi_value> {
  let mut result = ptr::null_mut();
  let status =
    unsafe { sys::napi_call_function(env, this, function, args.len(), args.as_ptr(), &mut result) };
  if status != sys::Status::napi_ok {
    return Err(Error::new(Status::from(status), "call_function failed"));
  }
  Ok(result)
}

fn create_reference(
  env: sys::napi_env,
  value: sys::napi_value,
  role: &str,
) -> Result<sys::napi_ref> {
  let mut reference = ptr::null_mut();
  napi_ohos::check_status!(
    unsafe { sys::napi_create_reference(env, value, 1, &mut reference) },
    "failed to retain {role}"
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
  napi_ohos::check_status!(
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
  let mut value_type = sys::ValueType::napi_undefined;
  napi_ohos::check_status!(unsafe { sys::napi_typeof(env, value, &mut value_type) })?;
  if value_type != sys::ValueType::napi_number {
    return Err(Error::new(
      Status::InvalidArg,
      format!("{role} must be an unsigned 32-bit integer"),
    ));
  }
  let mut number = 0.0;
  napi_ohos::check_status!(unsafe { sys::napi_get_value_double(env, value, &mut number) })?;
  if !number.is_finite() || number.fract() != 0.0 || number < 0.0 || number > u32::MAX as f64 {
    return Err(Error::new(
      Status::InvalidArg,
      format!("{role} must be an unsigned 32-bit integer"),
    ));
  }
  Ok(number as u32)
}

fn stream_id(env: sys::napi_env, value: sys::napi_value, role: &str) -> Result<u32> {
  let mut value_type = sys::ValueType::napi_undefined;
  napi_ohos::check_status!(unsafe { sys::napi_typeof(env, value, &mut value_type) })?;
  if value_type == sys::ValueType::napi_object {
    if let Ok(handle) = named_property(env, value, "handle") {
      return value_u32(env, handle, role);
    }
  }
  value_u32(env, value, role)
}

fn is_null_value(env: sys::napi_env, value: sys::napi_value) -> Result<bool> {
  let mut value_type = sys::ValueType::napi_undefined;
  napi_ohos::check_status!(unsafe { sys::napi_typeof(env, value, &mut value_type) })?;
  Ok(value_type == sys::ValueType::napi_null)
}

/// Resolve one branch of the canonical output-stream step carrier. A valid
/// non-selected branch is skipped; malformed own-key shapes are protocol
/// errors and therefore follow the caller's existing rollback path.
fn stream_step_payload(
  env: sys::napi_env,
  value: sys::napi_value,
  selector: &SessionValuePathSegment,
  role: &str,
) -> Result<Option<sys::napi_value>> {
  let mut value_type = sys::ValueType::napi_undefined;
  napi_ohos::check_status!(unsafe { sys::napi_typeof(env, value, &mut value_type) })?;
  if value_type != sys::ValueType::napi_object {
    return Err(Error::new(
      Status::InvalidArg,
      format!("{role} stream step must be an object"),
    ));
  }
  let keys = own_string_keys(env, value)?;
  if !keys.contains("kind") {
    return Err(Error::new(
      Status::InvalidArg,
      format!("{role} stream step is missing its own kind key"),
    ));
  }
  let kind = string_value(env, named_property(env, value, "kind")?)?.ok_or_else(|| {
    Error::new(
      Status::InvalidArg,
      format!("{role} stream step kind must be a string"),
    )
  })?;
  let (expected_keys, payload_key, branch_matches) = match kind.as_str() {
    "item" => (
      ["kind", "value"].as_slice(),
      Some("value"),
      matches!(selector, SessionValuePathSegment::StreamItem),
    ),
    "done" => (["kind"].as_slice(), None, false),
    "error" => (
      ["kind", "error"].as_slice(),
      Some("error"),
      matches!(selector, SessionValuePathSegment::StreamError),
    ),
    _ => {
      return Err(Error::new(
        Status::InvalidArg,
        format!("{role} stream step has an unknown kind"),
      ))
    }
  };
  if keys.len() != expected_keys.len() || expected_keys.iter().any(|key| !keys.contains(*key)) {
    return Err(Error::new(
      Status::InvalidArg,
      format!("{role} stream step has an invalid own-key shape"),
    ));
  }
  if !branch_matches {
    return Ok(None);
  }
  Ok(
    payload_key
      .map(|key| named_property(env, value, key))
      .transpose()?,
  )
}

/// Return the complete own string-key set, including non-enumerable keys, so
/// stream carriers cannot smuggle a second payload through an inherited or
/// hidden property. Symbols are excluded because the canonical DTO contract
/// has only the string keys listed by `stream_step_payload`.
fn own_string_keys(env: sys::napi_env, object: sys::napi_value) -> Result<BTreeSet<String>> {
  let mut properties = ptr::null_mut();
  napi_ohos::check_status!(unsafe {
    sys::napi_get_all_property_names(
      env,
      object,
      sys::KeyCollectionMode::own_only,
      sys::KeyFilter::all_properties | sys::KeyFilter::skip_symbols,
      sys::KeyConversion::numbers_to_strings,
      &mut properties,
    )
  })?;
  let mut length = 0;
  napi_ohos::check_status!(unsafe { sys::napi_get_array_length(env, properties, &mut length) })?;
  let mut keys = BTreeSet::new();
  for index in 0..length {
    let mut key = ptr::null_mut();
    napi_ohos::check_status!(unsafe { sys::napi_get_element(env, properties, index, &mut key) })?;
    let Some(key) = string_value(env, key)? else {
      return Err(Error::new(
        Status::InvalidArg,
        "stream step own key must be a string",
      ));
    };
    keys.insert(key);
  }
  Ok(keys)
}

/// Return-path walker used after the lifecycle gate has detached.  It is
/// deliberately independent of SessionState: only a return root is accepted
/// and all selectors retain the same fan-out/optional semantics as the live
/// session walker.
fn detached_result_values(
  env: sys::napi_env,
  return_value: sys::napi_value,
  path: &[SessionValuePathSegment],
) -> Result<Vec<sys::napi_value>> {
  let Some((SessionValuePathSegment::Return, rest)) = path.split_first() else {
    return Err(Error::new(
      Status::InvalidArg,
      "late result resource path must start at return",
    ));
  };
  let mut values = vec![return_value];
  for segment in rest {
    let mut next = Vec::new();
    for value in values {
      match segment {
        SessionValuePathSegment::Field(name) => next.push(named_property(env, value, name)?),
        SessionValuePathSegment::StreamItem | SessionValuePathSegment::StreamError => {
          if let Some(value) = stream_step_payload(env, value, segment, "late result resource")? {
            next.push(value);
          }
        }
        SessionValuePathSegment::Variant(name) => {
          if let Some(value) = resolve_variant(env, value, name)? {
            next.push(value);
          }
        }
        SessionValuePathSegment::Optional => {
          if !is_null_value(env, value)? {
            next.push(value);
          }
        }
        SessionValuePathSegment::SequenceElement => {
          let mut is_array = false;
          napi_ohos::check_status!(unsafe { sys::napi_is_array(env, value, &mut is_array) })?;
          if !is_array {
            return Err(Error::new(
              Status::InvalidArg,
              "late result path expected an array sequence/set",
            ));
          }
          let mut length = 0;
          napi_ohos::check_status!(unsafe { sys::napi_get_array_length(env, value, &mut length) })?;
          for index in 0..length {
            let mut element = ptr::null_mut();
            napi_ohos::check_status!(unsafe {
              sys::napi_get_element(env, value, index, &mut element)
            })?;
            next.push(element);
          }
        }
        SessionValuePathSegment::SetElement => {
          next.extend(iterable_values(env, value, "values")?);
        }
        SessionValuePathSegment::MapKey => {
          next.extend(
            iterable_entries(env, value)?
              .into_iter()
              .map(|(key, _)| key),
          );
        }
        SessionValuePathSegment::MapValue => {
          next.extend(
            iterable_entries(env, value)?
              .into_iter()
              .map(|(_, value)| value),
          );
        }
        SessionValuePathSegment::Argument(_) | SessionValuePathSegment::Return => {
          return Err(Error::new(
            Status::InvalidArg,
            "late result resource path contains a nested root",
          ));
        }
      }
    }
    values = next;
  }
  Ok(values)
}

fn iterable_values(
  env: sys::napi_env,
  object: sys::napi_value,
  method: &str,
) -> Result<Vec<sys::napi_value>> {
  let iterator = call_named(env, object, method, &[])?;
  let mut values = Vec::new();
  loop {
    let next = call_named(env, iterator, "next", &[])?;
    let done = named_property(env, next, "done")?;
    let mut is_done = false;
    napi_ohos::check_status!(unsafe { sys::napi_get_value_bool(env, done, &mut is_done) })?;
    if is_done {
      break;
    }
    values.push(named_property(env, next, "value")?);
  }
  Ok(values)
}

fn iterable_entries(
  env: sys::napi_env,
  object: sys::napi_value,
) -> Result<Vec<(sys::napi_value, sys::napi_value)>> {
  let entries = iterable_values(env, object, "entries")?;
  let mut result = Vec::with_capacity(entries.len());
  for entry in entries {
    let mut is_array = false;
    napi_ohos::check_status!(unsafe { sys::napi_is_array(env, entry, &mut is_array) })?;
    if !is_array {
      return Err(Error::new(
        Status::InvalidArg,
        "Map.entries() yielded a non-entry value",
      ));
    }
    let mut key = ptr::null_mut();
    let mut value = ptr::null_mut();
    napi_ohos::check_status!(unsafe { sys::napi_get_element(env, entry, 0, &mut key) })?;
    napi_ohos::check_status!(unsafe { sys::napi_get_element(env, entry, 1, &mut value) })?;
    result.push((key, value));
  }
  Ok(result)
}

fn resolve_variant(
  env: sys::napi_env,
  value: sys::napi_value,
  variant: &str,
) -> Result<Option<sys::napi_value>> {
  // The facade's canonical ECMAScript representation is a `tag` string
  // discriminant plus the variant's flattened payload fields.  Raw N-API
  // addons may use a different wire key (`type`), but that conversion happens
  // before the backend session.  Do not accept wire aliases or a
  // variant-named property here. Returning the original object lets a
  // following Field segment read the flattened payload.
  let discriminant = named_property(env, value, "tag")?;
  let mut value_type = sys::ValueType::napi_undefined;
  napi_ohos::check_status!(unsafe { sys::napi_typeof(env, discriminant, &mut value_type) })?;
  if value_type != sys::ValueType::napi_string {
    return Err(Error::new(
      Status::InvalidArg,
      "enum value has a missing or non-string canonical tag",
    ));
  }
  let tag = string_value(env, discriminant)?.ok_or_else(|| {
    Error::new(
      Status::InvalidArg,
      "enum value has an invalid canonical tag",
    )
  })?;
  Ok((tag == variant).then_some(value))
}

fn js_u32(env: sys::napi_env, value: u32) -> Result<sys::napi_value> {
  let mut result = ptr::null_mut();
  napi_ohos::check_status!(unsafe { sys::napi_create_uint32(env, value, &mut result) })?;
  Ok(result)
}

fn js_array(env: sys::napi_env, values: &[sys::napi_value]) -> Result<sys::napi_value> {
  let mut result = ptr::null_mut();
  napi_ohos::check_status!(unsafe {
    sys::napi_create_array_with_length(env, values.len(), &mut result)
  })?;
  for (index, value) in values.iter().copied().enumerate() {
    napi_ohos::check_status!(unsafe { sys::napi_set_element(env, result, index as u32, value) })?;
  }
  Ok(result)
}

fn js_undefined(env: sys::napi_env) -> Result<sys::napi_value> {
  let mut value = ptr::null_mut();
  napi_ohos::check_status!(unsafe { sys::napi_get_undefined(env, &mut value) })?;
  Ok(value)
}

fn get_global(env: sys::napi_env) -> Result<sys::napi_value> {
  let mut global = ptr::null_mut();
  napi_ohos::check_status!(unsafe { sys::napi_get_global(env, &mut global) })?;
  Ok(global)
}

fn create_plain_object(env: sys::napi_env) -> Result<sys::napi_value> {
  let mut object = ptr::null_mut();
  napi_ohos::check_status!(unsafe { sys::napi_create_object(env, &mut object) })?;
  Ok(object)
}

/// Keep a primitive cleanup argument alive by boxing it in a referenced
/// object. OHOS N-API rejects `napi_create_reference` for primitive values,
/// while cleanup hooks still need the original numeric handle after detach.
fn retain_late_cleanup_value(
  env: sys::napi_env,
  value: sys::napi_value,
  role: &str,
) -> Result<sys::napi_ref> {
  let holder = create_plain_object(env)?;
  set_property(env, holder, "value", value)?;
  create_reference(env, holder, role)
}

fn resolved_promise(env: sys::napi_env) -> Result<sys::napi_value> {
  let mut deferred = ptr::null_mut();
  let mut promise = ptr::null_mut();
  napi_ohos::check_status!(unsafe { sys::napi_create_promise(env, &mut deferred, &mut promise) })?;
  napi_ohos::check_status!(unsafe {
    sys::napi_resolve_deferred(env, deferred, js_undefined(env)?)
  })?;
  Ok(promise)
}

struct AsyncResultContext {
  shared: Rc<SettlementShared>,
  session_reference: Arc<PendingSessionRef>,
  result_resources: Vec<SessionResultResourceUseSite>,
  callback_arguments: Vec<SessionCallbackArgument>,
  scoped_callbacks: Vec<CallbackRegistrationToken>,
}

struct InputSettlementContext {
  shared: Rc<SettlementShared>,
  session_reference: Arc<PendingSessionRef>,
  stream_id: u32,
  cancel: bool,
}

struct OutputCancelContext {
  shared: Rc<SettlementShared>,
  session_reference: Arc<PendingSessionRef>,
  resource_reference: sys::napi_ref,
}

/// Cleanup state for an output produced after the lifecycle gate detached.
/// The ordinary cancel settlement cannot register against a detached gate;
/// this small Promise reaction therefore owns independent N-API references
/// and invokes release only after the asynchronous cancel hook settles.
struct LateOutputRelease {
  resource: Arc<PendingSessionRef>,
  handle: Option<Arc<PendingSessionRef>>,
  release_callback: Arc<PendingSessionRef>,
  settled: AtomicBool,
}

struct LateOutputReleaseContext {
  cleanup: Arc<LateOutputRelease>,
}

/// Resource callbacks retained independently from SessionState.  A Promise
/// may settle after the lifecycle gate detaches and destroys the State/Host
/// references; this owner keeps only the native callback functions needed to
/// dispose values from that late result and never keeps a revoked Host.
struct LateResultCleanup {
  env: sys::napi_env,
  release_object: Option<Arc<PendingSessionRef>>,
  release_input_stream: Option<Arc<PendingSessionRef>>,
  cancel_output_stream: Option<Arc<PendingSessionRef>>,
  release_output_stream: Option<Arc<PendingSessionRef>>,
}

impl Drop for LateOutputRelease {
  fn drop(&mut self) {
    // If both Promise reactions are finalized without either callback
    // running, reclaim the independent refs instead of leaking them until
    // environment teardown.  `PendingSessionRef::delete` is idempotent, so
    // the normal settlement path and this fallback cannot double-delete.
    self.resource.delete();
    if let Some(handle) = self.handle.as_ref() {
      handle.delete();
    }
    self.release_callback.delete();
  }
}

impl Drop for LateResultCleanup {
  fn drop(&mut self) {
    if let Some(reference) = self.release_object.as_ref() {
      reference.delete();
    }
    if let Some(reference) = self.release_input_stream.as_ref() {
      reference.delete();
    }
    if let Some(reference) = self.cancel_output_stream.as_ref() {
      reference.delete();
    }
    if let Some(reference) = self.release_output_stream.as_ref() {
      reference.delete();
    }
  }
}

impl LateResultCleanup {
  fn clone_ref(
    env: sys::napi_env,
    source: sys::napi_ref,
    role: &str,
  ) -> Result<Option<Arc<PendingSessionRef>>> {
    if source.is_null() {
      return Ok(None);
    }
    let value = reference_value(env, source, role)?;
    Ok(Some(PendingSessionRef::new(
      env,
      create_reference(env, value, role)?,
    )))
  }

  fn new(env: sys::napi_env, callbacks: &ResourceCallbacks) -> Result<Arc<Self>> {
    let release_object = Self::clone_ref(
      env,
      callbacks.release_object.get(),
      "late object release callback",
    )?;
    let release_input_stream = match Self::clone_ref(
      env,
      callbacks.release_input_stream.get(),
      "late input stream release callback",
    ) {
      Ok(value) => value,
      Err(error) => {
        if let Some(reference) = release_object.as_ref() {
          reference.delete();
        }
        return Err(error);
      }
    };
    let cancel_output_stream = match Self::clone_ref(
      env,
      callbacks.cancel_output_stream.get(),
      "late output cancel callback",
    ) {
      Ok(value) => value,
      Err(error) => {
        if let Some(reference) = release_object.as_ref() {
          reference.delete();
        }
        if let Some(reference) = release_input_stream.as_ref() {
          reference.delete();
        }
        return Err(error);
      }
    };
    let release_output_stream = match Self::clone_ref(
      env,
      callbacks.release_output_stream.get(),
      "late output release callback",
    ) {
      Ok(value) => value,
      Err(error) => {
        if let Some(reference) = release_object.as_ref() {
          reference.delete();
        }
        if let Some(reference) = release_input_stream.as_ref() {
          reference.delete();
        }
        if let Some(reference) = cancel_output_stream.as_ref() {
          reference.delete();
        }
        return Err(error);
      }
    };
    Ok(Arc::new(Self {
      env,
      release_object,
      release_input_stream,
      cancel_output_stream,
      release_output_stream,
    }))
  }

  fn release_direct(&self, resource: sys::napi_value, callback: Option<&Arc<PendingSessionRef>>) {
    let Some(callback) = callback else {
      return;
    };
    let result = (|| {
      let callback = reference_value(
        self.env,
        callback.reference.load(Ordering::Acquire),
        "late resource callback",
      )?;
      let handle = named_property(self.env, resource, "handle")?;
      call_function(self.env, resource, callback, &[handle]).map(|_| ())
    })();
    let _ = result;
    clear_pending_exception(self.env);
  }

  fn release_input_stream(&self, resource: sys::napi_value) {
    let Some(callback_ref) = self.release_input_stream.as_ref() else {
      return;
    };
    let result = (|| {
      let id = stream_id(self.env, resource, "late input stream result ID")?;
      let id = js_u32(self.env, id)?;
      let callback = reference_value(
        self.env,
        callback_ref.reference.load(Ordering::Acquire),
        "late input stream release callback",
      )?;
      let receiver = js_undefined(self.env)?;
      call_function(self.env, receiver, callback, &[id]).map(|_| ())
    })();
    let _ = result;
    clear_pending_exception(self.env);
  }

  fn release_output_stream(&self, resource: sys::napi_value) {
    self.release_direct(resource, self.release_output_stream.as_ref());
  }

  fn cancel_output_stream(&self, resource: sys::napi_value) {
    let Some(cancel_callback) = self.cancel_output_stream.as_ref() else {
      self.release_output_stream(resource);
      return;
    };
    let cancel_result = (|| {
      let callback = reference_value(
        self.env,
        cancel_callback.reference.load(Ordering::Acquire),
        "late output cancel callback",
      )?;
      let handle = named_property(self.env, resource, "handle")?;
      call_function(self.env, resource, callback, &[handle])
    })();
    let Ok(cancel_result) = cancel_result else {
      clear_pending_exception(self.env);
      self.release_output_stream(resource);
      return;
    };
    clear_pending_exception(self.env);
    if !is_thenable(self.env, cancel_result).unwrap_or(false) {
      self.release_output_stream(resource);
      return;
    }
    let Some(release_callback) = self.release_output_stream.as_ref() else {
      return;
    };
    if schedule_late_output_release_with_callback(
      self.env,
      cancel_result,
      resource,
      release_callback.reference.load(Ordering::Acquire),
    )
    .is_err()
    {
      clear_pending_exception(self.env);
      self.release_output_stream(resource);
    }
  }

  fn dispose(&self, resource: sys::napi_value, kind: SessionResourceReceiver) {
    match kind {
      SessionResourceReceiver::Object => {
        self.release_direct(resource, self.release_object.as_ref())
      }
      SessionResourceReceiver::InputStream => self.release_input_stream(resource),
      SessionResourceReceiver::OutputStream => self.cancel_output_stream(resource),
    }
  }

  fn dispose_result(
    &self,
    result_value: sys::napi_value,
    result_resources: &[SessionResultResourceUseSite],
  ) {
    let mut released = Vec::new();
    for use_site in result_resources {
      let Ok(values) = detached_result_values(self.env, result_value, &use_site.path) else {
        clear_pending_exception(self.env);
        continue;
      };
      for value in values {
        if released.iter().any(|(kind, existing)| {
          *kind == use_site.kind && strict_equals(self.env, *existing, value)
        }) {
          continue;
        }
        released.push((use_site.kind, value));
        self.dispose(value, use_site.kind);
      }
    }
  }
}

struct SettlementShared {
  gate: Arc<LifecycleGate>,
  settled: Cell<bool>,
  snapshot: RefCell<Option<InvocationSnapshot>>,
  session_references: RefCell<Vec<Arc<PendingSessionRef>>>,
  late_result_cleanup: Arc<LateResultCleanup>,
}

struct DeadlineContext {
  gate: Arc<LifecycleGate>,
}

struct HostProxyGetContext {
  lease: Arc<InvocationHostLease>,
  target: Arc<PendingSessionRef>,
}

struct HostProxyMethodContext {
  lease: Arc<InvocationHostLease>,
  target: Arc<PendingSessionRef>,
  method: Arc<PendingSessionRef>,
}

unsafe extern "C" fn host_proxy_get(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  callback_result(env, || {
    let (args, data) = callback_args_with_data(env, info, 3)?;
    let context = unsafe { &*(data.cast::<HostProxyGetContext>()) };
    if !context.lease.is_active() {
      return js_undefined(env);
    }
    let target = reference_value(
      env,
      context.target.reference.load(Ordering::Acquire),
      "Host proxy target",
    )?;
    let value = get_property(env, target, args[1])?;
    let mut value_type = sys::ValueType::napi_undefined;
    napi_ohos::check_status!(unsafe { sys::napi_typeof(env, value, &mut value_type) })?;
    if value_type != sys::ValueType::napi_function {
      return Ok(value);
    }
    let method = PendingSessionRef::new(env, create_reference(env, value, "Host proxy method")?);
    // Each captured method gets its own target reference.  The getter and
    // every method wrapper may be finalized independently by V8; sharing the
    // getter's Arc would let one method finalizer delete the host reference
    // still needed by another wrapper (or by a later property lookup).
    let method_target = PendingSessionRef::new(
      env,
      match create_reference(env, target, "Host proxy method target") {
        Ok(reference) => reference,
        Err(error) => {
          method.delete();
          return Err(error);
        }
      },
    );
    if !context.lease.gate.register_settlement_ref(method.clone()) {
      method.delete();
      method_target.delete();
      return js_undefined(env);
    }
    if !context
      .lease
      .gate
      .register_settlement_ref(method_target.clone())
    {
      method.delete();
      method_target.delete();
      context.lease.gate.unregister_settlement_ref(&method);
      return js_undefined(env);
    }
    let method_context = Box::into_raw(Box::new(HostProxyMethodContext {
      lease: context.lease.clone(),
      target: method_target,
      method,
    }));
    let method_result = create_callback_function(
      env,
      "uniffi_host_proxy_method",
      host_proxy_method,
      method_context.cast(),
      Some(finalize_host_proxy_method),
    );
    match method_result {
      Ok(value) => Ok(value),
      Err(error) => {
        // Callback creation can fail after both refs have been registered;
        // reclaim the context and unregister each independent reference on
        // this error path just as their finalizer would.
        let context = unsafe { Box::from_raw(method_context.cast::<HostProxyMethodContext>()) };
        context.target.delete();
        context.method.delete();
        context
          .lease
          .gate
          .unregister_settlement_ref(&context.target);
        context
          .lease
          .gate
          .unregister_settlement_ref(&context.method);
        Err(error)
      }
    }
  })
}

unsafe extern "C" fn host_proxy_method(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  callback_result(env, || {
    let (args, data) = callback_args_dynamic(env, info)?;
    let context = unsafe { &*(data.cast::<HostProxyMethodContext>()) };
    if !context.lease.is_active() {
      return js_undefined(env);
    }
    let target = reference_value(
      env,
      context.target.reference.load(Ordering::Acquire),
      "Host proxy target",
    )?;
    let method = reference_value(
      env,
      context.method.reference.load(Ordering::Acquire),
      "Host proxy method",
    )?;
    call_function(env, target, method, &args)
  })
}

unsafe extern "C" fn finalize_host_proxy_get(
  _env: sys::napi_env,
  data: *mut c_void,
  _hint: *mut c_void,
) {
  if !data.is_null() {
    let context = unsafe { Box::from_raw(data.cast::<HostProxyGetContext>()) };
    context.target.delete();
    context
      .lease
      .gate
      .unregister_settlement_ref(&context.target);
  }
}

unsafe extern "C" fn finalize_host_proxy_method(
  _env: sys::napi_env,
  data: *mut c_void,
  _hint: *mut c_void,
) {
  if !data.is_null() {
    let context = unsafe { Box::from_raw(data.cast::<HostProxyMethodContext>()) };
    context.target.delete();
    context.method.delete();
    context
      .lease
      .gate
      .unregister_settlement_ref(&context.target);
    context
      .lease
      .gate
      .unregister_settlement_ref(&context.method);
  }
}

fn create_invocation_host_proxy(
  env: sys::napi_env,
  host: sys::napi_value,
  lease: Arc<InvocationHostLease>,
) -> Result<sys::napi_value> {
  let target = PendingSessionRef::new(env, create_reference(env, host, "Host proxy target")?);
  if !lease.gate.register_settlement_ref(target.clone()) {
    target.delete();
    return Err(Error::new(Status::GenericFailure, "Host proxy detached"));
  }
  let get_context = Box::into_raw(Box::new(HostProxyGetContext {
    lease: lease.clone(),
    target: target.clone(),
  }));
  let get = match create_callback_function(
    env,
    "uniffi_host_proxy_get",
    host_proxy_get,
    get_context.cast(),
    Some(finalize_host_proxy_get),
  ) {
    Ok(value) => value,
    Err(error) => {
      target.delete();
      lease.gate.unregister_settlement_ref(&target);
      unsafe {
        drop(Box::from_raw(get_context));
      }
      return Err(error);
    }
  };
  let handler = create_plain_object(env)?;
  set_property(env, handler, "get", get)?;
  let global = get_global(env)?;
  let proxy_constructor = named_property(env, global, "Proxy")?;
  let mut argv = [host, handler];
  let mut proxy = ptr::null_mut();
  napi_ohos::check_status!(unsafe {
    sys::napi_new_instance(
      env,
      proxy_constructor,
      argv.len(),
      argv.as_mut_ptr(),
      &mut proxy,
    )
  })?;
  Ok(proxy)
}

unsafe extern "C" fn deadline_timer_callback(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  let (_, data) = match callback_args_with_data(env, info, 0) {
    Ok(value) => value,
    Err(_) => return ptr::null_mut(),
  };
  let context = unsafe { &*(data.cast::<DeadlineContext>()) };
  if let Some(state) = context.gate.state() {
    state.deadline_detach();
  } else {
    context.gate.detach_settlement_refs();
    context.gate.resolve_close();
  }
  js_undefined(env).unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn finalize_deadline_context(
  _env: sys::napi_env,
  data: *mut c_void,
  _hint: *mut c_void,
) {
  if !data.is_null() {
    drop(unsafe { Box::from_raw(data.cast::<DeadlineContext>()) });
  }
}

impl SettlementShared {
  fn state(&self) -> Option<&'static SessionState> {
    self.gate.state()
  }

  fn release_session_references(&self) {
    let references = std::mem::take(&mut *self.session_references.borrow_mut());
    for reference in references {
      reference.delete();
      self.gate.unregister_settlement_ref(&reference);
    }
  }

  fn finish(&self) {
    self.release_session_references();
    if let Some(state) = self.state() {
      state.finish_pending_work();
    } else {
      self.gate.finish_settlement();
    }
  }
}

unsafe extern "C" fn async_result_fulfilled(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  callback_result(env, || {
    let (args, data) = callback_args_with_data(env, info, 1)?;
    let context = unsafe { &*(data.cast::<AsyncResultContext>()) };
    if context.shared.settled.replace(true) {
      return js_undefined(env);
    }
    let state = context.shared.state();
    let outcome = if let Some(state) = state {
      if state.closing.get() || state.closed.get() {
        // close() has already drained every public lease and shut down the
        // callback release TSFN.  Do not run callback/input tracking for this
        // late result.  Native object/output values still own a native handle,
        // so release that handle directly without registering a new lease.
        context.shared.snapshot.borrow_mut().take();
        let session_value = reference_value(
          env,
          context.session_reference.reference.load(Ordering::Acquire),
          "pending session",
        )
        .unwrap_or_else(|_| js_undefined(env).unwrap_or(ptr::null_mut()));
        state.release_late_result(args[0], &context.result_resources, session_value);
        clear_pending_exception(env);
        Ok(())
      } else {
        let result_is_error = call_result_is_error(env, args[0]);
        match result_is_error {
          Ok(true) => {
            if let Some(snapshot) = context.shared.snapshot.borrow_mut().take() {
              state.rollback(snapshot);
            }
            Ok(())
          }
          Ok(false) => {
            match state.track_result_value(
              args[0],
              &context.result_resources,
              &context.callback_arguments,
            ) {
              Ok(()) => Ok(()),
              Err(error) => {
                if let Some(snapshot) = context.shared.snapshot.borrow_mut().take() {
                  state.rollback(snapshot);
                }
                Err(error)
              }
            }
          }
          Err(error) => {
            if let Some(snapshot) = context.shared.snapshot.borrow_mut().take() {
              state.rollback(snapshot);
            }
            Err(error)
          }
        }
      }
    } else {
      // The gate may have detached before the Promise settled.  The Host and
      // SessionState are intentionally gone in that case, but every resource
      // declared by the return-path DTO still owns native cleanup.  Dispose
      // only a successful value envelope through the independent callback
      // owner; rejected results carry no resource payload.
      if !call_result_is_error(env, args[0])? {
        if let Ok(value) = named_property(env, args[0], "value") {
          context
            .shared
            .late_result_cleanup
            .dispose_result(value, &context.result_resources);
        }
        clear_pending_exception(env);
      }
      Ok(())
    };
    if let Some(state) = context.shared.state() {
      state.release_scoped_callback_tokens(&context.scoped_callbacks);
    }
    context.shared.finish();
    outcome?;
    js_undefined(env)
  })
}

unsafe extern "C" fn async_result_rejected(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  let (_, data) = match callback_args_with_data(env, info, 1) {
    Ok(value) => value,
    Err(_) => return ptr::null_mut(),
  };
  let context = unsafe { &*(data.cast::<AsyncResultContext>()) };
  if context.shared.settled.replace(true) {
    return js_undefined(env).unwrap_or(ptr::null_mut());
  }
  if let Some(state) = context.shared.state() {
    let snapshot = context.shared.snapshot.borrow_mut().take();
    if !state.closing.get() && !state.closed.get() {
      if let Some(snapshot) = snapshot {
        state.rollback(snapshot);
      }
    }
    state.release_scoped_callback_tokens(&context.scoped_callbacks);
  }
  context.shared.finish();
  js_undefined(env).unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn input_settlement_fulfilled(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  callback_result(env, || {
    let (args, data) = callback_args_with_data(env, info, 1)?;
    let context = unsafe { &*(data.cast::<InputSettlementContext>()) };
    if context.shared.settled.replace(true) {
      return js_undefined(env);
    }
    let result = if let Some(state) = context.shared.state() {
      let terminal = if state.closing.get() || state.closed.get() || context.cancel {
        Ok(true)
      } else {
        input_step_is_terminal(env, args[0])
      };
      match terminal {
        Ok(true) if !state.closing.get() && !state.closed.get() => {
          state.release_input_stream(context.stream_id).map(|_| ())
        }
        Ok(_) => Ok(()),
        Err(error) => {
          clear_pending_exception(env);
          let _ = state.release_input_stream(context.stream_id);
          Err(error)
        }
      }
    } else {
      Ok(())
    };
    context.shared.finish();
    result?;
    js_undefined(env)
  })
}

unsafe extern "C" fn input_settlement_rejected(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  let (_, data) = match callback_args_with_data(env, info, 1) {
    Ok(value) => value,
    Err(_) => return ptr::null_mut(),
  };
  let context = unsafe { &*(data.cast::<InputSettlementContext>()) };
  if context.shared.settled.replace(true) {
    return js_undefined(env).unwrap_or(ptr::null_mut());
  }
  if let Some(state) = context.shared.state() {
    if !state.closing.get() && !state.closed.get() {
      let _ = state.release_input_stream(context.stream_id);
    }
  }
  context.shared.finish();
  js_undefined(env).unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn output_cancel_fulfilled(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  callback_result(env, || {
    let (_, data) = callback_args_with_data(env, info, 1)?;
    let context = unsafe { &*(data.cast::<OutputCancelContext>()) };
    if context.shared.settled.replace(true) {
      return js_undefined(env);
    }
    if let Some(state) = context.shared.state() {
      state.complete_output_cancel(context.resource_reference)?;
    }
    context.shared.finish();
    js_undefined(env)
  })
}

unsafe extern "C" fn output_cancel_rejected(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  let (_, data) = match callback_args_with_data(env, info, 1) {
    Ok(value) => value,
    Err(_) => return ptr::null_mut(),
  };
  let context = unsafe { &*(data.cast::<OutputCancelContext>()) };
  if context.shared.settled.replace(true) {
    return js_undefined(env).unwrap_or(ptr::null_mut());
  }
  if let Some(state) = context.shared.state() {
    let _ = state.complete_output_cancel(context.resource_reference);
  }
  context.shared.finish();
  js_undefined(env).unwrap_or(ptr::null_mut())
}

impl LateOutputRelease {
  fn release(&self, env: sys::napi_env) -> Result<()> {
    if self.settled.swap(true, Ordering::AcqRel) {
      return Ok(());
    }
    let result = (|| {
      let resource = reference_value(
        env,
        self.resource.reference.load(Ordering::Acquire),
        "late output cleanup resource",
      )?;
      let callback = reference_value(
        env,
        self.release_callback.reference.load(Ordering::Acquire),
        "late output cleanup callback",
      )?;
      let handle = match self.handle.as_ref() {
        Some(handle) => {
          let holder = reference_value(
            env,
            handle.reference.load(Ordering::Acquire),
            "late output cleanup handle",
          )?;
          named_property(env, holder, "value")?
        }
        None => named_property(env, resource, "handle")?,
      };
      match call_function(env, resource, callback, &[handle]) {
        Ok(_) => Ok(()),
        Err(_) => {
          let _ = take_pending_exception_message(env);
          clear_pending_exception(env);
          let receiver = js_undefined(env)?;
          call_function(env, receiver, callback, &[handle]).map(|_| ())
        }
      }
    })();
    clear_pending_exception(env);
    self.resource.delete();
    if let Some(handle) = self.handle.as_ref() {
      handle.delete();
    }
    self.release_callback.delete();
    result
  }
}

/// Schedule release of an output resource after an independent late cancel
/// Promise.  This path owns fresh references and never consults SessionState,
/// so it remains valid after the lifecycle gate has detached.
fn schedule_late_output_release_with_callback(
  env: sys::napi_env,
  cancel_promise: sys::napi_value,
  resource: sys::napi_value,
  release_callback: sys::napi_ref,
) -> Result<()> {
  if release_callback.is_null() {
    return Ok(());
  }
  let resource_ref = PendingSessionRef::new(
    env,
    create_reference(env, resource, "late output cleanup resource")?,
  );
  let handle_ref = match named_property(env, resource, "handle")
    .and_then(|handle| retain_late_cleanup_value(env, handle, "late output cleanup handle"))
  {
    Ok(reference) => Some(PendingSessionRef::new(env, reference)),
    Err(_) => {
      clear_pending_exception(env);
      None
    }
  };
  let release_callback_value =
    match reference_value(env, release_callback, "output release callback") {
      Ok(value) => value,
      Err(error) => {
        resource_ref.delete();
        if let Some(handle_ref) = handle_ref.as_ref() {
          handle_ref.delete();
        }
        return Err(error);
      }
    };
  let release_callback_ref =
    match create_reference(env, release_callback_value, "late output cleanup callback") {
      Ok(reference) => PendingSessionRef::new(env, reference),
      Err(error) => {
        resource_ref.delete();
        if let Some(handle_ref) = handle_ref.as_ref() {
          handle_ref.delete();
        }
        return Err(error);
      }
    };
  let cleanup = Arc::new(LateOutputRelease {
    resource: resource_ref,
    handle: handle_ref,
    release_callback: release_callback_ref,
    settled: AtomicBool::new(false),
  });
  let fulfilled_context = Box::into_raw(Box::new(LateOutputReleaseContext {
    cleanup: cleanup.clone(),
  }));
  let rejected_context = Box::into_raw(Box::new(LateOutputReleaseContext {
    cleanup: cleanup.clone(),
  }));
  let fulfilled = match create_callback_function(
    env,
    "uniffi_late_output_cancel_fulfilled",
    late_output_release_fulfilled,
    fulfilled_context.cast(),
    Some(finalize_late_output_release_context),
  ) {
    Ok(value) => value,
    Err(error) => {
      unsafe {
        drop(Box::from_raw(fulfilled_context));
        drop(Box::from_raw(rejected_context));
      }
      return Err(error);
    }
  };
  let rejected = match create_callback_function(
    env,
    "uniffi_late_output_cancel_rejected",
    late_output_release_rejected,
    rejected_context.cast(),
    Some(finalize_late_output_release_context),
  ) {
    Ok(value) => value,
    Err(error) => {
      unsafe {
        drop(Box::from_raw(rejected_context));
      }
      return Err(error);
    }
  };
  let then = named_property(env, cancel_promise, "then")?;
  call_function(env, cancel_promise, then, &[fulfilled, rejected])?;
  Ok(())
}

unsafe extern "C" fn late_output_release_fulfilled(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  let (_, data) = match callback_args_with_data(env, info, 1) {
    Ok(value) => value,
    Err(_) => return js_undefined(env).unwrap_or(ptr::null_mut()),
  };
  let context = unsafe { &*(data.cast::<LateOutputReleaseContext>()) };
  let _ = context.cleanup.release(env);
  clear_pending_exception(env);
  js_undefined(env).unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn late_output_release_rejected(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> sys::napi_value {
  let (_, data) = match callback_args_with_data(env, info, 1) {
    Ok(value) => value,
    Err(_) => return js_undefined(env).unwrap_or(ptr::null_mut()),
  };
  let context = unsafe { &*(data.cast::<LateOutputReleaseContext>()) };
  let _ = context.cleanup.release(env);
  js_undefined(env).unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn finalize_late_output_release_context(
  _env: sys::napi_env,
  data: *mut c_void,
  _hint: *mut c_void,
) {
  if !data.is_null() {
    drop(unsafe { Box::from_raw(data.cast::<LateOutputReleaseContext>()) });
  }
}

unsafe extern "C" fn finalize_async_result_context(
  _env: sys::napi_env,
  data: *mut c_void,
  _hint: *mut c_void,
) {
  if !data.is_null() {
    let context = unsafe { Box::from_raw(data.cast::<AsyncResultContext>()) };
    if !context.shared.settled.replace(true) {
      if let Some(state) = context.shared.state() {
        if let Some(snapshot) = context.shared.snapshot.borrow_mut().take() {
          if !state.closing.get() && !state.closed.get() {
            state.rollback(snapshot);
          }
        }
        state.release_scoped_callback_tokens(&context.scoped_callbacks);
      }
      context.shared.finish();
    }
    context.session_reference.delete();
  }
}

unsafe extern "C" fn finalize_input_settlement_context(
  _env: sys::napi_env,
  data: *mut c_void,
  _hint: *mut c_void,
) {
  if !data.is_null() {
    let context = unsafe { Box::from_raw(data.cast::<InputSettlementContext>()) };
    if !context.shared.settled.replace(true) {
      if let Some(state) = context.shared.state() {
        if !state.closed.get() {
          let _ = state.release_input_stream(context.stream_id);
        }
      }
      context.shared.finish();
    }
    context.session_reference.delete();
  }
}

unsafe extern "C" fn finalize_output_cancel_context(
  _env: sys::napi_env,
  data: *mut c_void,
  _hint: *mut c_void,
) {
  if !data.is_null() {
    let context = unsafe { Box::from_raw(data.cast::<OutputCancelContext>()) };
    if !context.shared.settled.replace(true) {
      if let Some(state) = context.shared.state() {
        let _ = state.complete_output_cancel(context.resource_reference);
      }
      context.shared.finish();
    }
    context.session_reference.delete();
  }
}

fn create_callback_function(
  env: sys::napi_env,
  name: &str,
  callback: unsafe extern "C" fn(sys::napi_env, sys::napi_callback_info) -> sys::napi_value,
  data: *mut c_void,
  finalizer: Option<unsafe extern "C" fn(sys::napi_env, *mut c_void, *mut c_void)>,
) -> Result<sys::napi_value> {
  let name = CString::new(name)?;
  let mut function = ptr::null_mut();
  napi_ohos::check_status!(unsafe {
    sys::napi_create_function(
      env,
      name.as_ptr(),
      name.as_bytes().len() as isize,
      Some(callback),
      data,
      &mut function,
    )
  })?;
  if let Some(finalizer) = finalizer {
    napi_ohos::check_status!(unsafe {
      sys::napi_add_finalizer(
        env,
        function,
        data,
        Some(finalizer),
        ptr::null_mut(),
        ptr::null_mut(),
      )
    })?;
  }
  Ok(function)
}

fn callback_args_with_data(
  env: sys::napi_env,
  info: sys::napi_callback_info,
  expected: usize,
) -> Result<(Vec<sys::napi_value>, *mut c_void)> {
  let mut argc = expected;
  let mut args = vec![ptr::null_mut(); expected];
  let mut this = ptr::null_mut();
  let mut data = ptr::null_mut();
  napi_ohos::check_status!(unsafe {
    sys::napi_get_cb_info(
      env,
      info,
      &mut argc,
      args.as_mut_ptr(),
      &mut this,
      &mut data,
    )
  })?;
  if argc != expected {
    return Err(Error::new(
      Status::InvalidArg,
      format!("expected {expected} arguments, received {argc}"),
    ));
  }
  if data.is_null() {
    return Err(Error::new(
      Status::GenericFailure,
      "missing settlement context",
    ));
  }
  Ok((args, data))
}

fn callback_args_dynamic(
  env: sys::napi_env,
  info: sys::napi_callback_info,
) -> Result<(Vec<sys::napi_value>, *mut c_void)> {
  let mut argc = 0;
  let mut this = ptr::null_mut();
  let mut data = ptr::null_mut();
  napi_ohos::check_status!(unsafe {
    sys::napi_get_cb_info(env, info, &mut argc, ptr::null_mut(), &mut this, &mut data)
  })?;
  let mut args = vec![ptr::null_mut(); argc];
  napi_ohos::check_status!(unsafe {
    sys::napi_get_cb_info(
      env,
      info,
      &mut argc,
      args.as_mut_ptr(),
      &mut this,
      &mut data,
    )
  })?;
  if data.is_null() {
    return Err(Error::new(
      Status::GenericFailure,
      "missing Host proxy context",
    ));
  }
  Ok((args, data))
}

fn is_thenable(env: sys::napi_env, value: sys::napi_value) -> Result<bool> {
  let mut value_type = sys::ValueType::napi_undefined;
  napi_ohos::check_status!(unsafe { sys::napi_typeof(env, value, &mut value_type) })?;
  if value_type != sys::ValueType::napi_object {
    return Ok(false);
  }
  let then = named_property(env, value, "then")?;
  let mut then_type = sys::ValueType::napi_undefined;
  napi_ohos::check_status!(unsafe { sys::napi_typeof(env, then, &mut then_type) })?;
  Ok(then_type == sys::ValueType::napi_function)
}

fn string_value(env: sys::napi_env, value: sys::napi_value) -> Result<Option<String>> {
  let mut length = 0;
  let status =
    unsafe { sys::napi_get_value_string_utf8(env, value, ptr::null_mut(), 0, &mut length) };
  if status != sys::Status::napi_ok {
    return Ok(None);
  }
  let mut bytes = vec![0_u8; length as usize + 1];
  let mut written = 0;
  napi_ohos::check_status!(unsafe {
    sys::napi_get_value_string_utf8(
      env,
      value,
      bytes.as_mut_ptr().cast(),
      bytes.len(),
      &mut written,
    )
  })?;
  Ok(Some(
    String::from_utf8_lossy(&bytes[..written]).into_owned(),
  ))
}

fn input_step_is_terminal(env: sys::napi_env, value: sys::napi_value) -> Result<bool> {
  let kind = named_property(env, value, "kind")?;
  Ok(matches!(
    string_value(env, kind)?.as_deref(),
    Some("done") | Some("error")
  ))
}

fn call_result_is_error(env: sys::napi_env, value: sys::napi_value) -> Result<bool> {
  let mut value_type = sys::ValueType::napi_undefined;
  napi_ohos::check_status!(unsafe { sys::napi_typeof(env, value, &mut value_type) })?;
  if value_type != sys::ValueType::napi_object {
    return Ok(false);
  }
  let kind = match named_property(env, value, "kind") {
    Ok(kind) => kind,
    Err(_) => return Ok(false),
  };
  Ok(string_value(env, kind)?.as_deref() == Some("error"))
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

/// Drop a pending JavaScript exception before best-effort cleanup invokes
/// another host hook.  N-API refuses to enter most APIs while an exception is
/// pending, which would otherwise turn one failed release hook into skipped
/// releases for every subsequent callback.
fn clear_pending_exception(env: sys::napi_env) {
  let mut exception = ptr::null_mut();
  let _ = unsafe { sys::napi_get_and_clear_last_exception(env, &mut exception) };
}

fn take_pending_exception_message(env: sys::napi_env) -> Option<String> {
  let mut exception = ptr::null_mut();
  let _status = unsafe { sys::napi_get_and_clear_last_exception(env, &mut exception) };
  if exception.is_null() {
    return None;
  }
  named_property(env, exception, "message")
    .ok()
    .and_then(|message| string_value(env, message).ok().flatten())
}

#[cfg(test)]
mod callback_invoker_tests {
  use super::*;

  #[test]
  fn clones_share_allocator_and_close_revokes_new_calls() {
    let gate = LifecycleGate::new(ptr::null_mut());
    let first = SessionCallbackInvoker::new(gate.clone());
    let second = first.clone();
    assert_eq!(first.next_invocation_id().unwrap(), 0);
    assert_eq!(second.next_invocation_id().unwrap(), 1);
    gate.invalidate_invocations();
    assert!(first.next_invocation_id().is_err());
    assert!(second.next_invocation_id().is_err());
  }

  #[test]
  fn check_open_does_not_consume_invocation_id_and_rejects_off_thread() {
    let gate = LifecycleGate::new(ptr::null_mut());
    let invoker = SessionCallbackInvoker::new(gate.clone());
    assert!(invoker.is_owner_thread());
    invoker.check_open().unwrap();
    assert_eq!(invoker.next_invocation_id().unwrap(), 0);

    let off_thread_invoker = invoker.clone();
    let off_thread = std::thread::spawn(move || {
      (!off_thread_invoker.is_owner_thread()) && off_thread_invoker.check_open().is_err()
    });
    assert!(off_thread.join().unwrap());

    gate.invalidate_invocations();
    assert!(invoker.check_open().is_err());
  }

  #[test]
  fn callback_lease_drop_claims_one_logical_release() {
    let queue = Arc::new(CallbackReleaseQueue {
      state: Mutex::new(CallbackReleaseState {
        next_token: 0,
        active: BTreeMap::new(),
        pending: Vec::new(),
      }),
      tsfn: AtomicPtr::new(ptr::null_mut()),
      tsfn_guard: Mutex::new(()),
      closed: AtomicBool::new(true),
    });
    let token = queue
      .register(CallbackKey {
        callback_type_id: 7,
        callback_id: 11,
      })
      .unwrap();
    let first = SessionCallbackLease {
      inner: Arc::new(CallbackLeaseInner {
        token,
        queue: queue.clone(),
      }),
    };
    let second = first.clone();
    drop(first);
    drop(second);
    assert_eq!(queue.take_pending().len(), 1);
    assert!(queue.take_pending().is_empty());
  }
}
