#!/usr/bin/env python3
"""Build the FigCraft Cloud project site (fork addition).

Usage: python3 site/build.py OUTPUT_DIR

Reads the page sources in site/, injects the shared header and footer from site/partials/,
renders the live progress sections from docs/saas/tracker.md (and milestone descriptions from
docs/saas/roadmap.md), copies the images the pages reference from docs/images/ and
assets/app-icon/, and writes the finished static site into OUTPUT_DIR.

Standard library only. Fails with a non-zero exit status when the tracker table cannot be parsed,
when a page references a missing image, or when a link to a repository file points at a file that
does not exist, so a broken site never deploys silently.
"""

from __future__ import annotations

import html
import json
import re
import shutil
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SITE = ROOT / "site"
TRACKER = ROOT / "docs" / "saas" / "tracker.md"
ROADMAP = ROOT / "docs" / "saas" / "roadmap.md"

PAGES = ["index.html", "why.html", "changes.html", "roadmap.html", "docs.html"]
IMAGE_DIRS = [ROOT / "docs" / "images", ROOT / "assets" / "app-icon"]
REPO_BLOB = "https://github.com/schnsrw/figcraft/blob/main/"
BASE_URL = "https://schnsrw.github.io/figcraft/"
SOCIAL_IMAGE = "shot-7-collab.png"
# Repository files served whole in llms-full.txt (plain Markdown, in this order).
LLMS_FULL_DOCS = [
    "README.md",
    "docs/saas/README.md",
    "docs/saas/product-requirements.md",
    "docs/saas/architecture.md",
    "docs/saas/security.md",
    "docs/saas/deployment-operations.md",
    "docs/saas/branding-and-configuration.md",
    "docs/saas/roadmap.md",
]
MAX_LLMS_FULL_BYTES = 2_000_000

MAX_SOURCE_BYTES = 4 * 1024 * 1024  # tracker and pages are far smaller; refuse anything absurd
MAX_ROWS = 40
MAX_TASKS = 200
MAX_TEXT = 400

# Milestone ids in the order the roadmap presents them. The Core slice was pulled forward and
# sits between S3 and S4, as in the tracker.
EXPECTED_IDS = ["S0", "S1", "S2", "S3", "CORE", "S4", "S5", "S6", "S7", "S8", "S9"]


class BuildError(Exception):
    pass


def esc(text: str) -> str:
    return html.escape(text, quote=True)


def plain(text: str) -> str:
    """Strip Markdown emphasis and code ticks, normalise dashes, and bound the length."""
    text = re.sub(r"\*\*|`", "", text)
    text = text.replace("—", "-").replace("–", "-")
    text = " ".join(text.split())
    if len(text) > MAX_TEXT:
        text = text[: MAX_TEXT - 3].rstrip() + "..."
    return text


def read_text(path: Path) -> str:
    try:
        size = path.stat().st_size
    except OSError as err:
        raise BuildError(f"cannot read {path}: {err}") from err
    if size > MAX_SOURCE_BYTES:
        raise BuildError(f"{path} is unexpectedly large ({size} bytes)")
    return path.read_text(encoding="utf-8")


# --------------------------------------------------------------------------------------------
# Tracker parsing
# --------------------------------------------------------------------------------------------

ROW_RE = re.compile(
    r"^\|\s*(?P<name>[^|]+?)\s*\|\s*(?P<status>[^|]+?)\s*\|\s*(?P<done>\d{1,4})\s*/\s*(?P<total>\d{1,4})\s*\|\s*(?P<gate>[^|]*?)\s*\|\s*$"
)
UPDATED_RE = re.compile(r"\*\*Last updated:\*\*\s*(\d{4}-\d{2}-\d{2})")
TASK_RE = re.compile(r"^- \[(?P<mark>[ xX])\] \*\*(?P<id>[A-Z0-9]+-[A-Z0-9]+)\*\*\s+(?P<title>.+)$")


def parse_milestones(text: str) -> list[dict]:
    start = text.find("## Milestone status")
    if start < 0:
        raise BuildError("tracker: no '## Milestone status' section")
    section = text[start:]
    nxt = section.find("\n## ", 5)
    if nxt > 0:
        section = section[:nxt]

    rows = []
    for line in section.splitlines():
        line = line.strip()
        if not line.startswith("|") or line.startswith("|---") or line.startswith("| Milestone"):
            continue
        m = ROW_RE.match(line)
        if not m:
            raise BuildError(f"tracker: cannot parse milestone row: {line[:120]!r}")
        raw_name = m.group("name")
        if raw_name.startswith("**Core slice**"):
            mid = "CORE"
            label = "Core slice"
            title = plain(raw_name[len("**Core slice**") :])
        else:
            parts = raw_name.split(None, 1)
            mid = parts[0]
            label = parts[0]
            title = plain(parts[1]) if len(parts) > 1 else ""
            if not re.fullmatch(r"S\d", mid):
                raise BuildError(f"tracker: unexpected milestone id {mid!r}")
        done = int(m.group("done"))
        total = int(m.group("total"))
        if total <= 0 or done < 0 or done > total:
            raise BuildError(f"tracker: impossible count {done}/{total} for {mid}")
        status = plain(m.group("status")).lower()
        if status not in {"complete", "in progress", "not started"}:
            raise BuildError(f"tracker: unknown status {status!r} for {mid}")
        rows.append(
            {
                "id": mid,
                "label": label,
                "title": title,
                "status": status,
                "done": done,
                "total": total,
                "gate": plain(m.group("gate")),
            }
        )
        if len(rows) > MAX_ROWS:
            raise BuildError("tracker: too many milestone rows")

    ids = [r["id"] for r in rows]
    if sorted(ids) != sorted(EXPECTED_IDS):
        raise BuildError(f"tracker: expected milestones {EXPECTED_IDS}, found {ids}")
    order = {mid: i for i, mid in enumerate(EXPECTED_IDS)}
    rows.sort(key=lambda r: order[r["id"]])
    return rows


def parse_updated(text: str) -> str:
    m = UPDATED_RE.search(text[:2000])
    if not m:
        raise BuildError("tracker: no '**Last updated:** YYYY-MM-DD' in the status line")
    return m.group(1)


def parse_tasks(text: str, milestone: str) -> list[dict]:
    """Return the task lines (first line of each) of one milestone section, e.g. 'S5'."""
    heading = re.search(rf"^## {re.escape(milestone)}\b.*$", text, re.M)
    if not heading:
        raise BuildError(f"tracker: no section for {milestone}")
    body = text[heading.end() :]
    nxt = re.search(r"^## ", body, re.M)
    if nxt:
        body = body[: nxt.start()]
    tasks = []
    for line in body.splitlines():
        m = TASK_RE.match(line.strip())
        if not m or m.group("id").endswith("GATE"):
            continue
        title = m.group("title")
        title = re.split(r"\s+Depends on\b", title, maxsplit=1)[0]
        tasks.append({"id": m.group("id"), "done": m.group("mark") != " ", "title": plain(title)})
        if len(tasks) > MAX_TASKS:
            raise BuildError(f"tracker: too many tasks in {milestone}")
    if not tasks:
        raise BuildError(f"tracker: no tasks found for {milestone}")
    return tasks


def parse_roadmap(text: str) -> dict[str, dict]:
    """Milestone summary table: | ID | Milestone | Estimate | Depends on | Exit outcome |."""
    start = text.find("## Milestone summary")
    if start < 0:
        raise BuildError("roadmap: no '## Milestone summary' section")
    out = {}
    for line in text[start:].splitlines()[1:]:
        if line.startswith("## "):
            break
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) != 5 or not re.fullmatch(r"S\d", cells[0]):
            continue
        out[cells[0]] = {"estimate": plain(cells[2]), "depends": plain(cells[3]), "outcome": plain(cells[4])}
    missing = [m for m in EXPECTED_IDS if m != "CORE" and m not in out]
    if missing:
        raise BuildError(f"roadmap: milestone summary rows missing for {missing}")
    return out


# --------------------------------------------------------------------------------------------
# Rendering
# --------------------------------------------------------------------------------------------

STATUS_CLASS = {"complete": "is-done", "in progress": "is-active", "not started": "is-todo"}


def pct(done: int, total: int) -> int:
    return round(100 * done / total) if total else 0


def render_progress(rows, roadmap, current_tasks, current_id, updated) -> str:
    done = sum(r["done"] for r in rows)
    total = sum(r["total"] for r in rows)
    complete = sum(1 for r in rows if r["status"] == "complete")
    out = []
    out.append('<div class="progress-summary reveal">')
    out.append(
        f'<p class="big-number"><span>{pct(done, total)}%</span> of planned tasks complete</p>'
    )
    out.append(
        f'<p class="muted">{done} of {total} tracked tasks across {len(rows)} milestones; '
        f"{complete} milestones complete. Generated from "
        f'<a href="{REPO_BLOB}docs/saas/tracker.md">docs/saas/tracker.md</a>, '
        f'updated <time datetime="{esc(updated)}">{esc(updated)}</time>.</p>'
    )
    out.append("</div>")
    out.append('<ol class="milestones">')
    for i, r in enumerate(rows):
        p = pct(r["done"], r["total"])
        info = roadmap.get(r["id"], {})
        desc = info.get("outcome") or r["gate"]
        cls = STATUS_CLASS[r["status"]]
        label = "Core" if r["id"] == "CORE" else r["id"]
        out.append(f'<li class="milestone reveal {cls}" style="--p:{p / 100:.3f};--i:{i}">')
        out.append(f'<div class="ms-id" aria-hidden="true">{esc(label)}</div>')
        out.append('<div class="ms-body">')
        out.append(
            f'<h3><span class="visually-hidden">{esc(r["label"])}: </span>{esc(r["title"])}</h3>'
        )
        out.append(f'<p class="ms-desc">{esc(desc)}</p>')
        out.append(
            f'<div class="bar" role="img" aria-label="{esc(r["label"])}: {r["done"]} of {r["total"]} tasks, {p}%">'
            '<span class="bar-fill"></span></div>'
        )
        meta = [
            f'<span class="pill {cls}">{esc(r["status"])}</span>',
            f'<span class="ms-count">{r["done"]} / {r["total"]} tasks</span>',
        ]
        if info.get("estimate"):
            meta.append(f'<span class="ms-est">Estimate {esc(info["estimate"])}</span>')
        out.append('<p class="ms-meta">' + "".join(meta) + "</p>")
        out.append(f'<p class="ms-gate"><strong>Gate:</strong> {esc(r["gate"])}</p>')
        out.append("</div></li>")
    out.append("</ol>")

    if current_tasks:
        cur = next(r for r in rows if r["id"] == current_id)
        open_tasks = [t for t in current_tasks if not t["done"]]
        done_tasks = [t for t in current_tasks if t["done"]]
        out.append('<div class="current reveal">')
        out.append(
            f'<h3 id="current-milestone">Now building: {esc(cur["label"])} {esc(cur["title"])}</h3>'
        )
        out.append(
            f'<p class="muted">{len(done_tasks)} done, {len(open_tasks)} open. '
            "Read straight from the tracker at build time.</p>"
        )
        out.append('<ul class="tasklist">')
        for t in done_tasks + open_tasks:
            state = "done" if t["done"] else "open"
            word = "Done" if t["done"] else "Open"
            out.append(
                f'<li class="task task-{state}"><span class="visually-hidden">{word}: </span>'
                f'<code>{esc(t["id"])}</code> {esc(t["title"])}</li>'
            )
        out.append("</ul></div>")
    return "\n".join(out)


def render_strip(rows, updated) -> str:
    done = sum(r["done"] for r in rows)
    total = sum(r["total"] for r in rows)
    p = pct(done, total)
    out = ['<div class="strip reveal">']
    out.append(
        f'<p class="strip-head"><span class="big-number"><span>{p}%</span></span> '
        f"<span>{done} of {total} planned tasks done. Updated "
        f'<time datetime="{esc(updated)}">{esc(updated)}</time>.</span></p>'
    )
    out.append('<ol class="strip-cells">')
    for i, r in enumerate(rows):
        rp = pct(r["done"], r["total"])
        cls = STATUS_CLASS[r["status"]]
        label = "Core" if r["id"] == "CORE" else r["id"]
        out.append(
            f'<li class="strip-cell {cls}" style="--p:{rp / 100:.3f};--i:{i}">'
            f'<span class="strip-bar" aria-hidden="true"><span class="bar-fill"></span></span>'
            f'<span class="strip-label">{esc(label)}</span>'
            f'<span class="visually-hidden">{esc(r["title"])}: {r["done"]} of {r["total"]} tasks, {esc(r["status"])}</span>'
            "</li>"
        )
    out.append("</ol></div>")
    return "\n".join(out)


# --------------------------------------------------------------------------------------------
# Assembly and checks
# --------------------------------------------------------------------------------------------

def apply_stage_status(text: str, rows) -> str:
    """Give each <li data-ms="S1 S2"> a status class from the tracker rows it names."""
    by_id = {r["id"]: r for r in rows}

    def sub(m: re.Match) -> str:
        ids = m.group(1).split()
        if not ids or any(i not in by_id for i in ids):
            raise BuildError(f"stage names unknown milestones: {m.group(1)!r}")
        states = [by_id[i]["status"] for i in ids]
        if all(s == "complete" for s in states):
            cls = "is-done"
        elif any(s != "not started" for s in states):
            cls = "is-active"
        else:
            cls = "is-todo"
        return f'<li class="{cls}" data-ms="{esc(m.group(1))}">'

    return re.sub(r'<li data-ms="([A-Z0-9 ]{1,40})">', sub, text)


def mark_active(header: str, page: str) -> str:
    target = f'href="{page}"'
    if target in header:
        header = header.replace(target, f'{target} aria-current="page"', 1)
    return header


def find_image(name: str) -> Path:
    if "/" in name or "\\" in name or name.startswith("."):
        raise BuildError(f"bad image name {name!r}")
    for d in IMAGE_DIRS:
        p = d / name
        if p.is_file():
            return p
    raise BuildError(f"image img/{name} is not in docs/images/ or assets/app-icon/")


def check_repo_links(page: str, text: str) -> None:
    for m in re.finditer(r'href="' + re.escape(REPO_BLOB) + r'([^"#?]+)', text):
        rel = html.unescape(m.group(1))
        if ".." in rel or not (ROOT / rel).is_file():
            raise BuildError(f"{page}: link to missing repository file {rel}")


def check_docs_index(text: str) -> None:
    """Every docs/saas/*.md must be listed on docs.html."""
    missing = [
        p.name
        for p in sorted((ROOT / "docs" / "saas").glob("*.md"))
        if f"{REPO_BLOB}docs/saas/{p.name}" not in text
    ]
    if missing:
        # A new doc should not block the site deploy: warn (GitHub Actions annotation) instead.
        print(f"::warning::docs.html does not list: {', '.join(missing)}", file=sys.stderr)



def page_meta(page: str, text: str) -> tuple[str, str]:
    """The page's <title> and meta description (both required)."""
    title = re.search(r"<title>(.*?)</title>", text, re.S)
    desc = re.search(r'<meta name="description" content="([^"]*)"', text)
    if not title or not desc:
        raise BuildError(f"{page}: needs a <title> and a meta description")
    return plain(title.group(1)), desc.group(1)


def page_url(page: str) -> str:
    return BASE_URL if page == "index.html" else BASE_URL + page


def head_tags(page: str, title: str, desc: str) -> str:
    """Canonical, Open Graph, Twitter card, manifest and theme tags for one page."""
    url = esc(page_url(page))
    image = esc(BASE_URL + "img/" + SOCIAL_IMAGE)
    t = esc(title)
    return (
        f'  <link rel="canonical" href="{url}">\n'
        f'  <link rel="manifest" href="site.webmanifest">\n'
        f'  <link rel="apple-touch-icon" href="img/figcraft-256.png">\n'
        f'  <meta name="theme-color" content="#0f1412">\n'
        f'  <meta property="og:type" content="website">\n'
        f'  <meta property="og:site_name" content="FigCraft Cloud">\n'
        f'  <meta property="og:title" content="{t}">\n'
        f'  <meta property="og:description" content="{desc}">\n'
        f'  <meta property="og:url" content="{url}">\n'
        f'  <meta property="og:image" content="{image}">\n'
        f'  <meta name="twitter:card" content="summary_large_image">\n'
        f'  <meta name="twitter:title" content="{t}">\n'
        f'  <meta name="twitter:description" content="{desc}">\n'
        f'  <meta name="twitter:image" content="{image}">\n'
    )


def write_discovery_files(out_dir: Path, metas: dict[str, tuple[str, str]], updated: str, rows) -> None:
    """sitemap.xml, robots.txt, llms.txt, llms-full.txt and site.webmanifest."""
    urls = "".join(
        f"  <url><loc>{esc(page_url(page))}</loc><lastmod>{esc(updated)}</lastmod>"
        f"<priority>{'1.0' if page == 'index.html' else '0.8'}</priority></url>\n"
        for page in PAGES
    )
    (out_dir / "sitemap.xml").write_text(
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n' + urls + "</urlset>\n",
        encoding="utf-8",
    )
    (out_dir / "robots.txt").write_text(
        f"User-agent: *\nAllow: /\n\nSitemap: {BASE_URL}sitemap.xml\n", encoding="utf-8"
    )
    progress = "\n".join(f"- {r['label']}: {r['status']}, {r['done']}/{r['total']} tasks" for r in rows)
    pages = "\n".join(f"- [{metas[p][0]}]({page_url(p)}): {metas[p][1]}" for p in PAGES)
    docs = "\n".join(
        f"- [{d.split('/')[-1]}]({REPO_BLOB}{d})" for d in LLMS_FULL_DOCS[1:]
    )
    llms = (
        "# FigCraft Cloud\n\n"
        "> Self-hostable, browser-based collaborative interface design in pure Rust: the FigCraft design "
        "editor in the browser with real-time co-editing, every confirmed edit stored in PostgreSQL and "
        "S3-compatible storage, and multi-tenancy, OIDC/SAML sign-in and permissions. An independent, "
        "open-source (MIT OR Apache-2.0) fork of FigCraft; early alpha.\n\n"
        f"Progress as of {updated} (generated from the project tracker):\n\n{progress}\n\n"
        f"## Site\n\n{pages}\n\n"
        f"## Documentation\n\n{docs}\n- [Progress tracker]({REPO_BLOB}docs/saas/tracker.md)\n\n"
        f"## Source\n\n- [Repository](https://github.com/schnsrw/figcraft)\n"
        f"- [Upstream FigCraft](https://github.com/storytold/figcraft)\n\n"
        "## Optional\n\n"
        f"- [Full text of the main documents]({BASE_URL}llms-full.txt)\n"
    )
    (out_dir / "llms.txt").write_text(llms, encoding="utf-8")
    full = [llms, "\n"]
    for doc in LLMS_FULL_DOCS:
        full.append(f"\n\n---\n\n# Source: {REPO_BLOB}{doc}\n\n")
        full.append(read_text(ROOT / doc))
    body = "".join(full)
    if len(body.encode("utf-8")) > MAX_LLMS_FULL_BYTES:
        raise BuildError("llms-full.txt is over its size limit")
    (out_dir / "llms-full.txt").write_text(body, encoding="utf-8")
    manifest = {
        "name": "FigCraft Cloud",
        "short_name": "FigCraft",
        "description": metas["index.html"][1],
        "start_url": "./",
        "scope": "./",
        "display": "browser",
        "background_color": "#0f1412",
        "theme_color": "#0f1412",
        "icons": [
            {"src": "img/figcraft-256.png", "sizes": "256x256", "type": "image/png"},
            {"src": "img/figcraft-512.png", "sizes": "512x512", "type": "image/png"},
        ],
    }
    (out_dir / "site.webmanifest").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")


def not_found_page(header: str, footer: str) -> str:
    return (
        '<!doctype html>\n<html lang="en">\n<head>\n  <meta charset="utf-8">\n'
        '  <meta name="viewport" content="width=device-width, initial-scale=1">\n'
        "  <title>Page not found</title>\n"
        '  <meta name="robots" content="noindex">\n'
        f'  <base href="{BASE_URL}">\n'
        '  <link rel="icon" type="image/png" href="img/figcraft-64.png">\n'
        '  <link rel="stylesheet" href="style.css">\n</head>\n<body>\n'
        + header
        + '\n<main id="main"><section class="band"><div class="wrap section-head">'
        '<h1>Page not found</h1><p>That page does not exist. Try the <a href="./">home page</a> or the '
        '<a href="docs.html">documentation</a>.</p></div></section></main>\n'
        + footer
        + "\n</body>\n</html>\n"
    )


def build(out_dir: Path) -> None:
    tracker = read_text(TRACKER)
    rows = parse_milestones(tracker)
    updated = parse_updated(tracker)
    roadmap = parse_roadmap(read_text(ROADMAP))

    current = next((r for r in rows if r["status"] == "in progress" and r["id"] != "S0"), None)
    current_tasks = parse_tasks(tracker, current["id"]) if current else []
    if current:
        n_done = sum(1 for t in current_tasks if t["done"])
        if n_done != current["done"] or len(current_tasks) != current["total"]:
            raise BuildError(
                f"tracker: {current['id']} table says {current['done']}/{current['total']} "
                f"but its section lists {n_done}/{len(current_tasks)} tasks"
            )

    progress_html = render_progress(rows, roadmap, current_tasks, current["id"] if current else "", updated)
    strip_html = render_strip(rows, updated)

    header = read_text(SITE / "partials" / "header.html")
    footer = read_text(SITE / "partials" / "footer.html").replace("<!-- UPDATED -->", esc(updated))

    if out_dir.exists():
        if not (out_dir / ".figcraft-site").exists() and any(out_dir.iterdir()):
            raise BuildError(f"{out_dir} exists, is not empty and was not made by this script")
        shutil.rmtree(out_dir)
    (out_dir / "img").mkdir(parents=True)
    (out_dir / ".figcraft-site").write_text("built by site/build.py\n", encoding="utf-8")
    (out_dir / ".nojekyll").write_text("", encoding="utf-8")

    images: set[str] = {SOCIAL_IMAGE, "figcraft-256.png", "figcraft-512.png", "figcraft-64.png"}
    metas: dict[str, tuple[str, str]] = {}
    for page in PAGES:
        text = read_text(SITE / page)
        title, desc = page_meta(page, text)
        metas[page] = (title, desc)
        anchor = re.search(r'<meta name="description" content="[^"]*">\n', text)
        if anchor is None:
            raise BuildError(f"{page}: meta description must end its line")
        text = text[: anchor.end()] + head_tags(page, title, desc) + text[anchor.end() :]
        for marker in ("<!-- HEADER -->", "<!-- FOOTER -->"):
            if text.count(marker) != 1:
                raise BuildError(f"{page}: expected exactly one {marker}")
        text = text.replace("<!-- HEADER -->", mark_active(header, page))
        text = text.replace("<!-- FOOTER -->", footer)
        if page == "roadmap.html":
            if text.count("<!-- PROGRESS -->") != 1:
                raise BuildError("roadmap.html: expected exactly one <!-- PROGRESS --> marker")
            text = text.replace("<!-- PROGRESS -->", progress_html)
        text = apply_stage_status(text, rows)
        if "<!-- PROGRESS_STRIP -->" in text:
            text = text.replace("<!-- PROGRESS_STRIP -->", strip_html)
        if page == "index.html" and strip_html not in text:
            raise BuildError("index.html: missing <!-- PROGRESS_STRIP --> marker")
        text = text.replace("<!-- UPDATED -->", esc(updated))
        if page == "docs.html":
            check_docs_index(text)
        check_repo_links(page, text)
        images.update(re.findall(r'(?:src|href)="img/([^"]+)"', text))
        (out_dir / page).write_text(text, encoding="utf-8")

    shutil.copyfile(SITE / "style.css", out_dir / "style.css")
    write_discovery_files(out_dir, metas, updated, rows)
    (out_dir / "404.html").write_text(not_found_page(mark_active(header, ""), footer), encoding="utf-8")
    for name in sorted(images):
        shutil.copyfile(find_image(name), out_dir / "img" / name)

    print(
        f"built {len(PAGES)} pages and {len(images)} images into {out_dir} "
        f"(tracker updated {updated}; "
        + ", ".join(f"{r['label']} {r['done']}/{r['total']}" for r in rows)
        + ")"
    )


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: python3 site/build.py OUTPUT_DIR", file=sys.stderr)
        return 2
    try:
        build(Path(argv[1]).resolve())
    except BuildError as err:
        print(f"site build failed: {err}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
