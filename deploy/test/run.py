#!/usr/bin/env python3
"""Boot the relay behind each shipped reverse-proxy configuration and check it.

`deploy/caddy/Caddyfile` and `deploy/nginx/sunrise.conf.template` each claim a
list of properties in their header. This is what makes those claims tested
rather than hoped: for each proxy it starts a fresh `sunrise-server` on its
default loopback bind, starts the proxy's official image on the host network
with the shipped file mounted unchanged, and drives it from client containers
on a bridge network, each with its own address:

- the per-address limit trips at the configured threshold for two clients
  independently, and a forged `X-Forwarded-For` or `Forwarded` moves neither;
- the relay logs each refusal under the real client's network, not the
  proxy's;
- the sync event stream arrives unbuffered, and survives longer than the
  relay's 15 s keep-alive interval;
- a body over the limit is refused by the proxy with the relay's own 413;
- `/metrics` is unreachable from outside, while the relay serves it on
  loopback.

Needs Docker and a built `sunrise-server`. CI runs it as the `deploy-test` job:

    cargo build -p sunrise-server --bin sunrise-server
    python3 deploy/test/run.py --server target/debug/sunrise-server

Exit 0 when every check passes for every proxy, 1 when one fails (the relay's
log and the proxy's are printed), 2 when the test cannot run.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[2]
NETWORK = "sunrise-deploy-test"
SUBNET = "10.77.0.0/16"
# The host's address on the bridge: where the client containers reach a proxy
# listening on the host network.
GATEWAY = "10.77.0.1"
DOMAIN = "relay.test"
UPSTREAM = "127.0.0.1:8443"
CLIENTS = {"a": "10.77.1.10", "b": "10.77.2.10", "c": "10.77.3.10"}
IMAGES = {
    "caddy": "caddy:2.8.4",
    "nginx": "nginx:1.27.3",
    "curl": "curlimages/curl:8.10.1",
}
# `[limits] meta_per_min` in deploy/test/sunrise.toml.
META_LIMIT = 5
# Bits 32-35, `REQUIRED_CLIENT_BITS` in sunrise-wire-protocol: the capabilities
# a session must offer. Protocol constants, so stable across releases.
REQUIRED_CLIENT_BITS = 0xF << 32
OVERSIZE = 3 * 1024 * 1024


class Failure(Exception):
    """A check did not hold."""


def expect(cond: bool, message: str) -> None:
    if not cond:
        raise Failure(message)


def run(*args: str, check: bool = True, timeout: float = 120) -> subprocess.CompletedProcess:
    return subprocess.run(args, check=check, capture_output=True, text=True, timeout=timeout)


def docker(*args: str, check: bool = True, timeout: float = 300) -> subprocess.CompletedProcess:
    return run("docker", *args, check=check, timeout=timeout)


class Response:
    def __init__(self, status: int, headers: dict[str, str], body: str) -> None:
        self.status = status
        self.headers = headers
        self.body = body

    def json(self) -> dict:
        try:
            return json.loads(self.body)
        except json.JSONDecodeError as e:
            raise Failure(f"expected JSON, got {self.body[:200]!r}") from e

    def __repr__(self) -> str:
        return f"<{self.status} {self.headers.get('content-type', '')} {self.body[:120]!r}>"


def parse_include(raw: str) -> Response:
    """Parse `curl -i` output, skipping any interim 1xx head."""
    rest = raw
    while True:
        head, sep, body = rest.partition("\r\n\r\n")
        if not sep:
            head, sep, body = rest.partition("\n\n")
        lines = head.splitlines()
        if not lines or not lines[0].startswith("HTTP/"):
            return Response(0, {}, raw)
        status = int(lines[0].split()[1])
        if 100 <= status < 200 and body.startswith("HTTP/"):
            rest = body
            continue
        headers = {}
        for line in lines[1:]:
            name, _, value = line.partition(":")
            headers[name.strip().lower()] = value.strip()
        return Response(status, headers, body)


class Client:
    """A container on the bridge network with its own address."""

    def __init__(self, name: str, address: str) -> None:
        self.name = f"sunrise-deploy-client-{name}"
        self.address = address

    def start(self) -> None:
        docker("rm", "-f", self.name, check=False)
        docker(
            "run", "-d", "--name", self.name, "--network", NETWORK, "--ip", self.address,
            "--entrypoint", "sleep", IMAGES["curl"], "infinity",
        )

    def stop(self) -> None:
        docker("rm", "-f", self.name, check=False)

    def sh(self, script: str) -> subprocess.CompletedProcess:
        return docker("exec", self.name, "sh", "-c", script, check=False)

    def curl(
        self,
        path: str,
        *,
        method: str = "GET",
        headers: dict[str, str] | None = None,
        data: str | None = None,
        data_file: str | None = None,
        max_time: float = 10,
        stream: bool = False,
        url: str | None = None,
    ) -> tuple[int, Response]:
        """One request through the proxy; the curl exit code and the response."""
        args = [
            "exec", self.name, "curl", "-sk", "-i", "--max-time", str(max_time),
            "--resolve", f"{DOMAIN}:443:{GATEWAY}", "-X", method,
        ]
        if stream:
            args.append("-N")
        for name, value in (headers or {}).items():
            args += ["-H", f"{name}: {value}"]
        if data is not None:
            args += ["--data-binary", data]
        if data_file is not None:
            args += ["--data-binary", f"@{data_file}"]
        args.append(url or f"https://{DOMAIN}{path}")
        done = docker(*args, check=False, timeout=max_time + 30)
        return done.returncode, parse_include(done.stdout)


class Relay:
    """`sunrise-server` on its default loopback bind, logging to a file."""

    def __init__(self, binary: pathlib.Path, workdir: pathlib.Path) -> None:
        self.binary = binary
        self.workdir = workdir
        self.log_path = workdir / "relay.log"
        self.proc: subprocess.Popen | None = None

    def start(self) -> None:
        data = self.workdir / "data"
        data.mkdir()
        template = (ROOT / "deploy/test/sunrise.toml").read_text()
        config = self.workdir / "sunrise.toml"
        config.write_text(template.replace("@DATA_DIR@", str(data)))
        log = self.log_path.open("w")
        self.proc = subprocess.Popen(
            [str(self.binary), "--config", str(config)],
            stdout=log,
            stderr=subprocess.STDOUT,
            env={"SUNRISE_LOG": "info", "PATH": "/usr/bin:/bin"},
        )
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise Failure(f"the relay exited with {self.proc.returncode}")
            if local("/api/v1/health")[0] == 200:
                return
            time.sleep(0.2)
        raise Failure("the relay did not answer /api/v1/health within 30 s")

    def stop(self) -> None:
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.proc.kill()

    def records(self, ev: str) -> list[dict]:
        out = []
        for line in self.log_path.read_text().splitlines():
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue
            if record.get("ev") == ev:
                out.append(record)
        return out


def local(path: str) -> tuple[int, str]:
    """A GET straight to the relay on loopback, bypassing the proxy."""
    request = urllib.request.Request(f"http://{UPSTREAM}{path}")
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            return response.status, response.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")
    except OSError:
        return 0, ""


def local_declared_oversize(path: str) -> int:
    """The status the relay answers a POST declaring an oversize body with.

    Over a raw socket, because the relay answers from the declared length and
    closes before the body is sent: a client that insists on writing the body
    first, as `urllib` does, gets a broken pipe instead of the response.
    """
    host, port = UPSTREAM.split(":")
    with socket.create_connection((host, int(port)), timeout=10) as sock:
        sock.sendall(
            (
                f"POST {path} HTTP/1.1\r\nHost: {UPSTREAM}\r\n"
                f"Content-Type: application/json\r\nContent-Length: {OVERSIZE}\r\n\r\n"
            ).encode()
        )
        head = sock.recv(4096).decode(errors="replace")
    try:
        return int(head.split()[1])
    except (IndexError, ValueError):
        return 0


def start_proxy(kind: str, workdir: pathlib.Path) -> str:
    name = f"sunrise-deploy-{kind}"
    docker("rm", "-f", name, check=False)
    common = ["run", "-d", "--name", name, "--network", "host"]
    if kind == "caddy":
        docker(
            *common,
            "-e", f"SUNRISE_DOMAIN={DOMAIN}",
            "-e", "SUNRISE_TLS=internal",
            "-e", f"SUNRISE_UPSTREAM={UPSTREAM}",
            "-v", f"{ROOT / 'deploy/caddy/Caddyfile'}:/etc/caddy/Caddyfile:ro",
            IMAGES["caddy"],
        )
    else:
        certs = workdir / "certs"
        certs.mkdir()
        run(
            "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
            "-subj", f"/CN={DOMAIN}", "-addext", f"subjectAltName=DNS:{DOMAIN}",
            "-keyout", str(certs / "privkey.pem"), "-out", str(certs / "fullchain.pem"),
        )
        for pem in certs.iterdir():
            pem.chmod(0o644)
        docker(
            *common,
            "-e", f"SUNRISE_DOMAIN={DOMAIN}",
            "-e", f"SUNRISE_UPSTREAM={UPSTREAM}",
            "-e", "SUNRISE_TLS_DIR=/etc/sunrise-tls",
            "-v", f"{ROOT / 'deploy/nginx/sunrise.conf.template'}:/etc/nginx/templates/sunrise.conf.template:ro",
            "-v", f"{certs}:/etc/sunrise-tls:ro",
            IMAGES["nginx"],
        )
    return name


def wait_for_proxy(client: Client, proxy: str) -> None:
    deadline = time.monotonic() + 60
    last = None
    while time.monotonic() < deadline:
        _, last = client.curl("/api/v1/health", max_time=5)
        if last.status == 200:
            return
        if docker("inspect", "-f", "{{.State.Running}}", proxy, check=False).stdout.strip() != "true":
            break
        time.sleep(1)
    raise Failure(f"{proxy} did not proxy /api/v1/health within 60 s: {last!r}")


# --- the checks ---------------------------------------------------------------


def check_limits_per_client(a: Client, b: Client) -> None:
    for client in (a, b):
        for n in range(META_LIMIT):
            _, r = client.curl("/api/v1/meta")
            expect(r.status == 200, f"{client.address}: meta #{n + 1} should pass, got {r!r}")
        _, r = client.curl("/api/v1/meta")
        expect(r.status == 429, f"{client.address}: meta #{META_LIMIT + 1} should be 429, got {r!r}")
        expect(r.json().get("code") == "RATE_LIMITED", f"the 429 must carry RATE_LIMITED: {r!r}")
        retry = r.headers.get("retry-after", "")
        expect(retry.isdigit() and int(retry) >= 1, f"the 429 must carry Retry-After: {r.headers}")
    # A forged hop must not buy a fresh bucket.
    for header in ({"X-Forwarded-For": "203.0.113.9"}, {"Forwarded": "for=203.0.113.9"}):
        _, r = a.curl("/api/v1/meta", headers=header)
        expect(r.status == 429, f"a forged {header} moved the client to another bucket: {r!r}")


def check_logged_by_client_network(relay: Relay) -> None:
    wanted = {"10.77.1.0/24", "10.77.2.0/24"}
    seen: set[str] = set()
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        seen = {r.get("client_net", "") for r in relay.records("srv.ratelimit.rejected")}
        if wanted <= seen:
            break
        time.sleep(0.5)
    expect(wanted <= seen, f"refusals must be logged by the client's network; saw {sorted(seen)}")
    expect("127.0.0.0/24" not in seen, "a refusal was logged under the proxy's address")
    text = relay.log_path.read_text()
    for address in ("10.77.1.10", "10.77.2.10"):
        expect(address not in text, f"the full client address {address} reached the log")


def check_event_stream(c: Client) -> None:
    _, meta = c.curl("/api/v1/meta")
    meta = meta.json()
    hello = {
        "client_app_v": "0.0.0-deploy-test",
        "client_platform": "deploy-test",
        "wire_proto_supported": meta["wire_proto_supported"],
        "doc_schema_min": meta["doc_schema_floor"],
        "doc_schema_max": meta["doc_schema_floor"] + 1000,
        "crypto_suite_supported": meta["crypto_suite_supported"],
        "capabilities": REQUIRED_CLIENT_BITS,
        "trace": "01J000000000000000000000000",
    }
    auth = {"Authorization": "Bearer deploy-test", "Content-Type": "application/json"}
    _, r = c.curl("/api/v1/sync/session", method="POST", headers=auth, data=json.dumps(hello))
    expect(r.status == 201, f"a sync session should open through the proxy: {r!r}")
    session = r.json()["session_id"]
    headers = {**auth, "X-Sunrise-Session": session}
    subscribe = {"streams": [{"stream_id": "11" * 16, "cursors": []}]}
    _, r = c.curl("/api/v1/sync/subscribe", method="POST", headers=headers, data=json.dumps(subscribe))
    expect(r.status == 204, f"subscribe should pass through the proxy: {r!r}")

    # Longer than the 15 s keep-alive, so a proxy timeout shorter than it would
    # cut the stream before the comment arrives.
    started = time.monotonic()
    code, r = c.curl(
        "/api/v1/sync/events",
        headers={"Authorization": "Bearer deploy-test", "X-Sunrise-Session": session},
        max_time=20,
        stream=True,
    )
    elapsed = time.monotonic() - started
    expect(r.status == 200, f"the event stream should open: {r!r}")
    expect("text/event-stream" in r.headers.get("content-type", ""), f"not an event stream: {r.headers}")
    # A buffering proxy holds a ~100-byte stream in its buffer until the
    # client gives up, and the client then sees nothing at all.
    expect("caught_up" in r.body, f"the first event never arrived, so the stream was buffered: {r.body!r}")
    expect(
        any(line.startswith(":") and "sunrise" in line for line in r.body.splitlines()),
        f"no keep-alive comment arrived in 20 s; the stream was cut or buffered: {r.body!r}",
    )
    expect(code == 28 and elapsed >= 19, f"the stream ended early (curl {code} after {elapsed:.1f} s)")


def check_oversize_body(c: Client) -> None:
    c.sh(f"head -c {OVERSIZE} /dev/zero > /tmp/oversize")
    _, r = c.curl(
        "/api/v1/devices",
        method="POST",
        headers={"Authorization": "Bearer deploy-test", "Content-Type": "application/json"},
        data_file="/tmp/oversize",
        max_time=30,
    )
    status = local_declared_oversize("/api/v1/devices")
    expect(status == 413, f"the relay itself should refuse the body with 413, got {status}")
    expect(r.status == status, f"the proxy should refuse with the relay's {status}, got {r!r}")
    expect(
        "problem+json" not in r.headers.get("content-type", ""),
        f"the 413 came from the relay, so the body was forwarded rather than refused: {r!r}",
    )


def check_metrics_withheld(c: Client) -> None:
    _, r = c.curl("/metrics")
    expect(r.status == 404, f"/metrics must not be served through the proxy: {r!r}")
    expect("sunrise_" not in r.body, "the proxy forwarded /metrics")
    status, body = local("/metrics")
    expect(status == 200 and "sunrise_build_info" in body, "the relay should serve /metrics on loopback")
    code, r = c.curl("", url=f"http://{GATEWAY}:8443/metrics", max_time=5)
    expect(r.status == 0 and code != 0, f"the relay's own port must not be reachable from outside: {r!r}")


CHECKS = [
    ("limits trip per client and ignore forged hops", lambda env: check_limits_per_client(env["a"], env["b"])),
    ("refusals are logged by the client's network", lambda env: check_logged_by_client_network(env["relay"])),
    ("the event stream is unbuffered and outlives the keep-alive", lambda env: check_event_stream(env["c"])),
    ("an oversize body is refused at the proxy", lambda env: check_oversize_body(env["c"])),
    ("/metrics is unreachable from outside", lambda env: check_metrics_withheld(env["c"])),
]


def run_proxy(kind: str, binary: pathlib.Path, clients: dict[str, Client]) -> bool:
    print(f"== {kind}")
    workdir = pathlib.Path(tempfile.mkdtemp(prefix=f"sunrise-deploy-{kind}-"))
    relay = Relay(binary, workdir)
    proxy = None
    ok = True
    try:
        relay.start()
        proxy = start_proxy(kind, workdir)
        wait_for_proxy(clients["c"], proxy)
        env = {**clients, "relay": relay}
        for name, check in CHECKS:
            try:
                check(env)
                print(f"   ok    {name}")
            except Failure as e:
                ok = False
                print(f"   FAIL  {name}: {e}")
    except Failure as e:
        ok = False
        print(f"   FAIL  setup: {e}")
    finally:
        if not ok:
            print(f"--- relay log ({kind})")
            print(relay.log_path.read_text() if relay.log_path.exists() else "(none)")
            if proxy:
                print(f"--- {proxy} log")
                logs = docker("logs", proxy, check=False)
                print(logs.stdout + logs.stderr)
        if proxy:
            docker("rm", "-f", proxy, check=False)
        relay.stop()
        shutil.rmtree(workdir, ignore_errors=True)
    return ok


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--server", required=True, type=pathlib.Path, help="a built sunrise-server")
    parser.add_argument("--proxy", choices=["caddy", "nginx", "all"], default="all")
    args = parser.parse_args()

    if not args.server.is_file():
        print(f"no server binary at {args.server}", file=sys.stderr)
        return 2
    if shutil.which("docker") is None:
        print("docker is not on PATH", file=sys.stderr)
        return 2

    for image in IMAGES.values():
        docker("pull", "-q", image)
    docker("network", "rm", NETWORK, check=False)
    docker("network", "create", "--subnet", SUBNET, "--gateway", GATEWAY, NETWORK)
    clients = {name: Client(name, address) for name, address in CLIENTS.items()}
    try:
        for client in clients.values():
            client.start()
        kinds = ["caddy", "nginx"] if args.proxy == "all" else [args.proxy]
        results = [run_proxy(kind, args.server.resolve(), clients) for kind in kinds]
    finally:
        for client in clients.values():
            client.stop()
        docker("network", "rm", NETWORK, check=False)
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
