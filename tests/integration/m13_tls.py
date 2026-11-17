"""Verify a packaged Linux CLI against a real TLS endpoint from an independent guest."""

import argparse
import json
import shutil
import ssl
import subprocess
import tarfile
import tempfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

from m05a_registry import RunnerHost
from publication_faults import publication_proxy
from workspace_fixture import ROOT, workspace_fixture


def verify(office, root, openssl, bundle, vm):
    ca_key, ca = root / "ca-key.pem", root / "temporary-ca.pem"
    key, certificate = root / "tls-key.pem", root / "tls.pem"
    request, extensions = root / "tls.csr", root / "leaf.cnf"
    extensions.write_text("basicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n"
                          "subjectAltName=IP:127.0.0.1\n", encoding="utf-8")
    # TCG guests can lag the Windows clock after a long qualification session.
    valid_from = (datetime.now(timezone.utc) - timedelta(hours=1)).strftime("%Y%m%d%H%M%SZ")

    def issue(*arguments):
        subprocess.run([str(openssl), *map(str, arguments)], check=True, capture_output=True, timeout=25)

    issue("req", "-x509", "-newkey", "rsa:2048", "-noenc", "-days", "1", "-not_before", valid_from,
          "-keyout", ca_key, "-out", ca, "-subj", "/CN=CoCodeOp test authority",
          "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign")
    issue("req", "-new", "-newkey", "rsa:2048", "-noenc", "-keyout", key, "-out", request,
          "-subj", "/CN=127.0.0.1")
    issue("x509", "-req", "-in", request, "-CA", ca, "-CAkey", ca_key, "-set_serial", "1",
          "-days", "1", "-not_before", valid_from, "-extfile", extensions, "-out", certificate)
    policy = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    policy.minimum_version = ssl.TLSVersion.TLSv1_2
    policy.load_cert_chain(certificate, key)

    cli = root / "office"
    with tarfile.open(bundle / "linux-amd64.tar.gz") as archive:
        with cli.open("xb") as output:
            shutil.copyfileobj(archive.extractfile("office"), output)
    cli.chmod(0o700)
    token, identity = office.agent(office.owner_a, "TLS-confirmed lead", "lead")
    host = RunnerHost(root, True, cli, vm_state=vm)
    try:
        host.setup({"lead": token})
        address = f"{host.root}/office"
        host.replace("ca.pem", ca.read_text(encoding="utf-8"))
        with publication_proxy(office.endpoint, tls=policy) as proxy:
            def verify_client(trusted):
                forward, endpoint = host.connection_for_runner(proxy.endpoint)
                base = ["doctor", "--url", f"https://{endpoint}", "--agent", identity["agent_id"],
                        "--token-file", f"{host.root}/lead.token", "--json"]
                trust = [f"SSL_CERT_FILE={host.root}/ca.pem"] if trusted else ["-u", "SSL_CERT_FILE"]
                return host.invoke(["env", *trust, address, *base], forward=forward, timeout=40)
            rejected = verify_client(False)
            assert rejected.returncode == 1, rejected
            checks = {entry["component"]: entry for entry in json.loads(rejected.stdout)["checks"]}
            assert checks["network"]["status"] == "failed", checks
            correct = verify_client(True)
            assert correct.returncode == 0, (correct.stdout, correct.stderr)
            live = {entry["component"]: entry for entry in json.loads(correct.stdout)["checks"]}
            assert live["network"]["status"] == "passed" and "verified HTTPS" in live["network"]["detail"]
            assert live["mcp_auth"]["status"] == "passed" and identity["agent_id"] in live["mcp_auth"]["detail"]
            return {"checks": ["untrusted_tls_certificate_rejected", "valid_tls_peer_negotiates_mcp_and_exact_agent"],
                    "guest": host.topology["machine_id_sha256"], "agent_id": identity["agent_id"],
                    "cli": host.invoke([address, "--version"], check=True).stdout.strip()}
    finally:
        host.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--openssl", type=Path, default=Path("openssl"))
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--vm", type=Path, default=ROOT / ".cocodeop-test/linux-vm")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="cocodeop-tls-") as directory:
        root = Path(directory)
        with workspace_fixture(root, "tls") as office:
            print(json.dumps(verify(office, root, args.openssl, args.bundle, args.vm)))
