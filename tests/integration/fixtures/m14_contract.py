"""Exercise the agreed API and CLI on an isolated CoCodeOp source snapshot."""

import json
import select
import subprocess
import sys
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request, urlopen


root = Path(__file__).resolve().parents[2]
contract = json.loads((root / "contracts/api.json").read_text(encoding="utf-8"))
assert contract in (
    {"collection": "/todos", "title_type": "string", "errors": "nested"},
    {"collection": "/todos", "title_type": "string", "errors": "nested", "max_title_length": 120},
)
backend = Path(__file__).with_name("http_host.py")
client = root / "src/client/cli.py"
server = subprocess.Popen([sys.executable, "-B", backend, "0"], cwd=root, text=True, stdout=subprocess.PIPE)
component = "backend"
try:
    ready, _, _ = select.select([server.stdout], [], [], 7)
    assert ready, "The backend did not print its bound port"
    port = int(server.stdout.readline().strip())
    base = f"http://127.0.0.1:{port}"

    def api(path, data=None):
        request = Request(base + path, data=json.dumps(data).encode() if data is not None else None,
                          headers={"Content-Type": "application/json"})
        try:
            with urlopen(request, timeout=4) as response:
                return response.status, json.load(response)
        except HTTPError as response:
            with response:
                return response.code, json.load(response)

    def cli(*args):
        result = subprocess.run([sys.executable, "-B", client, "--url", base, *args],
                                cwd=root, capture_output=True, encoding="utf-8", timeout=8)
        assert result.returncode == 0, result.stderr
        return json.loads(result.stdout)

    initial = api("/todos")
    assert initial == (200, {"todos": []}), f"GET /todos returned {initial!r}; expected 200 and {{'todos': []}}"
    code, created = api("/todos", {"title": "one"})
    assert code == 201 and created == {"id": 1, "title": "one", "done": False}, "created_item_invalid"
    component = "client"
    assert cli("list") == {"todos": [created]}, "cli_list_response_invalid"
    second = cli("add", "два")
    assert second == {"id": 2, "title": "два", "done": False}, "cli_create_response_invalid"
    component = "backend"
    assert api("/todos") == (200, {"todos": [created, second]}), "collection_after_cli_create_invalid"
    assert api("/missing") == (404, {"error": {"code": "not_found"}}), "missing_route_error_invalid"
    assert api("/todos", {"title": "  "}) == (400, {"error": {"code": "invalid_title"}}), "blank_title_rejected_incorrectly"
    assert api("/todos", []) == (400, {"error": {"code": "invalid_title"}}), "non_object_rejected_incorrectly"
    assert api("/todos") == (200, {"todos": [created, second]}), "invalid_requests_changed_collection"
    if limit := contract.get("max_title_length"):
        assert api("/todos", {"title": "λ" * (limit + 1)}) == (
            400, {"error": {"code": "invalid_title"}},
        ), "title_limit_not_enforced"
        assert api("/todos") == (200, {"todos": [created, second]}), "overlong_title_changed_collection"
        code, item = api("/todos", {"title": "λ" * limit})
        assert code == 201 and item["title"] == "λ" * limit, "title_at_limit_invalid"
    print("HTTP and CLI contract passed")
except Exception as error:
    diagnostic = f"{type(error).__name__}: {error}"
    print("M14_CONTRACT_FAILURE " + json.dumps({"component": component, "diagnostic": diagnostic}),
          file=sys.stderr, flush=True)
    raise
finally:
    server.terminate()
    try:
        server.wait(timeout=4)
    except subprocess.TimeoutExpired:
        server.kill()
        server.wait(timeout=4)
