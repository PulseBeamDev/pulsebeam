"""Release-binary startup, scoped admission, and enrolled peer restart on loopback."""

from contextlib import ExitStack, contextmanager
import hashlib
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import platform
import socket
import subprocess
import time


def reserve_ports():
    with ExitStack() as sockets:
        def reserve(family, kind, address, port=0):
            listener = sockets.enter_context(socket.socket(family, kind))
            listener.bind((address, port))
            return listener.getsockname()[1]

        nodes = []
        for _ in range(2):
            rtc = reserve(socket.AF_INET6, socket.SOCK_STREAM, "::")
            reserve(socket.AF_INET6, socket.SOCK_DGRAM, "::", rtc)
            nodes.append({
                "rtc": rtc,
                "http": reserve(socket.AF_INET, socket.SOCK_STREAM, "127.0.0.1"),
                "internal": reserve(socket.AF_INET, socket.SOCK_STREAM, "127.0.0.1"),
                "gossip": reserve(socket.AF_INET6, socket.SOCK_DGRAM, "::1"),
                "wan": reserve(socket.AF_INET6, socket.SOCK_DGRAM, "::1"),
                "control": reserve(socket.AF_INET6, socket.SOCK_STREAM, "::1"),
            })
        return nodes


def request(port, path, *, body=None, token=None, owner=None, deadline=None):
    timeout = 2 if deadline is None else min(2, deadline - time.monotonic())
    if timeout <= 0:
        raise TimeoutError("readiness deadline expired")
    conn = HTTPConnection("127.0.0.1", port, timeout=timeout)
    try:
        headers = {} if token is None else {"Authorization": "Bearer " + token}
        if owner is not None:
            headers.update({"PulseBeam-Owner": owner, "PulseBeam-Attempt": "1"})
        if body is not None:
            headers["Content-Type"] = "application/json"
            body = json.dumps(body)
        conn.request("GET" if body is None else "POST", path, body=body, headers=headers)
        with conn.getresponse() as response:
            return response.status, response.read()
    finally:
        conn.close()


def wait_for(node, check, message, *, startup=False):
    deadline = time.monotonic() + 12
    while node.poll() is None and time.monotonic() < deadline:
        try:
            ready = check(deadline) if startup else check()
            if ready and time.monotonic() < deadline and node.poll() is None:
                return
        except ConnectionError:
            pass
        except TimeoutError:
            if not startup:
                raise
        time.sleep(0.025)
    raise AssertionError(message)


def metric_values(report, metric):
    return [float(line.rsplit(" ", 1)[1]) for line in report.decode().splitlines()
            if line.startswith((metric + "{", metric + " "))]


def process_usage(node):
    proc = Path(f"/proc/{node.pid}")
    stat = (proc / "stat").read_text().rpartition(") ")[2].split()
    memory = {key: int(value.split()[0])
              for line in (proc / "status").read_text().splitlines()
              for key, _, value in [line.partition(":")]
              if key in {"VmRSS", "VmHWM"}}
    return {"cpu_seconds": (int(stat[11]) + int(stat[12])) / os.sysconf("SC_CLK_TCK"),
            "resident_kib": memory["VmRSS"], "lifetime_peak_resident_kib": memory["VmHWM"]}


def loopback_received_bytes():
    for line in Path("/proc/net/dev").read_text().splitlines():
        interface, _, counters = line.partition(":")
        if interface.strip() == "lo":
            return int(counters.split()[0])
    raise RuntimeError("the profile requires a loopback traffic counter")


def run_profile(command, clients, nodes, specs, fanout, seconds, env):
    before = [process_usage(node) for node in nodes]
    wire_before = loopback_received_bytes()
    start = time.monotonic()
    result = subprocess.run(command, input=json.dumps(clients), text=True, stdout=subprocess.PIPE,
                            env=env, timeout=45 + seconds)
    elapsed = time.monotonic() - start
    wire_bytes = loopback_received_bytes() - wire_before
    after = [process_usage(node) for node in nodes]
    sample = {"fanout": fanout, "wall_seconds": elapsed, "loopback_received_bytes": wire_bytes,
              "processes": [{"baseline_resident_kib": initial["resident_kib"],
                             **{**final, "cpu_seconds": final["cpu_seconds"] - initial["cpu_seconds"]}}
                            for initial, final in zip(before, after)], "exit": result.returncode}
    if result.stdout.strip():
        sample.update(json.loads(result.stdout))
        sample["viewers"].sort(key=lambda viewer: viewer["viewer"])
        first_decodes = sorted(viewer["first_decode_ms"] for viewer in sample["viewers"]
                               if "first_decode_ms" in viewer)
        sample["viewers_with_first_decode"] = len(first_decodes)
        sample["p95_first_decode_ms"] = (
            first_decodes[(95 * len(first_decodes) + 99) // 100 - 1] if first_decodes else None
        )
        sample["decode_gaps_over_350_ms"] = sum(viewer.get("gaps_over_350_ms", 0)
                                                for viewer in sample["viewers"])
        for node, ports in zip(nodes, specs):
            def participants_gone():
                status, report = request(ports["internal"], "/metrics")
                counts = metric_values(report, "pulsebeam_shard_participants_live")
                return status == 200 and counts and sum(counts) == 0

            wait_for(node, participants_gone, "publisher or viewer state did not return to baseline")
        sample["cleanup_seconds"] = time.monotonic() - start - elapsed
    print("release profile sample:", json.dumps(sample), flush=True)
    return sample


def smoke_enrolled_restart(binary, fixture_tool, media_tool, directory, env,
                           fanouts, profile_seconds, connection_limit):
    directory.mkdir()
    fixture = json.loads(subprocess.check_output(
        [fixture_tool, directory, *map(str, fanouts)], env=env, timeout=10,
    ))
    specs = reserve_ports()
    paths = [directory / f"node-{i}" for i in range(2)]
    policy = directory / "leap-seconds.list"
    # Pinned-peer admission does not require a synchronized discovery clock.
    policy.write_text("#@ 4500000000\n2272060800 10 # 1972 origin\n2287785600 11 # July 1972\n")

    def state(index, name):
        return json.loads((paths[index] / "state" / name).read_text())

    @contextmanager
    def serve(index):
        path, ports = paths[index], specs[index]
        log = path / "server.log"
        server_env = dict(env, RUST_LOG="pulsebeam=info" if profile_seconds else (
            "pulsebeam=info,pulsebeam::control::controller::regional_routing=debug,"
            "pulsebeam::link::wan_host=debug,pulsebeam::clock=debug"
        ))
        cap = (["--max-connections", str(connection_limit)]
               if index == 0 and connection_limit is not None else [])
        with log.open("wb") as output, subprocess.Popen([
            str(binary), "--config", str(path / "server.toml"), "--data-dir", str(path / "state"),
            "--shards", "1", "--rtc-port", str(ports["rtc"]), "--utc-policy", str(policy),
            "--http-listen", f"127.0.0.1:{ports['http']}",
            "--internal-listen", f"127.0.0.1:{ports['internal']}",
            *cap,
        ], cwd=path, env=server_env, stdout=output, stderr=subprocess.STDOUT) as node:
            try:
                wait_for(node, lambda deadline: (
                    request(ports["internal"], "/healthz", deadline=deadline)[0] == 200
                    and request(ports["http"], "/api/v1/native/admissions", deadline=deadline)[0] == 405
                ), "internal and signaling listeners did not become live", startup=True)
                yield node
            except Exception:
                print(log.read_text(errors="replace"))
                if node.poll() is None:
                    try:
                        _, metrics = request(ports["internal"], "/metrics")
                        print("\n".join(line for line in metrics.decode().splitlines()
                                        if line.startswith(("fabric_wan", "regional_", "fabric_rtp"))))
                    except OSError:
                        pass
                raise
            finally:
                if node.poll() is None:
                    node.terminate()
                    try:
                        node.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        node.kill()
                        node.wait()

    for index, (path, ports) in enumerate(zip(paths, specs)):
        (path / "key-registry.json").write_text(json.dumps(fixture["registry"]))
        (path / "server.toml").write_text(f'''[cluster]
        id = "release-contract"
        topology = "single_node"
        advertise_host = "::1"
        [cluster.gossip]
        listen = "[::1]:{ports['gossip']}"
        ''')
        with serve(index):
            assert request(ports["http"], "/api/v1/native/admissions")[0] == 405
            assert request(ports["http"], "/metrics")[0] == 404
            assert request(ports["internal"], "/metrics")[0] == 200
    ids = [state(index, "membership-state.json")["node_id"] for index in range(2)]
    roles = ["edge", "router"]
    members = "\n".join(
        f'''[[cluster.region_tree.regions.members]]
        node = "{ids[i]}"
        generation = 1
        role = "{roles[i]}"
        catalog = true
        boot_key = "{fixture['receipts'][i]}"
        ''' + (f'media_key = "{fixture["receipts"][i]}"\n' if i == 1 else "")
        for i in range(2)
    )
    configs = []
    for index, (path, ports) in enumerate(zip(paths, specs)):
        peer = 1 - index
        config = path / "server.toml"
        contents = f'''[cluster]
        id = "release-contract"
        topology = "multi_node"
        boot_admission = true
        protected_base_stream_bps = 2000000
        advertise_host = "::1"
        [cluster.gossip]
        listen = "[::1]:{ports['gossip']}"
        [cluster.region_tree]
        borders = []
        [[cluster.region_tree.regions]]
        name = "loopback"
        {members}
        [[cluster.adjacency]]
        listen = "[::1]:{ports['wan']}"
        local_node = "{ids[index]}"
        local_generation = 1
        peer_node = "{ids[peer]}"
        peer_generation = 1
        peer_endpoint = "[::1]:{specs[peer]['wan']}"
        authority_epoch = "01010101010101010101010101010101"
        receipt_key_file = {json.dumps(str(path / 'receipt.pkcs8'))}
        peer_receipt_key = "{fixture['receipts'][peer]}"
        protected_egress_bps = 8000000
        [cluster.adjacency.regional_control]
        region = "loopback"
        role = "{roles[index]}"
        peer_role = "{roles[peer]}"
        listen = "[::1]:{ports['control']}"
        endpoint = "[::1]:{specs[peer]['control']}"
        peer_name = "node-{peer}.smoke.test"
        certificate_pem = {json.dumps(str(path / 'tls.pem'))}
        private_key_pem = {json.dumps(str(path / 'tls-key.pem'))}
        peer_ca_pem = {json.dumps(str(directory / 'ca.pem'))}
        peer_certificate_der = {json.dumps(str(paths[peer] / 'tls.der'))}
        '''
        config.write_text(contents)
        configs.append(contents)
        subprocess.run([
            str(binary), "--config", str(config), "--data-dir", str(path / "state"),
            "--enroll-regional-boots",
        ], cwd=path, env=env, timeout=10, check=True)

    token = fixture["token"]
    def admission(room="release-roster", bearer=token):
        return request(specs[0]["http"], "/api/v1/native/admissions", body={"room": room}, token=bearer)

    def authorized():
        status, body = admission()
        assert status in {200, 503}, status
        if status == 200:
            assert isinstance(json.loads(body)["owner"], str)
        return status == 200

    def peer_ready():
        status, metrics = request(specs[0]["internal"], "/metrics")
        return status == 200 and b'regional_control_boot_slots{kind="direct",state="available"} 1' in metrics

    def peer_floor():
        [(observed, floor)] = state(0, "regional-generations.json")["floors"]
        assert observed == ids[1]
        return (floor["generation"], floor["source"]["direct"]["media_leaf"])

    with serve(0) as first:
        assert admission(bearer=None)[0] == 401
        assert admission()[0] == 503
        with serve(1):
            wait_for(first, authorized, "scoped admission did not become available")
            wait_for(first, peer_ready, "authenticated direct control not reported available")
            status, issued = admission()
            assert status == 200
            owner = json.loads(issued)["owner"]
            assert request(
                specs[0]["http"], "/api/v1/native", token=token, owner=owner,
                body={"profile": "pulsebeam.media.v1", "room": "out-of-scope", "offer": ""},
            )[0] == 403
            prior_generation, prior_pin = peer_floor()
            assert len(prior_pin) == 32
        # Use the exact same files and stable TLS credentials. Only the committed boot changes.
        with serve(1) as restarted:
            wait_for(first, lambda: peer_floor()[0] > prior_generation, "survivor did not admit the restarted boot")
            wait_for(first, authorized, "scoped admission did not recover")
            wait_for(first, peer_ready, "restarted direct control not reported available")
            generation, pin = peer_floor()
            assert generation == prior_generation + 1
            assert pin != prior_pin
            assert (paths[1] / "server.toml").read_text() == configs[1]
            command = [
                media_tool,
                f"http://127.0.0.1:{specs[1]['http']}",
                f"http://127.0.0.1:{specs[0]['http']}",
                f"http://127.0.0.1:{specs[0]['internal']}/metrics",
                str(profile_seconds),
            ]
            if not profile_seconds:
                [wave] = fixture["waves"]
                subprocess.run([*command, wave["publisher"]], input=json.dumps({"tokens": wave["tokens"]}),
                               text=True, env=env, timeout=45, check=True)
                return

            report = {
                "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                "host": {"kernel": platform.release(), "architecture": platform.machine(),
                         "available_logical_cpus": len(os.sched_getaffinity(0))},
                "topology": "two processes, one routed loopback path, clients on the same host",
                "workload": "one low-motion 160x96 I420 source at 10 FPS, native SDK encoding",
                "sample_seconds": profile_seconds,
                "destination_connection_limit": connection_limit,
                "samples": [],
            }
            output = Path(env["TEST_UNDECLARED_OUTPUTS_DIR"]) / "release-profile.json"
            print("release profile:", json.dumps(report), flush=True)
            for fanout, wave in zip(fanouts, fixture["waves"], strict=True):
                clients = {"tokens": wave["tokens"]}
                if fanout == connection_limit:
                    clients["capacity_probe_token"] = token
                sample = run_profile([*command, wave["publisher"]], clients,
                                     [first, restarted], specs, fanout, profile_seconds, env)
                report["samples"].append(sample)
                output.write_text(json.dumps(report, indent=2) + "\n")
                assert sample["exit"] == 0, f"clients failed at fanout {fanout}"
