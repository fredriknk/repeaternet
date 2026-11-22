use super::{km, report::legend, Hop};
use crate::{escape, rows, ExportError};
use std::{collections::BTreeSet, io::Write};

pub const MAX_PROFILE_POINTS: usize = 1_000;

struct Line {
    label: String,
    color: &'static str,
    values: Vec<Option<f64>>,
}

pub(super) fn write_profile(
    out: &mut dyn Write,
    hop: &Hop<'_>,
    target: f64,
) -> Result<(), ExportError> {
    let profile = &hop.data["profile"];
    let distances = rows(&profile["distances_m"]);
    let unavailable = |out: &mut dyn Write| -> Result<(), ExportError> {
        write!(
            out,
            "<p class=\"muted\">Terrain profile unavailable for this hop.</p>"
        )?;
        Ok(())
    };
    let Some(length) = hop.distance().filter(|n| *n > 0.0) else {
        return unavailable(out);
    };
    if distances.len() < 2 {
        return unavailable(out);
    }
    let count = distances.len();
    let index = |i: usize| if hop.reversed { count - 1 - i } else { i };
    let read = |key: &str, i| rows(&profile[key]).get(index(i))?.as_f64();
    let distance = |i| hop.orient_distance(read("distances_m", i));
    let line = |label: String, color, values| Line {
        label,
        color,
        values,
    };
    let mut lines = vec![line(
        "Ground + earth bulge".into(),
        "#865631",
        (0..count)
            .map(|i| Some(read("dtm_elevation_m", i)? + read("earth_bulge_m", i)?))
            .collect(),
    )];
    if profile["surface_available"] == true {
        lines.push(line(
            "Surface + earth bulge".into(),
            "#327845",
            (0..count)
                .map(|i| read("effective_obstruction_m", i))
                .collect(),
        ));
    }
    lines.push(line(
        "Line of sight".into(),
        "#24639a",
        (0..count).map(|i| read("los_elevation_m", i)).collect(),
    ));
    lines.push(line(
        format!("{:.0}% Fresnel boundary", target * 100.0),
        "#a5660e",
        (0..count)
            .map(|i| Some(read("los_elevation_m", i)? - target * read("fresnel_radius_m", i)?))
            .collect(),
    ));

    let mut important = BTreeSet::from([0, count - 1]);
    let mut marks = Vec::new();
    let interior = if count > 2 { 1..count - 1 } else { 0..count };
    let minimum = interior
        .filter_map(|i| Some((i, read("clearance_m", i)?)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i);
    let fresnel = hop.clearance_distance().and_then(|position| {
        (0..count)
            .filter_map(|i| Some((i, (distance(i)? - position).abs())))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    });
    for (sample, label, color) in [
        (
            minimum,
            if minimum == fresnel {
                "Minimum clearance and minimum Fresnel clearance"
            } else {
                "Minimum clearance"
            },
            "#b44032",
        ),
        (
            fresnel.filter(|_| minimum != fresnel),
            "Minimum Fresnel clearance",
            "#6437a2",
        ),
    ] {
        if let Some(i) = sample {
            important.insert(i);
            if let Some((d, z)) = distance(i).zip(read("effective_obstruction_m", i)) {
                marks.push((label, color, "dot", d, z));
            }
        }
    }
    for obstacle in rows(&hop.data["dominant_obstacles"]) {
        if let Some((d, z)) = hop
            .orient_distance(obstacle["distance_m"].as_f64())
            .zip(obstacle["elevation_m"].as_f64())
        {
            marks.push(("Diffraction obstacle", "#293843", "diamond", d, z));
        }
    }
    let mut low = f64::INFINITY;
    let mut high = f64::NEG_INFINITY;
    for line in &lines {
        let values = || {
            line.values
                .iter()
                .enumerate()
                .filter_map(|(i, z)| Some((i, z.filter(|n| n.is_finite())?)))
        };
        if let Some((i, z)) = values().min_by(|a, b| a.1.total_cmp(&b.1)) {
            low = low.min(z);
            important.insert(i);
        }
        if let Some((i, z)) = values().max_by(|a, b| a.1.total_cmp(&b.1)) {
            high = high.max(z);
            important.insert(i);
        }
    }
    for &(_, _, _, _, z) in &marks {
        low = low.min(z);
        high = high.max(z);
    }
    if !low.is_finite() || !high.is_finite() {
        return unavailable(out);
    }
    let pad = ((high - low) * 0.06).max(1.0);
    low -= pad;
    high += pad;
    if !(high - low).is_finite() || high <= low {
        return unavailable(out);
    }
    let indices = if count <= MAX_PROFILE_POINTS {
        (0..count).collect()
    } else {
        let remaining = MAX_PROFILE_POINTS - important.len();
        important.extend((0..remaining).map(|i| i * (count - 1) / (remaining - 1)));
        important
    };
    let x = |d: f64| 125.0 + 560.0 * (d / length);
    let y = |z: f64| 230.0 - 210.0 * ((z - low) / (high - low));
    write!(out, "<figure><svg class=\"profile-chart\" viewBox=\"0 0 760 276\" role=\"img\" aria-label=\"Elevation profile for {}\" data-profile-points=\"{}\">", escape(&hop.name()), indices.len())?;
    for i in 0..5 {
        let fraction = f64::from(i) / 4.0;
        let elevation = low + fraction * (high - low);
        let py = y(elevation);
        let d = fraction * length;
        let class = if i % 2 == 1 {
            " class=\"intermediate-tick\""
        } else {
            ""
        };
        write!(out, "<path d=\"M125 {py:.1}H685\" stroke=\"#d5dfda\" vector-effect=\"non-scaling-stroke\"/><text{class} x=\"118\" y=\"{:.1}\" text-anchor=\"end\">{elevation:.1} m</text><text{class} x=\"{:.1}\" y=\"263\" text-anchor=\"middle\">{:.2} km</text>", py + 4.0, x(d), d / 1000.0)?;
    }
    let mut labels = Vec::new();
    for line in &lines {
        labels.push((line.label.as_str(), line.color, "line"));
        let color = line.color;
        write!(out, "<path d=\"")?;
        let mut preceding = None;
        for &i in &indices {
            if let Some((d, z)) = distance(i).zip(line.values[i].filter(|n| n.is_finite())) {
                let connected = preceding.is_some_and(|start| {
                    (start..i).all(|sample: usize| {
                        distance(sample).is_some()
                            && line.values[sample].is_some_and(f64::is_finite)
                    })
                });
                let command = if connected { 'L' } else { 'M' };
                write!(out, "{command}{:.1} {:.1}", x(d), y(z))?;
                preceding = Some(i);
            } else {
                preceding = None;
            }
        }
        write!(
            out,
            "\" fill=\"none\" stroke=\"{color}\" stroke-width=\"2\" vector-effect=\"non-scaling-stroke\"><title>{}</title></path>",
            escape(&line.label)
        )?;
    }
    for (label, color, shape, d, z) in marks {
        let (px, py) = (x(d), y(z));
        let tag = if shape == "diamond" {
            write!(out, "<path d=\"M{px:.1} {:.1}l6 6 -6 6 -6 -6Z\"", py - 6.0)?;
            "path"
        } else {
            write!(out, "<circle cx=\"{px:.1}\" cy=\"{py:.1}\" r=\"4.5\"")?;
            "circle"
        };
        write!(
            out,
            " fill=\"{color}\" stroke=\"white\"><title>{label} at {} km</title></{tag}>",
            km(Some(d))
        )?;
        if !labels.contains(&(label, color, shape)) {
            labels.push((label, color, shape));
        }
    }
    write!(out, "</svg>")?;
    legend(out, labels, "figcaption")?;
    write!(out, "</figure>")?;
    Ok(())
}
