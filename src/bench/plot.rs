//! Latency plot rendering — scatter plot with rolling average trendlines.

use super::execute::ExecResult;
use std::collections::BTreeMap;

/// Render a latency-vs-DAG-size scatter plot with rolling average trendlines.
///
/// Replicates the Python plot_bench_latency.py output:
/// - Filters to phase 2 (test molecules) only
/// - Color-coded scatter points per molecule type (30% opacity)
/// - 50-point rolling average trendlines (solid lines)
/// - Per-type avg/p95 annotation text
/// - Grid, legend, axis labels
pub fn render_latency_plot(
    results: &[ExecResult],
    path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use plotters::prelude::*;

    // Color palette matching the Python script
    let type_colors: Vec<(&str, RGBColor)> = vec![
        ("meta", RGBColor(0x21, 0x96, 0xF3)),
        ("value-transfer", RGBColor(0xFF, 0x98, 0x00)),
        ("token-create", RGBColor(0x4C, 0xAF, 0x50)),
        ("token-request", RGBColor(0x9C, 0x27, 0xB0)),
        ("rule", RGBColor(0xF4, 0x43, 0x36)),
        ("burn", RGBColor(0x79, 0x55, 0x48)),
    ];
    let default_color = RGBColor(0x99, 0x99, 0x99);

    // Filter to phase 2 and group by mol_type
    let mut groups: BTreeMap<String, Vec<(usize, u64)>> = BTreeMap::new();
    for r in results {
        if r.phase == 2 {
            groups
                .entry(r.mol_type.clone())
                .or_default()
                .push((r.dag_index, r.latency_ms));
        }
    }

    if groups.is_empty() {
        return Err("No phase 2 results to plot".into());
    }

    // Compute axis bounds
    let max_x = groups
        .values()
        .flatten()
        .map(|(x, _)| *x)
        .max()
        .unwrap_or(1);
    let max_y = groups
        .values()
        .flatten()
        .map(|(_, y)| *y)
        .max()
        .unwrap_or(100);
    let y_upper = ((max_y as f64 * 1.1) as u64).max(10);

    // Create the bitmap backend (2100x900 = 14x6 inches @ 150dpi)
    let root = BitMapBackend::new(path, (2100, 900)).into_drawing_area();
    root.fill(&WHITE)?;

    let mut chart = ChartBuilder::on(&root)
        .caption(
            "Molecule Processing Latency vs DAG Growth",
            ("sans-serif", 28),
        )
        .margin(20)
        .x_label_area_size(50)
        .y_label_area_size(70)
        .build_cartesian_2d(0usize..max_x, 0u64..y_upper)?;

    chart
        .configure_mesh()
        .x_desc("DAG Size (accepted molecules)")
        .y_desc("Latency (ms)")
        .x_label_style(("sans-serif", 20))
        .y_label_style(("sans-serif", 20))
        .axis_desc_style(("sans-serif", 22))
        .light_line_style(RGBColor(220, 220, 220))
        .draw()?;

    // Draw each molecule type
    for (mol_type, points) in &groups {
        let color = type_colors
            .iter()
            .find(|(name, _)| *name == mol_type.as_str())
            .map(|(_, c)| *c)
            .unwrap_or(default_color);

        // Scatter points (30% opacity via mix)
        chart.draw_series(
            points
                .iter()
                .map(|&(x, y)| Circle::new((x, y), 3, color.mix(0.3).filled())),
        )?;

        // 50-point rolling average trendline
        if points.len() >= 50 {
            let mut sorted = points.clone();
            sorted.sort_by_key(|(x, _)| *x);

            let window = 50;
            let mut rolling: Vec<(usize, u64)> = Vec::new();
            for i in 0..sorted.len() {
                let start = if i >= window { i - window + 1 } else { 0 };
                let slice = &sorted[start..=i];
                if slice.len() >= 10 {
                    let avg =
                        slice.iter().map(|(_, y)| *y).sum::<u64>() / slice.len() as u64;
                    rolling.push((sorted[i].0, avg));
                }
            }

            chart
                .draw_series(LineSeries::new(rolling, color.stroke_width(3)))?
                .label(mol_type.as_str())
                .legend(move |(x, y)| {
                    Rectangle::new([(x, y - 5), (x + 20, y + 5)], color.filled())
                });
        } else {
            // Still add legend entry for small series
            chart
                .draw_series(LineSeries::new(
                    std::iter::empty::<(usize, u64)>(),
                    color.stroke_width(3),
                ))?
                .label(mol_type.as_str())
                .legend(move |(x, y)| {
                    Rectangle::new([(x, y - 5), (x + 20, y + 5)], color.filled())
                });
        }
    }

    chart
        .configure_series_labels()
        .position(SeriesLabelPosition::UpperLeft)
        .background_style(WHITE.mix(0.8))
        .border_style(BLACK.mix(0.3))
        .label_font(("sans-serif", 20))
        .draw()?;

    // Draw per-type annotation text (avg, p95) in upper-right area
    let mut anno_y = 60i32;
    for (mol_type, points) in &groups {
        let color = type_colors
            .iter()
            .find(|(name, _)| *name == mol_type.as_str())
            .map(|(_, c)| *c)
            .unwrap_or(default_color);

        let mut latencies: Vec<u64> = points.iter().map(|(_, y)| *y).collect();
        latencies.sort();
        let avg = latencies.iter().sum::<u64>() as f64 / latencies.len() as f64;
        let p95_idx = ((latencies.len() as f64) * 0.95).ceil() as usize;
        let p95 = latencies[p95_idx.min(latencies.len() - 1)];

        let label = format!("{}: avg={:.0}ms, p95={}ms", mol_type, avg, p95);
        root.draw(&Text::new(
            label,
            (1750, anno_y),
            ("sans-serif", 18).into_font().color(&color),
        ))?;
        anno_y += 25;
    }

    root.present()?;
    Ok(())
}
