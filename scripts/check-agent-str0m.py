#!/usr/bin/env python3

import json
import subprocess
import sys
from collections import deque


def fail(message):
    sys.exit(message)


metadata = json.loads(
    subprocess.check_output(
        ["cargo", "metadata", "--format-version=1"],
        text=True,
    )
)
packages = {package["id"]: package for package in metadata["packages"]}
nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}


def package_named(name):
    matches = [package for package in metadata["packages"] if package["name"] == name]
    if len(matches) != 1:
        fail(f"expected one {name} package, found {len(matches)}")
    return matches[0]


def dependency_kind(dep):
    kinds = dep.get("dep_kinds", [])
    if not kinds:
        fail(f"dependency edge {dep['name']} has no dependency kind")
    return kinds


def reachable(root_id):
    queue = deque([(root_id, False)])
    seen = set()
    edges = []
    while queue:
        package_id, is_dev = queue.popleft()
        state = (package_id, is_dev)
        if state in seen:
            continue
        seen.add(state)
        node = nodes.get(package_id)
        if node is None:
            fail(f"metadata is missing resolve node {package_id}")
        for dependency in node["deps"]:
            kinds = dependency_kind(dependency)
            for kind in kinds:
                next_is_dev = is_dev or kind["kind"] == "dev"
                edges.append((package_id, dependency, next_is_dev))
                queue.append((dependency["pkg"], next_is_dev))
    return edges


def source(package):
    return package["source"] or ""


def is_registry_str0m(package):
    return (
        package["name"] == "str0m"
        and package["version"] == "0.23.1"
        and source(package) == "registry+https://github.com/rust-lang/crates.io-index"
    )


def is_fork_str0m(package):
    return (
        package["name"] == "str0m"
        and package["version"] == "0.23.1"
        and source(package).startswith(
            "git+https://github.com/PulseBeamDev/str0m.git?branch=patch/0.23.1#"
        )
    )


def direct_dependencies(package_id):
    node = nodes.get(package_id)
    if node is None:
        fail(f"metadata is missing resolve node {package_id}")
    return node["deps"]


def direct_dependency(package_id, name, kind):
    matches = []
    for dependency in direct_dependencies(package_id):
        if dependency["name"] != name:
            continue
        if any(dep_kind["kind"] == kind for dep_kind in dependency_kind(dependency)):
            matches.append(dependency)
    if len(matches) != 1:
        fail(f"expected one {kind} dependency {name}, found {len(matches)}")
    return packages[matches[0]["pkg"]]


agent = package_named("pulsebeam-agent")
rtc = package_named("pulsebeam-rtc")

agent_str0m = direct_dependency(agent["id"], "str0m", None)
if not is_registry_str0m(agent_str0m):
    fail(
        "pulsebeam-agent must use crates.io str0m 0.23.1, got "
        f"{agent_str0m['version']} from {source(agent_str0m) or 'path dependency'}"
    )

rtc_str0m = direct_dependency(rtc["id"], "str0m", None)
if not is_fork_str0m(rtc_str0m):
    fail(
        "pulsebeam-rtc production str0m must use PulseBeamDev/str0m "
        f"patch/0.23.1, got {rtc_str0m['version']} from "
        f"{source(rtc_str0m) or 'path dependency'}"
    )

rtc_is = direct_dependency(rtc["id"], "is", None)
if not source(rtc_is).startswith(
    "git+https://github.com/PulseBeamDev/str0m.git?branch=patch/0.23.1#"
):
    fail(f"pulsebeam-rtc production is must use the patch/0.23.1 fork, got {source(rtc_is)}")

rtc_dcsctp = direct_dependency(rtc["id"], "dcsctp", None)
if rtc_dcsctp["version"] != "0.1.14":
    fail(f"pulsebeam-rtc must resolve dcsctp 0.1.14, got {rtc_dcsctp['version']}")

upstream = direct_dependency(rtc["id"], "str0m_upstream", "dev")
if not is_registry_str0m(upstream):
    fail(
        "pulsebeam-rtc's independent upstream str0m reference must use crates.io "
        f"0.23.1, got {upstream['version']} from {source(upstream)}"
    )

if agent_str0m["id"] == rtc_str0m["id"] or upstream["id"] == rtc_str0m["id"]:
    fail("Cargo did not resolve distinct registry and PulseBeamDev str0m packages")

for _, dependency, _ in reachable(agent["id"]):
    package = packages[dependency["pkg"]]
    if package["name"] == "str0m" and not is_registry_str0m(package):
        fail(
            "pulsebeam-agent has a str0m dependency outside crates.io "
            f"0.23.1: {package['version']} from {source(package)}"
        )

for _, dependency, is_dev in reachable(rtc["id"]):
    package = packages[dependency["pkg"]]
    if package["name"] != "str0m" or is_dev:
        continue
    if not is_fork_str0m(package):
        fail(
            "pulsebeam-rtc has a production str0m dependency outside the "
            f"PulseBeamDev fork: {package['version']} from {source(package)}"
        )

print(
    "dependency boundary verified: agent uses crates.io str0m 0.23.1; "
    "rtc production uses the patch/0.23.1 fork and dev reference uses crates.io"
)
