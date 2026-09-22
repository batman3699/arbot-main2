#!/usr/bin/env bash
set -euo pipefail

python - <<'PY'
from pathlib import Path
import re
import sys

ROOT = Path('config')
url_line_hint = re.compile(r"(https?://|wss?://|\brpc_http\b|\brpc_ws\b)", re.IGNORECASE)
placeholder_patterns = [
    re.compile(r"\.example(?::|/|$)", re.IGNORECASE),
    re.compile(r"\$\{[^}]*\}", re.IGNORECASE),
    re.compile(r"\b(localhost|127\.0\.0\.1|::1)\b", re.IGNORECASE),
]

files = []
for path in ROOT.glob('*'):
    if not path.is_file() or '.example.' in path.name:
        continue
    if path.suffix.lower() not in {'.json', '.json5', '.yaml', '.yml'}:
        continue
    files.append(path)

failures = []
for path in sorted(files):
    text = path.read_text(encoding='utf-8')
    for idx, line in enumerate(text.splitlines(), start=1):
        stripped = line.strip()
        if not stripped or stripped.startswith('#') or not url_line_hint.search(line):
            continue
        for pattern in placeholder_patterns:
            if pattern.search(line):
                failures.append(f"{path}:{idx}: {stripped}")
                break

if failures:
    print('placeholder endpoint lint failed:')
    for failure in failures:
        print(f'  - {failure}')
    sys.exit(1)

print(f'placeholder endpoint lint passed ({len(files)} files scanned)')
PY
