import smoldot, { Smoldot, SmoldotClient } from 'smoldot';

// Test the export type

// smoldot;  // $ExpectType Smoldot

// Test when supplying all options and all params to logCallback

// $ExpectType Promise<SmoldotClient>
let sp = smoldot.start({
  maxLogLevel: 3,
  logCallback: (level, target, message) => { },
  forbidTcp: false,
  forbidWs: false,
  forbidWss: false,
});

// Test when not supplying optional options and optional params

// $ExpectType Promise<SmoldotClient>
sp = smoldot.start();

sp.then(async (sm) => {
  // $ExpectType Promise<SmoldotChain>
  const chain1 = sm.addChain({ chainSpec: '' });
  // $ExpectType Promise<SmoldotChain>
  const chain2Promise = sm.addChain({ chainSpec: '', potentialRelayChains: [await chain1], jsonRpcCallback: (resp) => { } });
  // $ExpectType SmoldotChain
  const chain2 = await chain2Promise;
  // $ExpectType void
  chain2.sendJsonRpc('{"id":8,"jsonrpc":"2.0","method":"system_health","params":[]}');
  // $ExpectType void
  chain2.remove();
  // $ExpectType void
  sm.terminate();
});
