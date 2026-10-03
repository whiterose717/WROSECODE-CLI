#!/usr/bin/env bash
set -euo pipefail
out="${1:-.ctf/reports/benchmark.json}"
mkdir -p "$(dirname "$out")"
start=$(date +%s%N)
tests=$(cargo test --quiet 2>&1)
end=$(date +%s%N)
python3 - "$out" "$start" "$end" "tests/flag_corpus.json" <<'PY'
import json,sys
path,start,end,corpus=sys.argv[1],int(sys.argv[2]),int(sys.argv[3]),sys.argv[4]
with open(corpus, encoding="utf-8") as source:
    cases=len(json.load(source))
with open(path,"w",encoding="utf-8") as target:
    json.dump({"test_latency_ms":(end-start)/1e6,"flag_detection_cases":cases,"status":"pass"},target,indent=2)
PY
echo "$tests"
echo "wrote $out"
