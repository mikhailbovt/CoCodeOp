"""Exercise note visibility, immutable revisions and atomic event persistence on PostgreSQL."""

import hashlib
import json
import os
import secrets
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from uuid import uuid4

from m01 import ROOT, compose_runner, wait_for_status
from m02_bootstrap import call, request


def digest(token):
    return hashlib.sha256(token.encode()).hexdigest()


def run(tmp):
    secrets_dir = tmp / "secrets"
    subprocess.run([sys.executable, ROOT / "deploy/init-secrets.py", secrets_dir], check=True)
    operator = tmp / "operator"
    operator.mkdir(mode=0o700)
    env = dict(os.environ, COCODEOP_SECRETS_DIR=str(secrets_dir), COCODEOP_HTTP_PORT="0",
               COCODEOP_EVENT_RETAIN_COUNT="8")
    compose = compose_runner(f"cocodeop-notes-{uuid4().hex[:12]}", env)
    office = ROOT / "target" / "release" / ("office.exe" if os.name == "nt" else "office")
    checks = []

    def api(path, token, body=None):
        status, reply = request(endpoint, path, token=token, body=body)
        assert status == 200, (status, reply)
        return reply["result"]

    def tool(token, name, arguments):
        status, reply = call(endpoint, token, "tools/call", {"name": name, "arguments": arguments})
        assert status == 200, (status, reply)
        return reply

    def result(reply):
        return reply["result"]["structuredContent"]

    def sql(query):
        return compose("exec", "-T", "db", "psql", "-U", "postgres", "-d", "cocodeop",
                       "-XAt", "-v", "ON_ERROR_STOP=1", "-c", query).stdout.strip()

    def cli(token, *args):
        process = subprocess.run(
            [str(office), *args, "--url", f"http://{endpoint}", "--json"],
            env=dict(os.environ, OFFICE_TOKEN=token), check=True, capture_output=True,
            text=True, encoding="utf-8", timeout=10,
        )
        return json.loads(process.stdout)

    try:
        compose("up", "--build", "--wait", "--wait-timeout", "90", timeout=720)
        endpoint = compose("port", "server", "8400").stdout.strip()
        user_flag = ["--user", f"{os.getuid()}:{os.getgid()}"] if hasattr(os, "getuid") else []
        setup = compose(
            "run", "--rm", "--no-deps", "-T", *user_flag, "--entrypoint", "/usr/local/bin/office",
            "-v", f"{operator}:/operator", "server", "bootstrap", "--user", "Alice", "--workspace", "Notes",
            "--token-file", "/operator/alice.token", "--json",
        )
        alice = json.loads(setup.stdout)
        a = (operator / "alice.token").read_text().strip()
        workspace = alice["memberships"][0]["workspace_id"]
        invitation, b = secrets.token_hex(32), secrets.token_hex(32)
        api(f"/v1/workspaces/{workspace}/invitations", a, {
            "idempotency_key": str(uuid4()), "display_name": "Bob", "invitation_digest": digest(invitation),
        })
        bob = api("/v1/invitations/accept", b, {"idempotency_key": str(uuid4()), "invitation_token": invitation})

        def agent(token, name):
            key = secrets.token_hex(32)
            identity = api(f"/v1/workspaces/{workspace}/agents", token, {
                "idempotency_key": str(uuid4()), "display_name": name, "role": "lead",
                "credential_digest": digest(key),
            })
            return identity, key

        agent_a, lead_a = agent(a, "lead-a")
        agent_b, lead_b = agent(b, "lead-b")
        _, sibling_a = agent(a, "sibling-a")

        def create(token, title, body, change=None):
            payload = {"workspace_id": workspace, "idempotency_key": str(uuid4()), "title": title, "body": body,
                       "change": change or {"operation": "create"}}
            return payload, result(tool(token, "office_upsert_note", payload))

        def get(token, note, revision=None):
            return result(tool(token, "office_get_note", {
                "workspace_id": workspace, "note_id": note["note_id"], "revision": revision,
            }))

        def revise(token, note, body):
            return result(tool(token, "office_upsert_note", {
                "workspace_id": workspace, "idempotency_key": str(uuid4()), "title": note["title"], "body": body,
                "change": {"operation": "revise", "note_id": note["note_id"], "expected_revision": note["revision"]},
            }))

        def inbox(token, cursor=None, limit=None):
            return result(tool(token, "office_read_inbox", {
                "workspace_id": workspace, "cursor": cursor, "limit": limit,
            }))

        def ack(token, event):
            return result(tool(token, "office_ack_inbox", {
                "workspace_id": workspace, "event_id": event,
            }))

        initial_b = inbox(lead_b)
        assert initial_b["events"] == [] and not initial_b["has_more"]
        original, private = create(lead_a, "Private", "alpha-private-marker")
        assert private["author"] == {"kind": "agent", "agent_id": agent_a["agent_id"]}
        assert private["visibility"] == {"kind": "owner_private"}
        assert get(a, private) == private
        for rejected in (b, lead_b, sibling_a):
            assert get(rejected, private)["code"] == "FORBIDDEN"
        unauthorized_status, _ = request(endpoint, "/v1/notes/read", token=b, body={
            "workspace_id": workspace, "note_id": private["note_id"],
        })
        assert unauthorized_status == 403
        uri = f"office://workspace/{workspace}/notes/{private['note_id']}/revisions/1"
        assert "error" in call(endpoint, lead_b, "resources/read", {"uri": uri})[1]
        allowed_resource = call(endpoint, lead_a, "resources/read", {"uri": uri})[1]
        assert json.loads(allowed_resource["result"]["contents"][0]["text"]) == private
        forged = dict(original, owner_user_id=bob["user_id"])
        rejected = tool(lead_a, "office_upsert_note", forged)
        assert "error" in rejected or rejected.get("result", {}).get("isError") is True, rejected
        checks.extend(["owner_private_acl", "resource_and_http_acl", "derived_author_and_forgery_denial"])

        assert result(tool(lead_a, "office_upsert_note", original)) == private
        assert result(tool(lead_a, "office_upsert_note", dict(original, body="different")))["code"] == "IDEMPOTENCY_CONFLICT"
        cli_file = operator / "workspace-note.json"
        cli_file.write_text(json.dumps({
            "workspace_id": workspace, "idempotency_key": str(uuid4()), "title": "Public note", "body": "shared",
            "change": {"operation": "create", "visibility": {"kind": "workspace"}},
        }), encoding="utf-8")
        shared = cli(a, "notes", "upsert", "--file", str(cli_file))
        assert cli(b, "notes", "get", "--workspace", workspace, "--id", shared["note_id"], "--revision", "1") == shared
        assert revise(lead_b, shared, "denied")["code"] == "FORBIDDEN"
        checks.extend(["idempotent_replay", "idempotency_payload_conflict", "workspace_note_and_cli"])

        _, reader_note = create(lead_a, "Reader", "reader marker", {
            "operation": "create", "visibility": {"kind": "explicit_acl"},
            "grants": [{"user_id": bob["user_id"], "right": "read"}],
        })
        assert get(b, reader_note) == reader_note
        assert get(lead_b, reader_note)["code"] == "FORBIDDEN"
        assert revise(b, reader_note, "denied")["code"] == "FORBIDDEN"
        _, writer_note = create(lead_a, "Writer", "revision one", {
            "operation": "create", "visibility": {"kind": "explicit_acl"},
            "grants": [{"user_id": bob["user_id"], "agent_id": agent_b["agent_id"], "right": "write"}],
        })
        assert get(b, writer_note)["code"] == "FORBIDDEN"
        with ThreadPoolExecutor(max_workers=2) as pool:
            left = pool.submit(revise, lead_a, writer_note, "revision from Alice")
            right = pool.submit(revise, lead_b, writer_note, "revision from Bob")
            outcomes = [left.result(), right.result()]
        successes = [item for item in outcomes if item.get("revision") == 2]
        failures = [item for item in outcomes if item.get("code") == "REVISION_CONFLICT"]
        assert len(successes) == len(failures) == 1, outcomes
        assert get(lead_b, writer_note, 1) == writer_note
        assert get(lead_b, writer_note) == successes[0]
        checks.extend(["explicit_subject_and_read_grant", "explicit_edit_grant", "optimistic_race_and_history"])

        team = str(uuid4())
        sql(f"INSERT INTO teams (id, workspace_id, owner_user_id, lead_agent_id, name) VALUES "
            f"('{team}', '{workspace}', '{alice['user_id']}', '{agent_a['agent_id']}', 'Integration'); "
            f"INSERT INTO team_memberships (workspace_id, team_id, user_id, agent_id, role) VALUES "
            f"('{workspace}', '{team}', '{alice['user_id']}', '{agent_a['agent_id']}', 'lead'), "
            f"('{workspace}', '{team}', '{bob['user_id']}', '{agent_b['agent_id']}', 'reviewer')")
        team_change = {"operation": "create", "visibility": {"kind": "team", "team_id": team}}
        _, team_note = create(lead_a, "Team", "team marker", team_change)
        assert get(lead_b, team_note) == team_note
        assert get(b, team_note)["code"] == "FORBIDDEN"
        assert create(sibling_a, "Intruder", "denied", team_change)[1]["code"] == "FORBIDDEN"
        checks.append("team_scope_bound_to_member")

        bad_change = {"operation": "create", "visibility": {"kind": "explicit_acl"},
                      "grants": [{"user_id": str(uuid4()), "right": "read"}]}
        assert create(lead_a, "Bad grant", "rolled back", bad_change)[1]["code"] == "INVALID_INPUT"
        assert sql("SELECT (SELECT count(*) FROM notes), (SELECT count(*) FROM note_revisions), "
                   "(SELECT count(*) FROM events), (SELECT count(*) FROM outbox), "
                   "(SELECT count(*) FROM idempotency_records), (SELECT count(*) FROM data_scopes), "
                   "(SELECT last_sequence FROM workspace_event_counters)") == "5|6|6|6|6|5|6"
        assert sql("SET ROLE cocodeop; SELECT (SELECT count(*) FROM notes), (SELECT count(*) FROM events), "
                   "(SELECT count(*) FROM outbox), (SELECT count(*) FROM idempotency_records)").splitlines()[-1] == "0|0|0|0"
        checks.extend(["transactional_note_event_outbox_and_idempotency", "unscoped_sql_read_denied"])

        first = inbox(lead_b, initial_b["next_cursor"], 1)
        assert first["has_more"] and len(first["events"]) == 1
        delivery = first["events"][0]
        assert delivery["payload"]["note_id"] == shared["note_id"]
        assert delivery["acknowledged_at"] is None and "sequence" not in delivery
        receipt = ack(lead_b, delivery["event_id"])
        assert ack(lead_b, delivery["event_id"]) == receipt
        repeat = inbox(lead_b, initial_b["next_cursor"], 1)
        assert repeat["events"][0]["acknowledged_at"] == receipt["acknowledged_at"]
        assert repeat["events"][0]["event_id"] == delivery["event_id"]
        a_events = inbox(lead_a)["events"]
        a_private = next(e for e in a_events if e["payload"]["note_id"] == private["note_id"])
        assert ack(lead_b, a_private["event_id"])["code"] == "FORBIDDEN"
        assert next(e for e in a_events if e["event_id"] == delivery["event_id"])["acknowledged_at"] is None
        assert inbox(b, first["next_cursor"])["code"] == "CURSOR_EXPIRED"
        rest = inbox(lead_b, first["next_cursor"])
        assert not rest["has_more"]
        assert [e["payload"]["note_id"] for e in rest["events"]] == [writer_note["note_id"]] * 2 + [team_note["note_id"]]
        checks.extend(["bounded_delivery_filters_private_scopes", "idempotent_actor_scoped_ack", "cursor_bound_to_exact_actor"])

        public = {"operation": "create", "visibility": {"kind": "workspace"}}
        _, missed = create(lead_a, "While disconnected", "event after last read", public)
        compose("restart", "server", timeout=45)
        endpoint = compose("port", "server", "8400").stdout.strip()
        wait_for_status(endpoint, "/readyz", 200)
        assert get(lead_b, writer_note) == successes[0]
        assert result(tool(lead_a, "office_upsert_note", original)) == private
        assert sql("SELECT count(*) FROM outbox") == "7"
        resumed = inbox(lead_b, rest["next_cursor"])
        assert [event["payload"]["note_id"] for event in resumed["events"]] == [missed["note_id"]]
        checks.extend(["restart_preserves_data_and_replay", "offline_delivery_survives_restart"])

        sql(
            "CREATE FUNCTION m03_hold_first_writer() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN "
            f"IF NEW.payload->>'note_id' = '{missed['note_id']}' THEN PERFORM pg_sleep(1.5); END IF; "
            "RETURN NEW; END $$; CREATE TRIGGER m03_wait BEFORE INSERT ON events "
            "FOR EACH ROW EXECUTE FUNCTION m03_hold_first_writer()"
        )
        with ThreadPoolExecutor(max_workers=3) as pool:
            first_writer = pool.submit(revise, lead_a, missed, "first writer pauses inside PostgreSQL")
            until = time.monotonic() + 2
            while sql("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE wait_event = 'PgSleep')") != "t":
                assert not first_writer.done(), first_writer.result()
                assert time.monotonic() < until, "the first transaction did not reach the commit-order control point"
                time.sleep(0.03)
            second_writer = pool.submit(create, lead_b, "Second writer", "competes while first is uncommitted", public)
            observed = inbox(b, None, 100)
            assert len(observed["events"]) == 3
            assert first_writer.result()["revision"] == 2
            second_note = second_writer.result()[1]
        final = inbox(lead_b, resumed["next_cursor"])
        assert [e["payload"]["note_id"] for e in final["events"]] == [missed["note_id"], second_note["note_id"]]
        checks.append("cursor_never_skips_contended_commit")

        expired = inbox(lead_b, initial_b["next_cursor"])
        assert expired["code"] == "CURSOR_EXPIRED" and "without a cursor" in expired["message"]
        fresh = inbox(lead_b, None)
        assert [e["payload"]["note_id"] for e in fresh["events"]][-2:] == [missed["note_id"], second_note["note_id"]]
        assert ack(lead_a, a_private["event_id"])["code"] == "FORBIDDEN"
        final_cursor = final["next_cursor"]
        sql(f"UPDATE inbox_cursors SET expires_at = transaction_timestamp() - interval '1 second' WHERE id = '{final_cursor}'")
        assert inbox(lead_b, final_cursor)["code"] == "CURSOR_EXPIRED"
        viewed = cli(b, "events", "tail", "--workspace", workspace, "--limit", "1")
        assert len(viewed["events"]) == 1 and "next_cursor" in viewed
        assert cli(b, "inbox", "ack", "--workspace", workspace, "--event", delivery["event_id"])["event_id"] == delivery["event_id"]
        assert sql("SET ROLE cocodeop; SELECT (SELECT count(*) FROM inbox_cursors), "
                   "(SELECT count(*) FROM inbox_receipts)").splitlines()[-1] == "0|0"
        checks.extend(["retention_expiry_and_resync", "cursor_idle_ttl", "inbox_cli_and_row_isolation"])

        def channel(token, title, audience):
            payload = {"workspace_id": workspace, "idempotency_key": str(uuid4()), "name": title, "audience": audience}
            return payload, result(tool(token, "office_create_channel", payload))

        def post(token, room, body, **options):
            payload = {"workspace_id": workspace, "idempotency_key": str(uuid4()),
                       "channel_id": room["channel_id"], "body": body, **options}
            return payload, result(tool(token, "office_send_message", payload))

        def thread(token, room, root=None, **options):
            return result(tool(token, "office_read_thread", {
                "workspace_id": workspace, "channel_id": room["channel_id"], "thread_id": root, **options,
            }))

        def search(token, query, limit=None):
            return result(tool(token, "office_search", {"workspace_id": workspace, "query": query, "limit": limit}))

        channel_request, workspace_room = channel(lead_a, "Engineering", {"kind": "workspace"})
        assert result(tool(lead_a, "office_create_channel", channel_request)) == workspace_room
        _, team_room = channel(lead_a, "Team channel", {"kind": "team", "team_id": team})
        assert channel(sibling_a, "Prohibited channel", {"kind": "team", "team_id": team})[1]["code"] == "FORBIDDEN"
        recipient = {"user_id": bob["user_id"], "agent_id": agent_b["agent_id"]}
        _, direct_room = channel(lead_a, "Direct", {"kind": "direct", "recipient": recipient})
        visible = result(tool(lead_b, "office_list_channels", {"workspace_id": workspace}))
        assert {room["channel_id"] for room in visible["channels"]} == {
            workspace_room["channel_id"], team_room["channel_id"], direct_room["channel_id"],
        }
        human_channels = cli(b, "channels", "list", "--workspace", workspace)
        assert human_channels["channels"] == [workspace_room]
        checks.extend(["scoped_channel_catalog", "idempotent_channel_creation"])

        new_head = inbox(lead_b)["next_cursor"]
        human_head = inbox(b)["next_cursor"]
        private_link = {"note_id": private["note_id"], "revision": 1}
        visible_link = {"note_id": shared["note_id"], "revision": 1}
        request_body, question = post(lead_a, workspace_room, "Cooperation question for the API",
                                      mentions=[recipient], notes=[private_link, visible_link])
        assert question["author"] == {"kind": "agent", "agent_id": agent_a["agent_id"]}
        assert question["message_id"] == question["thread_id"]
        assert result(tool(lead_a, "office_send_message", request_body)) == question
        assert result(tool(lead_a, "office_send_message", dict(request_body, body="different")))["code"] == "IDEMPOTENCY_CONFLICT"
        assert [entry["event_type"] for entry in inbox(lead_b, new_head)["events"]] == ["message.posted", "message.mentioned"]
        assert [entry["event_type"] for entry in inbox(b, human_head)["events"]] == ["message.posted"]
        assert thread(lead_b, workspace_room, question["thread_id"])["messages"][0]["notes"] == [visible_link]
        assert thread(lead_a, workspace_room, question["thread_id"])["messages"][0]["notes"] == [private_link, visible_link]
        checks.extend(["addressed_mention_delivery", "note_link_acl", "message_replay_without_duplicate_mentions"])

        _, answer = post(lead_b, workspace_room, "Cooperation reply with the answer",
                         reply_to=question["message_id"])
        assert answer["thread_id"] == question["thread_id"] and answer["reply_to"] == question["message_id"]
        assert post(lead_b, team_room, "other channel", reply_to=question["message_id"])[1]["code"] == "FORBIDDEN"
        assert post(lead_b, team_room, "Teamshare details")[1]["author"]["agent_id"] == agent_b["agent_id"]
        assert post(b, team_room, "prohibited")[1]["code"] == "FORBIDDEN"
        assert post(lead_b, workspace_room, "link inaccessible", notes=[private_link])[1]["code"] == "FORBIDDEN"
        _, private_message = post(lead_b, direct_room, "Whisperneedle is the private topic")
        assert thread(b, direct_room, private_message["thread_id"])["code"] == "FORBIDDEN"
        assert thread(lead_a, direct_room, private_message["thread_id"])["messages"] == [private_message]
        direct_uri = f"office://workspace/{workspace}/threads/{private_message['thread_id']}"
        assert "error" in call(endpoint, sibling_a, "resources/read", {"uri": direct_uri})[1]
        resource = call(endpoint, lead_a, "resources/read", {"uri": direct_uri})[1]["result"]
        assert json.loads(resource["contents"][0]["text"])["messages"] == [private_message]
        checks.extend(["thread_reply_and_scope", "team_post_requires_exact_membership", "direct_message_resource_acl"])

        first_message = thread(lead_a, workspace_room, question["thread_id"], limit=1)
        assert first_message["messages"] == [question] and first_message["next_after"] == question["message_id"]
        second_message = thread(lead_a, workspace_room, question["thread_id"], after=first_message["next_after"])
        assert second_message["messages"] == [answer] and second_message["next_after"] is None
        search_hits = search(lead_a, "cooperation", 1)
        assert len(search_hits["hits"]) == 1 and search_hits["truncated"]
        assert search(b, "whisperneedle") == {"hits": [], "truncated": False}
        assert search(lead_b, "whisperneedle")["hits"][0]["source"]["message_id"] == private_message["message_id"]
        assert search(lead_b, "alpha-private-marker") == {"hits": [], "truncated": False}
        assert search(lead_a, "alpha-private-marker")["hits"][0]["source"]["note_id"] == private["note_id"]
        assert cli(b, "search", "cooperation", "--workspace", workspace)["hits"]
        checks.extend(["bounded_thread_pagination", "fts_acl_before_results_and_counts"])

        compose("restart", "server", timeout=45)
        endpoint = compose("port", "server", "8400").stdout.strip()
        wait_for_status(endpoint, "/readyz", 200)
        restarted_thread = cli(b, "messages", "thread", "--workspace", workspace,
                               "--channel", workspace_room["channel_id"], "--thread", question["thread_id"])
        assert [entry["message_id"] for entry in restarted_thread["messages"]] == [question["message_id"], answer["message_id"]]
        assert sql("SELECT (SELECT count(*) FROM messages), (SELECT count(*) FROM message_mentions), "
                   "(SELECT count(*) FROM events WHERE event_type = 'message.mentioned')") == "4|1|1"
        checks.append("message_thread_persists_after_restart")
        return checks
    finally:
        compose("down", "--volumes", "--rmi", "local", "--remove-orphans", check=False, timeout=90)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="cocodeop-notes-") as root:
        print(json.dumps({"checks": run(Path(root))}))
