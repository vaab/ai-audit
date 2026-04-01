#!/usr/bin/env python3
"""Extract filter match/no-match info from run-fails.log files."""

import argparse
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class Criterion:
    index: int
    kind: str  # "TextContains" or "ToolFieldEquals"
    text: str  # the full criterion description


@dataclass
class Filter:
    index: int
    depth: int
    criteria_count: int
    criteria: list[Criterion] = field(default_factory=list)


@dataclass
class SessionFilterResult:
    session_id: str
    filter_index: int
    depth: int
    matched: bool


@dataclass
class PaneResult:
    pane: str
    filters: list[Filter] = field(default_factory=list)
    sessions_searched: int = 0
    filter_count: int = 0
    session_results: list[SessionFilterResult] = field(default_factory=list)
    final_match: str | None = None  # session id if matched, None if failed
    error: str | None = None  # error message if no filters/messages


# --- regexes (compiled once) ---

RE_BUILD_FILTERS = re.compile(r"build_filters: built (\d+) filters")
RE_FILTER_DEF = re.compile(r"filter\[(\d+)\]: depth=(\d+), criteria=(\d+)")
RE_CRITERION = re.compile(r"criterion\[(\d+)\]: (.+)")
RE_SEARCH = re.compile(r"find_session_by_filters: searching (\d+) sessions")
RE_MATCH_RESULT = re.compile(
    r"session_matches_filters: session=(ses_\w+), "
    r"filter\[(\d+)\] depth=(\d+) => (MATCH|NO MATCH)"
)
RE_FINAL_NO_MATCH = re.compile(r"find_session_by_filters: no match found")
RE_FINAL_MATCH = re.compile(r"find_session_by_filters: match found: (ses_\w+)")
RE_NO_MESSAGES = re.compile(r"No messages parsed")


def parse_log(pane_name: str, log_path: Path) -> PaneResult:
    result = PaneResult(pane=pane_name)
    current_filter: Filter | None = None

    with open(log_path, "r", errors="replace") as f:
        for line in f:
            m = RE_BUILD_FILTERS.search(line)
            if m:
                result.filter_count = int(m.group(1))
                continue

            m = RE_FILTER_DEF.search(line)
            if m:
                current_filter = Filter(
                    index=int(m.group(1)),
                    depth=int(m.group(2)),
                    criteria_count=int(m.group(3)),
                )
                result.filters.append(current_filter)
                continue

            m = RE_CRITERION.search(line)
            if m and current_filter is not None:
                idx = int(m.group(1))
                raw = m.group(2).strip()
                # classify criterion kind
                if raw.startswith("TextContains"):
                    kind = "TextContains"
                elif raw.startswith("ToolFieldEquals"):
                    kind = "ToolFieldEquals"
                else:
                    kind = "Unknown"
                current_filter.criteria.append(
                    Criterion(index=idx, kind=kind, text=raw)
                )
                continue

            m = RE_SEARCH.search(line)
            if m:
                result.sessions_searched = int(m.group(1))
                continue

            m = RE_MATCH_RESULT.search(line)
            if m:
                result.session_results.append(
                    SessionFilterResult(
                        session_id=m.group(1),
                        filter_index=int(m.group(2)),
                        depth=int(m.group(3)),
                        matched=(m.group(4) == "MATCH"),
                    )
                )
                continue

            m = RE_FINAL_MATCH.search(line)
            if m:
                result.final_match = m.group(1)
                continue

            if RE_FINAL_NO_MATCH.search(line):
                result.final_match = None
                continue

            if RE_NO_MESSAGES.search(line):
                result.error = "No messages parsed from scrollback"
                continue

    return result


def truncate(s: str, max_len: int = 100) -> str:
    if len(s) <= max_len:
        return s
    return s[: max_len - 3] + "..."


def format_criterion(c: Criterion, indent: str = "      ") -> str:
    return f"{indent}criterion[{c.index}]: {truncate(c.text, 120)}"


def format_result(r: PaneResult) -> str:
    """Format a single pane result as a string."""
    lines: list[str] = []

    lines.append(f"=== {r.pane} ===")

    if r.error:
        lines.append(f"  ERROR: {r.error}")
        lines.append("")
        return "\n".join(lines)

    # filters
    lines.append(f"  Filters: {r.filter_count}")
    for f in r.filters:
        lines.append(
            f"    filter[{f.index}] depth={f.depth} ({f.criteria_count} criteria):"
        )
        for c in f.criteria:
            lines.append(format_criterion(c))

    lines.append(f"  Sessions searched: {r.sessions_searched}")

    # find sessions that had at least one MATCH
    matched_sessions: dict[str, list[SessionFilterResult]] = {}
    for sr in r.session_results:
        if sr.matched:
            matched_sessions.setdefault(sr.session_id, []).append(sr)

    # find which filters those sessions failed on
    failed_filters: dict[str, list[SessionFilterResult]] = {}
    for sr in r.session_results:
        if sr.session_id in matched_sessions and not sr.matched:
            failed_filters.setdefault(sr.session_id, []).append(sr)

    if matched_sessions:
        lines.append(f"  Partial matches ({len(matched_sessions)} sessions):")
        for sid, matches in matched_sessions.items():
            match_strs = [f"filter[{m.filter_index}] depth={m.depth}" for m in matches]
            lines.append(f"    {sid}:")
            lines.append(f"      MATCHED:    {', '.join(match_strs)}")
            if sid in failed_filters:
                fail_strs = [
                    f"filter[{m.filter_index}] depth={m.depth}"
                    for m in failed_filters[sid]
                ]
                lines.append(f"      NO MATCH:   {', '.join(fail_strs)}")
    else:
        lines.append("  Partial matches: none")

    # count how many unique sessions were tested per filter
    tested_per_filter: dict[int, int] = {}
    no_match_per_filter: dict[int, int] = {}
    match_per_filter: dict[int, int] = {}
    for sr in r.session_results:
        tested_per_filter[sr.filter_index] = (
            tested_per_filter.get(sr.filter_index, 0) + 1
        )
        if sr.matched:
            match_per_filter[sr.filter_index] = (
                match_per_filter.get(sr.filter_index, 0) + 1
            )
        else:
            no_match_per_filter[sr.filter_index] = (
                no_match_per_filter.get(sr.filter_index, 0) + 1
            )

    if tested_per_filter:
        lines.append("  Filter hit rates:")
        for fi in sorted(tested_per_filter):
            total = tested_per_filter[fi]
            matched = match_per_filter.get(fi, 0)
            lines.append(f"    filter[{fi}]: {matched}/{total} sessions matched")

    # result
    if r.final_match:
        lines.append(f"  Result: MATCHED {r.final_match}")
    else:
        lines.append("  Result: FAILED")

    lines.append("")
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Extract filter match/no-match info from run-fails.log files."
    )
    parser.add_argument(
        "base_dir",
        nargs="?",
        default=None,
        help="Base directory containing pane subdirs (default: script's parent dir)",
    )
    parser.add_argument(
        "--save",
        action="store_true",
        help="Write analysis.txt in each pane subdirectory",
    )
    args = parser.parse_args()

    base_dir = Path(args.base_dir) if args.base_dir else Path(__file__).parent

    pane_dirs = sorted(
        d for d in base_dir.iterdir() if d.is_dir() and (d / "run-fails.log").exists()
    )

    if not pane_dirs:
        print(
            f"No pane directories with run-fails.log found in {base_dir}",
            file=sys.stderr,
        )
        sys.exit(1)

    results: list[tuple[Path, PaneResult]] = []
    for pane_dir in pane_dirs:
        log_path = pane_dir / "run-fails.log"
        result = parse_log(pane_dir.name, log_path)
        results.append((pane_dir, result))

    # print all and optionally save per-folder
    for pane_dir, r in results:
        text = format_result(r)
        print(text)
        if args.save:
            out_path = pane_dir / "analysis.txt"
            out_path.write_text(text + "\n")

    # summary
    total = len(results)
    errors = sum(1 for _, r in results if r.error)
    failed = sum(1 for _, r in results if not r.final_match and not r.error)
    matched = sum(1 for _, r in results if r.final_match)
    partial = sum(
        1
        for _, r in results
        if not r.final_match
        and not r.error
        and any(sr.matched for sr in r.session_results)
    )

    summary = (
        f"{'=' * 60}\n"
        f"SUMMARY: {total} panes — {matched} matched, {failed} failed "
        f"({partial} with partial matches), {errors} errors"
    )
    print(summary)

    if args.save:
        saved = [p.name for p, _ in results]
        print(f"\nSaved analysis.txt in {len(saved)} directories.")


if __name__ == "__main__":
    main()
