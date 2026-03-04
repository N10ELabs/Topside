# Performance Harness

Last updated: 2026-02-26

This project includes a reproducible local harness for the V0 performance target.

## Target

- Warm-index `search_context` p95 < 150ms
- Warm-index `read_entity` p95 < 150ms
- Corpus size: ~5,000 markdown files

## Commands

1. Initialize workspace (once):

```bash
topside init /path/to/workspace
```

2. Seed synthetic corpus (5k notes + benchmark project):

```bash
topside --workspace /path/to/workspace seed-bench --count 5000
```

3. Run benchmark:

```bash
topside --workspace /path/to/workspace bench --iterations 1000 --query benchmark-search-token
```

## Convenience Script

From repo root:

```bash
./scripts/bench_5k.sh /path/to/workspace 1000
```

## Output Fields

- `benchmark::search_p50_ms`
- `benchmark::search_p95_ms`
- `benchmark::read_p50_ms`
- `benchmark::read_p95_ms`

## Report Template

Record results with machine details:

- Date:
- OS/Version:
- CPU/RAM:
- Workspace path:
- Corpus count:
- Iterations:
- search_p50_ms:
- search_p95_ms:
- read_p50_ms:
- read_p95_ms:
