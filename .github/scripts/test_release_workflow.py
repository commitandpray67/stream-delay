"""The release workflow's job graph, as GitHub runs it: on a tag and on a dry run,
every job meant for that ref runs, and the checksums, which say a release passed
everything, come only when every other job passed.

GitHub skips a job whose `if` has no status function (success(), failure(),
cancelled(), always()) when any job before it, however far back, was skipped or
failed, even if the jobs it needs ran. That skipped the checksums of v0.3.0.

Run: python3 -m unittest discover -s .github/scripts
"""

import os
import re
import unittest

WORKFLOW = os.path.join(os.path.dirname(__file__), "..", "workflows", "release.yml")
TAG = "refs/tags/v1.2.3"
DRY_RUN = "refs/heads/main"
STATUS = ("success()", "failure()", "cancelled()", "always()")


def jobs(text):
    """{job: (needs, if)} from the workflow's `jobs:`. Reads the one-line
    `needs: [a, b]`, `needs: a` and `if: ...` the workflow uses; any other form
    fails, so that this is extended rather than read wrong."""
    graph, job, in_jobs = {}, None, False
    for line in text.splitlines():
        if line == "jobs:":
            in_jobs = True
            continue
        if not in_jobs:
            continue
        m = re.fullmatch(r"  ([A-Za-z0-9_-]+):\s*", line)
        if m:
            job = m.group(1)
            graph[job] = ([], None)
            continue
        m = re.fullmatch(r"    (needs|if):(.*)", line)
        if not m:
            continue
        key, value = m.group(1), m.group(2).strip()
        needs, cond = graph[job]
        if key == "needs":
            if value.startswith("["):
                if not value.endswith("]"):
                    raise ValueError(f"{job}: needs over several lines")
                needs = [n.strip() for n in value[1:-1].split(",") if n.strip()]
            elif value:
                needs = [value]
            else:
                raise ValueError(f"{job}: needs as a block list")
        else:
            if not value or value in ("|", ">", ">-", "|-"):
                raise ValueError(f"{job}: if over several lines")
            cond = value
        graph[job] = (needs, cond)
    for job, (needs, _) in graph.items():
        for n in needs:
            if n not in graph:
                raise ValueError(f"{job} needs {n}, which is not a job")
    return graph


def ancestors(graph, job):
    seen, todo = set(), list(graph[job][0])
    while todo:
        j = todo.pop()
        if j not in seen:
            seen.add(j)
            todo.extend(graph[j][0])
    return seen


def terms(cond):
    cond = (cond or "").strip()
    if cond.startswith("${{"):
        cond = cond[3:].removesuffix("}}").strip()
    if "||" in cond:
        raise ValueError(f"'||' is not understood here: {cond}")
    return [t.strip() for t in cond.split("&&")] if cond else []


def runs(cond, ref, results, before):
    """Whether a job with `if: cond` runs, `before` being the results of every
    job before it."""
    ts = terms(cond)
    if not any(t.lstrip("!") in STATUS for t in ts):
        ts.insert(0, "success()")
    for t in ts:
        body = t.lstrip("!")
        negate = (len(t) - len(body)) % 2 == 1
        if body == "success()":
            value = all(r == "success" for r in before)
        elif body == "failure()":
            value = "failure" in before
        elif body == "cancelled()":
            value = False
        elif body == "always()":
            value = True
        elif m := re.fullmatch(r"startsWith\(github\.ref, '([^']*)'\)", body):
            value = ref.startswith(m.group(1))
        elif m := re.fullmatch(r"needs\.([A-Za-z0-9_-]+)\.result == '(\w+)'", body):
            value = results[m.group(1)] == m.group(2)
        else:
            raise ValueError(f"condition not understood: {t}")
        if value == negate:
            return False
    return True


def meant_for(cond, ref):
    """Whether a job is meant to run on `ref`: its conditions on the ref hold."""
    for t in terms(cond):
        m = re.fullmatch(r"(!?)startsWith\(github\.ref, '([^']*)'\)", t)
        if m and ref.startswith(m.group(2)) == (m.group(1) == "!"):
            return False
    return True


def simulate(graph, ref, failing=None):
    """{job: 'success' | 'failure' | 'skipped'} for a run on `ref` where only the
    job `failing` fails."""
    results = {}
    while len(results) < len(graph):
        for job, (needs, cond) in graph.items():
            if job in results or any(n not in results for n in needs):
                continue
            before = [results[a] for a in ancestors(graph, job)]
            if runs(cond, ref, results, before):
                results[job] = "failure" if job == failing else "success"
            else:
                results[job] = "skipped"
    return results


def load():
    with open(WORKFLOW, encoding="utf-8") as f:
        return jobs(f.read())


class Release(unittest.TestCase):
    def setUp(self):
        self.graph = load()

    def test_every_job_meant_for_a_tag_runs(self):
        results = simulate(self.graph, TAG)
        skipped = sorted(j for j, r in results.items() if r == "skipped")
        self.assertEqual(skipped, ["desktop-dry-run"])
        self.assertEqual(results["checksums"], "success")

    def test_every_job_meant_for_a_dry_run_runs(self):
        results = simulate(self.graph, DRY_RUN)
        for job, (_, cond) in self.graph.items():
            want = "success" if meant_for(cond, DRY_RUN) else "skipped"
            self.assertEqual(results[job], want, job)
        for job in ("desktop-dry-run", "headless", "smoke", "container"):
            self.assertEqual(results[job], "success", job)

    def test_no_checksums_unless_everything_else_passed(self):
        passing = simulate(self.graph, TAG)
        for job, result in passing.items():
            if job == "checksums" or result != "success":
                continue
            with self.subTest(failing=job):
                self.assertEqual(simulate(self.graph, TAG, failing=job)["checksums"], "skipped")

    def test_a_dry_run_adds_no_checksums(self):
        self.assertEqual(simulate(self.graph, DRY_RUN)["checksums"], "skipped")


class Simulation(unittest.TestCase):
    """The rules the checks above rely on, on small graphs."""

    GRAPH = {
        "a": ([], "startsWith(github.ref, 'refs/tags/v')"),
        "b": ([], "${{ !startsWith(github.ref, 'refs/tags/v') }}"),
        "c": (["a", "b"], "${{ !failure() && !cancelled() }}"),
    }

    def test_a_skip_anywhere_before_skips_a_job_without_a_status_function(self):
        graph = dict(self.GRAPH, d=(["c"], "startsWith(github.ref, 'refs/tags/v')"))
        results = simulate(graph, TAG)
        self.assertEqual(results["c"], "success")
        # Skipped although c, the only job it needs, passed: v0.3.0's checksums.
        self.assertEqual(results["d"], "skipped")

    def test_a_status_function_lets_it_run(self):
        cond = "${{ !cancelled() && needs.c.result == 'success' }}"
        graph = dict(self.GRAPH, d=(["c"], cond))
        self.assertEqual(simulate(graph, TAG)["d"], "success")
        self.assertEqual(simulate(graph, TAG, failing="a")["d"], "skipped")

    def test_the_workflow_is_read_as_written(self):
        graph = load()
        self.assertEqual(graph["smoke"][0], ["desktop-dry-run", "publish-headless"])
        self.assertEqual(graph["ci"], ([], "startsWith(github.ref, 'refs/tags/v')"))
        self.assertIn("needs.smoke.result == 'success'", graph["checksums"][1])

    def test_unknown_forms_are_refused(self):
        with self.assertRaises(ValueError):
            jobs("jobs:\n  a:\n    needs:\n      - b\n")
        with self.assertRaises(ValueError):
            runs("${{ always() || success() }}", TAG, {}, [])
        with self.assertRaises(ValueError):
            runs("github.event_name == 'push'", TAG, {}, [])


if __name__ == "__main__":
    unittest.main()
