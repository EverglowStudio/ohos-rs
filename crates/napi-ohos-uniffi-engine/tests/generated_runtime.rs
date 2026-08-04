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

  let script = r#"
const assert = require('node:assert/strict');
const path = require('node:path');
const addon = require(path.resolve(process.argv[1]));
assert.deepEqual(Object.keys(addon), ['__uniffi_backend_factory']);

let session;
const callbackCalls = [];
const lifecycle = [];
const streams = [];
let deferredStarted = false;
let resolveDeferred;
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
    return Promise.resolve({ kind: 'item', value: streamId + 1 });
  },
  cancelInputStream(streamId) {
    streams.push(['cancel', streamId]);
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

(async () => {
  assert.equal((await session.invokeAsync(1, [6])).value, 7);
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
  assert.equal(output, host);
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

  const closeObject = { handle: 99 };
  assert.equal(session.invokeSync(5, [closeObject]).value, closeObject);
  const closeOutput = session.invokeSync(12, [12]).value;
  await session.close();
  // Use a clean session so the assertion below cannot be satisfied merely by
  // an unrelated resource-release Promise.  close() must retain the session
  // until this callback's deferred Host Promise settles.
  const closeSession = addon.__uniffi_backend_factory(host);
  const inFlight = closeSession.invokeAsync(8, [20, 55]);
  while (!deferredStarted) {
    await new Promise((resolve) => setImmediate(resolve));
  }
  let closeSettled = false;
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
  // close is idempotent after the first close has completed.
  await closeSession.close();
  const nextSession = addon.__uniffi_backend_factory(host);
  assert.equal(nextSession.invokeSync(18, []).value, 222);
  await nextSession.close();
  assert.deepEqual(callbackCalls, [
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
  assert.deepEqual(lifecycle, [
    ['retain', 0, 10],
    ['retain', 0, 20],
    ['release', 0, 10],
    ['release', 0, 20],
  ]);
  assert.deepEqual(streams, [
    ['pull', 9],
    ['cancel', 9],
    ['release', 9],
  ]);
  assert.throws(() => session.invokeSync(0, [1]), /closed/);
})().catch((error) => {
  console.error(error);
  process.exitCode = 1;
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
    "generated OHOS addon failed at runtime:\n{}\n{}",
    String::from_utf8_lossy(&node.stdout),
    String::from_utf8_lossy(&node.stderr),
  );
}
