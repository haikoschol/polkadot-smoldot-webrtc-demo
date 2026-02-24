# Polkadot/Smoldot WebRTC Demo

## Overview

This document describes how to run a local Polkadot dev chain node with libp2p-webrtc-direct transport enabled in litep2p and connect the PAPI Console
to it using Smoldot.

## Requirements

- git
- Rust toolchain (incl. target `wasm32-unknown-unknown`)
- protobuf compiler
- Node.js
- Python 3
- tmux (optional, used in the automated demo)
- Wireshark, incl. `text2pcap` executable (optional, used in the automated demo)

## Automated Demo

Execute `python3 demo.py /path/to/chrome` in a terminal that is not running tmux. Make sure no process is listening on TCP ports 5173 and 8080 and that
Chrome is not running yet. Wait for Chrome to launch and open to PAPI Console once the chainspec has been fetched from the running node. While you wait,
copy Alice's address displayed on the loading page.

Once Smoldot has connected, play around with it, make a storage query to fetch Alice's balance, etc. then quit Chrome. A CSV file will have been written
with information about subtreams between dialer (Chrome) and listener (the node), what libp2p protobuf flags were set in the messages, what
multistream-select protocol negotiation took place, etc.

## Manual Demo

If you don't want to run `demo.py`, perform the below steps manually to run the demo.

### Building the Polkadot Node

In terminal/tmux window A:

- clone [`ChainSafe/polkadot-sdk`](https://github.com/ChainSafe/polkadot-sdk/) and check out branch `haiko-webrtc-demo` (e.g. in directory `./polkadot-sdk`)
- build Polkadot (`cargo build -p polkadot`)
- build a dev chainspec (`./target/debug/polkadot build-spec --dev --raw > dev-chain-spec.json`)

### Building Smoldot

In terminal/tmux window B:

- clone [`ChainSafe/smoldot`](https://github.com/ChainSafe/smoldot/) and check out branch `haiko-webrtc-deadlock-fix` (e.g. in directory `./smoldot`)
- install dependencies (`cd smoldot/wasm-node/javascript && npm install`)
- build wasm light client (`npm run dev` (also in directory `smoldot/wasm-node/javascript`))

### PAPI Console

In terminal/tmux window B:

- clone [`haikoschol/papi-console`](https://github.com/haikoschol/papi-console) and check out branch `webrtc-demo` (e.g. in directory `./papi-console`)
- install dependencies (`corepack pnpm install`)
- run dev server (`corepack pnpm dev`)

### Running the Polkadot Node

In terminal/tmux window A:

In the root directory of this repository, run `node.py` like this:

```
$ python3 node.py --polkadot-bin ./polkadot-sdk/target/debug/polkadot --ts-output ./papi-console/src/state/chains/chainspecs/polkadot-dev-webrtc.ts
```

### Chrome with traffic logging

In terminal/tmux window C:

Run Chrome like this (e.g. with default install on macOS):

```
$ /Applications/Google\ Chrome.app/Contents/MacOS/Google\ Chrome \
    --auto-open-devtools-for-tabs \
    --enable-logging=stderr --log-level=0 --v=0 \
    --vmodule='*/webrtc/*=1' \
     2>&1 | grep -F SCTP_PACKET | text2pcap -D -t %H:%M:%S.%f -i 132 - webrtc-demo-$(date "+%s").pcapng
```

Once running, follow the instructions in the "Automated Demo" section. Make sure to quit Chrome to let the shell pipeline complete that writes the `pcapng` file.

### Analyze WebRTC Traffic

- clone [`ChainSafe/litep2p-perf`](https://github.com/ChainSafe/litep2p-perf) and check out the branch `haiko-capture-traffic` (e.g. in directory `./litep2p-perf`)
- run `pcap-analyzer` on the `.pcapng` file (`cd litep2p-perf && cargo run -p smoldot-automation --bin pcap-analyzer -- --all-messages ../webrtc-demo-<timestamp>.pcapng`)
- there's probably a lot of messages so try the CSV export (`--csv`)
- open the `.pcapng` file in Wireshark (configure it to parse the schema in `./litep2p-perf/smoldot-automation/protobuf/webrtc.proto`)

### Using WebSocket transport instead of WebRTC

By default `node.py` uses the WebRTC-direct address as the bootnode in the generated chainspec. To use a plain WebSocket connection instead, pass `--transport websocket`:

```
$ python3 node.py --polkadot-bin ./polkadot-sdk/target/debug/polkadot --ts-output ./papi-console/src/state/chains/chainspecs/polkadot-dev-webrtc.ts --transport websocket
```

Additionally, Smoldot's `forbidWs` option must be set to `false` in `papi-console/src/state/chains/smoldot.ts`, otherwise Smoldot will refuse to connect over
unencrypted WebSocket connections:

```ts
// papi-console/src/state/chains/smoldot.ts
forbidWs: false,
```
