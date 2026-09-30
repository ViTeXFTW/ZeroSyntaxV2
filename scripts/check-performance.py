#!/usr/bin/env python3
"""Compare Criterion and real-server performance results.

Exit 0 for pass/warnings, 1 for a performance failure, and 2 for bad input.
"""

import argparse
import json
import math
import pathlib
import sys
import tempfile


REQUIRED_SERVER_METRICS = (
    "workspace_ready",
    "open_to_diagnostics",
    "idle_completion_median",
    "idle_completion_p95",
    "full_semantic_tokens",
    "single_edit_to_completion",
    "four_edit_burst_to_completion",
    "eight_edit_burst_to_completion",
    "sixteen_edit_burst_to_completion",
    "sixteen_change_bulk_notification",
    "latest_diagnostics_after_editing",
)

ABSOLUTE_CEILINGS = {
    "workspace_ready": (3_000, 5_000),
    "open_to_diagnostics": (1_000, 2_000),
    "idle_completion_p95": (50, 100),
    "full_semantic_tokens": (500, 1_000),
    "single_edit_to_completion": (50, 100),
    "eight_edit_burst_to_completion": (250, 500),
    "sixteen_edit_burst_to_completion": (500, 1_000),
    "sixteen_change_bulk_notification": (500, 1_000),
    "latest_diagnostics_after_editing": (1_000, 2_000),
}

LEVEL = {"info": 0, "pass": 0, "warn": 1, "fail": 2}


class DataError(Exception):
    pass


def load_json(path):
    try:
        with pathlib.Path(path).open(encoding="utf-8") as source:
            return json.load(source)
    except (OSError, json.JSONDecodeError) as error:
        raise DataError(f"cannot read {path}: {error}") from error


def number(value, description, *, positive=False):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise DataError(f"{description} must be a number")
    value = float(value)
    if not math.isfinite(value):
        raise DataError(f"{description} must be finite")
    if positive and value <= 0:
        raise DataError(f"{description} must be positive")
    return value


def estimate(path, relative=False):
    data = load_json(path)
    try:
        mean = data["mean"]
        point = number(mean["point_estimate"], f"{path} mean.point_estimate")
        interval = mean["confidence_interval"]
        lower = number(interval["lower_bound"], f"{path} lower_bound")
        upper = number(interval["upper_bound"], f"{path} upper_bound")
    except (KeyError, TypeError) as error:
        raise DataError(f"malformed Criterion estimate {path}") from error
    if not relative and point <= 0:
        raise DataError(f"{path} mean.point_estimate must be positive")
    return point, lower, upper


def baseline_estimate(benchmark):
    preferred = (benchmark / "pr-base" / "estimates.json", benchmark / "base" / "estimates.json")
    for path in preferred:
        if path.is_file():
            return path
    candidates = [
        path
        for path in benchmark.glob("*/estimates.json")
        if path.parent.name not in {"new", "change", "report"}
    ]
    return candidates[0] if len(candidates) == 1 else None


def worse(current, candidate):
    return candidate if LEVEL[candidate] > LEVEL[current] else current


def criterion_rows(root):
    root = pathlib.Path(root)
    if not root.is_dir():
        raise DataError(f"Criterion directory is missing: {root}")
    rows = []
    compared = set()
    artifacts = list(root.rglob("estimates.json"))
    if not artifacts:
        raise DataError(f"no Criterion estimates found below {root}")

    for change_path in sorted(root.rglob("change/estimates.json")):
        benchmark = change_path.parent.parent
        name = benchmark.relative_to(root).as_posix()
        base_path = baseline_estimate(benchmark)
        head_path = benchmark / "new" / "estimates.json"
        if base_path is None or not head_path.is_file():
            raise DataError(f"missing base/head estimates for Criterion benchmark {name}")
        base = estimate(base_path)[0]
        head = estimate(head_path)[0]
        point, lower, upper = estimate(change_path, relative=True)
        status = "fail" if lower >= 0.50 else "warn" if lower >= 0.20 else "pass"
        rows.append(
            {
                "probe": f"Criterion: {name}",
                "base": format_ns(base),
                "head": format_ns(head),
                "change": f"{point:+.1%}",
                "confidence": f"[{lower:+.1%}, {upper:+.1%}]",
                "budget": "95% CI lower bound: warn +20%, fail +50%",
                "status": status,
            }
        )
        compared.add(name)

    benchmark_dirs = {
        path.parent.parent for path in root.rglob("new/estimates.json")
    } | {
        path.parent.parent
        for path in root.rglob("*/estimates.json")
        if path.parent.name not in {"new", "change", "report"}
    }
    for benchmark in sorted(benchmark_dirs):
        name = benchmark.relative_to(root).as_posix()
        if name in compared:
            continue
        base_path = baseline_estimate(benchmark)
        head_path = benchmark / "new" / "estimates.json"
        if base_path and not head_path.is_file():
            detail = "removed benchmark"
        elif not base_path and head_path.is_file():
            detail = "new benchmark"
        else:
            detail = "benchmark not compared"
        rows.append(
            {
                "probe": f"Criterion: {name}",
                "base": format_ns(estimate(base_path)[0]) if base_path else "—",
                "head": format_ns(estimate(head_path)[0]) if head_path.is_file() else "—",
                "change": "—",
                "confidence": "—",
                "budget": detail,
                "status": "info",
            }
        )
    return rows


def server_metrics(path):
    data = load_json(path)
    try:
        metrics = data["metrics_ms"]
    except (KeyError, TypeError) as error:
        raise DataError(f"{path} has no metrics_ms object") from error
    if not isinstance(metrics, dict):
        raise DataError(f"{path} metrics_ms must be an object")
    parsed = {
        name: number(value, f"{path} metrics_ms.{name}", positive=True)
        for name, value in metrics.items()
    }
    missing = [name for name in REQUIRED_SERVER_METRICS if name not in parsed]
    if missing:
        raise DataError(f"{path} is missing server metrics: {', '.join(missing)}")
    return parsed


def server_rows(base_path, head_path):
    head = server_metrics(head_path)
    base = server_metrics(base_path) if base_path else {}
    rows = []
    for name in sorted(set(base) | set(head)):
        if name not in head:
            rows.append(info_row(f"Server: {name}", f"{base[name]:.2f} ms", "—", "removed metric"))
            continue
        if name not in base:
            status = absolute_status(name, head[name])
            rows.append(
                {
                    "probe": f"Server: {name}",
                    "base": "—",
                    "head": f"{head[name]:.2f} ms",
                    "change": "—",
                    "confidence": "—",
                    "budget": budget_text(name),
                    "status": status,
                }
            )
            continue

        delta = head[name] - base[name]
        change = delta / base[name]
        relative = (
            "fail"
            if change >= 0.50 and delta >= 25
            else "warn"
            if change >= 0.20 and delta >= 10
            else "pass"
        )
        status = worse(relative, absolute_status(name, head[name]))
        rows.append(
            {
                "probe": f"Server: {name}",
                "base": f"{base[name]:.2f} ms",
                "head": f"{head[name]:.2f} ms",
                "change": f"{change:+.1%} ({delta:+.2f} ms)",
                "confidence": "direct",
                "budget": budget_text(name),
                "status": status,
            }
        )
    return rows


def absolute_status(name, value):
    ceiling = ABSOLUTE_CEILINGS.get(name)
    if not ceiling:
        return "pass"
    warn, fail = ceiling
    return "fail" if value >= fail else "warn" if value >= warn else "pass"


def budget_text(name):
    relative = "relative: warn +20%/+10 ms, fail +50%/+25 ms"
    if name not in ABSOLUTE_CEILINGS:
        return relative
    warn, fail = ABSOLUTE_CEILINGS[name]
    return f"{relative}; absolute: warn {warn:g} ms, fail {fail:g} ms"


def info_row(probe, base, head, detail):
    return {
        "probe": probe,
        "base": base,
        "head": head,
        "change": "—",
        "confidence": "—",
        "budget": detail,
        "status": "info",
    }


def format_ns(value):
    if value >= 1_000_000:
        return f"{value / 1_000_000:.2f} ms"
    if value >= 1_000:
        return f"{value / 1_000:.2f} µs"
    return f"{value:.2f} ns"


def markdown(rows):
    lines = [
        "## Performance",
        "",
        "| Probe | Base | Head | Change | 95% CI | Budget | Result |",
        "|---|---:|---:|---:|---:|---|---|",
    ]
    labels = {"pass": "pass", "warn": "⚠️ warning", "fail": "❌ failure", "info": "info"}
    for row in rows:
        cells = [
            row["probe"],
            row["base"],
            row["head"],
            row["change"],
            row["confidence"],
            row["budget"],
            labels[row["status"]],
        ]
        lines.append("| " + " | ".join(str(cell).replace("|", "\\|") for cell in cells) + " |")
    return "\n".join(lines) + "\n"


def annotate(rows):
    for row in rows:
        if row["status"] not in {"warn", "fail"}:
            continue
        kind = "warning" if row["status"] == "warn" else "error"
        message = (
            f"{row['probe']}: base {row['base']}, head {row['head']}, "
            f"change {row['change']}; budget {row['budget']}"
        )
        message = message.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
        print(f"::{kind} title=Performance::{message}")


def check(criterion, base, head, emit=True):
    if not head:
        raise DataError("--head is required")
    rows = []
    if criterion:
        rows.extend(criterion_rows(criterion))
    rows.extend(server_rows(base, head))
    if emit:
        annotate(rows)
    return (1 if any(row["status"] == "fail" for row in rows) else 0), rows


def write_summary(path, content):
    destination = pathlib.Path(path)
    destination.parent.mkdir(parents=True, exist_ok=True)
    with destination.open("a", encoding="utf-8") as output:
        output.write(content)


def fixture_server(path, **changes):
    values = {
        "workspace_ready": 100,
        "open_to_diagnostics": 100,
        "idle_completion_median": 5,
        "idle_completion_p95": 10,
        "full_semantic_tokens": 20,
        "single_edit_to_completion": 10,
        "four_edit_burst_to_completion": 20,
        "eight_edit_burst_to_completion": 30,
        "sixteen_edit_burst_to_completion": 40,
        "sixteen_change_bulk_notification": 40,
        "latest_diagnostics_after_editing": 300,
    }
    values.update(changes)
    path.write_text(json.dumps({"metrics_ms": values}), encoding="utf-8")


def fixture_criterion(root, *, point=0.0, lower=-0.01, upper=0.01, baseline=True):
    benchmark = root / "analysis" / "probe" / "50000"
    (benchmark / "new").mkdir(parents=True)
    absolute = {
        "mean": {
            "point_estimate": 110_000 if point else 100_000,
            "confidence_interval": {"lower_bound": 99_000, "upper_bound": 111_000},
        }
    }
    (benchmark / "new" / "estimates.json").write_text(json.dumps(absolute), encoding="utf-8")
    if baseline:
        (benchmark / "pr-base").mkdir()
        (benchmark / "pr-base" / "estimates.json").write_text(
            json.dumps(
                {
                    "mean": {
                        "point_estimate": 100_000,
                        "confidence_interval": {"lower_bound": 99_000, "upper_bound": 101_000},
                    }
                }
            ),
            encoding="utf-8",
        )
        (benchmark / "change").mkdir()
        (benchmark / "change" / "estimates.json").write_text(
            json.dumps(
                {
                    "mean": {
                        "point_estimate": point,
                        "confidence_interval": {"lower_bound": lower, "upper_bound": upper},
                    }
                }
            ),
            encoding="utf-8",
        )


def self_test():
    with tempfile.TemporaryDirectory(prefix="check-performance-") as directory:
        root = pathlib.Path(directory)

        def run(name, *, criterion_args=None, base_changes=None, head_changes=None):
            case = root / name
            case.mkdir()
            base = case / "base.json"
            head = case / "head.json"
            fixture_server(base, **(base_changes or {}))
            fixture_server(head, **(head_changes or {}))
            criterion = None
            if criterion_args is not None:
                criterion = case / "criterion"
                fixture_criterion(criterion, **criterion_args)
            return check(criterion, base, head, emit=False)

        assert run("pass", criterion_args={})[0] == 0
        result, rows = run(
            "warning",
            criterion_args={"point": 0.30, "lower": 0.25, "upper": 0.35},
        )
        assert result == 0 and any(row["status"] == "warn" for row in rows)
        assert (
            run(
                "blocking",
                criterion_args={"point": 0.60, "lower": 0.50, "upper": 0.70},
            )[0]
            == 1
        )
        assert run("absolute", head_changes={"workspace_ready": 5_001})[0] == 1
        assert (
            run(
                "tiny-noise",
                base_changes={"idle_completion_median": 0.1},
                head_changes={"idle_completion_median": 0.2},
            )[0]
            == 0
        )
        result, rows = run("new-benchmark", criterion_args={"baseline": False})
        assert result == 0 and any(row["budget"] == "new benchmark" for row in rows)

        malformed_criterion = root / "malformed-criterion"
        malformed_criterion.mkdir()
        base = malformed_criterion / "base.json"
        head = malformed_criterion / "head.json"
        fixture_server(base)
        fixture_server(head)
        change = malformed_criterion / "criterion" / "probe" / "change"
        change.mkdir(parents=True)
        (change / "estimates.json").write_text("{", encoding="utf-8")
        try:
            check(malformed_criterion / "criterion", base, head, emit=False)
            raise AssertionError("malformed Criterion JSON passed")
        except DataError:
            pass

        malformed_server = root / "malformed-server.json"
        malformed_server.write_text("{", encoding="utf-8")
        try:
            check(None, None, malformed_server, emit=False)
            raise AssertionError("malformed server JSON passed")
        except DataError:
            pass

    print("check-performance self-test: 8 cases passed")
    return 0


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--criterion", type=pathlib.Path)
    parser.add_argument("--base", type=pathlib.Path)
    parser.add_argument("--head", type=pathlib.Path)
    parser.add_argument("--summary", type=pathlib.Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if not args.summary:
        parser.error("--summary is required")
    try:
        result, rows = check(args.criterion, args.base, args.head)
        write_summary(args.summary, markdown(rows))
        return result
    except DataError as error:
        message = f"performance results are malformed or incomplete: {error}"
        print(f"::error title=Performance::{message}")
        write_summary(args.summary, f"## Performance\n\n❌ {message}\n")
        return 2


if __name__ == "__main__":
    sys.exit(main())
