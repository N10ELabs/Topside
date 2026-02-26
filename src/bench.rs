use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;

use crate::service::AppService;
use crate::types::SearchFilters;

#[derive(Debug, Clone)]
pub struct BenchReport {
    pub iterations: usize,
    pub query: String,
    pub search_p95_ms: f64,
    pub search_p50_ms: f64,
    pub read_p95_ms: f64,
    pub read_p50_ms: f64,
}

#[derive(Debug, Clone)]
pub struct SeedReport {
    pub requested: usize,
    pub created: usize,
    pub corpus_dir: String,
}

pub fn run_bench(service: &AppService, iterations: usize, query: &str) -> Result<BenchReport> {
    let iterations = iterations.max(1);

    let mut search_latencies = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let _ = service.search_context(
            query,
            &SearchFilters {
                entity_type: None,
                project_id: None,
                include_archived: false,
            },
            Some(20),
        )?;
        search_latencies.push(start.elapsed());
    }

    let ids = service.db.all_entity_ids()?;
    let mut read_latencies = Vec::new();
    for id in ids.into_iter().take(iterations) {
        let start = Instant::now();
        let _ = service
            .read_entity(&id)
            .with_context(|| format!("failed bench reading entity {id}"))?;
        read_latencies.push(start.elapsed());
    }

    Ok(BenchReport {
        iterations,
        query: query.to_string(),
        search_p95_ms: percentile_ms(&search_latencies, 95.0),
        search_p50_ms: percentile_ms(&search_latencies, 50.0),
        read_p95_ms: percentile_ms(&read_latencies, 95.0),
        read_p50_ms: percentile_ms(&read_latencies, 50.0),
    })
}

pub fn seed_synthetic_corpus(service: &AppService, count: usize) -> Result<SeedReport> {
    let count = count.max(1);
    let corpus_dir = service.config.notes_dir().join("bench-corpus");
    std::fs::create_dir_all(&corpus_dir)
        .with_context(|| format!("failed creating corpus dir {}", corpus_dir.display()))?;

    let now = Utc::now().to_rfc3339();
    let project_path = service.config.projects_dir().join("prj_bench.md");
    if !project_path.exists() {
        let project = format!(
            "\
---
id: prj_bench
type: project
title: Benchmark Project
status: active
created_at: {now}
updated_at: {now}
---
Synthetic benchmark project used for local performance evaluation.
"
        );
        std::fs::write(&project_path, project)
            .with_context(|| format!("failed writing {}", project_path.display()))?;
    }

    let mut created = 0usize;
    for index in 0..count {
        let note_id = format!("nte_bench_{index:05}");
        let note_path = corpus_dir.join(format!("{note_id}.md"));
        if note_path.exists() {
            continue;
        }

        let note = format!(
            "\
---
id: {note_id}
type: note
title: Benchmark Note {index}
project_id: prj_bench
created_at: {now}
updated_at: {now}
---
Benchmark corpus document {index}. Query token: benchmark-search-token.
Cross-ref [[project:prj_bench]] and repeated benchmark keywords for FTS sampling.
"
        );
        std::fs::write(&note_path, note)
            .with_context(|| format!("failed writing {}", note_path.display()))?;
        created += 1;
    }

    service.reindex_all()?;

    Ok(SeedReport {
        requested: count,
        created,
        corpus_dir: corpus_dir.to_string_lossy().to_string(),
    })
}

fn percentile_ms(values: &[Duration], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }

    let mut sorted = values.to_vec();
    sorted.sort_unstable();

    let rank = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank].as_secs_f64() * 1_000.0
}
