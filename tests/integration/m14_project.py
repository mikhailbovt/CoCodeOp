"""Run managed coding on one shared project from the independently owned guests."""

import json
import sys
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta, timezone
from urllib.parse import urlsplit
from uuid import uuid4

from m05a_claims import directory, file, hours, recipient
from m07_leases import register_runner
from m07_queue import configure
from m08a_gateway_read import rpc
from m08a_gateway_write import CALL_SOCKET
from m09_native_agents import get_attempt, install_backend, install_json
from m09_native_checkout import checkout_service, execute_scoped
from m10_activation import wait_for
from publication_faults import publication_proxy
from workspace_fixture import ROOT


BACKEND = "src/backend/server.py"
CLIENT = "src/client/cli.py"
CONTRACT = "contracts/api.json"
TEST = "tests/contract/smoke.py"
HTTP_HOST = "tests/contract/http_host.py"
SPEC = {"collection": "/todos", "title_type": "string", "errors": "nested"}
CONTRACT_BODY = json.dumps(SPEC, separators=(",", ":")) + "\n"
UPDATED_SPEC = {**SPEC, "max_title_length": 120}
MANIFEST = {"include": [directory(path) for path in ("src/backend", "src/client", "contracts", "tests/contract")]}


def expiry(hours_from_now):
    timestamp = datetime.now(timezone.utc) + timedelta(hours=hours_from_now + 1)
    return timestamp.strftime("%Y-%m-%dT%H:00:00Z")


def completed_results(record, name):
    return [call["result"] for call in record["calls"] if call["tool"] == name
            and call["status"] == "completed" and isinstance(call["result"], dict)
            and "code" not in call["result"]]


def tool_result(record, name):
    matches = completed_results(record, name)
    assert matches, (name, record)
    receipts = [{key: value for key, value in match.items() if key != "request_id"} for match in matches]
    assert all(receipt == receipts[0] for receipt in receipts), (name, matches)
    return matches[0]


def call_lead(lead, label, tool, arguments, instructions=""):
    params = dict(arguments, workspace_id=lead.identity["workspace_id"])
    name = f"office_{tool}"
    if tool == "office_create_task":
        instructions += " The objective is a string for your worker. Create the task and leave that work to the worker."
    prompts = (
        f"Call the MCP tool {name} now with exactly these arguments. {instructions}\n"
        f"{json.dumps(params, ensure_ascii=False, indent=2)}\nFinish after the tool returns.",
        f"Invoke {name} as an actual tool call. Do not print the arguments as your reply. {instructions}\n"
        f"Tool arguments: {json.dumps(params, ensure_ascii=False, separators=(',', ':'))}",
        f"Use this tool to complete the requested action: {name}. {instructions}\n"
        + "\n".join(f"Parameter {key} = {json.dumps(value, ensure_ascii=False)}" for key, value in params.items()),
    )
    for attempt, prompt in enumerate(prompts, 1):
        record = lead.turn(label if attempt == 1 else f"{label}_retry_{attempt - 1}", prompt, tool)
        results = completed_results(record, tool)
        if results:
            if tool == "office_create_task":
                # A model may repeat its request under a freshly generated key. Keep one
                # real draft and have its tech lead close any surplus drafts before assigning scopes.
                drafts = {result["task"]["task_id"]: result for result in results}
                selected = results[0]
                for task_id, receipt in drafts.items():
                    if task_id == selected["task"]["task_id"]:
                        continue
                    removed = call_lead(lead, "cancel_surplus_draft", "office_change_task", {
                        "task_id": task_id, "expected_revision": receipt["task"]["revision"],
                        "idempotency_key": str(uuid4()),
                        "change": {"operation": "cancel", "reason": "A separate draft already covers this request"},
                    })
                    assert removed["task"]["status"] == "cancelled", removed
                return selected
            return tool_result(record, tool)
    return tool_result(record, tool)


def negotiate(office, a, b):
    topic = f"todos-api-{uuid4().hex[:10]}"
    approvers = [recipient(lead.identity) for lead in (a, b)]
    proposed = call_lead(a, "propose_http_contract", "office_propose_decision", {
        "target": {"kind": "workspace", "topic": topic, "required_approvers": approvers},
        "content": {"title": "Task-list HTTP contract", "problem": "Agree on the endpoint and error envelope",
                    "options": [{"key": "nested", "description": "GET and POST /todos with nested JSON error.code."},
                                {"key": "plain", "description": "GET and POST /todos with plain text errors."}],
                    "recommendation": "nested"},
        "deadline": expiry(2), "idempotency_key": str(uuid4()),
    })
    proposal = proposed["proposal"]
    channel = office.tool(office.owner_a, "office_create_channel", name="Task-list contract discussion",
                          audience={"kind": "workspace"}, idempotency_key=str(uuid4()))
    question = ask_and_answer(office, channel["channel_id"], b, a,
                              "Can the CLI depend on JSON error.code in every error response?",
                              "Yes. The agreed nested shape is {\"error\":{\"code\":\"name\"}}.")
    target = {"proposal_id": proposal["proposal_id"], "expected_revision": proposal["revision"]}
    for lead, reason in ((b, "Client requires a stable nested error code"), (a, "Backend agrees with the error envelope")):
        call_lead(lead, "agree_on_contract_revision", "office_respond_proposal", {
            **target, "decision": "approve", "reason": reason, "idempotency_key": str(uuid4()),
        })
    decided = office.tool(b.token, "office_get_proposal", proposal_id=proposal["proposal_id"])
    assert decided["state"] == "accepted" and decided["revision"] == proposal["revision"], decided
    assert {value["responder"]["agent_id"] for value in decided["responses"]} == {a.identity["agent_id"], b.identity["agent_id"]}
    return decided, channel["channel_id"], question


def ask_and_answer(office, channel_id, asker, addressee, body, reply):
    to = recipient(addressee.identity)
    question = call_lead(asker, "ask_about_contract", "office_ask_question", {
        "channel_id": channel_id, "recipient_user_id": to["user_id"], "recipient_agent_id": to["agent_id"],
        "body": body, "idempotency_key": str(uuid4()), "expires_in_seconds": 1800,
    })
    answered = call_lead(addressee, "answer_contract_question", "office_answer_question", {
        "question_id": question["question_id"], "body": reply, "idempotency_key": str(uuid4()),
    })
    observed = office.tool(asker.token, "office_get_question", question_id=question["question_id"])
    assert observed["asker"] == recipient(asker.identity) and observed["recipient"] == to, observed
    assert observed["answer_id"] == answered["answer_id"], observed
    return observed


def verify_private_notes(office, leads):
    proofs, markers = [], []
    for lead, peer in (leads, leads[::-1]):
        marker = f"private{uuid4().hex}"
        saved = office.tool(lead.token, "office_upsert_note", idempotency_key=str(uuid4()),
                            title="Private owner test note", body=marker, change={"operation": "create"})
        assert saved["visibility"] == {"kind": "owner_private"}, saved
        query = office.tool(lead.token, "office_search", query=marker)
        assert query["hits"][0]["source"]["note_id"] == saved["note_id"], query
        assert office.tool(peer.token, "office_get_note", note_id=saved["note_id"])["code"] == "FORBIDDEN"
        assert office.tool(peer.token, "office_search", query=marker)["hits"] == []
        proofs.append({"note_id": saved["note_id"], "owner": saved["owner_user_id"], "peer_denied": True})
        markers.append(marker)
    return proofs, markers


def setup_checkout(office, hosts, leads, image):
    host = hosts[0]
    runner, token = register_runner(office, office.owner_a, "Owner A shared checkout")
    host.replace("checkout.token", token)
    host.invoke(["chmod", "600", f"{host.root}/checkout.token"], check=True)
    for path in MANIFEST["include"]:
        host.invoke(["mkdir", "-p", f"{host.root}/tree/code/{path['path']}"], check=True)
    for source in (BACKEND, CLIENT, CONTRACT):
        host.replace(f"tree/code/{source}", "")
    fixtures = ROOT / "tests/integration/fixtures"
    for source, destination in (("m14_contract.py", TEST), ("m14_http_host.py", HTTP_HOST)):
        host.replace(f"tree/code/{destination}", (fixtures / source).read_text(encoding="utf-8"))
    editable = ("tree/code", "tree/code/src", "tree/code/src/backend", "tree/code/src/client",
                "tree/code/contracts", f"tree/code/{BACKEND}", f"tree/code/{CLIENT}", f"tree/code/{CONTRACT}")
    host.invoke(["chmod", "go-w", "--", *(f"{host.root}/{path}" for path in editable)], check=True)
    project = office.tool(office.owner_a, "office_create_project", name="Shared task-list service",
                          visibility={"kind": "workspace"}, idempotency_key=str(uuid4()))["project"]
    checkout = office.tool(office.owner_a, "office_request_checkout", project_id=project["project_id"],
                           host_runner_id=runner, manifest=MANIFEST,
                           idempotency_key=str(uuid4()))["checkout"]
    enrolled = host.enroll(office.endpoint, checkout, "checkout", "tree/code")
    assert enrolled.returncode == 0, (enrolled.stdout, enrolled.stderr)
    checkout = json.loads(enrolled.stdout)
    checkout_id = checkout["checkout_id"]
    for lead in leads:
        view = office.tool(lead.token, "office_get_checkout", checkout_id=checkout_id)
        assert view["checkout_id"] == checkout_id and view["root_ref"] == checkout["root_ref"], view
    template = office.tool(office.owner_a, "office_create_command_template", checkout_id=checkout_id,
                           expected_revision=checkout["revision"], idempotency_key=str(uuid4()), template={
                               "name": "Verify agreed HTTP and CLI contract", "executable": "python3",
                               "arguments": [{"kind": "literal", "value": "-B"}, {"kind": "literal", "value": TEST}],
                               "source_inputs": MANIFEST["include"], "scratch_outputs": [], "timeout_seconds": 45,
                           })
    assert template.get("template_id"), template
    spec_path = install_json(host, "m14-command-template.json", template)
    sha = json.loads(host.invoke([host.installed, "checkout", "command-manifest", "--file", spec_path, "--json"],
                                 check=True).stdout)["spec_sha256"]
    commands = install_json(host, "m14-command-host.json", {"templates": [{
        "template_id": template["template_id"], "spec_sha256": sha, "image_id": image,
        "entrypoint": "/usr/local/bin/python3", "memory_mib": 128, "cpus": 1, "pids_limit": 64,
    }]})
    return checkout, template["template_id"], commands


def envelope(office, checkout, lead, paths, test_template):
    permissions = {"read": {"allow": MANIFEST["include"]}, "write": {"allow": [file(path) for path in paths]},
                   "write_operations": ["replace"], "command_template_ids": [test_template]}
    return office.tool(
        office.owner_a, "office_issue_checkout_grant", checkout_id=checkout["checkout_id"],
        expected_revision=checkout["revision"], idempotency_key=str(uuid4()), expires_at=hours(10),
        recipient=recipient(lead.identity), permissions=permissions,
    )["grant"]


def fixture_control(office, lead):
    def perform(_label, name, arguments, _instructions=""):
        result = office.tool(lead.token, name, **arguments)
        assert "code" not in result, (name, result)
        return result

    return perform


def prepare_task(lead, roster, checkout, grant, purpose, *, path=None, reads=(), commands=(), required=(),
                 control=None):
    perform = control or (lambda *args: call_lead(lead, *args))
    workspace = lead.identity["workspace_id"]
    team_id = roster["team"]["team_id"]
    worker = roster["worker"]["worker"]
    create = {
        "workspace_id": workspace, "team_id": team_id, "objective": purpose,
        "acceptance_criteria": ["Produce durable results through CoCodeOp and cite the evidence IDs"],
        "visibility": {"kind": "team", "team_id": team_id}, "execution": {"kind": "coding"},
        "review_policy": {"kind": "accountable"}, "grants": [], "parent_task_id": None,
        "idempotency_key": str(uuid4()),
    }
    scope = {
        "workspace_id": workspace, "checkout_id": checkout["checkout_id"], "grant_id": grant["grant_id"],
        "worker": worker, "expected_scope_revision": None, "expires_at": expiry(3), "idempotency_key": str(uuid4()),
        "permissions": {"read": {"allow": [file(entry) if isinstance(entry, str) else entry for entry in reads]},
                        "write": {"allow": [file(path)] if path else []},
                        "write_operations": ["replace"] if path else [], "command_template_ids": list(commands)},
        "required_read_paths": list(required),
    }
    draft = perform("create_draft", "office_create_task", create)["task"]
    assert draft["team_id"] == team_id and draft["status"] == "draft", draft
    scope_params = {**scope, "task_id": draft["task_id"], "expected_task_revision": draft["revision"]}
    stage = perform("set_worker_scope", "office_set_task_scope", scope_params,
                    "Keep read, write, write_operations and command_template_ids at the same level inside "
                    "permissions. The read and write objects contain only allow and deny. "
                    "Keep worker and permissions as objects.")
    definition = stage["scope"]["definition"]
    assert definition["task_id"] == draft["task_id"] and definition["worker"] == worker, stage
    assert definition.get("required_read_paths", []) == list(required), stage
    for mode in ("read", "write"):
        observed = {(entry["kind"], entry["path"]) for entry in definition["permissions"][mode]["allow"]}
        expected = {(entry["kind"], entry["path"]) for entry in scope["permissions"][mode]["allow"]}
        assert observed == expected, stage
    for field in ("write_operations", "command_template_ids"):
        assert definition["permissions"].get(field, []) == scope["permissions"][field], stage
    return draft, stage


def dispatch(office, lead, roster, task_id, revision, runtime, *, control=None):
    perform = control or (lambda *args: call_lead(lead, *args))
    def active():
        scope = office.tool(lead.token, "office_get_work_scope", task_id=task_id)
        return scope if scope.get("state") == "active" else None
    wait_for(active, 18)
    prepare = {"workspace_id": office.workspace, "task_id": task_id,
               "expected_revision": revision, "idempotency_key": str(uuid4())}
    launch = {"workspace_id": office.workspace, "task_id": task_id,
              "worker_agent_id": roster["worker"]["worker"]["agent_id"], "runtime_template_id": runtime,
              "idempotency_key": str(uuid4())}
    ready = perform("prepare_task", "office_prepare_task", prepare)["task"]
    used = perform("dispatch_worker", "office_queue_execution", {
        **launch, "expected_task_revision": ready["revision"],
    })
    job = used["job"]
    assert job["task_id"] == task_id and job["runtime_template_id"] == runtime, job
    return job


class Worker:
    def __init__(self, office, host, lead, backend, image, provider, model, trace):
        self.backend, self.host, self.image = backend, host, image
        owner = office.owner_a if host.owner == "owner-a" else office.owner_b
        self.runner, token = register_runner(office, owner, f"{host.owner} local {backend}")
        host.replace(f"{backend}.token", token)
        host.invoke(["chmod", "600", f"{host.root}/{backend}.token"], check=True)
        core_forward, self.service = host.connection_for_runner(trace)
        provider_forward, inference = host.connection_for_runner(urlsplit(provider).netloc)
        while self.service == inference:
            provider_forward, inference = host.connection_for_runner(urlsplit(provider).netloc)
        self.forwards = [core_forward, provider_forward]
        self.runtime, self.config, self.state = install_backend(
            office, host, owner, backend, image, inference, checkout=True, model=model, authenticated=False,
        )
        assert configure(office, owner, 270).get("budget")
        self.identity = lead.identity

    def execute(self, office, job, source_host):
        outcome = execute_scoped(self.backend, office, self.host, job, f"{source_host.root}/tree/code",
                                 self.service, self.forwards, self.config, self.state)
        run, evidence, events = get_attempt(self.host, self.state, outcome, require_success=False)
        assert run["lease"]["job_id"] == job["job_id"] and run["lease"]["runner_id"] == self.runner, run
        assert run["executor"]["user_id"] == self.identity["owner_user_id"], run
        assert run["executor"]["agent_id"] == job["worker_agent_id"], run
        return {"run": run, "usage": evidence["usage"], "session_id": evidence["session_id"],
                "host": self.host.topology["machine_id_sha256"], "image": self.image,
                "native_write_denied": True, "tools": [event for event in events if event.get("tool")]}


def tool_prefix(backend):
    return "mcp__office__office_" if backend == "codex" else "office_office_"


def coding_prompt(backend, relative, revision, requirements):
    prefix = tool_prefix(backend)
    contract_read, source_read, publish = (str(uuid4()) for _ in range(3))
    return (f"Implement {relative} with Python. {requirements} "
            f"Begin with {prefix}run_context, then use {prefix}read_file at scope revision 1. "
            f"For {CONTRACT} use idempotency_key {contract_read}. "
            f"For {relative} use {source_read}. After both reads call {prefix}replace_text_file. "
            f"Set path to {relative}, before_sha256 to the target file hash, content to the complete Python code, "
            f"scope_revision to 1, base_checkout_revision to {revision}, and idempotency_key to {publish}. "
            "Set dependency_path to the contract path and dependency_sha256 to its read hash. Follow any revised criteria "
            "and publication key from the review. After a successful publication report its change_id and finish. "
            "Use only the office tools named in this task. MCP resource browsing is unavailable.")


def verify_publication(office, checkout, run, path, base_revision, observed_calls, dependency=None):
    assert run["run"]["status"] == "succeeded", run
    run_id = run["run"]["run_id"]
    tool_names = {event["tool"] for event in run["tools"] if event.get("successful")}
    assert "office_read_file" in tool_names and tool_names & {"office_apply_changes", "office_replace_text_file"}, run
    lines = office.sql(
        "SELECT jsonb_build_object('id',id,'author',executor_user_id,'checkout_id',checkout_id,"
        "'event_sequence',event_sequence,'previous_revision',previous_revision,'scope_revision',scope_revision,"
        "'dependencies',read_dependencies,'outcome',outcome)::text "
        f"FROM checkout_publications WHERE run_id='{run_id}' AND state='applied'"
    ).splitlines()
    assert len(lines) == 1, lines
    publication = json.loads(lines[0])
    assert publication["checkout_id"] == checkout["checkout_id"] and publication["author"] == run["run"]["executor"]["user_id"]
    assert publication["id"] in run["run"]["summary"], publication
    matching = [call["base"] for call in observed_calls if call.get("change_id") == publication["id"]]
    assert matching and len(set(matching)) == 1, matching
    if base_revision is not None:
        assert matching[0] == base_revision, (matching, base_revision)
    assert 0 <= matching[0] <= publication["previous_revision"], publication
    publication["observed_base_revision"] = matching[0]
    assert [item["path"] for item in publication["outcome"]["files"]] == [path], publication
    if dependency:
        assert publication["dependencies"] == [{"path": CONTRACT, "sha256": dependency}], publication
    return publication


def capture_snapshot(office, checkout, lead, head):
    checkout_id = checkout["checkout_id"]
    created = call_lead(lead, "capture_sources", "office_create_checkout_snapshot", {
        "checkout_id": checkout_id, "expected_checkout_revision": head["checkout_revision"],
        "idempotency_key": str(uuid4()),
    })
    def sealed():
        state = office.tool(lead.token, "office_get_checkout_snapshot", checkout_id=checkout_id,
                            snapshot_id=created["snapshot_id"])
        return state if state.get("state") != "queued" else None
    snapshot = wait_for(sealed, 35)
    assert snapshot["state"] == "ready" and snapshot["proof"]["baseline"]["manifest_sha256"] == head["source_sha256"]
    return snapshot


def test_snapshot(office, checkout, verifier, roster, grant, worker, host, template, snapshot, code_path, calls):
    prefix = tool_prefix(worker.backend)
    required = {"office_read_file", "office_run_command"}
    control = fixture_control(office, verifier)
    failed_attempts = []
    interrupted_commands = []
    for _ in range(3):
        read = {"path": code_path, "scope_revision": 1, "idempotency_key": str(uuid4())}
        command = {"template_id": template, "scope_revision": 1, "source_snapshot_id": snapshot["snapshot_id"],
                   "arguments": {}, "idempotency_key": str(uuid4())}
        objective = f"""Read the other owner's already published file with {prefix}read_file:
{json.dumps(read)}
Then run the approved HTTP and CLI contract verifier on the immutable snapshot with {prefix}run_command:
{json.dumps(command)}
Retain the returned command_id. Call {prefix}get_command with the command_id and scope_revision 1 until its state is finished.
Keep this turn open while the command runs. Ending the turn early interrupts the command.
Report the real exit_code, the command_id and the file.sha256 from the read step once it finishes."""
        draft, stage = prepare_task(verifier, roster, checkout, grant, objective,
                                    reads=MANIFEST["include"], commands=[template], control=control)
        job = dispatch(office, verifier, roster, draft["task_id"], stage["task_revision"], worker.runtime, control=control)
        first_call = len(calls)
        run = worker.execute(office, job, host)
        tools = {evidence["tool"] for evidence in run["tools"] if evidence.get("successful")}
        if not required <= tools:
            failed_attempts.append(run)
            continue
        attempt = run["run"]["run_id"]

        def finished():
            return office.sql(
                "SELECT jsonb_build_object('id',id,'outcome',outcome,'snapshot',source_snapshot_id,'epoch',epoch)::text "
                f"FROM checkout_commands WHERE run_id='{attempt}' AND state='finished'"
            ).splitlines()

        report = wait_for(finished, 30)
        assert len(report) == 1, (report, run)
        measured = json.loads(report[0])
        assert measured["snapshot"] == snapshot["snapshot_id"], measured
        if measured["outcome"]["kind"] != "interrupted":
            break
        interrupted_commands.append(measured)
        failed_attempts.append(run)
    else:
        raise AssertionError(("No worker completed the snapshot check", failed_attempts, interrupted_commands))
    outcome = measured["outcome"]
    exit_code = outcome.get("exit_code")
    assert exit_code in (0, 1), measured
    failure = None
    if exit_code == 0:
        assert outcome["stdout"].strip() == "HTTP and CLI contract passed", measured
    else:
        marker = "M14_CONTRACT_FAILURE "
        failures = [json.loads(line.removeprefix(marker)) for line in outcome["stderr"].splitlines()
                    if line.startswith(marker)]
        assert len(failures) == 1, measured
        failure = failures[0]
        assert failure["component"] in ("backend", "client"), measured
    assert outcome["snapshot"]["sha256"] == snapshot["proof"]["baseline"]["manifest_sha256"], measured
    code_hash = host.invoke(["sha256sum", f"{host.root}/tree/code/{code_path}"], check=True).stdout.split()[0]
    reads = [call for call in calls[first_call:] if call.get("path") == code_path and call.get("sha256") == code_hash
             and call.get("checkout_id") == checkout["checkout_id"]]
    assert reads and all(call["host_runner_id"] == checkout["host_runner_id"] for call in reads), (reads, run)
    return {"command": measured, "worker": run, "cross_owner_read": reads[0], "prior_attempts": failed_attempts,
            "interrupted_commands": interrupted_commands, "failure": failure}


def publish_initial_contract(office, checkout, host, lead, roster, grant, worker, calls):
    prefix = tool_prefix(worker.backend)
    control = fixture_control(office, lead)
    failed_attempts = []
    for _ in range(3):
        objective = (
            f"Call {prefix}run_context. Use {prefix}read_file on {CONTRACT} with scope_revision 1 and key {uuid4()}. "
            f"Then use {prefix}replace_text_file with path {CONTRACT}, before_sha256 from the read, "
            f"scope_revision 1, base_checkout_revision 0, idempotency_key {uuid4()} and content {CONTRACT_BODY.strip()}. "
            "Use only the three registered office tools named here. MCP resource browsing is unavailable. "
            "After the replacement succeeds, report its returned change_id and finish the turn."
        )
        draft, stage = prepare_task(lead, roster, checkout, grant, objective, path=CONTRACT, reads=[CONTRACT], control=control)
        job = dispatch(office, lead, roster, draft["task_id"], stage["task_revision"], worker.runtime, control=control)
        run = worker.execute(office, job, host)
        run_id = run["run"]["run_id"]
        applied = int(office.sql(f"SELECT count(*) FROM checkout_publications WHERE run_id='{run_id}' AND state='applied'"))
        if applied:
            publication = verify_publication(office, checkout, run, CONTRACT, 0, calls)
            path = f"{host.root}/tree/code/{CONTRACT}"
            assert json.loads(host.invoke(["cat", path], check=True).stdout) == SPEC
            observed = host.invoke(["sha256sum", path], check=True).stdout.split()[0]
            assert publication["outcome"]["files"][0]["after_sha256"] == observed
            return run, publication, observed, failed_attempts
        failed_attempts.append(run)
    raise AssertionError(("The contract writer never published", failed_attempts))


def amend_contract(office, checkout, host, leads, roster, grant, worker, base, original_hash, calls):
    a, b = leads
    checkout_id = checkout["checkout_id"]
    current = office.tool(office.owner_a, "office_get_checkout", checkout_id=checkout_id)
    area = office.tool(office.owner_a, "office_define_area", checkout_id=checkout_id,
                       expected_checkout_revision=current["revision"], name="Joint HTTP contract",
                       paths=file(CONTRACT), policy={"kind": "shared", "approvers": [recipient(lead.identity) for lead in leads]},
                       idempotency_key=str(uuid4()))["area"]
    permissions = {"read": {"allow": [file(CONTRACT)]}, "write": {"allow": [file(CONTRACT)]},
                   "write_operations": ["replace"]}
    scope_request = {
        "workspace_id": office.workspace, "task_id": "TASK_ID", "expected_task_revision": "TASK_REVISION",
        "expected_scope_revision": 1, "permissions": permissions, "expires_at": expiry(1),
        "scope_expires_at": expiry(2), "reason": "Publish the agreed 120-character title limit",
        "idempotency_key": str(uuid4()),
    }
    reread_key, publish_key = str(uuid4()), str(uuid4())
    amended = json.dumps(UPDATED_SPEC, separators=(",", ":"))
    prefix = tool_prefix(worker.backend)
    objective = (
        f"Start with {prefix}run_context. If scope revision is 1, call {prefix}request_access with "
        f"{json.dumps(scope_request, separators=(',', ':'))}. "
        "Use task.task_id and task.task_revision from context for the two placeholders. "
        f"Use {prefix}get_access_request with workspace_id and access_request_id at most three times. "
        "If the scope is still pending, report the real access_request_id and finish. "
        f"With scope revision 2, read the contract using key {reread_key}. Replace it through {prefix}replace_text_file "
        f"with content {amended}, the hash in before_sha256, base checkout revision {base}, "
        f"scope_revision 2 and key {publish_key}. After one successful replacement, reply with its change_id and finish "
        "immediately. Use only the named office tools."
    )
    control = fixture_control(office, b)
    failed_attempts = []
    for _ in range(2):
        draft, stage = prepare_task(b, roster, checkout, grant, objective, reads=[CONTRACT], control=control)
        task_id = draft["task_id"]
        job = dispatch(office, b, roster, task_id, stage["task_revision"], worker.runtime, control=control)
        with ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(worker.execute, office, job, host)

            def opened():
                request_id = office.sql(f"SELECT id FROM access_requests WHERE task_id='{task_id}' AND state='requested'")
                if request_id:
                    return {"request": office.tool(a.token, "office_get_access_request", access_request_id=request_id)}
                return {"completed": future.result()} if future.done() else None

            observed = wait_for(opened, 180)
            if "completed" in observed:
                assert office.sql(f"SELECT count(*) FROM access_requests WHERE task_id='{task_id}'") == "0"
                assert office.sql(f"SELECT count(*) FROM checkout_publications WHERE task_id='{task_id}' AND state='applied'") == "0"
                failed_attempts.append(observed["completed"])
                continue
            request = observed["request"]
            denied = rejected_amendment_probe(worker, request, original_hash, base, amended)
            votes = approve_amendment(office, leads, host, request)
            run = future.result(timeout=425)
        break
    else:
        raise AssertionError(("Contract grant request ended before review", failed_attempts))
    approval = office.tool(b.token, "office_get_access_request", access_request_id=request["access_request_id"])
    application = run
    assert approval["state"] == "approved" and approval["run_id"] == application["run"]["run_id"]
    run_id = application["run"]["run_id"]
    applied = office.sql(f"SELECT count(*) FROM checkout_publications WHERE run_id='{run_id}' AND state='applied'")
    if applied == "0":
        assert application["run"]["status"] == "succeeded", application
        scope = office.tool(b.token, "office_get_work_scope", task_id=task_id, revision=2)
        assert scope["state"] == "active", scope
        task, _ = request_revision(office, b, application, "continue_approved_contract_change",
            "The requested scope is active. Publish the agreed contract as the next revision of this task.", [
                f"Scope revision 2 is active for AccessRequest {approval['access_request_id']}.",
                f"Read {CONTRACT} at scope_revision 2 with idempotency_key {uuid4()}.",
                f"Use {prefix}replace_text_file with scope_revision 2, base_checkout_revision {base}, "
                f"path {CONTRACT}, the actual before_sha256, content {amended}, and idempotency_key {uuid4()}.",
                "Report the change_id from the tool.",
            ])
        job = dispatch(office, b, roster, task_id, task["revision"], worker.runtime, control=control)
        run = worker.execute(office, job, host)
    publication = verify_publication(office, checkout, run, CONTRACT, None, calls)
    assert publication["scope_revision"] == 2
    contract_path = f"{host.root}/tree/code/{CONTRACT}"
    assert json.loads(host.invoke(["cat", contract_path], check=True).stdout) == UPDATED_SPEC
    observed = host.invoke(["sha256sum", contract_path], check=True).stdout.split()[0]
    delta = publication["outcome"]["files"][0]
    assert delta["before_sha256"] == original_hash
    assert delta["after_sha256"] == observed, delta
    return {"area": area, "rejections": [denied], "votes": votes, "approval": approval, "worker": run,
            "approval_attempt": application, "publication": publication, "sha256": observed, "prior_attempts": failed_attempts}


def rejected_amendment_probe(worker, request, original_hash, base, amended):
    payload = rpc([{"name": "office_replace_text_file", "arguments": {
        "scope_revision": 1, "base_checkout_revision": base, "idempotency_key": str(uuid4()),
        "path": CONTRACT, "before_sha256": original_hash, "content": amended,
    }}])
    run_path = f"{worker.state}/run-{request['run_id']}"
    reply = worker.host.invoke(["python3", "-c", CALL_SOCKET, run_path, worker.host.installed, payload], check=True)
    denied = json.loads(reply.stdout)["result"]["structuredContent"]
    assert denied["code"] in ("PATH_FORBIDDEN", "ACCESS_REQUEST_REQUIRED"), denied
    return denied


def approve_amendment(office, leads, host, request):
    assert {tuple(sorted(vote.items())) for vote in request["approvers"]} == {
        tuple(sorted(recipient(lead.identity).items())) for lead in leads
    }, request
    votes = []
    for lead in leads:
        state = votes[-1] if votes else request
        voted = office.tool(lead.token, "office_respond_access", access_request_id=request["access_request_id"],
                            expected_revision=state["revision"], decision="approve",
                            idempotency_key=str(uuid4()))["request"]
        votes.append(voted)
        if len(votes) == 1:
            assert voted["state"] == "requested" and voted["issued_scope_revision"] is None, voted
            assert json.loads(host.invoke(["cat", f"{host.root}/tree/code/{CONTRACT}"], check=True).stdout) == SPEC
    assert votes[1]["state"] == "approved_pending_apply" and votes[1]["issued_scope_revision"] == 2, votes
    return votes


def request_revision(office, lead, original, label, reason, criteria):
    task_id = original["run"]["task_id"]
    task = office.tool(lead.token, "office_get_task", task_id=task_id)["task"]
    submission = office.tool(lead.token, "office_get_task_result", task_id=task_id)
    refused = call_lead(lead, label, "office_review_task", {
        "task_id": task_id, "expected_revision": task["revision"], "run_id": original["run"]["run_id"],
        "decision": "request_changes", "binding": submission["binding"], "idempotency_key": str(uuid4()),
        "reason": reason,
    })["task"]
    assert refused["status"] == "changes_requested", refused
    updated = call_lead(lead, "add_review_criterion", "office_change_task", {
        "task_id": task_id, "expected_revision": refused["revision"], "idempotency_key": str(uuid4()),
        "change": {"operation": "revise_criteria", "expected_criteria_revision": task["acceptance_criteria_revision"],
                   "acceptance_criteria": criteria},
    })["task"]
    assert updated["acceptance_criteria_revision"] == task["acceptance_criteria_revision"] + 1, updated
    return updated, submission["binding"]["result_revision"]


def revise_contribution(office, checkout, lead, roster, worker, host, original, failed_check, head, calls, contract_hash):
    task_id = original["run"]["task_id"]
    revision = head["checkout_revision"]
    failure = failed_check["failure"]
    path = BACKEND if failure["component"] == "backend" else CLIENT
    diagnostic = " ".join(failure["diagnostic"].split())[:350]
    requirements = {
        BACKEND: "The HTTP fixture passes an already parsed Python value as body. Accept dict input directly. "
                 "GET /todos must return (200, {'todos': items}). Follow the task's HTTP contract and reject "
                 "titles above max_title_length with status 400 and "
                 "error.code invalid_title. Count Unicode characters. Accept a title of exactly that length.",
        CLIENT: "Preserve the CLI interface --url BASE list and --url BASE add TITLE. Print the received JSON value "
                "to stdout and preserve Unicode. Use the actual HTTP API shape in the current contract.",
    }
    updated, previous = request_revision(office, lead, original, "return_failed_contract_check",
        f"Command {failed_check['command']['id']} exited 1 on the reviewed snapshot. {diagnostic}", [
            f"Correct the failed check for {path}. Diagnostic: {diagnostic}. {requirements[path]}",
            f"Reread {CONTRACT} and {path} at scope_revision 1. Use the new hashes and base_checkout_revision {revision}. "
            f"Set dependency_path to {CONTRACT} and dependency_sha256 to its read hash when calling "
            f"office_replace_text_file. Use idempotency_key {uuid4()} and report the returned change_id.",
        ])
    job = dispatch(office, lead, roster, task_id, updated["revision"], worker.runtime)
    run = worker.execute(office, job, host)
    publication = verify_publication(office, checkout, run, path, None, calls, contract_hash)
    assert publication["previous_revision"] == revision, publication
    result = office.tool(lead.token, "office_get_task_result", task_id=task_id)
    old = office.tool(lead.token, "office_get_task_result", task_id=task_id, revision=previous)
    assert old["review"]["decision"] == "request_changes" and old["review"]["reviewer"] == recipient(lead.identity), old
    assert result["binding"]["result_revision"] == previous + 1
    assert result["binding"]["criteria_revision"] == updated["acceptance_criteria_revision"], result
    return {"failed_command_id": failed_check["command"]["id"], "previous": old, "revision": result,
            "worker": run, "publication": publication}


def review_and_accept(office, checkout, host, leads, snapshot, policy, tests, authored_runs, progress):
    approvals = []
    for author, run, check in authored_runs:
        binding = {"task_id": run["run"]["task_id"], "snapshot_id": snapshot["snapshot_id"],
                   "command_ids": [check["command"]["id"]]}
        reviewed = office.tool(author.token, "office_get_checkout_review", **binding)
        assert reviewed.get("content", {}).get("contributions"), reviewed
        durable = call_lead(author, "approve_reviewed_contribution", "office_approve_checkout_review", {
            **binding, "revision_sha256": reviewed["revision_sha256"],
            "reason": "Verified the frozen code and the other owner's successful contract command",
            "idempotency_key": str(uuid4()),
        })
        assert durable["review"] == reviewed and durable["reviewer"] == recipient(author.identity), durable
        task = office.tool(author.token, "office_get_task", task_id=binding["task_id"])["task"]
        assert task["status"] == "done", task
        approvals.append(durable)

    checkout_id = checkout["checkout_id"]
    requested = call_lead(leads[0], "accept_release_checkpoint", "office_request_checkout_release", {
        "checkout_id": checkout_id, "snapshot_id": snapshot["snapshot_id"],
        "expected_policy_revision": policy["revision"], "expected_commit_sha256": None,
        "acknowledge_external_baseline": True, "command_ids": [test["command"]["id"] for test in tests],
        "idempotency_key": str(uuid4()),
    })

    def resolved():
        release = office.tool(leads[0].token, "office_get_checkout_release", checkout_id=checkout_id,
                              release_id=requested["release_id"])
        return release if release.get("state") not in ("queued", "applying") else None

    release = wait_for(resolved, 28)
    contributions = sum(len(approval["review"]["content"]["contributions"]) for approval in approvals)
    assert release["state"] == "accepted" and release["approved_changes"] == contributions, release
    progress.update(approvals=approvals, release=release)
    assert release["snapshot_id"] == snapshot["snapshot_id"] and release["head"] == snapshot["head"], release
    proof = release["outcome"]
    assert proof["kind"] == "accepted", proof
    repo = ["git", f"--git-dir={host.root}/controller/checkpoints.git"]
    git_ref = host.invoke([*repo, "rev-parse", "refs/cocodeop/accepted"], check=True).stdout.strip()
    tree = host.invoke([*repo, "rev-parse", "refs/cocodeop/accepted^{tree}"], check=True).stdout.strip()
    assert git_ref == proof["commit_sha256"], (git_ref, proof)
    assert tree == proof["tree_sha256"] == snapshot["proof"]["git_tree_sha256"], (tree, proof, snapshot)
    progress.update(accepted_git_commit=git_ref, accepted_git_tree=tree)
    return {"release": release, "accepted_git_commit": git_ref}


def record_summaries(leads, channel, snapshot, release, authored, test_runs, attempts):
    summaries = []
    commands = [result["command"]["id"] for result in test_runs]
    workers = {attempt["run"]["run_id"]: attempt for attempt in attempts}
    for lead in leads:
        contributions = [item for item in authored if item["author"] == lead.identity["owner_user_id"]]
        change_ids = [item["id"] for item in contributions]
        usage = {"input_tokens": 0, "output_tokens": 0}
        for worker in workers.values():
            if worker["run"]["executor"]["user_id"] == lead.identity["owner_user_id"] and worker["usage"]:
                for name in usage:
                    usage[name] += worker["usage"][name]
        body = (
            f"Accepted task-list changes: {', '.join(change_ids)}. Snapshot {snapshot['snapshot_id']}. "
            f"Checkpoint {release['accepted_git_commit']}. Contract checks {', '.join(commands)} exited 0. "
            f"My worker runs reported {usage['input_tokens']} input and {usage['output_tokens']} output tokens. "
            "Open concerns for this fixture: none."
        )
        message = call_lead(lead, "publish_release_summary", "office_send_message", {
            "channel_id": channel, "body": body, "idempotency_key": str(uuid4()),
        })
        assert message["author"]["agent_id"] == lead.identity["agent_id"], message
        assert all(needed in message["body"] for needed in (release["accepted_git_commit"], *change_ids, *commands))
        summaries.append({"message": message, "worker_usage": usage})
    return summaries


def verify(office, hosts, leads, rosters, images, provider, model, progress):
    calls = []
    failures = []
    def observe(path, request, status, response):
        if path != "/run/mcp" or not request or not response:
            return
        frame = json.loads(request)
        params = frame.get("params", {})
        if frame.get("method") != "tools/call":
            return
        reply = json.loads(response)
        result = reply.get("result", {}).get("structuredContent") or reply.get("error", {})
        if params.get("name") == "office_read_file" and result.get("file"):
            receipt = {key: result[key] for key in ("read_id", "path", "checkout_id", "host_runner_id", "coordinator_epoch")}
            calls.append({**receipt, "sha256": result["file"]["sha256"]})
            return
        if params.get("name") not in ("office_apply_changes", "office_replace_text_file"):
            return
        change_id = result.get("change_id")
        if change_id:
            calls.append({"change_id": change_id, "base": params["arguments"]["base_checkout_revision"]})
        else:
            args = params.get("arguments", {})
            operations = args.get("operations", [{"kind": "replace", "path": args.get("path")}])
            failures.append({"status": status, "result": result or reply.get("result", {}).get("content", []),
                             "operations": [(op.get("kind"), op.get("path")) for op in operations if isinstance(op, dict)]
                             if isinstance(operations, list) else type(operations).__name__,
                             "revision": args.get("base_checkout_revision")})
    with publication_proxy(office.endpoint, observe=observe) as proxy:
        try:
            return verify_project(office, hosts, leads, rosters, images, provider, model, proxy.endpoint,
                                  calls, progress)
        except Exception:
            print(json.dumps({"publications": calls, "rejected_publications": failures})[:5000], file=sys.stderr)
            raise


def verify_project(office, hosts, leads, rosters, images, provider, model, trace, calls, progress):
    a, b = leads
    host = hosts[0]
    checkout, template, commands = setup_checkout(office, hosts, leads, images["command"])
    policy = call_lead(a, "bind_release_to_contract_check", "office_set_checkout_release_policy", {
        "checkout_id": checkout["checkout_id"], "expected_revision": 0, "required_template_ids": [template],
        "idempotency_key": str(uuid4()),
    })
    assert policy["required_template_ids"] == [template] and policy["revision"] == 1, policy
    left = envelope(office, checkout, a, [BACKEND, CONTRACT], template)
    right = envelope(office, checkout, b, [CLIENT, CONTRACT], template)
    grants = (left, right)
    workers = [Worker(office, location, lead, kind, images[kind], provider, model, trace)
               for location, lead, kind in zip(hosts, leads, ("codex", "opencode"))]
    private_notes, markers = verify_private_notes(office, leads)
    decision, channel_id, question = negotiate(office, a, b)
    progress.update(checkout_id=checkout["checkout_id"], release_policy=policy, grants=grants, decision=decision,
                    channel_id=channel_id, contract_question=question, private_notes=private_notes)
    progress["fixture_control"] = ["owner checkout registration and envelopes", "initial contract writer task setup",
                                   "read-only verification task setup",
                                   "contract amendment's initial read-only task setup", "preapproval write probe",
                                   "two access approval votes", "post-restart status inspection using each lead credential"]
    with checkout_service(host, office, checkout, commands_file=commands, lifetime_seconds=3600) as authority:
        progress["coordinator_epoch"] = authority["epoch"]
        contract_run, contract, hash_contract, contract_retries = publish_initial_contract(
            office, checkout, host, a, rosters[0], left, workers[0], calls)
        progress.update(contract=contract, contract_run=contract_run, contract_writer_retries=contract_retries)
        base = contract["outcome"]["head"]["checkout_revision"]
        goals = (
            "Define handle(method, path, body) and return a pair of integer status and JSON data. "
            "The test host has already parsed the request body into a Python dict, list, scalar or None. "
            "Use the dict directly; do not call json.loads. Implement the handler with an in-memory list. "
            "GET /todos returns exactly (200, {'todos': items}), where items is the list of stored items. "
            "POST /todos accepts a nonblank string title of any length in a dict and returns 201 and the new "
            "item with id (integers from 1), title and done (false). Preserve Unicode. For invalid input "
            "return 400 with nested error.code invalid_title. Other routes return 404 with nested error.code not_found.",
            "Use Python standard library. Command line: --url BASE list or --url BASE add TITLE. Use GET /todos "
            "for list and POST /todos with JSON containing title for add. Print the received JSON value to stdout. "
            "Preserve Unicode and exit 0 on success.",
        )

        def prepare(index, retry=False):
            lead, worker, roster = leads[index], workers[index], rosters[index]
            path = (BACKEND, CLIENT)[index]
            purpose = coding_prompt(worker.backend, path, base, goals[index])
            if retry:
                purpose += " The requested result is the one scoped file. Do not create helper files or tests."
            draft, stage = prepare_task(lead, roster, checkout, grants[index], purpose,
                                        path=path, reads=[path, CONTRACT], required=[CONTRACT])
            return dispatch(office, lead, roster, draft["task_id"], stage["task_revision"], worker.runtime)

        with ThreadPoolExecutor(max_workers=2) as pool:
            jobs = list(pool.map(prepare, range(2)))
            runs = list(pool.map(lambda item: item[0].execute(office, item[1], host), zip(workers, jobs)))
        failed_attempts = []
        for index in range(2):
            for _ in range(2):
                run = runs[index]
                run_id = run["run"]["run_id"]
                applied = int(office.sql(f"SELECT count(*) FROM checkout_publications WHERE run_id='{run_id}' AND state='applied'"))
                status = run["run"]["status"]
                if status == "succeeded" and applied == 1:
                    break
                assert not applied and status in ("succeeded", "failed"), run
                failed_attempts.append(run)
                print(f"Retrying {workers[index].backend} run {run_id} without a publication: {run['run']['summary']}",
                      file=sys.stderr, flush=True)
                if status == "succeeded":
                    lead, roster, worker = leads[index], rosters[index], workers[index]
                    task, _ = request_revision(office, lead, run, "return_missing_publication",
                        "The worker finished without publishing the requested file.", [
                            "Publish the complete source with office_replace_text_file. Put the target read hash "
                            "in before_sha256 and the whole text in content. Include dependency_path and dependency_sha256.",
                            f"Use base_checkout_revision {base} and idempotency_key {uuid4()}.",
                        ])
                    job = dispatch(office, lead, roster, task["task_id"], task["revision"], worker.runtime)
                else:
                    job = prepare(index, retry=True)
                runs[index] = workers[index].execute(office, job, host)
        publications = [verify_publication(office, checkout, run, path, base, calls, hash_contract)
                        for run, path in zip(runs, (BACKEND, CLIENT))]
        assert sorted(item["outcome"]["head"]["checkout_revision"] for item in publications) == [base + 1, base + 2], publications
        assert len({run["host"] for run in runs}) == 2
        head = office.tool(a.token, "office_get_checkout_head", checkout_id=checkout["checkout_id"])
        assert head["checkout_revision"] == base + 2, head
        progress.update(workers=runs, publications=publications, head=head, failed_attempts=failed_attempts)
        print("Verified two worker publications from the same base revision", file=sys.stderr, flush=True)
        question = ask_and_answer(office, channel_id, b, a, "Can the API declare a title length limit for the CLI?",
                                  "Yes, add max_title_length 120 measured in Unicode characters to the contract.")
        amendment = amend_contract(office, checkout, host, leads, rosters[1], right, workers[1],
                                   head["checkout_revision"], hash_contract, calls)
        progress.update(contract_amendment=amendment, amendment_question=question)
        amended_head = amendment["publication"]["outcome"]["head"]
        head = amended_head
        preliminary = capture_snapshot(office, checkout, a, head)
        rejected = test_snapshot(office, checkout, b, rosters[1], right, workers[1], host, template,
                                 preliminary, BACKEND, calls)
        assert rejected["failure"], rejected
        current_runs = runs.copy()
        corrections = []
        checked_snapshots = [{"snapshot": preliminary, "checks": [rejected]}]
        progress.update(preliminary_snapshot=preliminary, preliminary_check=rejected, corrections=corrections,
                        checked_snapshots=checked_snapshots)
        for _ in range(3):
            print(f"Command {rejected['command']['id']} found a {rejected['failure']['component']} contract error",
                  file=sys.stderr, flush=True)
            index = ("backend", "client").index(rejected["failure"]["component"])
            correction = revise_contribution(office, checkout, leads[index], rosters[index], workers[index], host,
                                             current_runs[index], rejected, head, calls, amendment["sha256"])
            current_runs[index] = correction["worker"]
            corrections.append(correction)
            head = correction["publication"]["outcome"]["head"]
            snapshot = capture_snapshot(office, checkout, a, head)
            tests = []
            checked_snapshots.append({"snapshot": snapshot, "checks": tests})
            for lead, roster, grant, worker, path in zip(leads, rosters, grants, workers, (CLIENT, BACKEND)):
                check = test_snapshot(office, checkout, lead, roster, grant, worker, host, template, snapshot, path, calls)
                tests.append(check)
                if check["failure"]:
                    rejected = check
                    break
            else:
                break
        else:
            raise AssertionError(("Both owners must pass the contract checks on the same snapshot", rejected))
        progress.update(head=head, snapshot=snapshot, tests=tests)
        release = review_and_accept(office, checkout, host, leads, snapshot, policy, tests, [
            (a, contract_run, tests[1]), (a, current_runs[0], tests[1]),
            (b, current_runs[1], tests[0]), (b, amendment["worker"], tests[0]),
        ], progress)
        published = [contract, *publications, amendment["publication"], *(change["publication"] for change in corrections)]
        attempts = [contract_run, *contract_retries, *runs, *failed_attempts, amendment["worker"],
                    amendment["approval_attempt"], *amendment["prior_attempts"], *(change["worker"] for change in corrections)]
        for checked in checked_snapshots:
            for check in checked["checks"]:
                attempts.extend([check["worker"], *check["prior_attempts"]])
        progress["summaries"] = record_summaries(leads, channel_id, snapshot, release,
                                                 published, tests, attempts)
    office.restart()
    with checkout_service(host, office, checkout, commands_file=commands) as reboot:
        assert reboot["epoch"] > authority["epoch"]
        progress["restart_epoch"] = reboot["epoch"]
        restored = office.tool(a.token, "office_get_checkout_head", checkout_id=checkout["checkout_id"])
        assert restored == head, (restored, head)
        observed = office.tool(a.token, "office_get_checkout_release", checkout_id=checkout["checkout_id"],
                               release_id=release["release"]["release_id"])
        assert observed == release["release"], observed
        counts = office.sql(
            "SELECT count(*) FROM checkout_publications "
            f"WHERE checkout_id='{checkout['checkout_id']}' AND state='applied'"
        )
        assert int(counts) == release["release"]["approved_changes"] == len(published), counts
        commit = host.invoke(["git", f"--git-dir={host.root}/controller/checkpoints.git",
                              "rev-parse", "refs/cocodeop/accepted"], check=True).stdout.strip()
        assert commit == release["accepted_git_commit"], commit
        expected_files = {item["path"]: item["after_sha256"] for change in published for item in change["outcome"]["files"]}
        for path, expected in expected_files.items():
            actual = host.invoke(["sha256sum", f"{host.root}/tree/code/{path}"], check=True).stdout.split()[0]
            assert actual == expected, (path, actual, expected)
        progress["files_after_restart"] = expected_files
        progress["accepted_git_commit_after_restart"] = commit
        for lead, summary in zip(leads, progress["summaries"]):
            saved = summary["message"]
            thread = office.tool(lead.token, "office_read_thread", channel_id=channel_id, thread_id=saved["thread_id"])
            assert thread["messages"] == [saved], thread
        progress["worker_status_after_restart"] = []
        for lead, roster, run in zip(leads, rosters, current_runs):
            profile = roster["worker"]["profile"]
            status = office.tool(lead.token, "office_worker_status",
                                 worker={"by": "name", "display_name": profile["display_name"]}, limit=20)
            assert status["worker"]["profile"] == profile, status
            matched = [entry["state"] for entry in status["tasks"]
                       if entry["state"]["task"]["task_id"] == run["run"]["task_id"]]
            assert len(matched) == 1 and matched[0]["task"]["status"] == "done", status
            assert run["run"]["run_id"] in {attempt["run_id"] for attempt in matched[0]["runs"]}, matched
            progress["worker_status_after_restart"].append(matched[0])
    serialized = json.dumps(progress)
    assert all(marker not in serialized for marker in markers)
    return progress
