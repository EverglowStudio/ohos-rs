use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[test]
fn generated_factory_executes_complete_host_session_contract() {
  let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/generated_fixture");
  let manifest = fixture.join("Cargo.toml");
  let build = Command::new(env!("CARGO"))
    .arg("build")
    .arg("--quiet")
    .arg("--manifest-path")
    .arg(&manifest)
    .output()
    .expect("run Cargo for the generated OHOS fixture");
  assert!(
    build.status.success(),
    "generated OHOS addon failed to build:\n{}\n{}",
    String::from_utf8_lossy(&build.stdout),
    String::from_utf8_lossy(&build.stderr),
  );

  let target = fixture.join("target/debug");
  let library = target.join(format!(
    "{}napi_ohos_uniffi_generated_fixture{}",
    std::env::consts::DLL_PREFIX,
    std::env::consts::DLL_SUFFIX,
  ));
  let addon = target.join("fixture.node");
  fs::copy(&library, &addon).expect("copy generated cdylib to Node addon suffix");
  if cfg!(target_os = "macos") {
    let sign = Command::new("codesign")
      .args(["--force", "--sign", "-"])
      .arg(&addon)
      .output()
      .expect("ad-hoc sign copied generated OHOS addon");
    assert!(
      sign.status.success(),
      "failed to ad-hoc sign copied generated OHOS addon (status={:?}):\n{}\n{}",
      sign.status,
      String::from_utf8_lossy(&sign.stdout),
      String::from_utf8_lossy(&sign.stderr),
    );
  }

  let script = r#"
const assert = require('node:assert/strict');
const path = require('node:path');
const originalSetTimeout = global.setTimeout;
const originalClearTimeout = global.clearTimeout;
const teardownTimerHandles = new Set();
let teardownTimersCreated = 0;
let teardownTimersCleared = 0;
global.setTimeout = function(callback, delay, ...args) {
  const timer = originalSetTimeout.call(this, callback, delay, ...args);
  if (delay === 40) {
    teardownTimersCreated += 1;
    teardownTimerHandles.add(timer);
  }
  return timer;
};
global.clearTimeout = function(timer) {
  if (teardownTimerHandles.delete(timer)) teardownTimersCleared += 1;
  return originalClearTimeout.call(this, timer);
};
const timerSnapshot = () => ({ created: teardownTimersCreated, cleared: teardownTimersCleared });
const assertOneTeardownTimer = (before, label) => {
  const after = timerSnapshot();
  assert.equal(after.created - before.created, 1, `${label}: exactly one 40ms timer created`);
  assert.equal(after.cleared - before.cleared, 1, `${label}: exactly one 40ms timer cleared`);
};
const addon = require(path.resolve(process.argv[1]));
assert.deepEqual(Object.keys(addon), ['__uniffi_backend_factory']);

let session;
const callbackCalls = [];
const lifecycle = [];
const streams = [];
let deferredStarted = false;
let resolveDeferred;
let resolveLateCallback;
let rejectLateCallback;
let resolvePendingPull;
let rejectPendingPull;
let resolvePendingCancel;
let rejectPendingCancel;
let savedFinally;
const host = {
  handle: 88,
  invokeCallbackSync(typeId, callbackId, methodId, args) {
    callbackCalls.push(['sync', typeId, callbackId, methodId, args]);
    if (methodId === 3 && args[0] === 88) {
      throw new Error('fixture infallible sync callback failed');
    }
    if (args[0] === 99) {
      assert.throws(
        () => session.invokeSync(9, [callbackId, 99]),
        /reentrancy/,
      );
    }
    return args[0] + 2;
  },
  invokeCallbackAsync(typeId, callbackId, methodId, invocationId, args) {
    callbackCalls.push([
      'async',
      typeId,
      callbackId,
      methodId,
      invocationId,
      args,
    ]);
    if (args[0] === 42) {
      return Promise.reject(new Error('fixture async callback rejected'));
    }
    if (args[0] === 55) {
      deferredStarted = true;
      return new Promise((resolve) => {
        resolveDeferred = resolve;
      });
    }
    if (args[0] === 56) {
      return new Promise((resolve) => {
        resolveLateCallback = resolve;
      });
    }
    if (args[0] === 57) {
      return new Promise((_resolve, reject) => {
        rejectLateCallback = reject;
      });
    }
    if (args[0] === 77) {
      const resolved = Promise.resolve(args[0] + 10);
      resolved.finally = (callback) => {
        savedFinally = callback;
        throw new Error('fixture finally trap');
      };
      return resolved;
    }
    return Promise.resolve(args[0] + 10);
  },
  retainCallback(typeId, callbackId) {
    lifecycle.push(['retain', typeId, callbackId]);
  },
  releaseCallback(typeId, callbackId) {
    lifecycle.push(['release', typeId, callbackId]);
  },
  pullInputStream(streamId) {
    streams.push(['pull', streamId]);
    if (streamId === 444) {
      return new Promise((resolve, reject) => {
        resolvePendingPull = resolve;
        rejectPendingPull = reject;
      });
    }
    return Promise.resolve({ kind: 'item', value: streamId + 1 });
  },
  cancelInputStream(streamId) {
    streams.push(['cancel', streamId]);
    if (streamId === 445) {
      return new Promise((resolve, reject) => {
        resolvePendingCancel = resolve;
        rejectPendingCancel = reject;
      });
    }
    return Promise.resolve();
  },
  releaseInputStream(streamId) {
    streams.push(['release', streamId]);
  },
};

session = addon.__uniffi_backend_factory(host);
assert.deepEqual(session.invokeSync(0, [7]), { kind: 'value', value: 7 });
assert.equal(
  session.invokeSync(2, [-9223372036854775808n]).value,
  -9223372036854775808n,
);
assert.equal(
  session.invokeSync(3, [18446744073709551615n]).value,
  18446744073709551615n,
);
assert.equal(session.invokeSync(2, [9223372036854775808n]).error.domain, 'validation');
assert.equal(session.invokeSync(3, [-1n]).error.domain, 'validation');
assert.equal(session.invokeSync(3, [36893488147419103232n]).error.domain, 'validation');
assert.equal(session.invokeSync(4, [9]).error.domain, 'declared');

const object = { handle: 77 };
  assert.equal(session.invokeSync(5, [object]).value, object);
  session.releaseObject(object);
  assert.equal(session.invokeSync(6, [10]).value, 15);
  const valueReleaseBefore = session.invokeSync(18, []).value;
  const record = { handle: 9999 };
  const valueEnum = { tag: 'ready', handle: 9999 };
  assert.equal(session.invokeSync(21, [record]).value, 9999);
  assert.equal(session.invokeSync(23, [valueEnum]).value, 9999);

(async () => {
  assert.equal((await session.invokeAsync(1, [6])).value, 7);
  assert.equal((await session.invokeAsync(22, [record])).value, 10000);
  assert.equal((await session.invokeAsync(24, [valueEnum])).value, 10000);
  assert.equal(session.invokeSync(18, []).value, valueReleaseBefore);
  // The native async proxy is backed by a real TSFN.  Its call is queued from
  // the Tokio worker and reaches Host.invokeCallbackAsync on the JS thread.
  assert.equal((await session.invokeAsync(7, [20])).value, 31);
  assert.equal(await session.invokeAsync(8, [20, 5]), 15);
  assert.equal(await session.invokeAsync(8, [20, 6]), 16);
  await assert.rejects(
    session.invokeAsync(8, [20, 42]),
    /fixture async callback rejected/,
  );
  assert.throws(
    () => session.invokeAsync(8, [20, 77]),
    /fixture finally trap/,
  );
  const callbackCountAfterTrap = callbackCalls.length;
  assert.equal(typeof savedFinally, 'function');
  // The failed finally registration leaves the forbidden callback guard live;
  // the next invocation is rejected before Host.invokeCallbackAsync runs.
  assert.throws(
    () => session.invokeAsync(8, [20, 78]),
    /reentrancy/,
  );
  assert.equal(callbackCalls.length, callbackCountAfterTrap);
  await savedFinally();
  assert.equal(await session.invokeAsync(8, [20, 78]), 88);
  assert.equal(await session.invokeAsync(10, [20, 6]), 16);
  await assert.rejects(
    session.invokeAsync(10, [20, 42]),
    /fixture async callback rejected/,
  );
  assert.equal(session.invokeSync(11, [10, 99]), 101);
  assert.throws(
    () => session.invokeSync(11, [10, 88]),
    /fixture infallible sync callback failed/,
  );

  assert.equal((await session.invokeAsync(15, [9])).value, 10);
  assert.deepEqual(await session.invokeAsync(16, [9]), {
    kind: 'item',
    value: 10,
  });
  await session.invokeAsync(17, [9]);

  const output = session.invokeSync(12, [11]).value;
  // HostAndArguments operations receive a revocable per-invocation proxy;
  // resource values preserve the public handle without aliasing the session
  // Host object itself.
  assert.notEqual(output, host);
  assert.equal(output.handle, host.handle);
  assert.equal((await session.invokeAsync(13, [output])).value, 89);
  // Calling cancel twice before the native Promise settles returns the same
  // in-flight settlement and invokes native cleanup only once.
  let cancel;
  try {
    cancel = session.cancelOutputStream(output);
  } catch (error) {
    throw new Error(`first output cancel failed: ${error}`);
  }
  let cancelAgain;
  try {
    cancelAgain = session.cancelOutputStream(output);
  } catch (error) {
    throw new Error(`second output cancel failed: ${error}`);
  }
  assert.equal(cancelAgain, cancel);
  try {
    await cancel;
  } catch (error) {
    throw new Error(`output cancel Promise rejected: ${error}`);
  }
  assert.equal(session.invokeSync(18, []).value, 111);

  // StreamStep owns resources inside the tagged output of OutputStreamNext.
  // The generated OHOS path must preserve the exact own-key shape and route
  // object release through the same session scheduler as ordinary results.
  const stepLiveSession = addon.__uniffi_backend_factory(host);
  const stepLiveOutput = stepLiveSession.invokeSync(12, [11]).value;
  const stepLiveBefore = stepLiveSession.invokeSync(18, []).value;
  const liveItemStep = (await stepLiveSession.invokeAsync(33, [stepLiveOutput])).value;
  assert.deepEqual(Object.keys(liveItemStep).sort(), ['kind', 'value']);
  assert.equal(liveItemStep.kind, 'item');
  assert.equal(liveItemStep.value.handle, 1001);
  stepLiveSession.releaseObject(liveItemStep.value);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(stepLiveSession.invokeSync(18, []).value, stepLiveBefore + 1);
  const stepLiveCloseTimers = timerSnapshot();
  await stepLiveSession.close();
  assertOneTeardownTimer(stepLiveCloseTimers, 'live StreamItem close');
  const stepLiveCount = addon.__uniffi_backend_factory(host);
  assert.equal(stepLiveCount.invokeSync(18, []).value, stepLiveBefore + 111);
  await stepLiveCount.close();

  const stepErrorSession = addon.__uniffi_backend_factory(host);
  const stepErrorOutput = stepErrorSession.invokeSync(12, [11]).value;
  const stepErrorBefore = stepErrorSession.invokeSync(18, []).value;
  const errorStep = (await stepErrorSession.invokeAsync(34, [stepErrorOutput])).value;
  assert.deepEqual(Object.keys(errorStep).sort(), ['error', 'kind']);
  assert.equal(errorStep.kind, 'error');
  assert.equal(errorStep.error.handle, 1002);
  const stepErrorCloseTimers = timerSnapshot();
  await stepErrorSession.close();
  assertOneTeardownTimer(stepErrorCloseTimers, 'StreamError close');
  const stepErrorCount = addon.__uniffi_backend_factory(host);
  assert.equal(stepErrorCount.invokeSync(18, []).value, stepErrorBefore + 111);
  await stepErrorCount.close();

  // After the close deadline, the detached walker still follows StreamItem
  // when the native future finally produces its object-bearing result.  The
  // output receiver and the late object each release exactly once.
  const lateStepController = addon.__uniffi_backend_factory(host);
  const lateStepSession = addon.__uniffi_backend_factory(host);
  const lateStepOutput = lateStepSession.invokeSync(12, [11]).value;
  const lateStepBefore = lateStepSession.invokeSync(18, []).value;
  const lateStepResult = lateStepSession.invokeAsync(35, [lateStepOutput]);
  const lateStepTimers = timerSnapshot();
  const lateStepClose = lateStepSession.close();
  await new Promise((resolve) => originalSetTimeout(resolve, 80));
  await lateStepClose;
  assertOneTeardownTimer(lateStepTimers, 'late StreamItem close');
  lateStepController.invokeSync(36, []);
  const lateItemStep = (await lateStepResult).value;
  assert.deepEqual(Object.keys(lateItemStep).sort(), ['kind', 'value']);
  assert.equal(lateItemStep.kind, 'item');
  assert.equal(lateItemStep.value.handle, 3001);
  await new Promise((resolve) => setImmediate(resolve));
  await new Promise((resolve) => originalSetTimeout(resolve, 80));
  const lateStepCount = addon.__uniffi_backend_factory(host);
  assert.equal(lateStepCount.invokeSync(18, []).value, lateStepBefore + 111);
  await lateStepCount.close();
  await lateStepController.close();

  // A native consumer can keep a retained callback proxy after the call
  // returns.  Its lease must remain live until the consumer explicitly drops
  // that proxy, then release exactly once on the JS-thread scheduler.
  assert.equal(session.invokeSync(19, [30]).value, 0);
  assert.deepEqual(lifecycle.slice(-1), [['retain', 0, 30]]);
  assert.equal(session.invokeSync(20, []).value, 0);
  for (
    let turn = 0;
    turn < 20 && !(lifecycle.at(-1)?.[0] === 'release' && lifecycle.at(-1)?.[2] === 30);
    turn++
  ) {
    await new Promise((resolve) => setImmediate(resolve));
  }
  assert.deepEqual(lifecycle.slice(-2), [
    ['retain', 0, 30],
    ['release', 0, 30],
  ]);

  const closeObject = { handle: 99 };
  assert.equal(session.invokeSync(5, [closeObject]).value, closeObject);
  const closeOutput = session.invokeSync(12, [12]).value;
  const naturalCloseTimers = timerSnapshot();
  await session.close();
  assertOneTeardownTimer(naturalCloseTimers, 'natural resource close');
  // Use a clean session so the assertion below cannot be satisfied merely by
  // an unrelated resource-release Promise.  close() must retain the session
  // until this callback's deferred Host Promise settles.
  const closeSession = addon.__uniffi_backend_factory(host);
  const inFlight = closeSession.invokeAsync(8, [20, 55]);
  while (!deferredStarted) {
    await new Promise((resolve) => setImmediate(resolve));
  }
  let closeSettled = false;
  const deferredCloseTimers = timerSnapshot();
  const closeRaw = closeSession.close();
  const closePromise = closeRaw.then(() => {
    closeSettled = true;
  });
  const closeAgain = closeSession.close();
  assert.equal(closeAgain, closeRaw);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(closeSettled, false, 'close must wait for an in-flight callback');
  resolveDeferred(65);
  assert.equal(await inFlight, 65);
  await closePromise;
  assertOneTeardownTimer(deferredCloseTimers, 'natural callback close');
  // close is idempotent after the first close has completed.
  await closeSession.close();
  const nextSession = addon.__uniffi_backend_factory(host);
  assert.equal(nextSession.invokeSync(18, []).value, 555);
  await nextSession.close();

  // The OHOS fixture carries an explicit short ClosePolicy.  A native future
  // that never settles must still let close() resolve at the one deadline;
  // repeated close() calls return that exact Promise and do not leave an
  // unhandled rejection behind.
  const nativeDeadlineSession = addon.__uniffi_backend_factory(host);
  const nativeNever = nativeDeadlineSession.invokeAsync(25, []);
  nativeNever.catch(() => undefined);
  const nativeDeadlineTimers = timerSnapshot();
  const nativeDeadlineClose = nativeDeadlineSession.close();
  assert.strictEqual(nativeDeadlineSession.close(), nativeDeadlineClose);
  let nativeDeadlineSettled = false;
  nativeDeadlineClose.then(() => {
    nativeDeadlineSettled = true;
  });
  await Promise.resolve();
  assert.equal(nativeDeadlineSettled, false);
  await nativeDeadlineClose;
  assertOneTeardownTimer(nativeDeadlineTimers, 'never-settle native close');
  const nativeWakeSession = addon.__uniffi_backend_factory(host);
  assert.equal(nativeWakeSession.invokeSync(27, []).value, 0);
  assert.equal((await nativeNever).value, 0);
  await nativeWakeSession.close();

  // A callback Promise whose finally guard runs after detach must be harmless:
  // the late resolve is observed by the returned Promise, but it cannot call
  // Host/application code or re-enter the detached session.
  const lateCallbackSession = addon.__uniffi_backend_factory(host);
  const callbackCallsBeforeLateResolve = callbackCalls.length;
  const lateCallbackResult = lateCallbackSession.invokeAsync(8, [20, 56]);
  while (typeof resolveLateCallback !== 'function') {
    await new Promise((resolve) => setImmediate(resolve));
  }
  const lateCallbackTimers = timerSnapshot();
  const lateCallbackClose = lateCallbackSession.close();
  assert.strictEqual(lateCallbackSession.close(), lateCallbackClose);
  let lateCallbackClosed = false;
  lateCallbackClose.then(() => {
    lateCallbackClosed = true;
  });
  await Promise.resolve();
  assert.equal(lateCallbackClosed, false);
  await lateCallbackClose;
  assertOneTeardownTimer(lateCallbackTimers, 'late callback close');
  assert.equal(callbackCalls.length, callbackCallsBeforeLateResolve + 1);
  resolveLateCallback(66);
  assert.equal(await lateCallbackResult, 66);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(callbackCalls.length, callbackCallsBeforeLateResolve + 1);

  // Rejected callback Promises are handled after deadline as well.  Awaiting
  // the user-visible rejection proves --unhandled-rejections=strict sees no
  // detached finally/settlement callback leak.
  const lateRejectSession = addon.__uniffi_backend_factory(host);
  const callbackCallsBeforeLateReject = callbackCalls.length;
  const lateRejectResult = lateRejectSession.invokeAsync(8, [20, 57]);
  const lateRejectHandled = lateRejectResult.catch((error) => error);
  while (typeof rejectLateCallback !== 'function') {
    await new Promise((resolve) => setImmediate(resolve));
  }
  const lateRejectTimers = timerSnapshot();
  const lateRejectClose = lateRejectSession.close();
  assert.strictEqual(lateRejectSession.close(), lateRejectClose);
  let lateRejectClosed = false;
  lateRejectClose.then(() => {
    lateRejectClosed = true;
  });
  await Promise.resolve();
  assert.equal(lateRejectClosed, false);
  await lateRejectClose;
  assertOneTeardownTimer(lateRejectTimers, 'late rejected callback close');
  rejectLateCallback(new Error('late fixture callback rejection'));
  const lateRejectError = await lateRejectHandled;
  assert.match(String(lateRejectError), /late fixture callback rejection/);
  assert.equal(callbackCalls.length, callbackCallsBeforeLateReject + 1);

  // Input next (pull) and return (cancel) each retain a stream token until
  // close.  Their Host Promises settle only after the deadline and must not
  // call releaseInputStream a second time.
  const deadlineInputPullSession = addon.__uniffi_backend_factory(host);
  await deadlineInputPullSession.invokeAsync(15, [444]);
  const pendingPull = deadlineInputPullSession.invokeAsync(16, [444]);
  while (typeof resolvePendingPull !== 'function') {
    await new Promise((resolve) => setImmediate(resolve));
  }
  const release444Before = streams.filter(([kind, id]) => kind === 'release' && id === 444).length;
  const deadlineInputPullTimers = timerSnapshot();
  const deadlineInputPullClose = deadlineInputPullSession.close();
  assert.strictEqual(deadlineInputPullSession.close(), deadlineInputPullClose);
  let deadlineInputPullClosed = false;
  deadlineInputPullClose.then(() => {
    deadlineInputPullClosed = true;
  });
  await Promise.resolve();
  assert.equal(deadlineInputPullClosed, false);
  await deadlineInputPullClose;
  assertOneTeardownTimer(deadlineInputPullTimers, 'late input next close');
  assert.equal(streams.filter(([kind, id]) => kind === 'release' && id === 444).length, release444Before + 1);
  resolvePendingPull({ kind: 'done' });
  await pendingPull;
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(streams.filter(([kind, id]) => kind === 'release' && id === 444).length, release444Before + 1);

  const deadlineInputReturnSession = addon.__uniffi_backend_factory(host);
  await deadlineInputReturnSession.invokeAsync(15, [445]);
  const pendingReturn = deadlineInputReturnSession.invokeAsync(17, [445]);
  const pendingReturnHandled = pendingReturn.catch((error) => error);
  while (typeof rejectPendingCancel !== 'function') {
    await new Promise((resolve) => setImmediate(resolve));
  }
  const release445Before = streams.filter(([kind, id]) => kind === 'release' && id === 445).length;
  const deadlineInputReturnTimers = timerSnapshot();
  const deadlineInputReturnClose = deadlineInputReturnSession.close();
  assert.strictEqual(deadlineInputReturnSession.close(), deadlineInputReturnClose);
  let deadlineInputReturnClosed = false;
  deadlineInputReturnClose.then(() => {
    deadlineInputReturnClosed = true;
  });
  await Promise.resolve();
  assert.equal(deadlineInputReturnClosed, false);
  await deadlineInputReturnClose;
  assertOneTeardownTimer(deadlineInputReturnTimers, 'late input return close');
  assert.equal(streams.filter(([kind, id]) => kind === 'release' && id === 445).length, release445Before + 1);
  rejectPendingCancel(new Error('late fixture input return rejection'));
  const pendingReturnError = await pendingReturnHandled;
  assert.match(String(pendingReturnError), /late fixture input return rejection/);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(streams.filter(([kind, id]) => kind === 'release' && id === 445).length, release445Before + 1);

  // Output cancel is deliberately pending for handle 777.  The close
  // deadline detaches first; waking the native cancel afterwards must still
  // run cancel and release exactly once, with no application callback.
  const outputHost = Object.assign({}, host, { handle: 777 });
  const outputSession = addon.__uniffi_backend_factory(outputHost);
  const output777 = outputSession.invokeSync(12, [31]).value;
  assert.equal(output777.handle, 777);
  const outputCountSession = addon.__uniffi_backend_factory(host);
  const outputReleaseBeforeLateCancel = outputCountSession.invokeSync(18, []).value;
  await outputCountSession.close();
  const pendingOutputCancel = outputSession.cancelOutputStream(output777);
  const pendingOutputCancelAgain = outputSession.cancelOutputStream(output777);
  assert.strictEqual(pendingOutputCancelAgain, pendingOutputCancel);
  const outputDeadlineTimers = timerSnapshot();
  const outputDeadlineClose = outputSession.close();
  assert.strictEqual(outputSession.close(), outputDeadlineClose);
  let outputDeadlineClosed = false;
  outputDeadlineClose.then(() => {
    outputDeadlineClosed = true;
  });
  await Promise.resolve();
  assert.equal(outputDeadlineClosed, false);
  await outputDeadlineClose;
  assertOneTeardownTimer(outputDeadlineTimers, 'late output cancel close');
  const outputWakeSession = addon.__uniffi_backend_factory(host);
  assert.equal(outputWakeSession.invokeSync(26, []).value, 0);
  await outputWakeSession.close();
  await pendingOutputCancel;
  await new Promise((resolve) => setImmediate(resolve));
  const outputCountAfterLateCancel = addon.__uniffi_backend_factory(host);
  assert.equal(
    outputCountAfterLateCancel.invokeSync(18, []).value,
    outputReleaseBeforeLateCancel + 110,
  );
  await outputCountAfterLateCancel.close();

  // Value receivers are lowered synchronously/inside the async future without
  // becoming resource leases.  A handle-shaped value survives close with no
  // native release hook invocation.
  const valueSession = addon.__uniffi_backend_factory(host);
  const valueCountBeforeClose = valueSession.invokeSync(18, []).value;
  const value9999Record = { handle: 9999 };
  const value9999Enum = { tag: 'ready', handle: 9999 };
  assert.equal(valueSession.invokeSync(21, [value9999Record]).value, 9999);
  assert.equal(valueSession.invokeSync(23, [value9999Enum]).value, 9999);
  assert.equal((await valueSession.invokeAsync(22, [value9999Record])).value, 10000);
  assert.equal((await valueSession.invokeAsync(24, [value9999Enum])).value, 10000);
  const valueCloseTimers = timerSnapshot();
  await valueSession.close();
  assertOneTeardownTimer(valueCloseTimers, 'value receiver close');
  const valueCountAfterClose = addon.__uniffi_backend_factory(host);
  assert.equal(valueCountAfterClose.invokeSync(18, []).value, valueCountBeforeClose);
  await valueCountAfterClose.close();

  assert.deepEqual(callbackCalls.slice(0, 14), [
    ['sync', 0, 10, 1, [5]],
    ['sync', 0, 10, 3, [6]],
    ['async', 0, 20, 0, 0, [5]],
    ['async', 0, 20, 2, 1, [6]],
    ['async', 0, 20, 0, 2, [5]],
    ['async', 0, 20, 0, 3, [6]],
    ['async', 0, 20, 0, 4, [42]],
    ['async', 0, 20, 0, 5, [77]],
    ['async', 0, 20, 0, 6, [78]],
    ['async', 0, 20, 2, 7, [6]],
    ['async', 0, 20, 2, 8, [42]],
    ['sync', 0, 10, 3, [99]],
    ['sync', 0, 10, 3, [88]],
    ['async', 0, 20, 0, 0, [55]],
  ]);
  assert.deepEqual(callbackCalls.slice(-2).map((call) => call[5]), [[56], [57]]);
  assert.deepEqual(lifecycle, [
    ['retain', 0, 10],
    ['release', 0, 10],
    ['retain', 0, 20],
    ['release', 0, 20],
    ['retain', 0, 30],
    ['release', 0, 30],
  ]);
  assert.deepEqual(streams.slice(0, 3), [
    ['pull', 9],
    ['cancel', 9],
    ['release', 9],
  ]);
  assert.equal(streams.filter(([kind, id]) => kind === 'release' && id === 444).length, 1);
  assert.equal(streams.filter(([kind, id]) => kind === 'release' && id === 445).length, 1);

  // Nested result-resource paths fan out through Optional, Sequence and
  // Variant selectors while preserving one owned lease for the same value.
  const nestedCountSession = addon.__uniffi_backend_factory(host);
  const nestedReleaseBefore = nestedCountSession.invokeSync(18, []).value;
  const nestedObjectSession = addon.__uniffi_backend_factory(host);
  const nestedObjectInput = { handle: 700 };
  const nestedObjectResult = nestedObjectSession.invokeSync(28, [nestedObjectInput]).value;
  assert.strictEqual(nestedObjectResult.optionalObject, nestedObjectInput);
  assert.strictEqual(nestedObjectResult.objects[0], nestedObjectInput);
  assert.strictEqual(nestedObjectResult.variant.object, nestedObjectInput);
  await nestedObjectSession.close();

  const nestedOutputSession = addon.__uniffi_backend_factory(host);
  const nestedOutputInput = { handle: 701 };
  const nestedOutputResult = nestedOutputSession.invokeSync(29, [nestedOutputInput]).value;
  assert.strictEqual(nestedOutputResult.optionalOutput, nestedOutputInput);
  assert.strictEqual(nestedOutputResult.outputs[0], nestedOutputInput);
  assert.strictEqual(nestedOutputResult.variant.output, nestedOutputInput);
  await nestedOutputSession.cancelOutputStream(nestedOutputResult.optionalOutput);
  await nestedOutputSession.close();

  const nestedInputSession = addon.__uniffi_backend_factory(host);
  const nestedInputValue = { handle: 702 };
  const nestedInputResult = nestedInputSession.invokeSync(30, [nestedInputValue]).value;
  assert.strictEqual(nestedInputResult.optionalInput, nestedInputValue);
  assert.strictEqual(nestedInputResult.inputs[0], nestedInputValue);
  assert.strictEqual(nestedInputResult.variant.input, nestedInputValue);
  const nestedInputReleaseBefore = streams.filter(([kind, id]) => kind === 'release' && id === 702).length;
  await nestedInputSession.close();
  assert.equal(streams.filter(([kind, id]) => kind === 'release' && id === 702).length, nestedInputReleaseBefore + 1);
  assert.equal(nestedCountSession.invokeSync(18, []).value, nestedReleaseBefore + 111);

  // An async nested result that outlives the 40ms deadline must be disposed
  // independently after its Promise settles, without reviving the Host.
  const lateNestedController = addon.__uniffi_backend_factory(host);
  const lateNestedSession = addon.__uniffi_backend_factory(host);
  const lateNestedInput = {
    optionalObject: { handle: 710 },
    objects: [{ handle: 711 }],
    variant: { tag: 'Ready', input: { handle: 712 } },
  };
  const lateNestedBefore = nestedCountSession.invokeSync(18, []).value;
  const lateNestedCallbacksBefore = callbackCalls.length;
  const lateNestedResult = lateNestedSession.invokeAsync(31, [lateNestedInput]);
  const lateNestedClose = lateNestedSession.close();
  await new Promise((resolve) => originalSetTimeout(resolve, 80));
  await lateNestedClose;
  lateNestedController.invokeSync(32, []);
  assert.deepEqual((await lateNestedResult).value, lateNestedInput);
  await new Promise((resolve) => setImmediate(resolve));
  await new Promise((resolve) => originalSetTimeout(resolve, 80));
  assert.equal(nestedCountSession.invokeSync(18, []).value, lateNestedBefore + 111);
  assert.equal(streams.filter(([kind, id]) => kind === 'release' && id === 712).length, 1);
  assert.equal(callbackCalls.length, lateNestedCallbacksBefore);
  await lateNestedController.close();
  await nestedCountSession.close();

  assert.throws(() => session.invokeSync(0, [1]), /closed/);
})().catch((error) => {
  console.error(error);
  process.exitCode = 1;
}).finally(() => {
  global.setTimeout = originalSetTimeout;
  global.clearTimeout = originalClearTimeout;
});
"#;
  let node = Command::new("node")
    .arg("--unhandled-rejections=strict")
    .arg("-e")
    .arg(script)
    .arg(&addon)
    .output()
    .expect("run Node against the generated OHOS addon");
  assert!(
    node.status.success(),
    "generated OHOS addon failed at runtime (status={:?}):\n{}\n{}",
    node.status,
    String::from_utf8_lossy(&node.stdout),
    String::from_utf8_lossy(&node.stderr),
  );
}
