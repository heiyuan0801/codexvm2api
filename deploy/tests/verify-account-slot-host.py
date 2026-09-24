#!/usr/bin/env python3
"""在隔离测试容器内运行真实 Host Docker adapter，只清理本轮随机实例资源"""
import importlib.util
import json
import os
from pathlib import Path
import uuid

spec = importlib.util.spec_from_file_location("slot_fixture", Path(__file__).with_name("verify-account-slots.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
docker = fixture.docker
ids = [str(uuid.uuid4()), str(uuid.uuid4())]
name = "cpr-host-verify-" + uuid.uuid4().hex[:12]
gateway_name = "cpr-host-gateway-" + uuid.uuid4().hex[:12]
image = os.environ.get("CPR_SLOT_IMAGE", "codex-slot-sidecar:local")
runner = os.environ.get("CPR_SLOT_HOST_TEST_IMAGE", "codex-slot-host-tests:local")
try:
    # Keep a separate bridge-network container as the gateway endpoint. A
    # host-network test runner cannot itself join user-defined slot bridges,
    # but it can still inspect and mutate the same Docker daemon resources.
    docker(
        "create",
        "--name",
        gateway_name,
        "--entrypoint",
        "/usr/bin/sleep",
        runner,
        "infinity",
    )
    docker("start", gateway_name)
    # CommandIptables and the listener bind must run in the Docker host's
    # network namespace. The test still uses the host daemon through the
    # mounted socket, so the bridge networks created by Bollard and the
    # iptables rules are observed in the same Linux namespace.
    docker(
        "create",
        "--name",
        name,
        "--network",
        "host",
        "--cap-add",
        "NET_ADMIN",
        "--cap-add",
        "NET_RAW",
        "-e",
        f"HOSTNAME={gateway_name}",
        "-v",
        "/var/run/docker.sock:/var/run/docker.sock",
        runner,
    )
    fixture.upload(name, {"fixture.json": json.dumps({"image": image, "instances": ids}).encode()})
    output = docker("start", "-a", name)
    result = json.loads(docker("inspect", name))[0]
    # 测试 panic 只包含合成夹具与脱敏类型，可用于定位真实 Engine 差异
    print(output.decode())
    assert result["State"]["ExitCode"] == 0, "Host Docker lifecycle verification failed"
finally:
    docker("rm", "-f", name, check=False)
    docker("rm", "-f", gateway_name, check=False)
    for instance in ids:
        stem = "cpr-slot-" + instance.replace("-", "")
        docker("rm", "-f", stem, check=False)
        docker("network", "rm", stem + "-net", check=False)
        docker("volume", "rm", stem + "-data", check=False)
