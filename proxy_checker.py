#!/usr/bin/env python3
"""
Test proxies listed in a JSON array.

Install:
    python -m pip install "requests[socks]"

Run:
    python proxy_checker.py proxies.json

Outputs:
    working_proxies.json
    dead_proxies.json
"""

import argparse
import json
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

import requests


TEST_URL = "https://api.ipify.org?format=json"


def test_proxy(proxy, timeout=10):
    """Return (proxy_with_test_results, is_working)."""
    result = dict(proxy)
    protocol = str(proxy.get("protocol", "")).lower()
    ip = proxy.get("ip")
    port = proxy.get("port")

    if not ip or not port or protocol not in {"http", "https", "socks4", "socks5"}:
        result.update({
            "test_status": "invalid",
            "test_error": "Missing IP/port or unsupported protocol",
            "test_latency_ms": None,
            "exit_ip": None,
        })
        return result, False

    scheme = "socks5h" if protocol == "socks5" else protocol
    if protocol == "socks4":
        scheme = "socks4a"
    proxy_url = f"{scheme}://{ip}:{port}"
    proxies = {"http": proxy_url, "https": proxy_url}

    started = time.perf_counter()
    try:
        with requests.Session() as session:
            response = session.get(
                TEST_URL,
                proxies=proxies,
                timeout=timeout,
            )
            response.raise_for_status()
            data = response.json()
            exit_ip = data.get("ip")

        result.update({
            "test_status": "working",
            "test_latency_ms": round((time.perf_counter() - started) * 1000, 2),
            "exit_ip": exit_ip,
            "test_error": None,
            "tested_at": time.time(),
        })
        return result, True

    except requests.RequestException as exc:
        result.update({
            "test_status": "failed",
            "test_latency_ms": round((time.perf_counter() - started) * 1000, 2),
            "exit_ip": None,
            "test_error": str(exc)[:300],
            "tested_at": time.time(),
        })
        return result, False
    except (ValueError, TypeError) as exc:
        result.update({
            "test_status": "failed",
            "test_latency_ms": round((time.perf_counter() - started) * 1000, 2),
            "exit_ip": None,
            "test_error": f"Invalid response: {exc}",
            "tested_at": time.time(),
        })
        return result, False


def main():
    global TEST_URL
    parser = argparse.ArgumentParser(description="Check a JSON list of proxies.")
    parser.add_argument("input", nargs="?", default="proxies.json",
                        help="Input JSON file (default: proxies.json)")
    parser.add_argument("--workers", type=int, default=30,
                        help="Concurrent tests (default: 30)")
    parser.add_argument("--timeout", type=float, default=10,
                        help="Timeout per proxy in seconds (default: 10)")
    parser.add_argument("--url", default=TEST_URL,
                        help="HTTPS endpoint used to test proxies")
    args = parser.parse_args()
    TEST_URL = args.url

    input_path = Path(args.input)
    try:
        proxies = json.loads(input_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise SystemExit(f"Could not read valid JSON from {input_path}: {exc}")

    if not isinstance(proxies, list):
        raise SystemExit("Input must be a JSON array of proxy objects.")

    working, dead = [], []
    total = len(proxies)
    print(f"Testing {total} proxies with up to {args.workers} workers...")

    with ThreadPoolExecutor(max_workers=max(1, args.workers)) as executor:
        futures = {
            executor.submit(test_proxy, proxy, args.timeout): index
            for index, proxy in enumerate(proxies)
        }
        for completed, future in enumerate(as_completed(futures), start=1):
            result, ok = future.result()
            (working if ok else dead).append(result)
            latency = result.get("test_latency_ms")
            latency_text = f"{latency} ms" if latency is not None else "n/a"
            print(
                f"[{completed}/{total}] "
                f"{result.get('protocol', '?')}://"
                f"{result.get('ip', '?')}:{result.get('port', '?')} "
                f"-> {result.get('test_status')} ({latency_text})"
            )

    # Sort the working list by measured latency, fastest first.
    working.sort(key=lambda item: item.get("test_latency_ms") or float("inf"))
    Path("working_proxies.json").write_text(
        json.dumps(working, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )

    # Also export a plain-text list of working proxies as protocol://ip:port.
    Path("working_proxies.txt").write_text(
        "\n".join(
            f"{item['protocol']}://{item['ip']}:{item['port']}"
            for item in working
        ) + ("\n" if working else ""),
        encoding="utf-8",
    )

    Path("dead_proxies.json").write_text(
        json.dumps(dead, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )

    print("\nFinished.")
    print(f"Working: {len(working)} -> working_proxies.json and working_proxies.txt")
    print(f"Failed:  {len(dead)} -> dead_proxies.json")


if __name__ == "__main__":
    main()
