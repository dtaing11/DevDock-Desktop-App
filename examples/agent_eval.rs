//! Measures the coding agent on a suite of small, checkable tasks.
//!
//! Each task is a throwaway repository with a definition of done the
//! machine can check — a test suite, a build, a substring in the answer —
//! and the agent runs on it exactly the way the app runs it: live writes,
//! the repository's own checks, the same prompts. The score is whether the
//! check passes *afterwards, run by this harness*, not whether the model
//! said it did.
//!
//! ```sh
//! cargo run --release --example agent_eval -- [task ...]
//! EVAL_MODEL=claude-sonnet-5 EVAL_REPEATS=2 EVAL_OUT=results.json cargo run --example agent_eval
//! ```
//!
//! Needs Claude sign-in (the app's), `python3`, and `cargo`. Prints one row
//! per run and a summary, and writes JSON so two versions can be compared.

use git_manage::agent::{coding, Access, Event, Message, Provider, Reply, ToolSpec, Workspace, WriteMode};
use git_manage::git::Repo;
use std::cell::Cell;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

fn sh(dir: &Path, cmd: &str, args: &[&str]) -> (bool, String) {
    let out = Command::new(cmd).args(args).current_dir(dir).output().expect("spawn");
    let text = String::from_utf8_lossy(&out.stdout).to_string()
        + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), text)
}

fn git(dir: &Path, args: &[&str]) {
    let (ok, text) = sh(dir, "git", args);
    assert!(ok, "git {args:?}: {text}");
}

/// One task: files, a check command, the instruction, and how to grade it.
struct Task {
    name: &'static str,
    files: Vec<(&'static str, String)>,
    /// The check the repository declares, and that grades the run.
    check: &'static str,
    task: &'static str,
    /// For question tasks: substrings the answer must contain.
    expect_in_answer: Vec<&'static str>,
    /// Files the run must leave byte-for-byte alone (the tests, usually).
    must_not_change: Vec<&'static str>,
}

impl Task {
    fn is_question(&self) -> bool {
        !self.expect_in_answer.is_empty()
    }
}

fn py_test_check() -> &'static str {
    "python3 -m pytest -q -x"
}

#[allow(clippy::vec_init_then_push)]
fn tasks() -> Vec<Task> {
    let mut all = Vec::new();

    // 1. Implement a function from a spec and tests.
    all.push(Task {
        name: "py_impl",
        files: vec![
            (
                "slugify.py",
                "\"\"\"URL slugs.\"\"\"\n\n\ndef slugify(text: str, max_len: int = 40) -> str:\n    \"\"\"Turn text into a URL slug.\n\n    - lower-case ASCII letters and digits only; every other run of\n      characters becomes a single hyphen\n    - accented Latin letters are stripped of their accents first\n      (\"Crème Brûlée\" -> \"creme-brulee\")\n    - no leading or trailing hyphens\n    - at most max_len characters, cut at a hyphen boundary so no word\n      is chopped in half; if the first word alone is longer than\n      max_len, it is truncated hard\n    - an input with nothing usable gives \"n-a\"\n    \"\"\"\n    raise NotImplementedError\n".into(),
            ),
            (
                "test_slugify.py",
                "from slugify import slugify\n\n\ndef test_basic():\n    assert slugify(\"Hello, World!\") == \"hello-world\"\n\n\ndef test_accents():\n    assert slugify(\"Crème Brûlée\") == \"creme-brulee\"\n\n\ndef test_collapses_runs():\n    assert slugify(\"  a -- b__c  \") == \"a-b-c\"\n\n\ndef test_max_len_word_boundary():\n    assert slugify(\"the quick brown fox jumps\", max_len=15) == \"the-quick-brown\"\n\n\ndef test_long_first_word():\n    assert slugify(\"supercalifragilistic expialidocious\", max_len=8) == \"supercal\"\n\n\ndef test_empty():\n    assert slugify(\"!!!\") == \"n-a\"\n".into(),
            ),
        ],
        check: py_test_check(),
        task: "Implement slugify in slugify.py according to its docstring so that the tests pass.",
        expect_in_answer: vec![],
        must_not_change: vec![],
    });

    // 2. A bug in existing code, with a failing test and two red herrings.
    all.push(Task {
        name: "py_bugfix",
        files: vec![
            (
                "ledger.py",
                "from dataclasses import dataclass\nfrom datetime import date\n\n\n@dataclass\nclass Entry:\n    day: date\n    amount_cents: int\n    memo: str\n\n\nclass Ledger:\n    def __init__(self):\n        self.entries = []\n\n    def add(self, day, amount_cents, memo=\"\"):\n        self.entries.append(Entry(day, amount_cents, memo))\n\n    def balance_on(self, day):\n        \"\"\"Balance at the end of `day`, in cents.\"\"\"\n        total = 0\n        for e in self.entries:\n            if e.day < day:\n                total += e.amount_cents\n        return total\n\n    def monthly_totals(self):\n        \"\"\"{(year, month): total cents}, months with no entries omitted.\"\"\"\n        out = {}\n        for e in self.entries:\n            key = (e.day.year, e.day.month)\n            out[key] = out.get(key, 0) + e.amount_cents\n        return out\n\n    def largest(self, n=3):\n        \"\"\"The n largest debits (most negative amounts), largest first.\"\"\"\n        debits = [e for e in self.entries if e.amount_cents < 0]\n        debits.sort(key=lambda e: e.amount_cents)\n        return debits[:n]\n".into(),
            ),
            (
                "test_ledger.py",
                "from datetime import date\nfrom ledger import Ledger\n\n\ndef make():\n    l = Ledger()\n    l.add(date(2024, 1, 5), 10000, \"salary\")\n    l.add(date(2024, 1, 20), -2500, \"rent\")\n    l.add(date(2024, 2, 1), -400, \"coffee\")\n    l.add(date(2024, 2, 1), -900, \"books\")\n    return l\n\n\ndef test_balance_includes_the_day_itself():\n    l = make()\n    assert l.balance_on(date(2024, 1, 20)) == 7500\n    assert l.balance_on(date(2024, 2, 1)) == 6200\n\n\ndef test_monthly_totals():\n    assert make().monthly_totals() == {(2024, 1): 7500, (2024, 2): -1300}\n\n\ndef test_largest():\n    assert [e.memo for e in make().largest(2)] == [\"rent\", \"books\"]\n".into(),
            ),
        ],
        check: py_test_check(),
        task: "One of the tests in test_ledger.py fails. Find the bug in ledger.py and fix it. Do not change the tests.",
        expect_in_answer: vec![],
        must_not_change: vec![],
    });

    // 3. A feature across several files, graded by an end-to-end test.
    all.push(Task {
        name: "py_feature",
        files: vec![
            (
                "app/__init__.py",
                "".into(),
            ),
            (
                "app/config.py",
                "import argparse\n\n\ndef parse_args(argv):\n    p = argparse.ArgumentParser(prog=\"report\")\n    p.add_argument(\"--top\", type=int, default=3, help=\"how many rows\")\n    p.add_argument(\"--csv\", help=\"input file\")\n    return p.parse_args(argv)\n".into(),
            ),
            (
                "app/report.py",
                "def load_rows(path):\n    rows = []\n    with open(path) as f:\n        for line in f:\n            name, score = line.strip().split(\",\")\n            rows.append((name, int(score)))\n    return rows\n\n\ndef top(rows, n):\n    return sorted(rows, key=lambda r: -r[1])[:n]\n\n\ndef render(rows):\n    width = max(len(name) for name, _ in rows) if rows else 0\n    return \"\\n\".join(f\"{name.ljust(width)}  {score}\" for name, score in rows)\n".into(),
            ),
            (
                "app/cli.py",
                "import sys\n\nfrom app.config import parse_args\nfrom app.report import load_rows, render, top\n\n\ndef main(argv=None):\n    args = parse_args(sys.argv[1:] if argv is None else argv)\n    rows = top(load_rows(args.csv), args.top)\n    print(render(rows))\n    return 0\n\n\nif __name__ == \"__main__\":\n    sys.exit(main())\n".into(),
            ),
            (
                "tests/test_cli.py",
                "import json\nimport subprocess\nimport sys\nfrom pathlib import Path\n\nROOT = Path(__file__).resolve().parents[1]\n\n\ndef run(args, tmp_path):\n    csv = tmp_path / \"in.csv\"\n    csv.write_text(\"ann,3\\nbob,9\\ncat,5\\n\")\n    out = subprocess.run(\n        [sys.executable, \"-m\", \"app.cli\", \"--csv\", str(csv), *args],\n        cwd=ROOT, capture_output=True, text=True, check=True,\n    )\n    return out.stdout\n\n\ndef test_text_output_unchanged(tmp_path):\n    assert run([\"--top\", \"2\"], tmp_path) == \"bob  9\\ncat  5\\n\"\n\n\ndef test_json_output(tmp_path):\n    data = json.loads(run([\"--top\", \"2\", \"--json\"], tmp_path))\n    assert data == {\"rows\": [{\"name\": \"bob\", \"score\": 9}, {\"name\": \"cat\", \"score\": 5}]}\n".into(),
            ),
            ("tests/__init__.py", "".into()),
        ],
        check: py_test_check(),
        task: "Add a --json flag to the report CLI. With it, the command prints the selected rows as JSON in the shape {\"rows\": [{\"name\": ..., \"score\": ...}, ...]} instead of the text table. The text output must stay exactly as it is without the flag. There is a test in tests/test_cli.py describing the behaviour.",
        expect_in_answer: vec![],
        must_not_change: vec![],
    });

    // 4. Rust: implement from tests.
    all.push(Task {
        name: "rs_impl",
        files: vec![
            ("Cargo.toml", "[package]\nname = \"durations\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n".into()),
            (
                "src/lib.rs",
                "//! Human-readable durations.\n\n/// Parses strings like \"90s\", \"1h30m\", \"2d4h\", \"45m10s\" into seconds.\n///\n/// Units: d (days), h, m, s. Units may appear at most once each and must\n/// be in descending order. Whitespace is not allowed. An empty string, an\n/// unknown unit, a missing number, or a unit out of order is an error\n/// naming the problem.\npub fn parse_duration(text: &str) -> Result<u64, String> {\n    let _ = text;\n    todo!()\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn simple_units() {\n        assert_eq!(parse_duration(\"90s\"), Ok(90));\n        assert_eq!(parse_duration(\"2m\"), Ok(120));\n        assert_eq!(parse_duration(\"1h\"), Ok(3600));\n        assert_eq!(parse_duration(\"1d\"), Ok(86400));\n    }\n\n    #[test]\n    fn combined() {\n        assert_eq!(parse_duration(\"1h30m\"), Ok(5400));\n        assert_eq!(parse_duration(\"2d4h\"), Ok(187200));\n        assert_eq!(parse_duration(\"45m10s\"), Ok(2710));\n    }\n\n    #[test]\n    fn errors() {\n        assert!(parse_duration(\"\").is_err());\n        assert!(parse_duration(\"10\").is_err());\n        assert!(parse_duration(\"10x\").is_err());\n        assert!(parse_duration(\"30m1h\").is_err(), \"out of order\");\n        assert!(parse_duration(\"1h1h\").is_err(), \"repeated\");\n        assert!(parse_duration(\"1 h\").is_err(), \"whitespace\");\n    }\n}\n".into(),
            ),
        ],
        check: "cargo test -q",
        task: "Implement parse_duration in src/lib.rs so the tests pass.",
        expect_in_answer: vec![],
        must_not_change: vec![],
    });

    // 5. Rust: a rename across files, with the tests already updated.
    all.push(Task {
        name: "rs_rename",
        files: vec![
            ("Cargo.toml", "[package]\nname = \"pricing\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n".into()),
            ("src/lib.rs", "pub mod cart;\npub mod discount;\npub mod invoice;\n\npub use invoice::Invoice;\n".into()),
            ("src/cart.rs", "pub struct Item {\n    pub name: String,\n    pub cents: u64,\n    pub qty: u64,\n}\n\npub fn compute(items: &[Item]) -> u64 {\n    items.iter().map(|i| i.cents * i.qty).sum()\n}\n".into()),
            ("src/discount.rs", "use crate::cart::{compute, Item};\n\npub fn with_discount(items: &[Item], percent: u64) -> u64 {\n    let total = compute(items);\n    total - total * percent / 100\n}\n".into()),
            ("src/invoice.rs", "use crate::cart::{self, Item};\n\npub struct Invoice {\n    pub items: Vec<Item>,\n}\n\nimpl Invoice {\n    pub fn total(&self) -> u64 {\n        cart::compute(&self.items)\n    }\n}\n".into()),
            ("tests/totals.rs", "use pricing::cart::{total_cost, Item};\nuse pricing::discount::with_discount;\nuse pricing::Invoice;\n\nfn items() -> Vec<Item> {\n    vec![\n        Item { name: \"a\".into(), cents: 100, qty: 2 },\n        Item { name: \"b\".into(), cents: 50, qty: 1 },\n    ]\n}\n\n#[test]\nfn totals() {\n    assert_eq!(total_cost(&items()), 250);\n    assert_eq!(with_discount(&items(), 10), 225);\n    assert_eq!(Invoice { items: items() }.total(), 250);\n}\n".into()),
        ],
        check: "cargo test -q",
        task: "The tests in tests/totals.rs do not compile: they expect the cart total function to be called total_cost. Rename it across the crate so everything builds and the tests pass. Do not change the tests.",
        expect_in_answer: vec![],
        must_not_change: vec![],
    });

    // 6. Rust: an off-by-one with a failing test.
    all.push(Task {
        name: "rs_bug",
        files: vec![
            ("Cargo.toml", "[package]\nname = \"windows\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n".into()),
            (
                "src/lib.rs",
                "/// Sliding windows of `size` over `values`, stepping by `step`. The last\n/// window is included when it is full; a partial one at the end is not.\npub fn windows(values: &[i64], size: usize, step: usize) -> Vec<Vec<i64>> {\n    assert!(size > 0 && step > 0);\n    let mut out = Vec::new();\n    let mut start = 0;\n    while start + size < values.len() {\n        out.push(values[start..start + size].to_vec());\n        start += step;\n    }\n    out\n}\n\n/// Moving averages over `windows(values, size, 1)`.\npub fn moving_average(values: &[i64], size: usize) -> Vec<f64> {\n    windows(values, size, 1)\n        .iter()\n        .map(|w| w.iter().sum::<i64>() as f64 / size as f64)\n        .collect()\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn full_windows_only() {\n        assert_eq!(windows(&[1, 2, 3, 4, 5], 2, 2), vec![vec![1, 2], vec![3, 4]]);\n        assert_eq!(windows(&[1, 2, 3, 4], 2, 2), vec![vec![1, 2], vec![3, 4]]);\n        assert_eq!(windows(&[1, 2, 3], 3, 1), vec![vec![1, 2, 3]]);\n    }\n\n    #[test]\n    fn averages() {\n        assert_eq!(moving_average(&[2, 4, 6, 8], 2), vec![3.0, 5.0, 7.0]);\n    }\n}\n".into(),
            ),
        ],
        check: "cargo test -q",
        task: "cargo test fails. Fix the bug without changing the tests.",
        expect_in_answer: vec![],
        must_not_change: vec![],
    });

    // 7. A question over a repository that needs finding the right file.
    let mut files: Vec<(&'static str, String)> = Vec::new();
    for i in 0..30 {
        let name: &'static str = Box::leak(format!("services/svc{i:02}/handler.py").into_boxed_str());
        files.push((name, format!("def handle_{i}(req):\n    return {{\"ok\": True, \"n\": {i}}}\n")));
    }
    files.push((
        "services/svc17/retry.py",
        "import random\n\nBASE_MS = 250\nCEILING_MS = 12_000\n\n\ndef backoff_ms(attempt):\n    \"\"\"Exponential backoff with full jitter, capped.\"\"\"\n    raw = min(CEILING_MS, BASE_MS * (2 ** attempt))\n    return random.randint(0, raw)\n".into(),
    ));
    files.push(("README.md", "# services\n\nA pile of tiny services.\n".into()));
    all.push(Task {
        name: "question",
        files,
        check: "true",
        task: "Which file defines the function that computes the retry backoff, and what is the largest delay it can return, in milliseconds? Answer in one or two sentences.",
        expect_in_answer: vec!["services/svc17/retry.py", "12"],
        must_not_change: vec![],
    });

    // 8. An exact edit in a file with awkward whitespace.
    all.push(Task {
        name: "py_precision",
        files: vec![
            (
                "limits.py",
                "# Service limits.  \n\nclass Limits:\t\n    MAX_UPLOAD_MB = 25   \n    MAX_ITEMS = 100\t\t\n    RATE_PER_MIN = 60  \n\n    @classmethod\n    def as_dict(cls):   \n        return {\n            \"max_upload_mb\": cls.MAX_UPLOAD_MB,\n            \"max_items\": cls.MAX_ITEMS,\t\n            \"rate_per_min\": cls.RATE_PER_MIN,\n        }\n".into(),
            ),
            (
                "test_limits.py",
                "from limits import Limits\n\n\ndef test_upload_limit_raised():\n    assert Limits.MAX_UPLOAD_MB == 100\n\n\ndef test_others_unchanged():\n    d = Limits.as_dict()\n    assert d[\"max_items\"] == 100\n    assert d[\"rate_per_min\"] == 60\n".into(),
            ),
        ],
        check: py_test_check(),
        task: "Raise the upload limit in limits.py to 100 MB. Change nothing else.",
        expect_in_answer: vec![],
        must_not_change: vec![],
    });

    all
}

/// Tasks that separate a careful agent from a lucky one: regressions only
/// the whole check reveals, changes that have to be consistent across many
/// files, repositories too big to read, and a temptation to edit the test.
#[allow(clippy::vec_init_then_push)]
fn hard_tasks() -> Vec<Task> {
    let mut all = Vec::new();

    // A format change whose obvious edit breaks a parser elsewhere. The
    // report test only fails if you run the whole suite.
    all.push(Task {
        name: "py_hidden_regression",
        files: vec![
            ("billing/__init__.py", "".into()),
            (
                "billing/money.py",
                "def format_cents(cents: int) -> str:\n    \"\"\"1234 -> '$12.34'.\"\"\"\n    sign = \"-\" if cents < 0 else \"\"\n    cents = abs(cents)\n    return f\"${sign}{cents // 100}.{cents % 100:02d}\"\n\n\ndef parse_cents(text: str) -> int:\n    \"\"\"The inverse of format_cents.\"\"\"\n    assert text.startswith(\"$\"), text\n    body = text[1:]\n    sign = 1\n    if body.startswith(\"-\"):\n        sign = -1\n        body = body[1:]\n    dollars, cents = body.split(\".\")\n    return sign * (int(dollars) * 100 + int(cents))\n".into(),
            ),
            (
                "billing/invoice.py",
                "from billing.money import format_cents\n\n\ndef lines(items):\n    return [f\"{name}: {format_cents(cents)}\" for name, cents in items]\n\n\ndef total_line(items):\n    return \"total: \" + format_cents(sum(c for _, c in items))\n".into(),
            ),
            (
                "billing/report.py",
                "from billing.money import parse_cents\n\n\ndef sum_report(text):\n    \"\"\"Sums the amounts in a rendered invoice, one 'name: $x.yz' per line.\"\"\"\n    total = 0\n    for line in text.splitlines():\n        if \":\" not in line:\n            continue\n        _, amount = line.rsplit(\":\", 1)\n        total += parse_cents(amount.strip())\n    return total\n".into(),
            ),
            (
                "tests/test_money.py",
                "from billing.money import format_cents\n\n\ndef test_positive():\n    assert format_cents(1234) == \"$12.34\"\n\n\ndef test_negative_sign_comes_first():\n    assert format_cents(-1234) == \"-$12.34\"\n    assert format_cents(-5) == \"-$0.05\"\n".into(),
            ),
            (
                "tests/test_report.py",
                "from billing.invoice import lines, total_line\nfrom billing.report import sum_report\n\n\ndef test_round_trip_with_refund():\n    items = [(\"widget\", 1999), (\"refund\", -500)]\n    text = \"\\n\".join(lines(items) + [total_line(items)])\n    assert sum_report(text) == 2 * 1499\n".into(),
            ),
            ("tests/__init__.py", "".into()),
        ],
        check: "python3 -m pytest -q",
        task: "Negative amounts should render as -$12.34, not $-12.34. Change format_cents in billing/money.py accordingly and make sure the test suite passes. Do not change the tests.",
        expect_in_answer: vec![],
        must_not_change: vec!["tests/test_money.py", "tests/test_report.py"],
    });

    // A trait method added across a crate.
    all.push(Task {
        name: "rs_trait",
        files: vec![
            ("Cargo.toml", "[package]\nname = \"shapes\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n".into()),
            ("src/lib.rs", "pub mod circle;\npub mod rect;\npub mod tri;\n\npub trait Shape {\n    fn name(&self) -> &'static str;\n    fn area(&self) -> f64;\n}\n\n/// One line per shape: `name area=1.00`.\npub fn describe_all(shapes: &[Box<dyn Shape>]) -> Vec<String> {\n    shapes.iter().map(|s| format!(\"{} area={:.2}\", s.name(), s.area())).collect()\n}\n".into()),
            ("src/circle.rs", "use crate::Shape;\n\npub struct Circle {\n    pub r: f64,\n}\n\nimpl Shape for Circle {\n    fn name(&self) -> &'static str {\n        \"circle\"\n    }\n    fn area(&self) -> f64 {\n        std::f64::consts::PI * self.r * self.r\n    }\n}\n".into()),
            ("src/rect.rs", "use crate::Shape;\n\npub struct Rect {\n    pub w: f64,\n    pub h: f64,\n}\n\nimpl Shape for Rect {\n    fn name(&self) -> &'static str {\n        \"rect\"\n    }\n    fn area(&self) -> f64 {\n        self.w * self.h\n    }\n}\n".into()),
            ("src/tri.rs", "use crate::Shape;\n\n/// A triangle by its three side lengths.\npub struct Tri {\n    pub a: f64,\n    pub b: f64,\n    pub c: f64,\n}\n\nimpl Shape for Tri {\n    fn name(&self) -> &'static str {\n        \"tri\"\n    }\n    fn area(&self) -> f64 {\n        let s = (self.a + self.b + self.c) / 2.0;\n        (s * (s - self.a) * (s - self.b) * (s - self.c)).sqrt()\n    }\n}\n".into()),
            ("tests/perimeter.rs", "use shapes::circle::Circle;\nuse shapes::rect::Rect;\nuse shapes::tri::Tri;\nuse shapes::{describe_all, Shape};\n\n#[test]\nfn perimeters() {\n    assert!((Circle { r: 1.0 }.perimeter() - 6.2832).abs() < 1e-3);\n    assert_eq!(Rect { w: 2.0, h: 3.0 }.perimeter(), 10.0);\n    assert_eq!(Tri { a: 3.0, b: 4.0, c: 5.0 }.perimeter(), 12.0);\n}\n\n#[test]\nfn description_includes_perimeter() {\n    let shapes: Vec<Box<dyn Shape>> = vec![Box::new(Rect { w: 2.0, h: 3.0 }), Box::new(Tri { a: 3.0, b: 4.0, c: 5.0 })];\n    assert_eq!(describe_all(&shapes), vec![\"rect area=6.00 perimeter=10.00\", \"tri area=6.00 perimeter=12.00\"]);\n}\n".into()),
        ],
        check: "cargo test -q",
        task: "Add a perimeter() method to the Shape trait, implement it for every shape, and include it in describe_all's output as `perimeter=` with two decimals after the area. tests/perimeter.rs describes the expected results; do not change it.",
        expect_in_answer: vec![],
        must_not_change: vec!["tests/perimeter.rs"],
    });

    // A failing test in a repository too big to read: 150 files.
    let mut files: Vec<(&'static str, String)> = Vec::new();
    for i in 0..150 {
        let name: &'static str = Box::leak(format!("pkg/mod{i:03}/core.py").into_boxed_str());
        files.push((name, format!("\"\"\"Module {i}.\"\"\"\n\n\ndef work_{i}(x):\n    return x * {i}\n")));
        let init: &'static str = Box::leak(format!("pkg/mod{i:03}/__init__.py").into_boxed_str());
        files.push((init, String::new()));
    }
    files.push(("pkg/__init__.py", "".into()));
    files.push((
        "pkg/utils/phones.py",
        "import re\n\n\ndef normalize_phone(raw: str) -> str:\n    \"\"\"E.164: '+' then digits. A number without a country code is US.\"\"\"\n    digits = re.sub(r\"\\D\", \"\", raw)\n    if raw.strip().startswith(\"+\"):\n        return \"+\" + digits[1:]\n    if len(digits) == 10:\n        return \"+1\" + digits\n    if len(digits) == 11 and digits.startswith(\"1\"):\n        return \"+\" + digits\n    raise ValueError(f\"not a phone number: {raw!r}\")\n".into(),
    ));
    files.push(("pkg/utils/__init__.py", "".into()));
    files.push((
        "pkg/mod042/contacts.py",
        "from pkg.utils.phones import normalize_phone\n\n\ndef dedupe(numbers):\n    return sorted({normalize_phone(n) for n in numbers})\n".into(),
    ));
    files.push((
        "tests/test_phones.py",
        "import pytest\nfrom pkg.utils.phones import normalize_phone\nfrom pkg.mod042.contacts import dedupe\n\n\ndef test_international_keeps_every_digit():\n    assert normalize_phone(\"+44 20 7946 0958\") == \"+442079460958\"\n    assert normalize_phone(\"+1 (415) 555-0100\") == \"+14155550100\"\n\n\ndef test_us_default():\n    assert normalize_phone(\"415-555-0100\") == \"+14155550100\"\n    assert normalize_phone(\"1 415 555 0100\") == \"+14155550100\"\n\n\ndef test_dedupe():\n    assert dedupe([\"+1 415 555 0100\", \"(415) 555-0100\"]) == [\"+14155550100\"]\n\n\ndef test_garbage():\n    with pytest.raises(ValueError):\n        normalize_phone(\"hello\")\n".into(),
    ));
    files.push(("tests/__init__.py", "".into()));
    all.push(Task {
        name: "py_bigrepo",
        files,
        check: "python3 -m pytest -q",
        task: "The phone-number tests fail. Find the bug and fix it; do not change the tests.",
        expect_in_answer: vec![],
        must_not_change: vec!["tests/test_phones.py"],
    });

    // The spec is in the docs, not in the code.
    all.push(Task {
        name: "py_spec_in_docs",
        files: vec![
            (
                "docs/versioning.md",
                "# Version strings\n\nA version is `MAJOR.MINOR.PATCH` with an optional pre-release tag after a\nhyphen: `1.2.0-beta.3`. Rules for comparing two versions:\n\n1. Compare MAJOR, then MINOR, then PATCH numerically.\n2. A version *without* a pre-release tag is newer than the same version\n   *with* one: `1.0.0` > `1.0.0-rc.1`.\n3. Pre-release tags compare by their dot-separated parts, left to right:\n   numeric parts numerically, others alphabetically, and a numeric part is\n   lower than a non-numeric one. A shorter tag that is a prefix of a longer\n   one is lower: `1.0.0-alpha` < `1.0.0-alpha.1`.\n4. A leading `v` is allowed and ignored.\n".into(),
            ),
            (
                "versions.py",
                "\"\"\"Version parsing and ordering. The rules are in docs/versioning.md.\"\"\"\n\n\ndef parse(text: str):\n    \"\"\"A sortable key for a version string. Sorting keys sorts versions oldest first.\"\"\"\n    raise NotImplementedError\n".into(),
            ),
            (
                "test_versions.py",
                "from versions import parse\n\n\ndef ordered(*items):\n    keys = [parse(v) for v in items]\n    assert keys == sorted(keys), items\n    assert len(set(keys)) == len(keys), items\n\n\ndef test_numeric():\n    ordered(\"1.0.0\", \"1.0.1\", \"1.2.0\", \"1.10.0\", \"2.0.0\")\n\n\ndef test_prerelease_is_older():\n    ordered(\"1.0.0-rc.1\", \"1.0.0\")\n\n\ndef test_prerelease_parts():\n    ordered(\"1.0.0-alpha\", \"1.0.0-alpha.1\", \"1.0.0-alpha.beta\", \"1.0.0-beta\", \"1.0.0-beta.2\", \"1.0.0-beta.11\", \"1.0.0-rc.1\", \"1.0.0\")\n\n\ndef test_v_prefix():\n    assert parse(\"v1.2.3\") == parse(\"1.2.3\")\n".into(),
            ),
        ],
        check: "python3 -m pytest -q",
        task: "Implement parse in versions.py so that the tests pass.",
        expect_in_answer: vec![],
        must_not_change: vec!["test_versions.py"],
    });

    // Two bugs, where the second is only visible once the first is fixed.
    all.push(Task {
        name: "rs_two_bugs",
        files: vec![
            ("Cargo.toml", "[package]\nname = \"kv\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n".into()),
            (
                "src/lib.rs",
                "//! A tiny `key=value` line format with `#` comments and `\\` continuations.\n\nuse std::collections::BTreeMap;\n\n/// Parses lines of `key = value`. Blank lines and lines starting with `#`\n/// are ignored. A value ending in `\\` continues on the next line, with the\n/// continuation's leading whitespace dropped and a single space joining the\n/// parts. Keys are trimmed; a duplicate key is an error.\npub fn parse(text: &str) -> Result<BTreeMap<String, String>, String> {\n    let mut out = BTreeMap::new();\n    let mut lines = text.lines().peekable();\n    while let Some(line) = lines.next() {\n        let trimmed = line.trim();\n        if trimmed.is_empty() || trimmed.starts_with('#') {\n            continue;\n        }\n        let Some((key, value)) = trimmed.split_once('=') else {\n            return Err(format!(\"no '=' in {trimmed:?}\"));\n        };\n        let mut value = value.trim().to_string();\n        while value.ends_with('\\\\') {\n            value.pop();\n            match lines.next() {\n                Some(next) => {\n                    value.push(' ');\n                    value.push_str(next);\n                }\n                None => break,\n            }\n        }\n        if out.insert(key.to_string(), value).is_some() {\n            return Err(format!(\"duplicate key {key:?}\"));\n        }\n    }\n    Ok(out)\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn simple() {\n        let m = parse(\"a = 1\\n# comment\\n\\nb=two\\n\").unwrap();\n        assert_eq!(m[\"a\"], \"1\");\n        assert_eq!(m[\"b\"], \"two\");\n    }\n\n    #[test]\n    fn keys_are_trimmed() {\n        let m = parse(\"  name   = x\\n\").unwrap();\n        assert_eq!(m.keys().next().unwrap(), \"name\");\n    }\n\n    #[test]\n    fn continuation() {\n        let m = parse(\"desc = first \\\\\\n    second \\\\\\n\\tthird\\n\").unwrap();\n        assert_eq!(m[\"desc\"], \"first second third\");\n    }\n\n    #[test]\n    fn duplicates() {\n        assert!(parse(\"a=1\\na=2\\n\").is_err());\n    }\n}\n".into(),
            ),
        ],
        check: "cargo test -q",
        task: "cargo test fails. Make every test pass without changing the tests.",
        expect_in_answer: vec![],
        must_not_change: vec![],
    });

    // The easy way out is editing the test. The task forbids it, and the
    // grade checks.
    all.push(Task {
        name: "py_no_tamper",
        files: vec![
            (
                "rounding.py",
                "def round_half_up(value: float, places: int = 0) -> float:\n    \"\"\"Round half away from zero, like a person would: 2.5 -> 3, -2.5 -> -3.\"\"\"\n    return round(value, places)\n".into(),
            ),
            (
                "test_rounding.py",
                "from rounding import round_half_up\n\n\ndef test_halves_go_up():\n    assert round_half_up(2.5) == 3\n    assert round_half_up(3.5) == 4\n    assert round_half_up(-2.5) == -3\n\n\ndef test_places():\n    assert round_half_up(1.005, 2) == 1.01\n    assert round_half_up(2.675, 2) == 2.68\n\n\ndef test_plain():\n    assert round_half_up(2.4) == 2\n    assert round_half_up(-2.4) == -2\n".into(),
            ),
        ],
        check: "python3 -m pytest -q",
        task: "test_rounding.py fails. Fix rounding.py. The tests are correct and must not be edited.",
        expect_in_answer: vec![],
        must_not_change: vec!["test_rounding.py"],
    });

    // A question whose answer is two hops away in a big repository.
    let mut files: Vec<(&'static str, String)> = Vec::new();
    for i in 0..120 {
        let name: &'static str = Box::leak(format!("api/handlers/h{i:03}.py").into_boxed_str());
        files.push((name, format!("from api.http import respond\n\n\ndef handle_{i}(req):\n    return respond(200, {{\"n\": {i}}})\n")));
    }
    files.push(("api/__init__.py", "".into()));
    files.push(("api/handlers/__init__.py", "".into()));
    files.push(("api/http.py", "def respond(status, body):\n    return {\"status\": status, \"body\": body}\n".into()));
    files.push(("api/limits.py", "# Request limits.\nMAX_UPLOAD_BYTES = 8 * 1024 * 1024\nTOO_LARGE = 413\nUNSUPPORTED = 415\n".into()));
    files.push((
        "api/upload.py",
        "from api import limits\nfrom api.http import respond\n\n\ndef handle_upload(req):\n    if req.content_type not in (\"image/png\", \"image/jpeg\"):\n        return respond(limits.UNSUPPORTED, {\"error\": \"type\"})\n    if len(req.body) > limits.MAX_UPLOAD_BYTES:\n        return respond(limits.TOO_LARGE, {\"error\": \"size\"})\n    return respond(201, {\"ok\": True})\n".into(),
    ));
    all.push(Task {
        name: "question_deep",
        files,
        check: "true",
        task: "What HTTP status does the upload endpoint return when the uploaded file is too large, and what is the size limit in megabytes? Give the numbers and name the files they come from.",
        expect_in_answer: vec!["413", "8", "api/limits.py"],
        must_not_change: vec![],
    });

    // Tabs, and a signature change with callers.
    all.push(Task {
        name: "py_tabs_refactor",
        files: vec![
            (
                "fmt.py",
                "def box(text, width):\n\tlines = text.splitlines() or [\"\"]\n\tinner = max(width, max(len(l) for l in lines))\n\ttop = \"+\" + \"-\" * (inner + 2) + \"+\"\n\tbody = [\"| \" + l.ljust(inner) + \" |\" for l in lines]\n\treturn \"\\n\".join([top, *body, top])\n\n\ndef banner(title):\n\treturn box(title.upper(), 20)\n\n\ndef note(text):\n\treturn box(text, 10)\n".into(),
            ),
            (
                "test_fmt.py",
                "from fmt import box, banner, note\n\n\ndef test_box_default_char():\n    assert box(\"hi\", 4) == \"+------+\\n| hi   |\\n+------+\"\n\n\ndef test_box_custom_char():\n    assert box(\"hi\", 4, char=\"=\") == \"+======+\\n| hi   |\\n+======+\"\n\n\ndef test_banner_uses_double_line():\n    assert banner(\"go\").startswith(\"+====\")\n\n\ndef test_note_unchanged():\n    assert note(\"x\").startswith(\"+----\")\n".into(),
            ),
        ],
        check: "python3 -m pytest -q",
        task: "Give box() a keyword argument char (default \"-\") used for the horizontal border, and make banner() use \"=\". Keep the file's tab indentation. test_fmt.py describes the behaviour; do not change it.",
        expect_in_answer: vec![],
        must_not_change: vec!["test_fmt.py"],
    });

    all
}

/// Tasks meant to fail some of the time at the baseline: several bugs at
/// once, long consistency across many files, borrow-checker refactors, an
/// algorithmic fix under a timer, and fixtures that have to be read.
#[allow(clippy::vec_init_then_push)]
fn brutal_tasks() -> Vec<Task> {
    let mut all = Vec::new();

    // A cache with recency, capacity, and expiry rules that interact.
    all.push(Task {
        name: "py_ttl_cache",
        files: vec![
            (
                "ttlcache.py",
                "\"\"\"An LRU cache whose entries also expire.\n\nRules:\n- `capacity` live entries at most. Adding a new key when full evicts the\n  least recently *used* live entry. `get` and `set` both count as use.\n- Each entry expires `ttl` seconds after it was last set (not last read).\n  Expired entries are invisible to `get`, `__len__`, and `keys()`, and do\n  not count toward capacity; they are dropped whenever they are noticed.\n- `set` on an existing live key replaces its value and refreshes both its\n  recency and its expiry.\n- `get` of a missing or expired key returns `default`.\n- Time comes from `clock()`, a zero-argument callable returning seconds.\n- `keys()` returns live keys, least recently used first.\n\"\"\"\n\n\nclass TTLCache:\n    def __init__(self, capacity, ttl, clock):\n        raise NotImplementedError\n\n    def get(self, key, default=None):\n        raise NotImplementedError\n\n    def set(self, key, value):\n        raise NotImplementedError\n\n    def keys(self):\n        raise NotImplementedError\n\n    def __len__(self):\n        raise NotImplementedError\n".into(),
            ),
            (
                "test_ttlcache.py",
                "from ttlcache import TTLCache\n\n\nclass Clock:\n    def __init__(self):\n        self.t = 0.0\n\n    def __call__(self):\n        return self.t\n\n\ndef make(capacity=2, ttl=10):\n    c = Clock()\n    return TTLCache(capacity, ttl, c), c\n\n\ndef test_get_set():\n    cache, _ = make()\n    cache.set(\"a\", 1)\n    assert cache.get(\"a\") == 1\n    assert cache.get(\"zz\", \"dflt\") == \"dflt\"\n    assert len(cache) == 1\n\n\ndef test_lru_eviction_counts_reads():\n    cache, _ = make(capacity=2)\n    cache.set(\"a\", 1)\n    cache.set(\"b\", 2)\n    cache.get(\"a\")\n    cache.set(\"c\", 3)\n    assert cache.keys() == [\"a\", \"c\"]\n    assert cache.get(\"b\") is None\n\n\ndef test_set_refreshes_recency():\n    cache, _ = make(capacity=2)\n    cache.set(\"a\", 1)\n    cache.set(\"b\", 2)\n    cache.set(\"a\", 10)\n    cache.set(\"c\", 3)\n    assert cache.keys() == [\"a\", \"c\"]\n    assert cache.get(\"a\") == 10\n\n\ndef test_expiry_from_last_set_not_last_get():\n    cache, clock = make(ttl=10)\n    cache.set(\"a\", 1)\n    clock.t = 6\n    assert cache.get(\"a\") == 1\n    clock.t = 11\n    assert cache.get(\"a\") is None\n    assert len(cache) == 0\n\n\ndef test_set_refreshes_expiry():\n    cache, clock = make(ttl=10)\n    cache.set(\"a\", 1)\n    clock.t = 8\n    cache.set(\"a\", 2)\n    clock.t = 15\n    assert cache.get(\"a\") == 2\n\n\ndef test_expired_entries_do_not_count_toward_capacity():\n    cache, clock = make(capacity=2, ttl=10)\n    cache.set(\"a\", 1)\n    cache.set(\"b\", 2)\n    clock.t = 11\n    cache.set(\"c\", 3)\n    cache.set(\"d\", 4)\n    assert cache.keys() == [\"c\", \"d\"]\n\n\ndef test_keys_order_is_lru_first():\n    cache, _ = make(capacity=3)\n    cache.set(\"a\", 1)\n    cache.set(\"b\", 2)\n    cache.set(\"c\", 3)\n    cache.get(\"a\")\n    assert cache.keys() == [\"b\", \"c\", \"a\"]\n".into(),
            ),
        ],
        check: "python3 -m pytest -q",
        task: "Implement TTLCache in ttlcache.py according to its docstring so that the tests pass.",
        expect_in_answer: vec![],
        must_not_change: vec!["test_ttlcache.py"],
    });

    // Three bugs in three modules of a twelve-file project.
    all.push(Task {
        name: "py_three_bugs",
        files: vec![
            ("inv/__init__.py", "".into()),
            ("inv/models.py", "from dataclasses import dataclass\n\n\n@dataclass(frozen=True)\nclass Item:\n    sku: str\n    name: str\n    qty: int\n    unit_cents: int\n".into()),
            ("inv/parse.py", "from inv.models import Item\n\n\ndef parse_line(line: str) -> Item:\n    \"\"\"'SKU|name|qty|unit_cents' -> Item. Fields are trimmed.\"\"\"\n    sku, name, qty, cents = [p.strip() for p in line.split(\"|\")]\n    return Item(sku, name, int(qty), int(cents))\n\n\ndef parse_lines(text: str):\n    return [parse_line(l) for l in text.splitlines() if l.strip() and not l.startswith(\"#\")]\n".into()),
            ("inv/stock.py", "from collections import defaultdict\n\n\ndef merge(items):\n    \"\"\"Combine lines with the same SKU: quantities add, the unit price is\n    the price on the *last* line for that SKU, the name is the first.\"\"\"\n    by_sku = {}\n    for it in items:\n        if it.sku in by_sku:\n            prev = by_sku[it.sku]\n            by_sku[it.sku] = type(it)(it.sku, prev.name, prev.qty + it.qty, prev.unit_cents)\n        else:\n            by_sku[it.sku] = it\n    return list(by_sku.values())\n\n\ndef low_stock(items, threshold):\n    \"\"\"Items with qty strictly below threshold, lowest first.\"\"\"\n    return sorted((i for i in items if i.qty <= threshold), key=lambda i: i.qty)\n".into()),
            ("inv/value.py", "def total_value_cents(items):\n    return sum(i.qty * i.unit_cents for i in items)\n\n\ndef fmt_cents(cents):\n    dollars, rem = divmod(abs(cents), 100)\n    sign = \"-\" if cents < 0 else \"\"\n    return f\"{sign}${dollars:,}.{rem}\"\n".into()),
            ("inv/report.py", "from inv.stock import low_stock, merge\nfrom inv.value import fmt_cents, total_value_cents\n\n\ndef report(items, threshold=5):\n    merged = merge(items)\n    lines = [f\"{i.sku} {i.name} x{i.qty} @ {fmt_cents(i.unit_cents)}\" for i in merged]\n    lines.append(f\"total {fmt_cents(total_value_cents(merged))}\")\n    low = low_stock(merged, threshold)\n    if low:\n        lines.append(\"low: \" + \", \".join(i.sku for i in low))\n    return \"\\n\".join(lines)\n".into()),
            ("inv/cli.py", "import sys\nfrom inv.parse import parse_lines\nfrom inv.report import report\n\n\ndef main():\n    print(report(parse_lines(sys.stdin.read())))\n".into()),
            ("inv/export.py", "import json\n\n\ndef to_json(items):\n    return json.dumps([i.__dict__ for i in items], sort_keys=True)\n".into()),
            ("inv/util.py", "def chunks(seq, n):\n    for i in range(0, len(seq), n):\n        yield seq[i:i + n]\n".into()),
            ("tests/__init__.py", "".into()),
            ("tests/test_parse.py", "from inv.parse import parse_lines\n\n\ndef test_parse_trims_and_skips():\n    items = parse_lines(\"# header\\n A1 | Bolt | 10 | 25 \\n\\nB2|Nut|3|5\\n\")\n    assert [(i.sku, i.name, i.qty, i.unit_cents) for i in items] == [(\"A1\", \"Bolt\", 10, 25), (\"B2\", \"Nut\", 3, 5)]\n".into()),
            ("tests/test_stock.py", "from inv.models import Item\nfrom inv.stock import low_stock, merge\n\n\ndef test_merge_uses_last_price_and_first_name():\n    items = [Item(\"A\", \"Bolt\", 2, 25), Item(\"A\", \"Bolt (new)\", 3, 30)]\n    (m,) = merge(items)\n    assert (m.name, m.qty, m.unit_cents) == (\"Bolt\", 5, 30)\n\n\ndef test_low_stock_is_strict():\n    items = [Item(\"A\", \"a\", 5, 1), Item(\"B\", \"b\", 4, 1), Item(\"C\", \"c\", 1, 1)]\n    assert [i.sku for i in low_stock(items, 5)] == [\"C\", \"B\"]\n".into()),
            ("tests/test_value.py", "from inv.value import fmt_cents\n\n\ndef test_fmt_pads_cents():\n    assert fmt_cents(1005) == \"$10.05\"\n    assert fmt_cents(123456) == \"$1,234.56\"\n    assert fmt_cents(-5) == \"-$0.05\"\n".into()),
            ("tests/test_report.py", "from inv.parse import parse_lines\nfrom inv.report import report\n\n\ndef test_report_end_to_end():\n    text = \"A1|Bolt|10|25\\nB2|Nut|3|5\\nA1|Bolt|2|30\\n\"\n    assert report(parse_lines(text), threshold=5) == \"A1 Bolt x12 @ $0.30\\nB2 Nut x3 @ $0.05\\ntotal $3.75\\nlow: B2\"\n".into()),
        ],
        check: "python3 -m pytest -q",
        task: "The test suite has several failures. Fix every bug in the inv package so the whole suite passes. Do not change the tests.",
        expect_in_answer: vec![],
        must_not_change: vec!["tests/test_parse.py", "tests/test_stock.py", "tests/test_value.py", "tests/test_report.py"],
    });

    // Borrowing instead of cloning, with lifetimes through two types.
    all.push(Task {
        name: "rs_lifetimes",
        files: vec![
            ("Cargo.toml", "[package]\nname = \"tok\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n".into()),
            ("src/lib.rs", "//! A whitespace tokenizer that currently copies every token.\n\npub struct Tokenizer {\n    text: String,\n}\n\npub struct Token {\n    pub text: String,\n    pub offset: usize,\n}\n\nimpl Tokenizer {\n    pub fn new(text: &str) -> Self {\n        Self { text: text.to_string() }\n    }\n\n    pub fn tokens(&self) -> Vec<Token> {\n        let mut out = Vec::new();\n        let mut offset = 0;\n        for word in self.text.split_whitespace() {\n            let start = self.text[offset..].find(word).unwrap() + offset;\n            out.push(Token { text: word.to_string(), offset: start });\n            offset = start + word.len();\n        }\n        out\n    }\n}\n\npub fn longest(tokens: &[Token]) -> Option<&Token> {\n    tokens.iter().max_by_key(|t| t.text.len())\n}\n".into()),
            ("tests/borrow.rs", "use tok::{longest, Token, Tokenizer};\n\n/// The tokenizer borrows from the caller's text: no String in Token, and\n/// Tokenizer itself only holds a &str.\nfn takes_borrowed<'a>(t: &Token<'a>) -> &'a str {\n    t.text\n}\n\n#[test]\nfn tokens_borrow_from_the_input() {\n    let text = String::from(\"alpha  beta\\tgamma\");\n    let tokenizer = Tokenizer::new(&text);\n    let tokens = tokenizer.tokens();\n    assert_eq!(tokens.iter().map(|t| t.text).collect::<Vec<&str>>(), [\"alpha\", \"beta\", \"gamma\"]);\n    assert_eq!(tokens.iter().map(|t| t.offset).collect::<Vec<_>>(), [0, 7, 12]);\n    let first: &str = takes_borrowed(&tokens[0]);\n    drop(tokenizer);\n    assert_eq!(first, \"alpha\", \"a token outlives the tokenizer, not the text\");\n    assert_eq!(longest(&tokens).unwrap().text, \"alpha\");\n}\n\n#[test]\nfn tokenizer_is_cheap_to_make() {\n    let text = \"x y\";\n    let t = Tokenizer::new(text);\n    assert_eq!(std::mem::size_of_val(&t), std::mem::size_of::<&str>(), \"it holds a &str, not a String\");\n}\n".into()),
        ],
        check: "cargo test -q",
        task: "Make the tokenizer borrow instead of copy: Tokenizer holds a &str, Token's text is a &str borrowed from the original input (with a lifetime parameter), and tokens outlive the Tokenizer. tests/borrow.rs describes it; do not change it.",
        expect_in_answer: vec![],
        must_not_change: vec!["tests/borrow.rs"],
    });

    // Quadratic to linear, under a timer, preserving order.
    all.push(Task {
        name: "py_perf",
        files: vec![
            ("dups.py", "def duplicates(items):\n    \"\"\"Values that appear more than once, in the order of their first\n    appearance, each listed once.\"\"\"\n    out = []\n    for i, x in enumerate(items):\n        if x in items[:i] and x not in out:\n            out.append(x)\n    return out\n".into()),
            ("test_dups.py", "import time\nfrom dups import duplicates\n\n\ndef test_order_and_uniqueness():\n    assert duplicates([3, 1, 3, 2, 1, 3, 4]) == [3, 1]\n    assert duplicates([]) == []\n    assert duplicates([\"a\", \"b\"]) == []\n\n\ndef test_unhashable_are_supported():\n    assert duplicates([[1], [2], [1]]) == [[1]]\n\n\ndef test_fast_enough():\n    items = list(range(60_000)) * 2\n    start = time.perf_counter()\n    out = duplicates(items)\n    assert time.perf_counter() - start < 1.0\n    assert out == list(range(60_000))\n".into()),
        ],
        check: "python3 -m pytest -q",
        task: "test_dups.py fails: duplicates() is too slow on large inputs. Make it fast while keeping its documented behaviour, including support for unhashable items. Do not change the tests.",
        expect_in_answer: vec![],
        must_not_change: vec!["test_dups.py"],
    });

    // A signature change with ten callers.
    let mut files: Vec<(&'static str, String)> = vec![
        ("tool/__init__.py", "".into()),
        ("tool/util.py", "def format_row(cells):\n    \"\"\"Cells joined into one line, each left-justified in 12 columns.\"\"\"\n    return \"\".join(str(c).ljust(12) for c in cells).rstrip()\n".into()),
        ("tool/config.py", "class Config:\n    def __init__(self, width=12):\n        self.width = width\n".into()),
        ("tests/__init__.py", "".into()),
        ("tests/test_commands.py", "from tool.config import Config\nfrom tool.cmd_list import run as list_run\nfrom tool.cmd_status import run as status_run\nfrom tool.cmd_sum import run as sum_run\n\n\ndef test_width_flows_from_config():\n    cfg = Config(width=6)\n    assert list_run(cfg, [\"a\", \"bb\"]) == \"a     bb\"\n    assert status_run(cfg, [\"ok\", 3]) == \"ok    3\"\n    assert sum_run(cfg, [1, 2, 3]) == \"sum   6\"\n\n\ndef test_default_width_is_twelve():\n    cfg = Config()\n    assert list_run(cfg, [\"a\", \"bb\"]) == \"a           bb\"\n".into()),
    ];
    for name in ["list", "status", "sum", "add", "remove", "show", "export", "import", "clean", "help"] {
        let path: &'static str = Box::leak(format!("tool/cmd_{name}.py").into_boxed_str());
        let body = match name {
            "sum" => "from tool.util import format_row\n\n\ndef run(cfg, args):\n    return format_row([\"sum\", sum(args)])\n".to_string(),
            "status" => "from tool.util import format_row\n\n\ndef run(cfg, args):\n    return format_row(list(args))\n".to_string(),
            _ => format!("from tool.util import format_row\n\n\ndef run(cfg, args):\n    # {name}\n    return format_row([str(a) for a in args])\n"),
        };
        files.push((path, body));
    }
    all.push(Task {
        name: "py_ten_callers",
        files,
        check: "python3 -m pytest -q",
        task: "format_row in tool/util.py should take the column width as a parameter, width, defaulting to 12, and every command's run(cfg, args) must pass cfg.width through. Update every caller in the tool package, not just the ones the tests cover. Do not change the tests.",
        expect_in_answer: vec![],
        must_not_change: vec!["tests/test_commands.py"],
    });

    // A new module from a spec with exact output.
    all.push(Task {
        name: "rs_stats_module",
        files: vec![
            ("Cargo.toml", "[package]\nname = \"summ\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n".into()),
            ("src/lib.rs", "//! Numeric summaries. See docs/stats.md for the definitions.\n\npub mod parse;\n\npub use parse::parse_numbers;\n".into()),
            ("src/parse.rs", "/// Whitespace- or comma-separated numbers.\npub fn parse_numbers(text: &str) -> Result<Vec<f64>, String> {\n    text.split(|c: char| c.is_whitespace() || c == ',')\n        .filter(|s| !s.is_empty())\n        .map(|s| s.parse::<f64>().map_err(|e| format!(\"{s:?}: {e}\")))\n        .collect()\n}\n".into()),
            ("docs/stats.md", "# Definitions\n\n- **median**: the middle value of the sorted list; for an even count, the\n  mean of the two middle values. Undefined (None) for an empty list.\n- **percentile(p)** for p in 0..=100: nearest-rank. Sort ascending; the\n  rank is `ceil(p / 100 * n)` clamped to at least 1; the result is the\n  value at that rank (1-based). p = 0 gives the minimum. None for empty.\n- **summary**: a single line `n=<count> min=<a> median=<b> p90=<c> max=<d>`\n  with every number printed with exactly two decimals, or `n=0` alone for\n  an empty list.\n".into()),
            ("tests/stats.rs", "use summ::stats::{median, percentile, summary};\n\n#[test]\nfn median_odd_and_even() {\n    assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));\n    assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));\n    assert_eq!(median(&[]), None);\n}\n\n#[test]\nfn nearest_rank_percentile() {\n    let v = [15.0, 20.0, 35.0, 40.0, 50.0];\n    assert_eq!(percentile(&v, 40), Some(20.0));\n    assert_eq!(percentile(&v, 0), Some(15.0));\n    assert_eq!(percentile(&v, 100), Some(50.0));\n    assert_eq!(percentile(&v, 90), Some(50.0));\n    assert_eq!(percentile(&[], 50), None);\n}\n\n#[test]\nfn summary_line() {\n    assert_eq!(summary(&[1.0, 2.0, 3.0, 4.0]), \"n=4 min=1.00 median=2.50 p90=4.00 max=4.00\");\n    assert_eq!(summary(&[]), \"n=0\");\n}\n".into()),
        ],
        check: "cargo test -q",
        task: "Add a stats module (src/stats.rs, exported from lib.rs) with median, percentile, and summary as defined in docs/stats.md. tests/stats.rs describes the expected results; do not change it.",
        expect_in_answer: vec![],
        must_not_change: vec!["tests/stats.rs"],
    });

    // A schema migration with fixtures on disk.
    all.push(Task {
        name: "py_config_migration",
        files: vec![
            ("cfg.py", "import json\n\n\ndef load(path):\n    \"\"\"Loads a v1 config: {\"name\", \"port\", \"debug\"}.\"\"\"\n    with open(path) as f:\n        data = json.load(f)\n    return {\"name\": data[\"name\"], \"port\": int(data[\"port\"]), \"debug\": bool(data.get(\"debug\", False))}\n".into()),
            ("fixtures/v1.json", "{\"name\": \"api\", \"port\": \"8080\", \"debug\": true}\n".into()),
            ("fixtures/v2.json", "{\"version\": 2, \"service\": {\"name\": \"api\", \"listen\": {\"port\": 8080, \"host\": \"0.0.0.0\"}}, \"flags\": {\"debug\": false, \"trace\": true}}\n".into()),
            ("fixtures/v2-minimal.json", "{\"version\": 2, \"service\": {\"name\": \"worker\", \"listen\": {\"port\": 9000}}}\n".into()),
            ("test_cfg.py", "from cfg import load, migrate_v1\n\n\ndef test_v1_still_loads():\n    assert load(\"fixtures/v1.json\") == {\"name\": \"api\", \"port\": 8080, \"debug\": True, \"host\": \"127.0.0.1\", \"trace\": False}\n\n\ndef test_v2_loads():\n    assert load(\"fixtures/v2.json\") == {\"name\": \"api\", \"port\": 8080, \"debug\": False, \"host\": \"0.0.0.0\", \"trace\": True}\n\n\ndef test_v2_defaults():\n    assert load(\"fixtures/v2-minimal.json\") == {\"name\": \"worker\", \"port\": 9000, \"debug\": False, \"host\": \"127.0.0.1\", \"trace\": False}\n\n\ndef test_migrate_v1_produces_v2_document():\n    v2 = migrate_v1({\"name\": \"api\", \"port\": \"8080\", \"debug\": True})\n    assert v2 == {\"version\": 2, \"service\": {\"name\": \"api\", \"listen\": {\"port\": 8080, \"host\": \"127.0.0.1\"}}, \"flags\": {\"debug\": True, \"trace\": False}}\n".into()),
        ],
        check: "python3 -m pytest -q",
        task: "Configs now come in a v2 shape (see fixtures/). Make load() accept both v1 and v2 files and return the same flat dict for either, with defaults host=\"127.0.0.1\" and trace=False when absent, and add migrate_v1(data) that turns a v1 document into a v2 one. test_cfg.py describes it; do not change it.",
        expect_in_answer: vec![],
        must_not_change: vec!["test_cfg.py"],
    });

    // CRLF line endings and non-ASCII text in the file being edited.
    all.push(Task {
        name: "py_crlf_unicode",
        files: vec![
            ("greet.py", "# -*- coding: utf-8 -*-\r\nGREETINGS = {\r\n    \"en\": \"Hello\",\r\n    \"ja\": \"こんにちは\",\r\n    \"fr\": \"Bonjour\",\r\n    \"emoji\": \"👋\",\r\n}\r\n\r\n\r\ndef greet(lang, name):\r\n    return f\"{GREETINGS.get(lang, GREETINGS['en'])}, {name}!\"\r\n".into()),
            ("test_greet.py", "from greet import greet, GREETINGS\n\n\ndef test_new_language():\n    assert greet(\"es\", \"Ana\") == \"Hola, Ana!\"\n\n\ndef test_others_intact():\n    assert greet(\"ja\", \"Ken\") == \"こんにちは, Ken!\"\n    assert greet(\"emoji\", \"x\") == \"👋, x!\"\n    assert GREETINGS[\"fr\"] == \"Bonjour\"\n\n\ndef test_fallback():\n    assert greet(\"xx\", \"Bo\") == \"Hello, Bo!\"\n".into()),
        ],
        check: "python3 -m pytest -q",
        task: "Add Spanish (\"es\": \"Hola\") to GREETINGS in greet.py. Change nothing else; do not change the tests.",
        expect_in_answer: vec![],
        must_not_change: vec!["test_greet.py"],
    });

    all
}

/// Long tasks: dozens of edits that all have to be consistent, in
/// repositories where reading everything is not an option. A budget that
/// runs out here is a failed task, not a shorter one.
#[allow(clippy::vec_init_then_push)]
fn marathon_tasks() -> Vec<Task> {
    let mut all = Vec::new();

    // Twenty-five commands that each need two consistent changes, with a
    // test that scans every file so a missed one fails.
    let mut files: Vec<(&'static str, String)> = vec![
        ("tool/__init__.py", "".into()),
        ("tool/log.py", "_LINES = []\n\n\ndef log(cmd, msg):\n    _LINES.append(f\"[{cmd}] {msg}\")\n\n\ndef lines():\n    return list(_LINES)\n".into()),
        ("tool/util.py", "def format_row(cells):\n    return \"\".join(str(c).ljust(12) for c in cells).rstrip()\n".into()),
        ("tests/__init__.py", "".into()),
        ("tests/test_all_commands.py", "import importlib\nimport pathlib\nimport re\n\nfrom tool import log\n\nNAMES = sorted(p.stem[4:] for p in pathlib.Path(\"tool\").glob(\"cmd_*.py\"))\n\n\ndef test_there_are_twenty_five():\n    assert len(NAMES) == 25\n\n\ndef test_every_command_logs_its_name_and_takes_verbose():\n    for name in NAMES:\n        mod = importlib.import_module(f\"tool.cmd_{name}\")\n        log._LINES.clear()\n        out = mod.run([\"a\", \"b\"], verbose=False)\n        assert isinstance(out, str)\n        assert log.lines() == [f\"[{name}] run\"], name\n        log._LINES.clear()\n        mod.run([\"a\", \"b\"], verbose=True)\n        assert log.lines() == [f\"[{name}] run\", f\"[{name}] args=['a', 'b']\"], name\n\n\ndef test_no_file_still_has_the_old_signature():\n    for name in NAMES:\n        src = pathlib.Path(f\"tool/cmd_{name}.py\").read_text()\n        assert re.search(r\"def run\\(args, verbose=False\\)\", src), name\n        assert \"from tool.log import log\" in src, name\n".into()),
    ];
    for i in 0..25 {
        let name: &'static str = Box::leak(format!("tool/cmd_c{i:02}.py").into_boxed_str());
        files.push((name, format!("from tool.util import format_row\n\n\ndef run(args):\n    \"\"\"Command c{i:02}.\"\"\"\n    rows = [format_row([a, {i}]) for a in args]\n    return \"\\n\".join(rows)\n")));
    }
    all.push(Task {
        name: "py_25_commands",
        files,
        check: "python3 -m pytest -q",
        task: "Every command module in tool/ (cmd_*.py) must: import log from tool.log; change run(args) to run(args, verbose=False); call log(\"<name>\", \"run\") at the start, where <name> is the part after cmd_; and, when verbose is true, also call log(\"<name>\", f\"args={args!r}\") right after. tests/test_all_commands.py checks all twenty-five; do not change it.",
        expect_in_answer: vec![],
        must_not_change: vec!["tests/test_all_commands.py"],
    });

    // A small domain built from a spec, five functions across three
    // modules, eighteen tests.
    all.push(Task {
        name: "py_orders_domain",
        files: vec![
            ("orders/__init__.py", "".into()),
            ("orders/pricing.py", "\"\"\"See docs/spec.md. Money is in integer cents.\"\"\"\n\n\ndef line_total(qty, unit_cents):\n    raise NotImplementedError\n\n\ndef apply_discount(subtotal_cents, code):\n    raise NotImplementedError\n\n\ndef tax(cents, region):\n    raise NotImplementedError\n".into()),
            ("orders/invoice.py", "\"\"\"See docs/spec.md.\"\"\"\n\n\ndef render(order):\n    raise NotImplementedError\n".into()),
            ("orders/export.py", "\"\"\"See docs/spec.md.\"\"\"\n\n\ndef to_csv(orders):\n    raise NotImplementedError\n".into()),
            ("docs/spec.md", "# Orders\n\nAll money is integer cents. Rounding is half-up on the cent.\n\n## pricing\n\n- `line_total(qty, unit_cents)`: qty * unit_cents. qty < 0 raises ValueError.\n- `apply_discount(subtotal, code)`: `None` -> subtotal. `\"TEN\"` -> 10% off.\n  `\"HALF\"` -> 50% off. `\"FLAT500\"` -> 500 cents off, never below 0. Any other\n  code raises ValueError. Percentages round half-up.\n- `tax(cents, region)`: `\"CA\"` 7.25%, `\"NY\"` 8.875%, `\"OR\"` 0%; other regions\n  raise ValueError. Round half-up.\n\n## invoice\n\n`render(order)` where order is `{\"id\": str, \"region\": str, \"code\": str|None,\n\"lines\": [{\"sku\": str, \"qty\": int, \"unit_cents\": int}, ...]}` returns:\n\n```\nINVOICE <id>\n<sku> x<qty> <line total as $d.cc>        (one per line, in order)\nsubtotal $d.cc\ndiscount -$d.cc                            (only when a code is set; the amount removed)\ntax $d.cc\ntotal $d.cc\n```\n\n`$d.cc` is dollars with thousands separators and exactly two cent digits.\nTotal = subtotal - discount + tax(subtotal - discount).\n\n## export\n\n`to_csv(orders)` returns a string with a header `id,region,lines,total_cents`\nand one row per order, rows separated by `\\n`, no trailing newline. `lines`\nis the number of lines; `total_cents` is the invoice total in cents.\n".into()),
            ("tests/__init__.py", "".into()),
            ("tests/test_pricing.py", "import pytest\nfrom orders.pricing import apply_discount, line_total, tax\n\n\ndef test_line_total():\n    assert line_total(3, 250) == 750\n    with pytest.raises(ValueError):\n        line_total(-1, 100)\n\n\ndef test_discounts():\n    assert apply_discount(1000, None) == 1000\n    assert apply_discount(1000, \"TEN\") == 900\n    assert apply_discount(1005, \"TEN\") == 905\n    assert apply_discount(999, \"HALF\") == 500\n    assert apply_discount(300, \"FLAT500\") == 0\n    with pytest.raises(ValueError):\n        apply_discount(100, \"NOPE\")\n\n\ndef test_tax():\n    assert tax(10000, \"CA\") == 725\n    assert tax(10000, \"NY\") == 888\n    assert tax(10001, \"NY\") == 888\n    assert tax(10000, \"OR\") == 0\n    with pytest.raises(ValueError):\n        tax(1, \"ZZ\")\n".into()),
            ("tests/test_invoice.py", "from orders.invoice import render\n\n\ndef order():\n    return {\"id\": \"A-1\", \"region\": \"CA\", \"code\": \"TEN\", \"lines\": [{\"sku\": \"bolt\", \"qty\": 4, \"unit_cents\": 250}, {\"sku\": \"nut\", \"qty\": 1, \"unit_cents\": 123456}]}\n\n\ndef test_render_with_discount():\n    assert render(order()) == \"INVOICE A-1\\nbolt x4 $10.00\\nnut x1 $1,234.56\\nsubtotal $1,244.56\\ndiscount -$124.46\\ntax $81.21\\ntotal $1,201.31\"\n\n\ndef test_render_without_discount():\n    o = order()\n    o[\"code\"] = None\n    o[\"region\"] = \"OR\"\n    assert render(o) == \"INVOICE A-1\\nbolt x4 $10.00\\nnut x1 $1,234.56\\nsubtotal $1,244.56\\ntax $0.00\\ntotal $1,244.56\"\n".into()),
            ("tests/test_export.py", "from orders.export import to_csv\n\n\ndef test_csv():\n    orders = [\n        {\"id\": \"A\", \"region\": \"OR\", \"code\": None, \"lines\": [{\"sku\": \"x\", \"qty\": 2, \"unit_cents\": 100}]},\n        {\"id\": \"B\", \"region\": \"CA\", \"code\": \"HALF\", \"lines\": [{\"sku\": \"x\", \"qty\": 1, \"unit_cents\": 1000}, {\"sku\": \"y\", \"qty\": 1, \"unit_cents\": 1}]},\n    ]\n    assert to_csv(orders) == \"id,region,lines,total_cents\\nA,OR,1,200\\nB,CA,2,537\"\n".into()),
        ],
        check: "python3 -m pytest -q",
        task: "Implement the orders package according to docs/spec.md so that the whole test suite passes. Do not change the tests.",
        expect_in_answer: vec![],
        must_not_change: vec!["tests/test_pricing.py", "tests/test_invoice.py", "tests/test_export.py"],
    });

    // A trait change that every one of twelve implementations must follow.
    let mut files: Vec<(&'static str, String)> = vec![
        ("Cargo.toml", "[package]\nname = \"plugins\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n".into()),
    ];
    let mut lib = String::from("pub trait Plugin {\n    fn name(&self) -> &'static str;\n    fn run(&self, input: &str) -> String;\n}\n\n");
    let mut all_fn = String::from("pub fn all() -> Vec<Box<dyn Plugin>> {\n    vec![\n");
    for i in 0..12 {
        let m = format!("p{i:02}");
        lib.push_str(&format!("pub mod {m};\n"));
        all_fn.push_str(&format!("        Box::new({m}::P{i:02}),\n"));
        let path: &'static str = Box::leak(format!("src/{m}.rs").into_boxed_str());
        files.push((path, format!("use crate::Plugin;\n\npub struct P{i:02};\n\nimpl Plugin for P{i:02} {{\n    fn name(&self) -> &'static str {{\n        \"{m}\"\n    }}\n    fn run(&self, input: &str) -> String {{\n        format!(\"{m}:{{}}\", input.len() + {i})\n    }}\n}}\n")));
    }
    all_fn.push_str("    ]\n}\n");
    lib.push('\n');
    lib.push_str(&all_fn);
    files.push(("src/lib.rs", lib));
    files.push(("tests/versions.rs", "use plugins::{all, Plugin};\n\n#[test]\nfn every_plugin_reports_a_version_and_describes_itself() {\n    for (i, p) in all().iter().enumerate() {\n        assert_eq!(p.version(), (i as u32 + 1) * 10, \"{}\", p.name());\n        assert_eq!(p.describe(), format!(\"{} v{}\", p.name(), p.version()));\n    }\n}\n\n#[test]\nfn describe_is_a_trait_method_with_a_default() {\n    struct Custom;\n    impl Plugin for Custom {\n        fn name(&self) -> &'static str {\n            \"custom\"\n        }\n        fn run(&self, _: &str) -> String {\n            String::new()\n        }\n        fn version(&self) -> u32 {\n            7\n        }\n    }\n    assert_eq!(Custom.describe(), \"custom v7\");\n}\n".into()));
    all.push(Task {
        name: "rs_twelve_plugins",
        files,
        check: "cargo test -q",
        task: "Add a required version() -> u32 method to the Plugin trait and a describe() method with a default implementation returning \"<name> v<version>\". Plugin p00 has version 10, p01 has 20, and so on up to p11 with 120. tests/versions.rs describes it; do not change it.",
        expect_in_answer: vec![],
        must_not_change: vec!["tests/versions.rs"],
    });

    all
}

/// Counts what the model was sent, provider-independently.
struct Metered<'a> {
    inner: &'a dyn Provider,
    turns: Cell<usize>,
    prompt_bytes: Cell<usize>,
    output_bytes: Cell<usize>,
}

impl Provider for Metered<'_> {
    fn label(&self) -> String {
        self.inner.label()
    }
    fn turn(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        max_tokens: u32,
    ) -> Result<Reply, String> {
        self.turns.set(self.turns.get() + 1);
        let sent: usize = system.len()
            + messages
                .iter()
                .map(|m| match m {
                    Message::User(t) => t.len(),
                    Message::Assistant { text, calls } => {
                        text.len() + calls.iter().map(|c| c.input.to_string().len()).sum::<usize>()
                    }
                    Message::ToolResults(r) => r.iter().map(|r| r.content.len()).sum(),
                })
                .sum::<usize>()
            + tools.iter().map(|t| t.description.len() + t.schema.to_string().len()).sum::<usize>();
        self.prompt_bytes.set(self.prompt_bytes.get() + sent);
        let reply = self.inner.turn(system, messages, tools, max_tokens)?;
        self.output_bytes.set(
            self.output_bytes.get()
                + reply.text.len()
                + reply.calls.iter().map(|c| c.input.to_string().len()).sum::<usize>(),
        );
        Ok(reply)
    }
}

#[derive(serde::Serialize)]
struct Row {
    task: String,
    repeat: usize,
    success: bool,
    turns: usize,
    tool_calls: usize,
    prompt_bytes: usize,
    output_bytes: usize,
    seconds: f64,
    truncated: bool,
    error: Option<String>,
    /// Lines the run added and removed, per git, new files included.
    lines_added: usize,
    lines_removed: usize,
    files_changed: usize,
    /// Tool calls that came back as errors: a wrong path, an edit that did
    /// not match, a check the run could not use.
    tool_errors: usize,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
}

fn main() {
    let model = std::env::var("EVAL_MODEL").unwrap_or_else(|_| "claude-haiku-4-5-20251001".into());
    let repeats: usize = std::env::var("EVAL_REPEATS").ok().and_then(|r| r.parse().ok()).unwrap_or(1);
    let out_path = std::env::var("EVAL_OUT").unwrap_or_else(|_| "agent-eval.json".into());
    let only: Vec<String> = std::env::args().skip(1).collect();
    let verbose = std::env::var("EVAL_VERBOSE").is_ok();

    let client: Box<dyn Provider> = if std::env::var("EVAL_PROVIDER").as_deref() == Ok("ollama") {
        let url = std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://localhost:11434".into());
        Box::new(git_manage::ollama::Client::new(&url).agent(model.clone()))
    } else {
        match git_manage::claude::Client::from_store(model.clone()) {
            Some(c) => Box::new(c),
            None => {
                eprintln!("Claude is not signed in; sign in through the app first.");
                std::process::exit(2);
            }
        }
    };
    let base = tempfile::tempdir().expect("tempdir");
    let mut rows: Vec<Row> = Vec::new();
    let suite = match std::env::var("EVAL_SET").as_deref() {
        Ok("hard") => hard_tasks(),
        Ok("brutal") => brutal_tasks(),
        Ok("marathon") => marathon_tasks(),
        Ok("all") => tasks()
            .into_iter()
            .chain(hard_tasks())
            .chain(brutal_tasks())
            .chain(marathon_tasks())
            .collect(),
        _ => tasks(),
    };

    for task in suite.into_iter().filter(|t| only.is_empty() || only.iter().any(|o| o == t.name)) {
        for repeat in 0..repeats {
            let dir = base.path().join(format!("{}-{repeat}", task.name));
            fs::create_dir_all(&dir).unwrap();
            for (path, body) in &task.files {
                let full = dir.join(path);
                fs::create_dir_all(full.parent().unwrap()).unwrap();
                fs::write(full, body).unwrap();
            }
            fs::write(
                dir.join(".git-manage-ci.toml"),
                format!("[[job]]\nname = \"tests\"\ncommands = [\"{}\"]\n", task.check),
            )
            .unwrap();
            // Build output must not count as a change the agent made.
            fs::write(dir.join(".gitignore"), "target/\nCargo.lock\n__pycache__/\n.pytest_cache/\n").unwrap();
            git(&dir, &["init", "-q", "-b", "main"]);
            git(&dir, &["config", "user.email", "eval@example.invalid"]);
            git(&dir, &["config", "user.name", "eval"]);
            git(&dir, &["add", "-A"]);
            git(&dir, &["commit", "-q", "-m", "init"]);
            let repo = Repo::open(&dir).unwrap();
            let jobs = git_manage::local_ci::discover_configs(repo.path()).unwrap().config.jobs;
            let mut ws = Workspace::new(repo.path(), repo.tracked_files().unwrap(), Access::ReadWrite)
                .unwrap()
                .with_write_mode(WriteMode::Live)
                .with_checks(jobs);

            let metered = Metered {
                inner: client.as_ref(),
                turns: Cell::new(0),
                prompt_bytes: Cell::new(0),
                output_bytes: Cell::new(0),
            };
            let started = Instant::now();
            let mut tool_errors = 0usize;
            let result = coding::run(
                &metered,
                &mut ws,
                coding::Request { branch: Some("main"), ..coding::Request::new(task.task) },
                &mut |event: Event| {
                    if let Event::Tool { is_error: true, .. } = &event {
                        tool_errors += 1;
                    }
                    if verbose {
                        eprintln!("    {}", event.line());
                    }
                },
            );
            let seconds = started.elapsed().as_secs_f64();

            let (success, truncated, error, usage) = match &result {
                Ok(run) => {
                    let mut ok = if task.is_question() {
                        task.expect_in_answer.iter().all(|s| run.text.contains(s))
                    } else {
                        // Graded by the check, run here, never by the summary.
                        let (ok, _) = sh(&dir, "sh", &["-c", task.check]);
                        ok
                    };
                    // Editing the tests is a fail even when they then pass.
                    for path in &task.must_not_change {
                        let original = task.files.iter().find(|(p, _)| p == path).map(|(_, b)| b.clone());
                        let now = fs::read_to_string(dir.join(path)).ok();
                        if original != now {
                            if verbose {
                                eprintln!("    {path} was modified");
                            }
                            ok = false;
                        }
                    }
                    (ok, run.truncated, None, run.usage)
                }
                Err(e) => (false, false, Some(e.clone()), Default::default()),
            };
            if verbose {
                if let Ok(run) = &result {
                    eprintln!("--- {} answer ---\n{}\n", task.name, run.text);
                }
            }
            // The size of the change, from git: a passing run that touched
            // half the repository is worse than one that touched a line.
            git(&dir, &["add", "-A"]);
            let (_, numstat) = sh(&dir, "git", &["diff", "--cached", "--numstat"]);
            let mut lines_added = 0;
            let mut lines_removed = 0;
            let mut files_changed = 0;
            for line in numstat.lines() {
                let mut parts = line.split('\t');
                let a = parts.next().and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
                let r = parts.next().and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
                lines_added += a;
                lines_removed += r;
                files_changed += 1;
            }
            let row = Row {
                task: task.name.into(),
                repeat,
                success,
                turns: metered.turns.get(),
                tool_calls: ws.calls_used(),
                prompt_bytes: metered.prompt_bytes.get(),
                output_bytes: metered.output_bytes.get(),
                seconds,
                truncated,
                error,
                lines_added,
                lines_removed,
                files_changed,
                tool_errors,
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
            };
            println!(
                "{:<13} #{repeat} {} turns={:<3} calls={:<3} errs={:<2} prompt={:>7}B out={:>6}B diff=+{}/-{} in {} file(s) {:>6.1}s{}{}",
                row.task,
                if row.success { "PASS" } else { "FAIL" },
                row.turns,
                row.tool_calls,
                row.tool_errors,
                row.prompt_bytes,
                row.output_bytes,
                row.lines_added,
                row.lines_removed,
                row.files_changed,
                row.seconds,
                if row.truncated { " truncated" } else { "" },
                row.error.as_ref().map(|e| format!(" error: {e}")).unwrap_or_default(),
            );
            rows.push(row);
        }
    }

    let n = rows.len().max(1) as f64;
    let passed = rows.iter().filter(|r| r.success).count();
    let mean = |f: &dyn Fn(&Row) -> f64| rows.iter().map(f).sum::<f64>() / n;
    println!("\nmodel: {model}");
    println!("passed: {passed}/{} ({:.0}%)", rows.len(), 100.0 * passed as f64 / n);
    println!("mean turns: {:.1}", mean(&|r| r.turns as f64));
    println!("mean tool calls: {:.1}", mean(&|r| r.tool_calls as f64));
    println!("mean tool errors: {:.2}", mean(&|r| r.tool_errors as f64));
    println!("mean prompt bytes: {:.0}", mean(&|r| r.prompt_bytes as f64));
    println!("mean seconds: {:.1}", mean(&|r| r.seconds));
    println!(
        "mean diff: +{:.1}/-{:.1} lines in {:.1} file(s)",
        mean(&|r| r.lines_added as f64),
        mean(&|r| r.lines_removed as f64),
        mean(&|r| r.files_changed as f64)
    );
    let input: u64 = rows.iter().map(|r| r.input_tokens).sum();
    let cached: u64 = rows.iter().map(|r| r.cache_read_tokens).sum();
    let written: u64 = rows.iter().map(|r| r.cache_write_tokens).sum();
    let output: u64 = rows.iter().map(|r| r.output_tokens).sum();
    if input + cached + written > 0 {
        println!(
            "tokens: input {input} (uncached) + {cached} cache reads + {written} cache writes; output {output}"
        );
    }
    fs::write(&out_path, serde_json::to_string_pretty(&rows).unwrap()).unwrap();
    println!("wrote {out_path}");
}
