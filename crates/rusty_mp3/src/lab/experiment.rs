//! The experiment runner — pick a brick + variant, run it over the corpus,
//! produce a repeatable [`Report`].
//!
//! A report is pure data: same brick + variant + overrides ⇒ identical numbers,
//! every run, every machine. The `mp3lab` CLI serializes it to
//! `lab-results/<brick>-<variant>.json` so results accumulate and diff over time.

use super::{bricks, metrics::Metrics, quantizer, signals, variant};

/// CLI overrides applied on top of a variant's preset (the "modify on the fly"
/// path — no recompile). Unset fields keep the preset value.
#[derive(Debug, Clone, Copy, Default)]
pub struct Overrides {
    pub step: Option<f32>,
    pub bias: Option<f32>,
}

/// One corpus signal's score.
#[derive(Debug, Clone)]
pub struct Row {
    pub signal: String,
    pub metrics: Metrics,
}

/// The result of one experiment.
#[derive(Debug, Clone)]
pub struct Report {
    pub brick: String,
    pub variant: String,
    /// Human-readable note of the effective parameters used.
    pub params: String,
    pub rows: Vec<Row>,
    /// Corpus-wide summary.
    pub mean: Metrics,
}

/// Run `brick`'s `variant` over the standard corpus.
///
/// Returns `Err` (not a panic) when the brick is not yet runnable, so the CLI can
/// report status instead of crashing on a `todo!()`.
pub fn run(brick_id: &str, variant_name: &str, ov: Overrides) -> Result<Report, String> {
    let brick = bricks::by_id(brick_id)
        .ok_or_else(|| format!("unknown brick '{brick_id}' (try `mp3lab bricks`)"))?;

    match brick.id {
        "N4" => run_quantizer(variant_name, ov),
        other => Err(format!(
            "brick {other} is not runnable yet (status: {}). Next buildable: {}",
            brick.status.name(),
            bricks::next_unbuilt().map(|b| b.id).unwrap_or("—"),
        )),
    }
}

fn run_quantizer(variant_name: &str, ov: Overrides) -> Result<Report, String> {
    let mut cfg = variant::find(quantizer::VARIANTS, variant_name).ok_or_else(|| {
        format!(
            "unknown variant '{variant_name}' for N4. Available: {}",
            variant::names(quantizer::VARIANTS)
        )
    })?;
    if let Some(s) = ov.step {
        cfg.step = s;
    }
    if let Some(b) = ov.bias {
        cfg.bias = b;
    }

    let rows: Vec<Row> = signals::corpus()
        .iter()
        .map(|s| Row {
            signal: s.name.clone(),
            metrics: quantizer::eval(cfg, s),
        })
        .collect();
    let mean = Metrics::mean(&rows.iter().map(|r| r.metrics).collect::<Vec<_>>());

    Ok(Report {
        brick: "N4".to_string(),
        variant: variant_name.to_string(),
        params: format!(
            "step={} bias={} max_level={}",
            cfg.step, cfg.bias, cfg.max_level
        ),
        rows,
        mean,
    })
}

impl Report {
    /// Pretty console table.
    pub fn to_text(&self) -> String {
        let mut s = format!(
            "experiment: {} / {}\nparams: {}\n\n",
            self.brick, self.variant, self.params
        );
        for r in &self.rows {
            s.push_str(&format!("  {:<18} {}\n", r.signal, r.metrics.summary()));
        }
        s.push_str(&format!("  {:<18} {}\n", "── mean ──", self.mean.summary()));
        s
    }

    /// Stable JSON for the results log (hand-rolled — the crate has no serde).
    pub fn to_json(&self) -> String {
        let rows: Vec<String> = self
            .rows
            .iter()
            .map(|r| {
                format!(
                    "    {{\"signal\":\"{}\",\"max_abs_err\":{:.8},\"rmse\":{:.8},\"psnr_db\":{}}}",
                    r.signal,
                    r.metrics.max_abs_err,
                    r.metrics.rmse,
                    r.metrics.psnr_json()
                )
            })
            .collect();
        format!(
            "{{\n  \"brick\": \"{}\",\n  \"variant\": \"{}\",\n  \"params\": \"{}\",\n  \"mean\": {{\"max_abs_err\":{:.8},\"rmse\":{:.8},\"psnr_db\":{}}},\n  \"rows\": [\n{}\n  ]\n}}\n",
            self.brick,
            self.variant,
            self.params,
            self.mean.max_abs_err,
            self.mean.rmse,
            self.mean.psnr_json(),
            rows.join(",\n"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantizer_experiment_is_repeatable() {
        let a = run("N4", "iso", Overrides::default()).unwrap();
        let b = run("N4", "iso", Overrides::default()).unwrap();
        // Bit-identical across runs — the repeatability guarantee.
        assert_eq!(a.to_json(), b.to_json());
        assert_eq!(a.rows.len(), signals::corpus().len());
    }

    #[test]
    fn unbuilt_brick_errs_not_panics() {
        let r = run("L1", "whatever", Overrides::default());
        assert!(r.is_err());
    }

    #[test]
    fn override_changes_result() {
        let preset = run("N4", "iso", Overrides::default()).unwrap();
        let overridden = run(
            "N4",
            "iso",
            Overrides {
                bias: Some(0.0),
                step: None,
            },
        )
        .unwrap();
        assert_ne!(preset.to_json(), overridden.to_json());
    }
}
