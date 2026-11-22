use super::{field, id, km, number, worst_margin, Hop};
use crate::{
    coverage::CoverageOverlay, display, escape, limited, numeric, rows, site_coordinate,
    ExportError,
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    io::Write,
};

#[derive(Clone, Copy, Debug)]
pub enum CoverageOmission {
    NoCompletedRun,
    Incomplete,
    Stale,
    Unavailable,
    RouteChanged,
}

impl CoverageOmission {
    pub fn message(self) -> &'static str {
        match self {
            Self::NoCompletedRun => "No completed coverage run is available.",
            Self::Incomplete => "Coverage is omitted because the run is incomplete or has unresolved cells.",
            Self::Stale => "Coverage is omitted because it was computed for a different route, terrain or settings.",
            Self::Unavailable => "Coverage is omitted because the saved coverage data is unavailable.",
            Self::RouteChanged => "Coverage is omitted because the selected route changed during export.",
        }
    }
}

#[derive(Default)]
pub struct RouteReportOptions<'a> {
    /// UTC ISO-8601 timestamp supplied by the caller.
    pub generated_at: &'a str,
    pub endpoint_coordinates: [Option<[f64; 2]>; 2],
    pub terrain_resolution_m: Option<f64>,
    pub surface_available: Option<bool>,
    pub rf_settings: Option<&'a Value>,
    pub candidate_settings: Option<&'a Value>,
    /// Additional certified paths, each containing ordered site IDs.
    pub client_paths: &'a [Vec<String>],
    pub coverage: Option<&'a CoverageOverlay>,
    pub coverage_omission: Option<CoverageOmission>,
}

pub fn write_route_html(
    output: impl Write,
    route: &Value,
    options: RouteReportOptions<'_>,
) -> Result<(), ExportError> {
    limited(output, |out| render(out, route, options))
}

fn table_start(
    out: &mut dyn Write,
    columns: &[&str],
    caption: Option<&str>,
) -> Result<(), ExportError> {
    write!(out, "<div class=\"table-scroll\"><table>")?;
    if let Some(caption) = caption {
        write!(out, "<caption>{}</caption>", escape(caption))?;
    }
    write!(out, "<thead><tr>")?;
    for column in columns {
        write!(out, "<th scope=\"col\">{}</th>", escape(column))?;
    }
    write!(out, "</tr></thead><tbody>")?;
    Ok(())
}

fn cells(out: &mut dyn Write, values: impl IntoIterator<Item = String>) -> Result<(), ExportError> {
    for value in values {
        write!(out, "<td>{}</td>", escape(&value))?;
    }
    write!(out, "</tr>")?;
    Ok(())
}

fn pairs(out: &mut dyn Write, values: &[(&str, String)]) -> Result<(), ExportError> {
    write!(out, "<dl>")?;
    for (label, value) in values {
        write!(out, "<div><dt>{label}</dt><dd>{}</dd></div>", escape(value))?;
    }
    write!(out, "</dl>")?;
    Ok(())
}

fn ordered_links<'a>(
    links: &'a [Value],
    primary: &'a [Value],
    paths: &'a [Vec<String>],
) -> (Vec<Hop<'a>>, usize) {
    fn key<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
        if a < b {
            (a, b)
        } else {
            (b, a)
        }
    }
    let mut remaining: HashMap<_, _> = links
        .iter()
        .enumerate()
        .map(|(index, link)| (key(id(link, "source_id"), id(link, "target_id")), index))
        .collect();
    let mut ordered = Vec::with_capacity(links.len());
    let mut add_path = |path: Vec<&'a str>| {
        let before = ordered.len();
        for pair in path.windows(2) {
            if let Some(index) = remaining.remove(&key(pair[0], pair[1])) {
                let data = &links[index];
                ordered.push(Hop {
                    data,
                    reversed: pair[0] != id(data, "source_id"),
                });
            }
        }
        ordered.len() - before
    };
    let primary_hops = add_path(primary.iter().map(|site| id(site, "id")).collect());
    for path in paths {
        add_path(path.iter().map(String::as_str).collect());
    }
    for (index, data) in links.iter().enumerate() {
        if remaining.get(&key(id(data, "source_id"), id(data, "target_id"))) == Some(&index) {
            ordered.push(Hop {
                data,
                reversed: false,
            });
        }
    }
    (ordered, primary_hops)
}

fn role(site: &Value) -> &'static str {
    match site["kind"].as_str() {
        Some("endpoint_a") => "Endpoint A",
        Some("endpoint_b") => "Endpoint B",
        Some("client") => "Client",
        _ => "Repeater",
    }
}

fn site_details(site: &Value, nearest: &Value) -> [String; 4] {
    let ground = field(site, "ground_elevation_m", 1, " m");
    let mut mast = field(site, "antenna_height_m", 1, " m");
    if site["height_reference"] == "surface_dom" {
        mast.push_str(" above surface");
    }
    let installation = match (role(site), site["origin"].as_str()) {
        ("Repeater", Some("known")) => "Existing",
        ("Repeater", _) => "Proposed",
        _ => "—",
    };
    let road = match nearest.get(id(site, "id")) {
        Some(Value::Null) => "Beyond mapped road radius".into(),
        value => number(value.and_then(Value::as_f64), 0, " m"),
    };
    [ground, mast, installation.into(), road]
}

fn coordinates(site: &Value) -> Option<[f64; 2]> {
    site_coordinate(site).map(|[lon, lat]| [lat, lon])
}

fn coordinate_label(position: Option<[f64; 2]>) -> String {
    let [lat, lon] = position.map_or([None, None], |coordinate| coordinate.map(Some));
    format!("{}, {}", number(lat, 5, ""), number(lon, 5, ""))
}

fn render(
    out: &mut dyn Write,
    route: &Value,
    options: RouteReportOptions<'_>,
) -> Result<(), ExportError> {
    let alternatives = rows(&route["alternatives"]);
    let active = alternatives.iter().find(|item| {
        !route["active_alternative_id"].is_null() && item["id"] == route["active_alternative_id"]
    });
    let sites = rows(&route["route"]);
    let primary = active.map_or(sites, |item| rows(&item["route"]));
    let (links, primary_hops) = ordered_links(rows(&route["links"]), primary, options.client_paths);
    let empty = Value::Null;
    let nearest = active.map_or(&empty, |item| &item["nearest_road_distance_m"]);
    let rf = options.rf_settings.unwrap_or(&route["rf_settings"]);
    let candidates = options.candidate_settings.unwrap_or(&empty);
    let selected =
        active
            .and_then(|item| item["name"].as_str())
            .unwrap_or(if route["found"] == true {
                "Selected route"
            } else {
                "No route found"
            });
    let stamp = options.generated_at;
    write!(out, "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:\"><title>RF route report</title><style>{}\n{}</style></head><body><main><header><div><h1>RF route report</h1><p>{}</p><p class=\"muted\">Generated <time datetime=\"{}\">{}</time></p></div><button class=\"print\" type=\"button\" onclick=\"window.print()\">Print report</button></header><section><h2>Route summary</h2>",
        include_str!("assets/leaflet.css"), include_str!("assets/report.css"), escape(selected), escape(stamp), escape(&stamp.replace('T', " ").replace("+00:00", " UTC").replace('Z', " UTC")))?;
    let position = |index: usize, kind: &str| {
        options.endpoint_coordinates[index].or_else(|| {
            sites
                .iter()
                .find(|site| site["kind"] == kind)
                .or_else(|| {
                    if index == 0 {
                        primary.first()
                    } else {
                        primary.last()
                    }
                })
                .and_then(coordinates)
        })
    };
    let distance = |hops: &[Hop<'_>]| km(Some(hops.iter().filter_map(Hop::distance).sum()));
    let count = |key: &str| route[key].as_u64().unwrap_or(0);
    pairs(
        out,
        &[
            ("Endpoint A", coordinate_label(position(0, "endpoint_a"))),
            ("Endpoint B", coordinate_label(position(1, "endpoint_b"))),
            (
                "Repeaters",
                format!(
                    "{} total · {} proposed · {} existing",
                    count("router_count"),
                    route["proposed_router_count"]
                        .as_u64()
                        .unwrap_or_else(
                            || count("router_count").saturating_sub(count("existing_router_count"))
                        ),
                    count("existing_router_count")
                ),
            ),
            (
                "Hop count",
                format!("{} total · {primary_hops} primary", links.len()),
            ),
            ("Total link distance", format!("{} km", distance(&links))),
            (
                "Primary path distance",
                format!("{} km", distance(&links[..primary_hops])),
            ),
            (
                "Weakest usable margin",
                number(
                    links
                        .iter()
                        .filter_map(|hop| worst_margin(hop.data))
                        .reduce(f64::min),
                    2,
                    " dB",
                ),
            ),
            (
                "Independent paths",
                format!(
                    "{} / {} requested",
                    count("achieved_path_count"),
                    route["requested_path_count"].as_u64().unwrap_or(1)
                ),
            ),
            (
                "Solver status",
                if route["search_complete"] == true {
                    "Complete"
                } else {
                    "Search incomplete"
                }
                .into(),
            ),
            ("Solver time", field(route, "elapsed_seconds", 2, " s")),
        ],
    )?;
    write!(out, "<h3>Solver diagnostics</h3>")?;
    let mut seen = HashSet::new();
    for item in rows(&route["diagnostics"]).iter().chain(
        active
            .into_iter()
            .flat_map(|item| rows(&item["diagnostics"])),
    ) {
        let message = display(item);
        let first = seen.is_empty();
        if seen.insert(message.clone()) {
            if first {
                write!(out, "<ul>")?;
            }
            write!(out, "<li>{}</li>", escape(&message))?;
        }
    }
    if seen.is_empty() {
        write!(
            out,
            "<p class=\"muted\">No solver diagnostics recorded.</p>"
        )?;
    } else {
        write!(out, "</ul>")?;
    }
    write!(out, "</section>")?;
    assumptions(out, rf, candidates, &links, &options)?;
    let map = map_section(out, route, active, sites, &links, nearest, options.coverage)?;
    if let Some(coverage) = options.coverage {
        coverage.write_section(out)?;
    } else {
        write!(
            out,
            "<p class=\"muted\" id=\"saved-coverage-omission\">{}</p>",
            options
                .coverage_omission
                .unwrap_or(CoverageOmission::NoCompletedRun)
                .message()
        )?;
    }
    write!(out, "<section><h2>Sites</h2>")?;
    table_start(
        out,
        &[
            "Site ID",
            "Role",
            "Latitude",
            "Longitude",
            "Ground elevation",
            "Mast height",
            "Installation",
            "Nearest road",
        ],
        None,
    )?;
    for site in sites {
        write!(out, "<tr>")?;
        cells(
            out,
            [
                display(&site["id"]),
                role(site).into(),
                field(site, "latitude", 5, ""),
                field(site, "longitude", 5, ""),
            ]
            .into_iter()
            .chain(site_details(site, nearest)),
        )?;
    }
    write!(out, "</tbody></table></div></section>")?;
    link_table(out, &links, primary_hops)?;
    write!(out, "<section><h2>Alternatives</h2>")?;
    let mut others = alternatives
        .iter()
        .filter(|item| active.is_none_or(|current| !std::ptr::eq(*item, current)))
        .peekable();
    if others.peek().is_some() {
        table_start(
            out,
            &[
                "Solution",
                "Repeaters",
                "Weakest margin (dB)",
                "Total distance (km)",
                "Paths",
            ],
            None,
        )?;
        for item in others {
            write!(out, "<tr>")?;
            cells(
                out,
                [
                    display(&item["name"]),
                    display(&item["router_count"]),
                    field(item, "minimum_margin_db", 2, ""),
                    km(item["total_link_distance_m"].as_f64()),
                    format!(
                        "{} / {}",
                        display(&item["achieved_path_count"]),
                        display(&item["requested_path_count"])
                    ),
                ],
            )?;
        }
        write!(out, "</tbody></table></div>")?;
    } else {
        write!(out, "<p class=\"muted\">No other solutions recorded.</p>")?;
    }
    write!(out, "</section>")?;
    for (index, hop) in links.iter().enumerate() {
        let index = index + 1;
        write!(
            out,
            "<section class=\"hop\" id=\"hop-{index}\"><h2>Hop {index} · {}</h2>",
            escape(&hop.name())
        )?;
        super::profile::write_profile(
            out,
            hop,
            rf["required_fresnel_clearance"].as_f64().unwrap_or(0.6),
        )?;
        write!(
            out,
            "<details class=\"hop-budget\"><summary>Directional link budget</summary>"
        )?;
        budget(out, hop)?;
        write!(out, "</details></section>")?;
    }
    write!(out, "</main>")?;
    if let Some(map) = map {
        let safe = serde_json::to_string(&map)?.replace('<', "\\u003c");
        // The dashboard fills this slot with the displayed live raster, if available.
        let snapshot = json!([route["job_id"], route["snapshot_version"]])
            .to_string()
            .replace('<', "\\u003c");
        let leaflet = include_str!("assets/leaflet.js");
        let leaflet = leaflet
            .split_once("//# sourceMappingURL=")
            .map_or(leaflet, |(script, _)| script);
        write!(out, "<script id=\"route-map-data\" type=\"application/json\">{safe}</script><script id=\"report-live-coverage\" type=\"application/json\">{snapshot}</script><script>{leaflet}</script><script>{}</script>", include_str!("assets/map.js"))?;
    }
    write!(out, "<script>{}</script>", include_str!("assets/print.js"))?;
    write!(out, "</body></html>")?;
    Ok(())
}

fn assumptions(
    out: &mut dyn Write,
    rf: &Value,
    candidates: &Value,
    links: &[Hop<'_>],
    options: &RouteReportOptions<'_>,
) -> Result<(), ExportError> {
    let surface = options.surface_available.or_else(|| {
        links
            .iter()
            .filter_map(|link| link.data["profile"]["surface_available"].as_bool())
            .reduce(|a, b| a || b)
    });
    write!(out, "<section><h2>Planning assumptions</h2>")?;
    pairs(
        out,
        &[
            ("Frequency", field(rf, "frequency_mhz", 3, " MHz")),
            ("TX power", field(rf, "tx_power_dbm", 1, " dBm")),
            (
                "Receiver sensitivity",
                number(
                    rf["effective_sensitivity_dbm"]
                        .as_f64()
                        .or_else(|| rf["receiver_sensitivity_dbm"].as_f64()),
                    1,
                    " dBm",
                ),
            ),
            ("Fade margin", field(rf, "fade_margin_db", 1, " dB")),
            ("Earth k-factor", field(rf, "k_factor", 3, "")),
            (
                "Fresnel clearance target",
                number(
                    rf["required_fresnel_clearance"].as_f64().map(|n| n * 100.0),
                    1,
                    "%",
                ),
            ),
            (
                "Validation mode",
                if rf["validation_mode"] == "strict_los" {
                    "Strict LOS"
                } else {
                    "RF propagation"
                }
                .into(),
            ),
            (
                "Maximum hop length",
                candidates["maximum_link_distance_m"]
                    .as_f64()
                    .map_or_else(|| "Unlimited".into(), |n| format!("{} km", km(Some(n)))),
            ),
            (
                "Surface data (DOM)",
                match surface {
                    None => "Unknown",
                    Some(true) => "Available",
                    Some(false) => "Unavailable",
                }
                .into(),
            ),
            (
                "Diffraction surface",
                match rf["obstacle_surface"].as_str() {
                    Some("ground") => "Bare-earth terrain",
                    Some("surface") if surface == Some(false) => {
                        "Bare-earth terrain (surface unavailable)"
                    }
                    Some("surface") => "Surface model",
                    _ => "Unknown",
                }
                .into(),
            ),
            (
                "Terrain resolution",
                number(options.terrain_resolution_m, 1, " m"),
            ),
        ],
    )?;
    table_start(
        out,
        &[
            "Antenna role",
            "Gain (dBi)",
            "Feed loss (dB)",
            "Default height (m AGL)",
        ],
        None,
    )?;
    for (label, key) in [
        ("Endpoint A", "endpoint_a"),
        ("Endpoint B", "endpoint_b"),
        ("Repeater", "router"),
    ] {
        let antenna = &rf[key];
        write!(out, "<tr>")?;
        cells(
            out,
            [
                label.into(),
                field(antenna, "gain_dbi", 2, ""),
                field(antenna, "feed_loss_db", 2, ""),
                field(antenna, "height_agl_m", 1, ""),
            ],
        )?;
    }
    write!(out, "</tbody></table></div></section>")?;
    Ok(())
}

pub(super) fn legend(
    out: &mut dyn Write,
    items: impl IntoIterator<Item = (impl AsRef<str>, &'static str, &'static str)>,
    tag: &str,
) -> Result<(), ExportError> {
    write!(out, "<{tag} class=\"legend\">")?;
    for (label, color, shape) in items {
        write!(
            out,
            "<span><i class=\"swatch {shape}\" style=\"--swatch:{color}\"></i>{}</span>",
            escape(label.as_ref())
        )?;
    }
    write!(out, "</{tag}>")?;
    Ok(())
}

const COLORS: [&str; 3] = ["#ae4037", "#aa680e", "#217250"];

fn map_section(
    out: &mut dyn Write,
    route: &Value,
    active: Option<&Value>,
    sites: &[Value],
    hops: &[Hop<'_>],
    nearest: &Value,
    coverage: Option<&CoverageOverlay>,
) -> Result<Option<Value>, ExportError> {
    let positions: HashMap<_, _> = sites
        .iter()
        .filter_map(|site| Some((id(site, "id"), coordinates(site)?)))
        .collect();
    if positions.is_empty() {
        write!(
            out,
            "<section><h2>Route map</h2><p>No site coordinates available.</p></section>"
        )?;
        return Ok(None);
    }
    let markers: Vec<_> = sites.iter().filter_map(|site| {
        let id = id(site, "id");
        let position = positions.get(id)?;
        let [ground, mast, _, road] = site_details(site, nearest);
        Some(json!({"id": id, "coordinates": position, "endpoint": role(site) != "Repeater",
            "details": [role(site).to_string(), coordinate_label(Some(*position)), format!("Ground elevation · {ground}"), format!("Mast height · {mast}"), format!("Nearest road · {road}")]}))
    }).collect();
    let links: Vec<_> = hops
        .iter()
        .filter_map(|hop| {
            let margin = worst_margin(hop.data);
            let color = match margin {
                Some(n) if n < 3.0 => COLORS[0],
                Some(n) if n <= 10.0 => COLORS[1],
                Some(_) => COLORS[2],
                None => "#52675d",
            };
            Some(
                json!({"coordinates": [positions.get(hop.source())?, positions.get(hop.target())?],
            "color": color, "dashed": hop.data["los_clear"] != true}),
            )
        })
        .collect();
    let polylines = rows(&route["road_polylines"]);
    let roads: Vec<_> = active
        .into_iter()
        .flat_map(|item| rows(&item["road_polyline_indices"]))
        .filter_map(|index| polylines.get(usize::try_from(index.as_u64()?).ok()?))
        .filter(|line| {
            rows(line).len() >= 2 && rows(line).iter().all(|point| numeric::<2>(point).is_some())
        })
        .collect();
    write!(out, "<section class=\"map-section\"><h2>Route map</h2><div id=\"route-map\" role=\"region\" aria-label=\"Interactive route map\"></div>")?;
    let mut labels = vec![
        ("Endpoints", "#12395c", "dot"),
        ("Repeaters", "#6737a2", "dot"),
        ("Under 3 dB", COLORS[0], "line"),
        ("3–10 dB", COLORS[1], "line"),
        ("Over 10 dB", COLORS[2], "line"),
        ("Blocked LOS", "#52675d", "dash"),
    ];
    if !roads.is_empty() {
        labels.push(("Nearby roads", "#525c62", "line"));
    }
    legend(out, labels, "div")?;
    write!(out, "<div id=\"live-coverage-legend\" hidden></div>")?;
    if let Some(coverage) = coverage {
        write!(out, "<div id=\"saved-coverage-legend\">")?;
        legend(
            out,
            coverage
                .legend()
                .map(|(label, color)| (label, color, "square")),
            "div",
        )?;
        write!(out, "</div>")?;
    }
    write!(out, "</section>")?;
    Ok(Some(
        json!({"sites": markers, "links": links, "roads": roads,
        "coverage": coverage.map(|overlay| json!({"image": overlay.image(), "bounds": overlay.bounds()}))}),
    ))
}

fn link_table(out: &mut dyn Write, links: &[Hop<'_>], primary: usize) -> Result<(), ExportError> {
    write!(
        out,
        "<section class=\"link-summary\"><h2>Links</h2><p class=\"muted\">"
    )?;
    if primary > 0 {
        write!(out, "Primary route first ({primary} hops). ")?;
    }
    write!(out, "Paired values show forward / reverse.</p>")?;
    table_start(
        out,
        &[
            "Hop",
            "km",
            "Usable margin (dB)",
            "Raw margin (dB)",
            "RX power (dBm)",
            "FSPL (dB)",
            "Diffraction (dB)",
            "Clutter (dB)",
            "LOS",
            "Minimum Fresnel clearance",
            "Minimum clearance (m)",
        ],
        None,
    )?;
    for (index, hop) in links.iter().enumerate() {
        let index = index + 1;
        let class = if worst_margin(hop.data).is_some_and(|margin| margin < 3.0) {
            " class=\"weak-hop\""
        } else {
            ""
        };
        write!(
            out,
            "<tr{class}><th scope=\"row\"><a href=\"#hop-{index}\">{index}. {}</a></th>",
            escape(&hop.name())
        )?;
        let mut values = vec![km(hop.distance())];
        for key in [
            "usable_margin_db",
            "raw_margin_db",
            "received_power_dbm",
            "fspl_db",
            "diffraction_loss_db",
            "clutter_loss_db",
        ] {
            let [a, b] = hop.budgets().map(|dir| field(dir, key, 2, ""));
            values.push(format!("{a} / {b}"));
        }
        let ratio = hop.data["minimum_fresnel_clearance_ratio"].as_f64();
        let clearance = ratio.map_or_else(
            || "—".into(),
            |ratio| {
                format!(
                    "{ratio:.1}% at {} km",
                    km(hop.clearance_distance()),
                    ratio = ratio * 100.0
                )
            },
        );
        values.extend([
            match hop.data["los_clear"].as_bool() {
                Some(true) => "Clear",
                Some(false) => "Blocked",
                None => "—",
            }
            .into(),
            clearance,
            field(hop.data, "minimum_clearance_m", 2, ""),
        ]);
        cells(out, values)?;
    }
    write!(out, "</tbody></table></div></section>")?;
    Ok(())
}

fn budget(out: &mut dyn Write, hop: &Hop<'_>) -> Result<(), ExportError> {
    let model = match hop.data["diffraction_model"].as_str() {
        Some("bullington") => "Bullington",
        Some("delta_bullington") => "Delta-Bullington",
        Some("deygout3") => "Three-edge Deygout",
        _ => "Unavailable",
    };
    write!(out, "<p>Diffraction model · {model}</p>")?;
    table_start(
        out,
        &[
            "Budget item",
            &format!("Forward · {}", hop.name()),
            &format!("Reverse · {} → {}", hop.target(), hop.source()),
        ],
        None,
    )?;
    let directions = hop.budgets();
    for (label, key) in [
        ("TX power (dBm)", "tx_power_dbm"),
        ("TX gain (dBi)", "tx_gain_dbi"),
        ("TX feed loss (dB)", "tx_feed_loss_db"),
        ("FSPL (dB)", "fspl_db"),
        ("Diffraction loss (dB)", "diffraction_loss_db"),
        ("Clutter loss (dB)", "clutter_loss_db"),
        ("Model correction (dB)", "model_correction_db"),
        ("Terminal clutter (dB)", "terminal_clutter_db"),
        ("Terrain sampling allowance (dB)", "step_allowance_db"),
        ("Total path loss (dB)", "total_path_loss_db"),
        ("RX gain (dBi)", "rx_gain_dbi"),
        ("RX feed loss (dB)", "rx_feed_loss_db"),
        ("Miscellaneous loss (dB)", "miscellaneous_loss_db"),
        ("Received power (dBm)", "received_power_dbm"),
        ("Sensitivity (dBm)", "sensitivity_dbm"),
        ("Raw margin (dB)", "raw_margin_db"),
        ("Usable margin (dB)", "usable_margin_db"),
        ("Departure angle (°)", "departure_angle_deg"),
        ("Arrival angle (°)", "arrival_angle_deg"),
    ] {
        write!(out, "<tr>")?;
        let [a, b] = directions.map(|dir| field(dir, key, 2, ""));
        cells(out, [label.into(), a, b])?;
    }
    let [a, b] = directions.map(|dir| {
        number(
            dir["raw_margin_db"]
                .as_f64()
                .zip(dir["usable_margin_db"].as_f64())
                .map(|(raw, usable)| raw - usable),
            2,
            "",
        )
    });
    write!(out, "<tr>")?;
    cells(out, ["Fade margin (dB)".into(), a, b])?;
    let [a, b] = directions.map(|dir| {
        match dir["valid"].as_bool() {
            Some(true) => "Yes",
            Some(false) => "No",
            None => "—",
        }
        .into()
    });
    write!(out, "<tr>")?;
    cells(out, ["Usable".into(), a, b])?;
    write!(out, "</tbody></table></div>")?;
    Ok(())
}
