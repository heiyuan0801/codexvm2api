#!/usr/bin/env python3
"""在 Linux Docker Engine 上验证两个真实 sidecar 的隔离、代理出口和重启持久性。
只使用随机测试资源与合成凭据，finally 清理本次创建的容器、网络和卷
运行前构建 codex-slot-sidecar:local，或通过 CPR_SLOT_IMAGE 指定镜像
"""
import io
import base64
import json
import os
import secrets
import subprocess
import tarfile
import time
import uuid

IMAGE = os.environ.get("CPR_SLOT_IMAGE", "codex-slot-sidecar:local")
PYTHON_IMAGE = os.environ.get("CPR_SLOT_TEST_PYTHON_IMAGE", "python:3.13-alpine")
PREFIX = "cpr-slot-verify-" + uuid.uuid4().hex[:12]
containers, networks, volumes = [], [], []


def docker(*args, data=None, check=True):
    result = subprocess.run(["docker", *args], input=data, capture_output=True, timeout=120)
    if check and result.returncode:
        # Docker 错误可能附带参数，不回显包含测试凭据的输入
        raise RuntimeError("Docker operation failed: " + args[0])
    return result.stdout


def upload(container, files):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w") as archive:
        for name, data in files.items():
            info = tarfile.TarInfo(name)
            info.mode = 0o600
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
    docker("cp", "-", container + ":/", data=stream.getvalue())


PROXY = b'''
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json, os
class Proxy(BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
        result = json.dumps({'exit': os.environ['EXIT_NAME'], 'path': self.path,
            'authorization': self.headers.get('Authorization'),
            'internal': self.headers.get('X-Cpr-Slot-Secret'),
            'proxyAuthenticated': self.headers.get('Proxy-Authorization') == open('/expected-auth').read(),
            'body': json.loads(body)}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(result)))
        self.end_headers()
        self.wfile.write(result)
ThreadingHTTPServer(('0.0.0.0', 3128), Proxy).serve_forever()
'''
CLIENT = r'''
import json, sys, urllib.request, urllib.error
request = json.load(sys.stdin)
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
body = json.dumps({'path': '/backend-api/codex/responses', 'headers': [
    ['authorization', list(b'Bearer synthetic-upstream')],
    ['x-cpr-slot-secret', list(request['token'].encode())]],
    'body': {'model': 'slot-test', 'stream': True, 'marker': request['marker']}}).encode()
headers = {'Authorization': 'Bearer ' + request['token'], 'Content-Type': 'application/json'}
try:
    response = opener.open(urllib.request.Request(request['url'], data=body, headers=headers), timeout=8)
except urllib.error.HTTPError as error:
    response = error
print(json.dumps({'status': response.status, 'slotError': response.headers.get('x-cpr-slot-error'), 'body': response.read().decode()}))
'''


def request(gateway, slot, token=None):
    payload = {"url": "http://" + slot["name"] + ":8090/internal/v1/forward",
               "token": token or slot["token"], "marker": slot["marker"]}
    return json.loads(docker("exec", "-i", gateway, "python", "-c", CLIENT,
                             data=json.dumps(payload).encode()))


def create_slot(slot):
    docker("create", "--name", slot["name"], "--hostname", slot["hostname"],
           "--network", slot["network"], "--mount", "type=volume,src=" + slot["volume"] + ",dst=/var/lib/cpr-slot",
           "--cap-drop", "ALL", "--security-opt", "no-new-privileges:true",
           "--memory", "512m", "--cpus", "1", "--pids-limit", "128",
           "-e", "HOME=/var/lib/cpr-slot/home", "-e", "CPR_SLOT_AUTH_FILE=/var/lib/cpr-slot/secrets/auth",
           "-e", "CPR_SLOT_PROXY_FILE=/var/lib/cpr-slot/secrets/proxy", "-e", "CPR_SLOT_UPSTREAM_ORIGIN=http://upstream.invalid",
           "--label", "io.codex-proxy-rs.test=" + PREFIX, IMAGE)
    upload(slot["name"], {"var/lib/cpr-slot/secrets/auth": slot["token"].encode(),
                         "var/lib/cpr-slot/secrets/proxy": slot["proxy_url"].encode(),
                         "var/lib/cpr-slot/identity/installation-id": slot["installation"].encode(),
                         "etc/machine-id": slot["machine"].encode()})
    docker("start", slot["name"])


def wait_ready(gateway, slot):
    for _ in range(30):
        try:
            result = request(gateway, slot)
            if result["status"] == 200:
                return result
        except (RuntimeError, ValueError):
            pass
        time.sleep(1)
    raise AssertionError("slot did not become ready")


def main():
    docker("image", "inspect", IMAGE)
    gateway = PREFIX + "-gateway"
    containers.append(gateway)
    docker("run", "-d", "--name", gateway, PYTHON_IMAGE,
           "python", "-c", "import time; time.sleep(900)")
    slots = []
    for index in (1, 2):
        name = PREFIX + "-" + str(index)
        slot = {"name": name, "hostname": name, "network": name + "-net", "volume": name + "-home",
                "proxy": name + "-proxy", "token": secrets.token_hex(32), "marker": uuid.uuid4().hex,
                "machine": uuid.uuid4().hex, "installation": str(uuid.uuid4())}
        slot["proxy_password"] = secrets.token_hex(16)
        slot["proxy_url"] = "http://user:" + slot["proxy_password"] + "@" + slot["proxy"] + ":3128"
        networks.append(slot["network"])
        volumes.append(slot["volume"])
        containers.extend([slot["name"], slot["proxy"]])
        docker("network", "create", slot["network"])
        docker("volume", "create", slot["volume"])
        docker("network", "connect", slot["network"], gateway)
        docker("create", "--name", slot["proxy"], "--network", slot["network"], "-e", "EXIT_NAME=" + str(index),
               PYTHON_IMAGE, "python", "/proxy.py")
        upload(slot["proxy"], {"proxy.py": PROXY, "expected-auth": b"Basic " + base64.b64encode(("user:" + slot["proxy_password"]).encode())})
        docker("start", slot["proxy"])
        # 预建持久化目录，侧车镜像保持最小运行时
        docker("run", "--rm", "--network", "none", "-v", slot["volume"] + ":/slot", PYTHON_IMAGE,
               "python", "-c", "import os; [os.makedirs('/slot/'+p, exist_ok=True) for p in ('home','secrets','identity')]")
        create_slot(slot)
        upload(slot["name"], {"var/lib/cpr-slot/home/persistence": slot["marker"].encode()})
        result = wait_ready(gateway, slot)
        body = json.loads(result["body"])
        assert body["exit"] == str(index)
        assert body["path"] == "http://upstream.invalid/backend-api/codex/responses"
        assert body["body"]["marker"] == slot["marker"]
        assert body["proxyAuthenticated"]
        assert body["authorization"] == "Bearer synthetic-upstream" and body["internal"] is None
        assert request(gateway, slot, "incorrect-token")["status"] == 401
        inspect = json.loads(docker("inspect", slot["name"]))[0]
        assert list(inspect["NetworkSettings"]["Networks"]) == [slot["network"]]
        assert not inspect["HostConfig"]["PortBindings"]
        assert inspect["Config"]["Hostname"] == slot["hostname"]
        assert any(m["Name"] == slot["volume"] for m in inspect["Mounts"])
        assert docker("exec", slot["name"], "cat", "/etc/machine-id").decode() == slot["machine"]
        assert docker("exec", slot["name"], "stat", "-c", "%a", "/var/lib/cpr-slot/secrets/auth").strip() == b"600"
        exposed = docker("inspect", slot["name"]) + docker("logs", slot["name"])
        assert all(slot[key].encode() not in exposed for key in ("token", "proxy_url", "proxy_password"))
        slots.append(slot)
    for key in ("name", "network", "volume", "hostname", "machine", "installation", "marker"):
        assert slots[0][key] != slots[1][key]
    # 重建容器复用身份与 HOME 卷，同时另一账号继续可用
    first = slots[0]
    docker("rm", "-f", first["name"])
    create_slot(first)
    wait_ready(gateway, first)
    assert docker("exec", first["name"], "cat", "/var/lib/cpr-slot/home/persistence").decode() == first["marker"]
    assert request(gateway, slots[1])["status"] == 200
    docker("stop", "-t", "1", first["proxy"])
    failure = request(gateway, first)
    assert failure["status"] == 502 and failure["slotError"] == "true"
    assert request(gateway, slots[1])["status"] == 200
    print(json.dumps({"result": "passed", "slots": 2, "checks": ["distinct resources and identities", "separate proxy exits", "bearer authentication", "no internal credential forwarding", "secret file permissions", "inspect and log redaction", "container recreation", "proxy failure isolation"]}))


if __name__ == "__main__":
    try:
        main()
    finally:
        for container in reversed(containers):
            docker("rm", "-f", container, check=False)
        for network in reversed(networks):
            docker("network", "rm", network, check=False)
        for volume in reversed(volumes):
            docker("volume", "rm", volume, check=False)
