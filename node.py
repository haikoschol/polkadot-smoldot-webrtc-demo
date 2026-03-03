#!/usr/bin/env python3
import argparse
import http.server
import socketserver
import subprocess
import threading
import re
import json
import sys
import os
import time

# --- Configuration & State ---
WEBRTC_ADDR = None
WEBSOCKET_ADDR = None
RPC_SERVER_READY = False
ADDR_LOCK = threading.Lock()
LOG_FILE = None
LOADING_PORT = 8080

LOADING_PAGE_HTML = """\
<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Polkadot/Smoldot WebRTC Demo Loading...</title>
    <style>
        * { box-sizing: border-box; margin: 0; padding: 0; }
        body {
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
            background: #0d0d0d;
            color: #e0e0e0;
            min-height: 100vh;
            display: flex;
            justify-content: center;
            align-items: center;
        }
        .card {
            background: #1a1a1a;
            border: 1px solid #2a2a2a;
            border-radius: 12px;
            padding: 3rem 2.5rem;
            max-width: 620px;
            width: 90%;
            text-align: center;
        }
        .spinner-wrap {
            width: 56px;
            height: 56px;
            margin: 0 auto 2rem;
            position: relative;
        }
        .spinner-wrap .dot {
            width: 56px;
            height: 56px;
            border-radius: 50%;
            background: #e6007a;
        }
        .spinner-wrap::after {
            content: '';
            position: absolute;
            inset: -6px;
            border-radius: 50%;
            border: 3px solid transparent;
            border-top-color: #e6007a;
            animation: spin 1.2s linear infinite;
        }
        @keyframes spin { to { transform: rotate(360deg); } }
        h1 {
            font-size: 1.5rem;
            font-weight: 600;
            color: #ffffff;
            margin-bottom: 1.25rem;
        }
        p {
            font-size: 0.95rem;
            line-height: 1.7;
            color: #999;
        }
    </style>
</head>
<body>
    <div class="card">
        <div class="spinner-wrap"><div class="dot"></div></div>
        <h1>Polkadot/Smoldot WebRTC Demo Loading...</h1>
        <p>The node is currently starting up.</p><br/>
        <p>Once the RPC endpoint responds with the chainspec, PAPI Console will be opened
        in the browser. Wait until blocks appear and play around with it. For example, run
        a storage query to fetch the balance of Alice (<code>15oF4uVJwmo4TdGW7VfQxNLavjCXviqxT9S1MgbjMNHr6Sp5</code>).</p><br/>
        <p>Then quit Chrome to ensure that the pcapng file containing the WebRTC network
        traffic gets written to disk. This file will then be processed by the pcap-analyzer
        tool to create a CSV file with information about substreams opening/closing,
        multistream-select negotiation, payload sizes, etc.</p>
    </div>
</body>
</html>
"""

class Colors:
    RESET = "\033[0m"
    POLKADOT = "\033[35m"  # Magenta
    TS_GEN = "\033[36m"    # Cyan
    SUCCESS = "\033[32m"   # Green
    WARN = "\033[33m"      # Yellow

def print_log(source, message, color):
    timestamp = time.strftime("%H:%M:%S")
    # Print to console with color
    sys.stdout.write(f"{color}[{timestamp}] [{source}] {message}{Colors.RESET}\n")
    sys.stdout.flush()

    # Write to log file without color codes
    if LOG_FILE:
        try:
            with open(LOG_FILE, 'a') as f:
                f.write(f"[{timestamp}] [{source}] {message}\n")
        except Exception as e:
            sys.stderr.write(f"Error writing to log file: {e}\n")

def fetch_chainspec_from_node():
    """Fetch chainspec from running Polkadot node via JSON-RPC with retries"""
    import urllib.request
    import urllib.error

    url = f"http://{ARGS.ip}:9944"
    payload = {
        "id": 1,
        "jsonrpc": "2.0",
        "method": "sync_state_genSyncSpec",
        "params": [True]
    }

    max_retries = 10
    retry_delay = 5  # seconds

    for attempt in range(1, max_retries + 1):
        try:
            req = urllib.request.Request(
                url,
                data=json.dumps(payload).encode('utf-8'),
                headers={'Content-Type': 'application/json'}
            )
            with urllib.request.urlopen(req, timeout=60) as response:
                result = json.loads(response.read().decode('utf-8'))
                chainspec = result.get('result')

                # Verify we actually got data
                if chainspec:
                    print_log("TS_Gen", f"Successfully fetched chainspec on attempt {attempt}", Colors.SUCCESS)
                    return chainspec
                else:
                    raise Exception("Response contained no 'result' field or result was empty")

        except Exception as e:
            if attempt < max_retries:
                print_log("TS_Gen", f"Attempt {attempt}/{max_retries} failed: {e}. Retrying in {retry_delay}s...", Colors.WARN)
                time.sleep(retry_delay)
            else:
                print_log("TS_Gen", f"Failed to fetch chainspec after {max_retries} attempts: {e}", Colors.WARN)
                return None

    return None

def rewrite_multiaddr_host(address, public_addr):
    """Replace the local IP in a multiaddress with a public hostname or IP.

    If public_addr is an IP address, replaces /ip4/<local> with /ip4/<public>.
    If public_addr is a hostname, replaces /ip4/<local> with /dns4/<hostname>.
    """
    if not public_addr:
        return address

    import ipaddress as ipaddr_mod
    try:
        ipaddr_mod.ip_address(public_addr)
        replacement = f"/ip4/{public_addr}"
    except ValueError:
        replacement = f"/dns4/{public_addr}"

    return re.sub(r'^/ip[46]/[^/]+', replacement, address)

def generate_ts_file(address):
    """Fetches chainspec from node and generates TypeScript file with bootnode"""

    # Rewrite address if --public-addr is set
    address = rewrite_multiaddr_host(address, ARGS.public_addr)

    # Fetch chainspec from the running node
    print_log("TS_Gen", "Fetching chainspec from running node...", Colors.TS_GEN)
    spec_data = fetch_chainspec_from_node()

    if spec_data is None:
        print_log("TS_Gen", "Failed to fetch chainspec from node", Colors.WARN)
        return

    # Parse the chainspec (it's returned as a JSON string in the result)
    try:
        if isinstance(spec_data, str):
            spec_data = json.loads(spec_data)
    except Exception as e:
        print_log("TS_Gen", f"Error parsing chainspec: {e}", Colors.WARN)
        return

    # Replace bootNodes array with only our WebRTC/Websocket address
    spec_data['bootNodes'] = [address]

    # Create the TypeScript content
    json_str = json.dumps(spec_data)
    ts_content = f"// Auto-generated by node.py\nexport const chainSpec = JSON.stringify({json_str});"

    try:
        with open(ARGS.ts_output, 'w') as f:
            f.write(ts_content)
        print_log("TS_Gen", f"Updated {ARGS.ts_output} with chainspec and bootnode!", Colors.SUCCESS)
        return True
    except Exception as e:
        print_log("TS_Gen", f"Error writing TS file: {e}", Colors.WARN)

class LoadingPageHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.end_headers()
        self.wfile.write(LOADING_PAGE_HTML.encode('utf-8'))

    def log_message(self, format, *args):
        pass  # Suppress request logs

def start_loading_server():
    with socketserver.TCPServer(("", LOADING_PORT), LoadingPageHandler) as httpd:
        httpd.serve_forever()

def generate_ts_and_navigate(address):
    """Generate the chainspec TS file, then navigate Chrome to the demo URL if --chrome-bin is set."""
    success = generate_ts_file(address)
    if success and ARGS.chrome_bin:
        url = "http://localhost:5173/explorer#networkId=polkadot-dev-webrtc&endpoint=light-client"
        print_log("System", f"Navigating Chrome to {url}", Colors.SUCCESS)
        subprocess.Popen([ARGS.chrome_bin, url])

def run_polkadot():
    global WEBRTC_ADDR, WEBSOCKET_ADDR, RPC_SERVER_READY

    cmd = [
        ARGS.polkadot_bin,
        "--dev",
        "-lsub-libp2p=debug",
        "-llitep2p=debug",
        "-llitep2p::notification::handle=error",  # Enable notification send logging
        "-lsub-libp2p::peerset=trace",
        "-lsub-libp2p::behaviour=debug",
        f"--listen-addr=/ip4/{ARGS.ip}/tcp/30333",
        f"--listen-addr=/ip4/{ARGS.ip}/tcp/9945/ws",
        f"--listen-addr=/ip4/{ARGS.ip}/udp/30334/webrtc-direct",
        "--sync=full",
        "--rpc-cors=all",
        "--unsafe-rpc-external",
        "--rpc-methods=unsafe",  # Allow all RPC methods
        "--rpc-max-response-size=26214400" # 25MB to fit chainspec
    ]

    print_log("System", f"Starting Polkadot...", Colors.SUCCESS)

    try:
        process = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1
        )
    except FileNotFoundError:
        print_log("System", f"Error: Binary {ARGS.polkadot_bin} not found.", Colors.WARN)
        os._exit(1)

    # Patterns to detect
    webrtc_pattern = re.compile(r"listening on:\s+(/.*(webrtc|webrtc-direct).*certhash.*p2p.*)", re.IGNORECASE)
    websocket_pattern = re.compile(r"listening on:\s+(/.*/tcp/\d+/ws/p2p/\w+)", re.IGNORECASE)
    rpc_pattern = re.compile(r"Running JSON-RPC server", re.IGNORECASE)

    ts_file_generated = False

    while True:
        line = process.stdout.readline()
        if not line and process.poll() is not None:
            break
        if line:
            clean_line = line.strip()
            print_log("Polkadot", clean_line, Colors.POLKADOT)

            # Check for WebRTC address
            webrtc_match = webrtc_pattern.search(clean_line)
            if webrtc_match:
                found_addr = webrtc_match.group(1).strip()
                with ADDR_LOCK:
                    if WEBRTC_ADDR != found_addr:
                        WEBRTC_ADDR = found_addr
                        print_log("System", f"Captured WebRTC Address: {WEBRTC_ADDR}", Colors.SUCCESS)

            # Check for WebSocket address
            websocket_match = websocket_pattern.search(clean_line)
            if websocket_match:
                found_addr = websocket_match.group(1).strip()
                with ADDR_LOCK:
                    if WEBSOCKET_ADDR != found_addr:
                        WEBSOCKET_ADDR = found_addr
                        print_log("System", f"Captured WebSocket Address: {WEBSOCKET_ADDR}", Colors.SUCCESS)

            # Check for RPC server ready
            rpc_match = rpc_pattern.search(clean_line)
            if rpc_match:
                with ADDR_LOCK:
                    RPC_SERVER_READY = True
                    print_log("System", "RPC server is ready", Colors.SUCCESS)

            # Generate TS file when address and RPC server are ready
            with ADDR_LOCK:
                # Choose which address to use based on --transport flag
                chosen_addr = None
                if ARGS.transport == "webrtc" and WEBRTC_ADDR:
                    chosen_addr = WEBRTC_ADDR
                elif ARGS.transport == "websocket" and WEBSOCKET_ADDR:
                    chosen_addr = WEBSOCKET_ADDR

                if chosen_addr and RPC_SERVER_READY and not ts_file_generated:
                    print_log("System", f"Using {ARGS.transport} address and RPC server ready, generating chainspec...", Colors.SUCCESS)
                    # Run in background thread so we can continue reading logs
                    fetch_thread = threading.Thread(target=generate_ts_and_navigate, args=(chosen_addr,))
                    fetch_thread.daemon = True
                    fetch_thread.start()
                    ts_file_generated = True

    rc = process.poll()
    print_log("System", f"Polkadot exited with code {rc}", Colors.WARN)
    os._exit(rc)

if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--ip", default="127.0.0.1")
    parser.add_argument("--polkadot-bin", default="./polkadot-sdk/target/debug/polkadot")

    parser.add_argument("--ts-output", default="./papi-console/src/state/chains/chainspecs/polkadot-dev-webrtc.ts",
                        help="Path to write the .ts file (e.g., src/local-chain.ts)")

    parser.add_argument("--transport", choices=["webrtc", "websocket"], default="webrtc",
                        help="Transport to use in chainspec bootNodes (default: webrtc)")

    parser.add_argument("--public-addr", default=None,
                        help="Public hostname or IP for chainspec bootNodes (e.g., rpc.example.com or 1.2.3.4). "
                             "Rewrites the local IP in the multiaddress so external clients can connect.")

    parser.add_argument("--chrome-bin", default=None,
                        help="Path to Chrome binary. If set, navigates Chrome to the demo URL after the chainspec is generated.")

    parser.add_argument("--log-file", default=None,
                        help="Path to write log file (e.g., node.log). Logging to file is disabled by default.")

    ARGS = parser.parse_args()
    LOG_FILE = ARGS.log_file

    # Start loading page server
    server_thread = threading.Thread(target=start_loading_server, daemon=True)
    server_thread.start()
    print_log("System", f"Loading page available at http://localhost:{LOADING_PORT}", Colors.SUCCESS)

    # Navigate Chrome to loading page now that the server is guaranteed to be running
    if ARGS.chrome_bin:
        print_log("System", f"Navigating Chrome to http://localhost:{LOADING_PORT}", Colors.SUCCESS)
        subprocess.Popen([ARGS.chrome_bin, f"http://localhost:{LOADING_PORT}"])

    # Initialize log file only when --log-file is passed
    if LOG_FILE:
        try:
            with open(LOG_FILE, 'w') as f:
                f.write(f"=== Log started at {time.strftime('%Y-%m-%d %H:%M:%S')} ===\n")
            print_log("System", f"Logging to {LOG_FILE}", Colors.SUCCESS)
        except Exception as e:
            print_log("System", f"Warning: Could not initialize log file {LOG_FILE}: {e}", Colors.WARN)

    try:
        run_polkadot()
    except KeyboardInterrupt:
        print_log("System", "Shutting down...", Colors.SUCCESS)
        sys.exit(0)

