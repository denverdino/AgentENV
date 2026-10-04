"""Raw smoke measurements, not an M4 performance-qualification engine."""
import json
import math
from pathlib import Path
import time


def summarize(samples: list[dict], conditions: dict) -> dict:
    groups = {}
    for sample in samples:
        key = sample["backend"] + "/" + sample["operation"]
        groups.setdefault(key, []).append(sample)
    results = {}
    for key, rows in groups.items():
        values = sorted(row["seconds"] for row in rows if row["ok"])
        def percentile(fraction, minimum):
            return values[math.ceil(len(values) * fraction) - 1] if len(values) >= minimum else None
        results[key] = {"count": len(rows), "successes": len(values),
                        "failure_rate": (len(rows) - len(values)) / len(rows),
                        "p50_seconds": percentile(0.5, 1), "p95_seconds": percentile(0.95, 20),
                        "p99_seconds": percentile(0.99, 100)}
    kernels = conditions.get("kernel_sha256", {})
    comparable = bool(kernels.get("firecracker")) and kernels.get("firecracker") == kernels.get("dragonball")
    return {"groups": results, "samples": samples, "conditions": conditions,
            "conditions_comparable": comparable, "performance_qualified": False,
            "limitations": ["single_host_smoke_only", "incomplete_capacity_matrix",
                            "unfrozen_capture_and_peak_slo", "capture_pause_not_measured",
                            "object_store_read_upload_bytes_not_measured", "shared_host_page_cache"]}


def resources(node):
    result = {"memory_current_bytes": int((node.cgroup / "memory.current").read_text()),
              "memory_peak_bytes": int((node.cgroup / "memory.peak").read_text()),
              "cpu_stat": dict(line.split() for line in (node.cgroup / "cpu.stat").read_text().splitlines()),
              "io_stat": (node.cgroup / "io.stat").read_text() if (node.cgroup / "io.stat").exists() else None,
              "rss_kib": 0, "pss_kib": 0}
    for pid in (node.cgroup / "cgroup.procs").read_text().split():
        try:
            for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines():
                for name, key in (("Rss:", "rss_kib"), ("Pss:", "pss_kib")):
                    if line.startswith(name):
                        result[key] += int(line.split()[1])
        except (FileNotFoundError, ProcessLookupError):
            continue
    return result


def measure(node, backend, operation, samples, action):
    sample = {"backend": backend, "operation": operation, "ok": False,
              "resources_before": resources(node)}
    started = time.monotonic()
    try:
        value = action()
        sample["ok"] = True
        return value
    except Exception as error:
        sample["error"] = str(error)
        raise
    finally:
        sample["seconds"] = time.monotonic() - started
        sample["resources_after"] = resources(node)
        samples.append(sample)
        with (node.lab.root / "samples.jsonl").open("a") as output:
            output.write(json.dumps(sample) + "\n")
