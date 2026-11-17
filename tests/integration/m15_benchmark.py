"""Measure the real Core, PostgreSQL and dispatch on a bounded concurrent fixture."""

import argparse
import json
import math
import os
import statistics
import subprocess
import tempfile
import time
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
from pathlib import Path
from uuid import uuid4

from m07_leases import register_runner
from m07_queue import configure, create_template, enqueue
from workspace_fixture import workspace_fixture


def note(office, token, index):
    return office.tool(token, "office_upsert_note", title=f"Benchmark revision {index}",
                       body="Evidence from a concurrent writer.", idempotency_key=str(uuid4()),
                       change={"operation": "create", "visibility": {"kind": "workspace"}})


def prefix_events(office, scope, events):
    # Seed only the benchmark history. Timed requests below use the real MCP handlers.
    office.sql(f"""
    WITH counter AS (SELECT last_sequence FROM workspace_event_counters WHERE workspace_id='{office.workspace}'),
    seeded AS (
        INSERT INTO events(workspace_id, sequence, id, scope_id, event_type, payload, created_at)
        SELECT '{office.workspace}', counter.last_sequence + n, gen_random_uuid(), '{scope}',
               'note.updated', jsonb_build_object('note_id','{scope}','fixture_seq',n),
               clock_timestamp() - INTERVAL '2 hours'
        FROM counter CROSS JOIN generate_series(1,{events}) n
        RETURNING sequence
    )
    UPDATE workspace_event_counters SET last_sequence=(SELECT max(sequence) FROM seeded)
    WHERE workspace_id='{office.workspace}';
    """)


def health_resources(office):
    out = office.compose("exec", "-T", "server", "sh", "-ec",
                         "awk '/VmRSS/ {print $2}' /proc/1/status; ls -1 /proc/1/fd | wc -l").stdout.splitlines()
    assert len(out) == 2, out
    db_connections = int(office.sql("SELECT count(*) FROM pg_stat_activity WHERE usename='cocodeop' AND datname='cocodeop'"))
    return {"rss_kib": int(out[0]), "descriptors": int(out[1]), "postgres_connections": db_connections}


def elapsed(operation):
    start = time.perf_counter()
    result = operation()
    return (time.perf_counter() - start) * 1000, result


def describe(samples):
    ranked = sorted(samples)
    return {"samples": len(ranked), "p50_ms": round(statistics.median(ranked), 3),
            "p95_ms": round(ranked[math.ceil(.95 * len(ranked)) - 1], 3), "max_ms": round(ranked[-1], 3)}


def traffic(office, actors, seconds, rate, target_events):
    expected = seconds * rate
    reports = []
    def round_trip(n):
        token, identity = actors[n % len(actors)]
        mutation, created = elapsed(lambda: note(office, token, n))
        assert created.get("note_id"), created
        reading, context = elapsed(lambda: office.tool(token, "office_get_context", limit=24))
        assert context.get("actor", {}).get("agent_id") == identity["agent_id"], context
        assert len(json.dumps(context, separators=(",", ":")).encode("utf-8")) <= 32 * 1024
        return mutation, reading, created["note_id"]

    pending = set()
    resources = [health_resources(office)]
    first = time.monotonic()
    with ThreadPoolExecutor(max_workers=len(actors)) as pool:
        for i in range(expected):
            delay = first + i / rate - time.monotonic()
            if delay > 0:
                time.sleep(delay)
            if len(pending) >= len(actors):
                ready, pending = wait(pending, timeout=10, return_when=FIRST_COMPLETED)
                assert ready, "The benchmark request window stopped making progress"
                reports.extend(f.result() for f in ready)
            pending.add(pool.submit(round_trip, i))
            if i and i % (rate * 30) == 0:
                resources.append(health_resources(office))
        reports.extend(future.result() for future in pending)
    duration = time.monotonic() - first
    resources.append(health_resources(office))
    mutations = [mutation for mutation, _, _ in reports]
    contexts = [context for _, context, _ in reports]
    assert len({note_id for _, _, note_id in reports}) == expected
    count = int(office.sql(f"SELECT count(*) FROM notes WHERE workspace_id='{office.workspace}'"))
    assert count == expected + 1, count
    office.restart()
    assert int(office.sql(f"SELECT count(*) FROM notes WHERE workspace_id='{office.workspace}'")) == count
    for _, _, note_id in reports[-3:]:
        assert office.tool(actors[0][0], "office_get_note", note_id=note_id)["note_id"] == note_id
    summary = {"agents": len(actors), "seeded_events": target_events, "duration_seconds": round(duration, 3),
               "mutation_rate": round(expected / duration, 3), "mutations": describe(mutations),
               "contexts": describe(contexts), "resource_samples": resources, "notes_after_restart": count}
    assert summary["mutations"]["p95_ms"] <= 250 and summary["contexts"]["p95_ms"] <= 500, summary
    # Exercise both the descriptor bound and the configured connection budget.
    assert max(item["descriptors"] for item in resources) <= resources[0]["descriptors"] + 64, resources
    assert max(item["postgres_connections"] for item in resources) <= 32, resources
    return summary


def dispatch(office, actor, count):
    token, identity = actor
    workers = [office.agent(office.owner_a, f"Benchmark worker {i}", "worker")[1] for i in range(count)]
    runner, runner_token = register_runner(office, office.owner_a, "Benchmark runner")
    assert configure(office, office.owner_a, 500).get("budget")
    template = create_template(office, office.owner_a, "Benchmark runtime", units=10)
    office.api("/v1/runner-scheduler/profile", runner_token, {
        "capacity": count, "expected_revision": 0, "ttl_seconds": 120,
        "capabilities": ["linux"], "runtime_template_ids": [template],
    })
    lag = []
    acquired = set()
    for worker in workers:
        task = office.tool(token, "office_create_task", objective="Benchmark ready-to-lease latency",
                           acceptance_criteria=["Obtain the exact runner lease"], idempotency_key=str(uuid4()))["task"]
        ready = office.tool(token, "office_prepare_task", task_id=task["task_id"],
                            expected_revision=task["revision"], idempotency_key=str(uuid4()))["task"]
        begin = time.perf_counter()
        job = enqueue(office, token, ready, worker, template)["job"]
        claim_key = str(uuid4())
        lease = office.api("/v1/runner-scheduler/claim", runner_token, {"claim_key": claim_key})
        lag.append((time.perf_counter() - begin) * 1000)
        assert lease["run"]["lease"]["job_id"] == job["job_id"]
        assert lease["run"]["lease"]["runner_id"] == runner
        retry = office.api("/v1/runner-scheduler/claim", runner_token, {"claim_key": claim_key})
        assert retry["run"]["run_id"] == lease["run"]["run_id"]
        acquired.add(lease["run"]["run_id"])
    assert count == len(acquired), acquired
    measured = describe(lag)
    assert measured["p95_ms"] <= 2000, measured
    return {**measured, "duplicate_executions": count - len(acquired), "lead": identity["agent_id"]}


def verify(office, options):
    actors = [office.agent(office.owner_a, f"Benchmark participant {i}", "lead")
              for i in range(options.participants)]
    anchor = note(office, actors[0][0], "seed")
    prefix_events(office, anchor["note_id"], options.events)
    stress = traffic(office, actors, options.seconds, options.rate, options.events)
    delivery = dispatch(office, actors[0], 12)
    docker = json.loads(subprocess.check_output(["docker", "info", "--format", "{{json .}}"],
                                               text=True, encoding="utf-8", timeout=20))
    return {"schema_version": int(office.sql("SELECT max(version) FROM _sqlx_migrations")),
            "stand": {"docker_os": docker["OperatingSystem"], "docker_cpus": docker["NCPU"],
                      "docker_memory_bytes": docker["MemTotal"], "driver_os": os.name,
                      "postgres": office.sql("SHOW server_version")},
            "traffic": stress, "dispatch": delivery}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds", type=int, default=300)
    parser.add_argument("--participants", type=int, default=20)
    parser.add_argument("--rate", type=int, default=10)
    parser.add_argument("--events", type=int, default=100_000)
    parser.add_argument("--server-image", help="Use a locally imported Core image from a release bundle.")
    options = parser.parse_args()
    assert 1 <= options.seconds <= 7200 and 1 <= options.participants <= 64
    assert 1 <= options.rate <= 100 and 1 <= options.events <= 100_000
    with tempfile.TemporaryDirectory(prefix="cocodeop-benchmark-") as directory:
        with workspace_fixture(Path(directory), "benchmark", server_image=options.server_image) as office:
            print(json.dumps(verify(office, options)))
